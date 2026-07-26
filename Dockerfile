# syntax=docker/dockerfile:1.7
FROM rust:1.93-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --locked --release \
    && cp /build/target/release/webdav-filter /tmp/webdav-filter

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        curl \
        libxml2-utils \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home webdav-filter \
    && mkdir -p /etc/webdav-filter /var/lib/webdav-filter \
    && chown -R webdav-filter:webdav-filter /var/lib/webdav-filter
COPY --from=builder /tmp/webdav-filter /usr/local/bin/webdav-filter
USER webdav-filter
EXPOSE 9999
VOLUME ["/var/lib/webdav-filter"]
ENTRYPOINT ["/usr/local/bin/webdav-filter"]
CMD ["--config", "/etc/webdav-filter/config.yml"]
HEALTHCHECK --interval=30s --timeout=5s --retries=3 CMD ["/usr/local/bin/webdav-filter", "--healthcheck", "http://127.0.0.1:9999/-/healthz"]
