# syntax=docker/dockerfile:1
# 构建：docker build -f deploy/Dockerfile -t drive-relay:0.1 .
FROM rust:1.96-slim AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock* ./
COPY crates ./crates
ENV CARGO_NET_RETRY=5
RUN cargo build --release -p drive-relay

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 10001 drive && mkdir -p /data && chown drive /data
COPY --from=builder /src/target/release/drive-relay /usr/local/bin/drive-relay
USER drive
VOLUME ["/data"]
EXPOSE 8787
HEALTHCHECK --interval=30s --timeout=3s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8787/healthz || exit 1
ENTRYPOINT ["drive-relay"]
CMD ["--bind", "0.0.0.0:8787", "--vault-dir", "/data/vault"]
