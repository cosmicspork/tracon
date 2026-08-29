# tracon developer entry points. `just` with no recipe runs `check`.

default: check

# The node image for pod-hosted nodes, and the harness image those pods pull.
node-image tag="dev":
    podman build -f Dockerfile.node -t ghcr.io/cosmicspork/tracon-node:{{tag}} .
    podman build -f containers/harness/Containerfile -t ghcr.io/cosmicspork/tracon-harness:{{tag}} containers/harness
    podman build -f containers/harness-claude/Containerfile -t ghcr.io/cosmicspork/tracon-harness-claude:{{tag}} containers/harness-claude

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
    ./scripts/check-tests.sh
    cargo fmt --all --check
    cargo clippy --all-targets -- -D warnings
    cargo test
    cd spa && bun run check && bun test

fmt:
    cargo fmt --all

# Run a hub from source, in memory. Pass the first node's id to admit it:
# `just hub admit=<node id>`.
hub admit="":
    TRACON_HUB_ADMIT={{admit}} cargo run --bin tracon-hub

# Build the hub image as the release pipeline does.
hub-image:
    podman build -t localhost/tracon-hub .

# Build the gateway and harness images this node runs.
images:
    podman build -t localhost/tracon-gateway containers/gateway
    podman build -t localhost/tracon-harness containers/harness
    podman build -t localhost/tracon-harness-claude containers/harness-claude

# Create the harness network and gateway the node owns.
setup: images
    cargo run --bin tracon -- setup

# Verify the boundary, including an egress probe from inside it.
boundary:
    cargo run --bin tracon -- check-boundary --deep

# Static Linux binaries, as the release ships them. musl because the glibc on
# a host we do not control is not ours to depend on. Needs the musl C toolchain
# (`musl-tools` on Debian, `musl-cross` from Homebrew).
musl: spa
    rustup target add x86_64-unknown-linux-musl
    cargo build --release --target x86_64-unknown-linux-musl --bin tracon --bin tracon-hub
    @file target/x86_64-unknown-linux-musl/release/tracon

# The desktop bundle (AppImage and .deb here, .dmg on a Mac) with the node
# carried inside it as a sidecar, built in the same container as `wrapper`
# (plus `nodejs npm librsvg2-devel fuse` there). Builds the node first.
gui: spa
    cargo build --release --bin tracon
    mkdir -p wrapper/binaries
    cp target/release/tracon wrapper/binaries/tracon-$(rustc -vV | sed -n 's/^host: //p')
    distrobox enter tracon-build -- bash -c 'cd {{justfile_directory()}}/wrapper && APPIMAGE_EXTRACT_AND_RUN=1 NO_STRIP=true npx --yes @tauri-apps/cli@2 build --bundles appimage,deb --config "{\"bundle\":{\"externalBin\":[\"binaries/tracon\"]}}"'

# The desktop wrapper: its own workspace, and it needs webkit and gtk headers.
# On an immutable host, build it in a container that has them:
#   distrobox create --name tracon-build --image registry.fedoraproject.org/fedora:44 --yes
#   distrobox enter tracon-build -- sudo dnf install -y \
#     webkit2gtk4.1-devel gtk3-devel libappindicator-gtk3-devel rust cargo clippy rustfmt
wrapper:
    distrobox enter tracon-build -- bash -c 'cd {{justfile_directory()}}/wrapper && cargo build --release'

wrapper-check:
    distrobox enter tracon-build -- bash -c 'cd {{justfile_directory()}}/wrapper && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test'
