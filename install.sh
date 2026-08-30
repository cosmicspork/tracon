#!/bin/sh
# Install the tracon node binary from the latest GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/cosmicspork/tracon/main/install.sh | sh
#
# Linux x86_64 (static) and macOS on Apple Silicon. Anything else builds from
# source; see the README.
#
# Environment:
#   TRACON_VERSION   a tag like v0.2.0 (default: latest release)
#   TRACON_BIN_DIR   where to put the binary (default: ~/.local/bin)
#   TRACON_ENROLL    an invitation URL: after installing, enroll in the mesh,
#                    then set up the boundary and the service. One line from
#                    a cloud console's user-data to a serving node.
set -eu

repo="cosmicspork/tracon"
bin_dir="${TRACON_BIN_DIR:-$HOME/.local/bin}"

case "$(uname -s)/$(uname -m)" in
  Linux/x86_64 | Linux/amd64) target="x86_64-unknown-linux-musl" ;;
  Darwin/arm64 | Darwin/aarch64) target="aarch64-apple-darwin" ;;
  *)
    echo "tracon: no prebuilt binary for $(uname -s) $(uname -m); build from source:" >&2
    echo "  git clone https://github.com/$repo && cd tracon && just build" >&2
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
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/tracon" | cut -d' ' -f1)"
else
  actual="$(shasum -a 256 "$tmp/tracon" | cut -d' ' -f1)"
fi
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

if [ -n "${TRACON_ENROLL:-}" ]; then
  # Bootstrap: enroll (blocks until the inviter admits, up to ten minutes),
  # then best-effort setup and service install. Install already succeeded, so
  # the non-fatal steps warn and continue rather than abort under set -e.
  echo "tracon: enrolling"
  if ! "$bin_dir/tracon" enroll "$TRACON_ENROLL"; then
    echo "tracon: enroll failed or was not admitted in time; rerun with:" >&2
    echo "  tracon enroll <invitation url>" >&2
    exit 1
  fi
  if ! "$bin_dir/tracon" setup; then
    echo "tracon: setup failed (rootless podman missing?); the node still serves" >&2
    echo "  and relays without a boundary. Fix and rerun: tracon setup" >&2
  fi
  if ! "$bin_dir/tracon" service install; then
    echo "tracon: could not install the service; run the node yourself:" >&2
    echo "  tracon service install   (or: tracon serve)" >&2
  fi
  cat <<EOF

enrolled. next, on this machine:
  tracon check-boundary --deep          prove the boundary
  tracon auth issue --url <https url>   to reach this node from a phone
EOF
  exit 0
fi

cat <<EOF

the desktop app (tray, notifications, and it runs the node for you) is a
separate download from the same release: the .AppImage or .deb on Linux, the
.dmg on macOS. It carries its own copy of this binary.

next, on this machine:
  tracon enroll <invitation url>        join the mesh (run \`tracon mesh invite\` on an enrolled node)
  tracon setup                          create the harness network and gateway (needs rootless podman)
  tracon credential import <file>       seal a credential (or connect a provider on the Nodes screen)
  tracon check-boundary --deep          prove the boundary
  tracon service install                run the node under systemd or launchd
  tracon auth issue                     to reach this node from a phone or another machine
EOF
