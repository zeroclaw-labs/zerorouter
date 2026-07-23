# syntax=docker/dockerfile:1
FROM --platform=$BUILDPLATFORM rust:1.96.1-slim-bookworm@sha256:e18a79fc84dfcfc3ab5ba72290398a644c135c97eaa881447fddc354ee4701a3 AS builder

ARG BUILDARCH
ARG TARGETARCH

RUN set -eux; \
    test "${TARGETARCH}" = "arm64"; \
    apt-get update; \
    if [ "${BUILDARCH}" = "${TARGETARCH}" ]; then \
        apt-get install --yes --no-install-recommends ca-certificates git; \
    else \
        test "${BUILDARCH}" = "amd64"; \
        apt-get install --yes --no-install-recommends \
            ca-certificates \
            gcc-aarch64-linux-gnu \
            git \
            libc6-dev-arm64-cross; \
        rustup target add aarch64-unknown-linux-gnu; \
    fi; \
    rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY config ./config
RUN set -eux; \
    if [ "${BUILDARCH}" = "${TARGETARCH}" ]; then \
        cargo build --locked --release; \
        cp target/release/zerorouter /build/zerorouter; \
    else \
        CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
        AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar \
        CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
            cargo build --locked --release --target aarch64-unknown-linux-gnu; \
        cp target/aarch64-unknown-linux-gnu/release/zerorouter /build/zerorouter; \
    fi

FROM --platform=$BUILDPLATFORM debian:bookworm-slim@sha256:60eac759739651111db372c07be67863818726f754804b8707c90979bda511df AS certificates

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && curl --fail --location --silent --show-error \
        --output /rds-global-bundle.pem \
        https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem \
    && echo "e5bb2084ccf45087bda1c9bffdea0eb15ee67f0b91646106e466714f9de3c7e3  /rds-global-bundle.pem" \
        | sha256sum --check --status \
    && chmod 0444 /rds-global-bundle.pem

FROM busybox:1.37.0-musl@sha256:222ad6d973c0d198014546a65cd02c5fdedcc172123c5b4c2bf0af636550bd94 AS busybox

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:ce0d66bc0f64aae46e6a03add867b07f42cc7b8799c949c2e898057b7f75a151 AS runtime

COPY --from=builder /build/zerorouter /usr/local/bin/zerorouter
COPY --from=certificates /rds-global-bundle.pem /etc/zerorouter/rds-global-bundle.pem
COPY --from=busybox /bin/busybox /bin/busybox
COPY config/tiers.toml /etc/zerorouter/tiers.toml

ENV ZEROROUTER_BIND=0.0.0.0:8080
ENV ZEROROUTER_TIERS_PATH=/etc/zerorouter/tiers.toml
EXPOSE 8080

USER 10001:10001
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/bin/busybox", "wget", "--spider", "-q", "-T", "5", "http://127.0.0.1:8080/healthz"]

ENTRYPOINT ["/usr/local/bin/zerorouter"]
