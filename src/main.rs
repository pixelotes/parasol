//! parasol: a browserless Cloudflare challenge solver that speaks FlareSolverr's `/v1` API.
//!
//! Every request is fetched with cfsolve's fingerprinted client (the same browser profile the
//! solver impersonates). Only when Cloudflare answers with a challenge page does parasol run
//! cfsolve to clear it; the resulting `cf_clearance` and companion cookies go into the shared
//! cookie jar, so later requests to that site are plain HTTP fetches until Cloudflare stops
//! accepting the cookie.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode as HttpStatus;
use axum::routing::{get, post};
use axum::{Json, Router};
use cfsolve::fingerprint::Transport;
use cfsolve::http::{BrowserClient, navigate};
use cfsolve::{Browser, Options, PageDocument, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_TIMEOUT_MS: u64 = 60_000;

struct App {
    browser: Browser,
    /// One fingerprinted client for every fetch. Its cookie jar is the clearance cache.
    client: BrowserClient,
    user_agent: String,
    proxy: Option<String>,
    /// One lock per host, held while solving, so concurrent requests to a site that just
    /// started challenging wait for a single solve instead of each running their own.
    solving: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct V1Request {
    cmd: String,
    url: Option<String>,
    session: Option<String>,
    max_timeout: Option<u64>,
    #[serde(default)]
    cookies: Vec<RequestCookie>,
}

#[derive(Deserialize)]
struct RequestCookie {
    name: String,
    value: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Solution {
    url: String,
    status: u16,
    headers: HashMap<String, String>,
    response: String,
    cookies: Vec<Value>,
    user_agent: String,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Cloudflare's interstitial ("Just a moment...") is served with 403/503 and carries
/// `_cf_chl_opt`; a Turnstile-only page on a 200 is not something parasol clears.
fn is_challenge(page: &PageDocument) -> bool {
    let code = page.status.as_u16();
    (code == 403 || code == 503)
        && (page.html.contains("_cf_chl_opt") || page.html.contains("/cdn-cgi/challenge-platform/"))
}

impl App {
    async fn host_lock(&self, host: &str) -> Arc<Mutex<()>> {
        let mut map = self.solving.lock().await;
        Arc::clone(map.entry(host.to_owned()).or_default())
    }

    /// Fetches `url`, clearing a challenge page at most once.
    async fn fetch(&self, url: &Url) -> Result<(PageDocument, bool), String> {
        let page = navigate(&self.client, url)
            .await
            .map_err(|e| format!("fetching {url}: {e:#}"))?;
        if !is_challenge(&page) {
            return Ok((page, false));
        }

        let host = url.host_str().unwrap_or_default().to_owned();
        let lock = self.host_lock(&host).await;
        let _guard = lock.lock().await;

        // Another request may have cleared this host while we waited for the lock.
        let page = navigate(&self.client, url)
            .await
            .map_err(|e| format!("fetching {url}: {e:#}"))?;
        if !is_challenge(&page) {
            return Ok((page, false));
        }

        eprintln!("[parasol] challenge on {host}, solving");
        let started = Instant::now();
        let options = Options {
            proxy: self.proxy.clone(),
            ..Options::default()
        };
        let cleared = self
            .browser
            .clear(url, &options)
            .await
            .map_err(|e| format!("solving challenge on {host}: {e}"))?;
        for cookie in &cleared.cookies {
            self.client.add_cookie(&cookie.to_string(), url);
        }
        eprintln!(
            "[parasol] cleared {host} in {:.1}s ({} cookies cached)",
            started.elapsed().as_secs_f32(),
            cleared.cookies.len()
        );
        // The solver fetched the protected page with its own client, without the caller's
        // cookies (e.g. EZTV's layout=def_wlinks, which is what makes it list magnets). Fetch it
        // again with the shared client, which now carries both the clearance and those cookies.
        match navigate(&self.client, url).await {
            Ok(page) if !is_challenge(&page) => Ok((page, true)),
            _ => Ok((cleared.page, true)),
        }
    }

    fn cookies_json(&self, url: &Url) -> Vec<Value> {
        self.client
            .cookies(url)
            .iter()
            .map(|c| {
                json!({
                    "name": c.name(),
                    "value": c.value(),
                    "domain": c.domain().unwrap_or_else(|| url.host_str().unwrap_or_default()),
                    "path": c.path().unwrap_or("/"),
                    "httpOnly": c.http_only(),
                    "secure": c.secure(),
                    "expiry": c.expires()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs()),
                })
            })
            .collect()
    }
}

fn reply(status: HttpStatus, body: Value) -> (HttpStatus, Json<Value>) {
    (status, Json(body))
}

fn error(start: u128, message: String) -> (HttpStatus, Json<Value>) {
    eprintln!("[parasol] error: {message}");
    reply(
        HttpStatus::INTERNAL_SERVER_ERROR,
        json!({
            "status": "error",
            "message": message,
            "startTimestamp": start,
            "endTimestamp": now_ms(),
            "version": VERSION,
        }),
    )
}

async fn v1(State(app): State<Arc<App>>, Json(req): Json<V1Request>) -> (HttpStatus, Json<Value>) {
    let start = now_ms();
    let ok = |message: &str, extra: Value| {
        let mut body = json!({
            "status": "ok",
            "message": message,
            "startTimestamp": start,
            "endTimestamp": now_ms(),
            "version": VERSION,
        });
        if let (Value::Object(body), Value::Object(extra)) = (&mut body, extra) {
            body.extend(extra);
        }
        reply(HttpStatus::OK, body)
    };

    match req.cmd.as_str() {
        // parasol keeps one cookie jar for everything, so sessions are accepted and echoed
        // back for compatibility but carry no state of their own.
        "sessions.create" => {
            let session = req.session.unwrap_or_else(|| format!("parasol-{start}"));
            ok("Session created successfully.", json!({ "session": session }))
        }
        "sessions.list" => ok("", json!({ "sessions": [] })),
        "sessions.destroy" => ok("The session has been removed.", json!({})),
        "request.get" => {
            let Some(raw) = req.url else {
                return error(start, "Request parameter 'url' is mandatory in 'request.get' command.".into());
            };
            let url: Url = match raw.parse() {
                Ok(u) => u,
                Err(e) => return error(start, format!("invalid url {raw:?}: {e}")),
            };
            for c in &req.cookies {
                app.client.add_cookie(&format!("{}={}", c.name, c.value), &url);
            }
            let limit = Duration::from_millis(req.max_timeout.unwrap_or(DEFAULT_TIMEOUT_MS));
            let (page, solved) = match tokio::time::timeout(limit, app.fetch(&url)).await {
                Err(_) => return error(start, format!("Error solving the challenge. Timeout after {} seconds.", limit.as_secs())),
                Ok(Err(e)) => return error(start, e),
                Ok(Ok(result)) => result,
            };
            let message = if solved { "Challenge solved!" } else { "Challenge not detected!" };
            let solution = Solution {
                url: page.url.to_string(),
                status: page.status.as_u16(),
                headers: HashMap::new(),
                response: page.html,
                cookies: app.cookies_json(&url),
                user_agent: app.user_agent.clone(),
            };
            ok(message, json!({ "solution": solution }))
        }
        "request.post" => error(start, "request.post is not supported by parasol".into()),
        other => error(start, format!("Request parameter 'cmd' = '{other}' is invalid.")),
    }
}

async fn index(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!({
        "msg": "parasol is ready!",
        "version": VERSION,
        "userAgent": app.user_agent,
    }))
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

fn main() {
    // cfsolve answers Cloudflare's date probes with the zone *name* from /etc/localtime and the
    // *offsets* from chrono's Local, which honours TZ. A TZ that disagrees with /etc/localtime
    // (e.g. the TZ=Europe/Madrid every MangoStack service gets) makes the answers inconsistent
    // and the solve fail. Drop it before any thread exists so both come from /etc/localtime.
    if std::env::var_os("TZ").is_some() {
        // SAFETY: single-threaded at this point; the Tokio runtime is built afterwards.
        unsafe { std::env::remove_var("TZ") };
        eprintln!("[parasol] ignoring TZ; the solver uses the zone in /etc/localtime");
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("building Tokio runtime")
        .block_on(serve());
}

async fn serve() {
    let profile: PathBuf = std::env::var("PARASOL_PROFILE")
        .unwrap_or_else(|_| "/profiles/safari-26-ios.json".into())
        .into();
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8191);
    let host = std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".into());
    let proxy = std::env::var("PROXY").ok().filter(|p| !p.is_empty());

    let browser = Browser::read(&profile)
        .unwrap_or_else(|e| panic!("reading browser profile {}: {e}", profile.display()));
    let transport = Transport::read(&profile)
        .unwrap_or_else(|e| panic!("reading transport from {}: {e:#}", profile.display()));
    let user_agent = transport
        .headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let client = transport
        .client(proxy.as_deref())
        .unwrap_or_else(|e| panic!("building HTTP client: {e:#}"));

    let app = Arc::new(App {
        browser,
        client,
        user_agent,
        proxy,
        solving: Mutex::new(HashMap::new()),
    });
    let router = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/v1", post(v1))
        .with_state(app);

    let addr: SocketAddr = format!("{host}:{port}").parse().expect("valid HOST/PORT");
    let listener = tokio::net::TcpListener::bind(addr).await.expect("binding listener");
    eprintln!("[parasol] {VERSION} listening on {addr} with profile {}", profile.display());
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            // `docker stop` sends SIGTERM; as PID 1 we'd otherwise ignore it.
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("installing SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        })
        .await
        .expect("server error");
}
