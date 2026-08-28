# The hub image. Hub only: no node, no SPA. Pure-Rust TLS-free binary, so the
# runtime needs nothing but CA roots (for nothing today; kept for parity with
# the other homelab images and any future outbound call).

FROM rust:1-slim-bookworm AS builder
# The replica's SQLite is bundled and compiled in.
RUN apt-get update && apt-get install -y --no-install-recommends gcc libc6-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY . .
RUN cargo build -p tracon-hub --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /build/target/release/tracon-hub /usr/local/bin/tracon-hub
ENV TRACON_HUB_ADDR=0.0.0.0:8080 \
    TRACON_HUB_DATA_DIR=/data
EXPOSE 8080
# Runs as nobody (65534). /data is chowned so the image runs standalone; a
# mounted volume's ownership (fsGroup 65534) is the deployer's job.
RUN mkdir -p /data && chown nobody:nogroup /data
USER nobody
CMD ["tracon-hub"]
