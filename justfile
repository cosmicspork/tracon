# tracon developer entry points. `just` with no recipe runs `check`.

default: check

# Build the SPA into spa/dist (embedded by the node at compile time).
spa:
    cd spa && bun install --frozen-lockfile && bun run build

# Build the release binary with the SPA embedded.
build: spa
    cargo build --release

# Run the node from source against a live-reloading SPA (Vite proxies /api to the node).
dev:
    #!/usr/bin/env sh
    set -e
    trap 'kill 0' EXIT
    cargo run --bin tracon -- serve &
    cd spa && bun run dev

# Everything CI runs.
check:
    cargo fmt --all --check
    cargo clippy --all-targets -- -D warnings
    cargo test
    cd spa && bun run check && bun test

fmt:
    cargo fmt --all

# Build the gateway and harness images this node runs.
images:
    podman build -t localhost/tracon-gateway containers/gateway
    podman build -t localhost/tracon-harness containers/harness

# Create the harness network and gateway the node owns.
setup: images
    cargo run --bin tracon -- setup

# Verify the boundary, including an egress probe from inside it.
boundary:
    cargo run --bin tracon -- check-boundary --deep

# Static Linux binaries. musl because the glibc on a host we do not control is
# not ours to depend on; zig supplies the cross linker without a toolchain per
# target. Requires: brew install zig && cargo install cargo-zigbuild.
cross target="x86_64-unknown-linux-musl": spa
    rustup target add {{target}}
    cargo zigbuild --release --target {{target}}
    @file target/{{target}}/release/tracon
