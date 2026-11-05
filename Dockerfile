# Pinned to the same cfsolve revision as Cargo.toml; the browser profile ships
# with the solver and must match it.
ARG CFSOLVE_REV=59aef833d271dfdb86b0330c16f737b7b5010d52

FROM rust:1.98-slim-bookworm AS build
ARG CFSOLVE_REV
# cmake, clang, Go and perl build the patched BoringSSL that cfsolve's
# transport uses for browser TLS fingerprints.
RUN apt-get update && apt-get install -y --no-install-recommends \
        cmake clang libclang-dev g++ make perl git golang pkg-config ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock* rust-toolchain.toml ./
COPY src src
RUN cargo build --release --locked
RUN git clone --quiet https://github.com/chris124567/cfsolve /cfsolve \
    && git -C /cfsolve checkout --quiet "$CFSOLVE_REV"

# glibc + libstdc++ are needed by BoringSSL; no shell or package manager.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/parasol /parasol
COPY --from=build /cfsolve/profiles /profiles
# cfsolve answers Cloudflare's date probes from the host time zone, read from
# /etc/localtime and /etc/timezone. distroless ships neither; use UTC.
COPY --from=build /usr/share/zoneinfo/Etc/UTC /etc/localtime
COPY --from=build /etc/timezone /etc/timezone
ENV PARASOL_PROFILE=/profiles/safari-26-ios.json
# glibc otherwise grows one malloc arena per thread, which balloons RSS on a
# multi-threaded runtime. With one arena plus the malloc_trim parasol calls
# after each solve, idle memory after solving drops from ~160 MB to ~80 MB.
ENV MALLOC_ARENA_MAX=1
EXPOSE 8191
ENTRYPOINT ["/parasol"]
