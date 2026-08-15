# syntax=docker/dockerfile:1
# Edge-mode runtime image: the router binary only, built for whatever
# platform Docker is running on (Apple Silicon and x86_64 both work — the
# build is native, no cross toolchain).
#
# This is the root `Dockerfile` minus everything that only the hosted
# deployment needs: no portal SPA (the web plane is off unless
# `ZEROROUTER_PUBLIC_BASE_URL` is set, and an edge box talks to the admin CLI
# instead), no RDS CA bundle (the compose file keeps Postgres on loopback
# inside a shared network namespace — see docker-compose.yml for why), and no
# arm64-only cross-compile arm (the hosted image targets Fargate; this one
# targets your machine). The base images are pinned to the same multi-arch
# digests the root Dockerfile pins.
#
# Build context is the REPO ROOT (docker-compose.yml sets it), so COPY paths
# reach router/.
FROM rust:1.96.1-slim-bookworm@sha256:e18a79fc84dfcfc3ab5ba72290398a644c135c97eaa881447fddc354ee4701a3 AS builder

# git + CA certs: Cargo.toml pins zeroclaw-api/zeroclaw-providers to a git rev.
RUN set -eux; \
    apt-get update; \
    apt-get install --yes --no-install-recommends ca-certificates git; \
    rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY router/Cargo.toml router/Cargo.lock ./
COPY router/src ./src
COPY router/migrations ./migrations
COPY router/config ./config
# Production profile on purpose: the `testing` feature (scriptable upstream
# substitution) must be absent from any binary that can serve real traffic.
RUN cargo build --locked --release

FROM busybox:1.37.0-musl@sha256:222ad6d973c0d198014546a65cd02c5fdedcc172123c5b4c2bf0af636550bd94 AS busybox

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:ce0d66bc0f64aae46e6a03add867b07f42cc7b8799c949c2e898057b7f75a151 AS runtime

COPY --from=builder /build/target/release/zerorouter /usr/local/bin/zerorouter
# busybox provides the healthcheck's wget; distroless ships no shell or tools.
COPY --from=busybox /bin/busybox /bin/busybox
# The shipped catalog is baked at the same path the hosted image uses, as a
# default; the compose file mounts the edge catalog and points
# ZEROROUTER_TIERS_PATH at it instead.
COPY router/config/tiers.toml /etc/zerorouter/tiers.toml

ENV ZEROROUTER_BIND=0.0.0.0:8080
ENV ZEROROUTER_TIERS_PATH=/etc/zerorouter/tiers.toml
EXPOSE 8080

USER 10001:10001
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/bin/busybox", "wget", "--spider", "-q", "-T", "5", "http://127.0.0.1:8080/healthz"]

ENTRYPOINT ["/usr/local/bin/zerorouter"]
