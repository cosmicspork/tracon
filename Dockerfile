# The hub image. Hub only: no node, no SPA. Pure-Rust TLS-free binary, so the
# runtime needs nothing but CA roots (for nothing today; kept for parity with
# the other images and any future outbound call).

FROM docker.io/library/rust@sha256:ebd900bae66fd508b466cef82d64a83a5fb34682e4c8b2797a42908bddc95a57 AS builder
# The replica's SQLite is bundled and compiled in.
# Debian's live suite is mutable; this official snapshot fixes every apt
# dependency selected below.
RUN rm -f /etc/apt/sources.list.d/debian.sources \
    && printf '%s\n' \
      'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/20260901T000000Z bookworm main' \
      'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian-security/20260901T000000Z bookworm-security main' \
      > /etc/apt/sources.list \
    && apt-get -o Acquire::Check-Valid-Until=false update \
    && apt-get install -y --no-install-recommends gcc libc6-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY . .
RUN cargo build -p tracon-hub --release

FROM docker.io/library/debian@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171
# CA certificates are needed before apt can reach the HTTPS snapshot below.
# The base image's default sources are plain HTTP with Release signatures
# verified against the pre-installed debian-archive-keyring, so bootstrapping
# them needs no unverified transport.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -f /etc/apt/sources.list.d/debian.sources \
    && printf '%s\n' \
      'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/20260901T000000Z bookworm main' \
      'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian-security/20260901T000000Z bookworm-security main' \
      > /etc/apt/sources.list \
    && apt-get -o Acquire::Check-Valid-Until=false update \
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
