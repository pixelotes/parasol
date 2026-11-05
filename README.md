# parasol

A browserless Cloudflare challenge solver that speaks
[FlareSolverr](https://github.com/FlareSolverr/FlareSolverr)'s `/v1` API.

parasol wraps [cfsolve](https://github.com/chris124567/cfsolve), which clears
Cloudflare's "Just a moment..." interstitial with plain HTTP requests by
replaying a recorded browser profile. No Chromium, no Xvfb. That makes it
usable on a Raspberry Pi next to the rest of
[MangoStack](https://github.com/pixelotes/MangoStack).

Measured on arm64 (2026-11-05), solving btdirectory, EZTV, Uindex and Tokyo
Toshokan:

| | parasol | FlareSolverr 3.5.2 |
| --- | --- | --- |
| Image | 94 MB | 1.1 GB |
| Idle memory | ~6 MB | ~38 MB |
| Memory after solving | ~190 MB (one at a time), ~270 MB (four at once) | ~720 MB peak |
| First request to a site | ~7 s (solve) | ~12 s |
| Later requests | 0.1–0.6 s (cookie reused) | ~12 s unless the client reuses cookies |

Memory after solving is what glibc keeps from the solve, not a leak; it stays
flat on later requests.

## How it works

- Every request is fetched with cfsolve's fingerprinted HTTP client, which
  presents the same browser (Safari 26 on iOS) the solver impersonates.
- When a site answers with a challenge page, parasol runs cfsolve once for
  that host and stores the resulting `cf_clearance` and companion cookies in a
  shared cookie jar.
- Later requests to that host are ordinary HTTP fetches carrying the cookie,
  so they take milliseconds. parasol only solves again when Cloudflare serves
  the challenge again.
- Concurrent requests to a host that starts challenging wait for a single
  solve instead of each running their own.

## Using it

Any FlareSolverr client works unchanged. For Scarf:

```sh
FLARESOLVERR_URL=http://parasol:8191
```

and enable "Use FlareSolverr" on the indexers that need it.

```sh
docker build -t parasol .
docker run -d --name parasol -p 8191:8191 parasol
```

```sh
curl -s localhost:8191/v1 -H 'Content-Type: application/json' \
  -d '{"cmd":"request.get","url":"https://example.com/","maxTimeout":60000}'
```

## API

`POST /v1` takes FlareSolverr's request body and returns its response shape
(`status`, `message`, `solution.{url,status,response,cookies,userAgent}`).

| `cmd` | Behaviour |
| --- | --- |
| `request.get` | Fetches the URL, clearing a challenge if one is served. `cookies` in the request are added to the jar first. |
| `sessions.create` | Accepted and echoed back. parasol keeps one cookie jar for everything, so sessions carry no state. |
| `sessions.list`, `sessions.destroy` | Accepted, no-op. |
| `request.post` | Not supported. |

`GET /` returns `{"msg", "version", "userAgent"}` like FlareSolverr, and
`GET /health` returns `{"status":"ok"}`.

`solution.headers` is always empty.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `PORT` | `8191` | Listen port (FlareSolverr's default) |
| `HOST` | `0.0.0.0` | Listen address |
| `PARASOL_PROFILE` | `/profiles/safari-26-ios.json` | cfsolve browser profile |
| `PROXY` | unset | `http://`, `https://`, `socks5://` or `socks5h://` proxy for every request |
| `TZ` | ignored | The solver must report the zone in `/etc/localtime` (UTC in the image). A different `TZ` makes Cloudflare's date probes inconsistent, so parasol drops it at startup. |

## Limits

- It clears the interstitial challenge page only. A Turnstile widget embedded
  in an ordinary page (for example on a login form) is returned as-is.
- `cf_clearance` is bound to the IP address and user agent that earned it.
  Consumers must not reuse parasol's cookies from another IP, and should use
  `solution.userAgent` if they do reuse them.
- cfsolve ships one profile. When Cloudflare changes its challenge, solving
  fails until cfsolve updates the profile; bump the pinned revision in
  `Cargo.toml` and `Dockerfile` together.
- parasol relies on cfsolve's hidden transport API (`fingerprint::Transport`,
  `http::navigate`), which is not semver-stable. That is why the revision is
  pinned.
- Do not expose it beyond your LAN. Like FlareSolverr, it fetches any URL it
  is given.

## License

MIT. cfsolve and its patched dependencies keep their own licenses.
