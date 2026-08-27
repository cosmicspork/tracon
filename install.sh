#!/bin/sh
# Install the tracon node binary from the latest GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/cosmicspork/tracon/main/install.sh | sh
#
# Linux only (static musl builds for x86_64 and aarch64). macOS builds natively:
#   cargo install --git https://github.com/cosmicspork/tracon tracon
#
# Environment:
#   TRACON_VERSION   a tag like v0.2.0 (default: latest release)
#   TRACON_BIN_DIR   where to put the binary (default: ~/.local/bin)
set -eu

repo="cosmicspork/tracon"
bin_dir="${TRACON_BIN_DIR:-$HOME/.local/bin}"

os="$(uname -s)"
if [ "$os" != "Linux" ]; then
  echo "tracon: prebuilt binaries are Linux only; on $os build natively:" >&2
  echo "  cargo install --git https://github.com/$repo tracon" >&2
  exit 1
fi

case "$(uname -m)" in
  x86_64 | amd64) target="x86_64-unknown-linux-musl" ;;
  aarch64 | arm64) target="aarch64-unknown-linux-musl" ;;
  *)
    echo "tracon: no prebuilt binary for $(uname -m)" >&2
    exit 1
    ;;
esac

if [ -n "${TRACON_VERSION:-}" ]; then
  base="https://github.com/$repo/releases/download/$TRACON_VERSION"
else
  base="https://github.com/$repo/releases/latest/download"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "tracon: fetching tracon-$target"
curl -fsSL -o "$tmp/tracon" "$base/tracon-$target"
curl -fsSL -o "$tmp/checksums.txt" "$base/checksums.txt"

# Verify against the release's checksum line for this target only.
expected="$(grep " tracon-$target\$" "$tmp/checksums.txt" | cut -d' ' -f1)"
if [ -z "$expected" ]; then
  echo "tracon: no checksum for tracon-$target in the release" >&2
  exit 1
fi
actual="$(sha256sum "$tmp/tracon" | cut -d' ' -f1)"
if [ "$expected" != "$actual" ]; then
  echo "tracon: checksum mismatch (expected $expected, got $actual)" >&2
  exit 1
fi

mkdir -p "$bin_dir"
install -m 0755 "$tmp/tracon" "$bin_dir/tracon"
echo "tracon: installed $("$bin_dir/tracon" --version) to $bin_dir/tracon"

case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) echo "tracon: add $bin_dir to your PATH" ;;
esac

cat <<EOF

next, on this machine:
  tracon enroll <invitation url>        join the mesh (run \`tracon mesh invite\` on an enrolled node)
  tracon setup                          create the harness network and gateway (needs rootless podman)
  tracon harness import-credentials     copy a model-credential store into the node-owned volume
  tracon check-boundary --deep          prove the boundary
  tracon serve                          run the node
EOF
