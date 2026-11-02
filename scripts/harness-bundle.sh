#!/usr/bin/env bash
# Assemble the harness bundle for one architecture: the binaries a sandbox
# runs, which are mounted read-only at /opt/tracon rather than baked into an
# image.
#
#   scripts/harness-bundle.sh --arch amd64|arm64 \
#       (--agent PATH | --agent-release VERSION) \
#       [--sandbox PATH | --sandbox-release VERSION] \
#       [--claude-archive PATH] [--version VERSION] [--out DIR]
#
# Layout, under --out (default target/harness-bundle):
#
#   <version>/<arch>/bin/tracon-agent
#   <version>/<arch>/bin/claude
#   <version>/<arch>/bin/tracon-sandbox     (with --sandbox or --sandbox-release)
#   <version>/<arch>/manifest.json
#   tracon-bundle-<version>-<arch>.tar.gz          entries rooted at <version>/<arch>/
#   tracon-bundle-<version>-<arch>.tar.gz.sha256
#
# Unpacking every tarball into one directory keeps versions and architectures
# side by side; a session mounts one <version>/<arch> at /opt/tracon.
#
# manifest.json names each component with its path, version and sha256, so
# whoever mounts the bundle can check it without trusting the tarball's name:
#
#   {"schema": 1, "version": "...", "arch": "amd64",
#    "components": {"<name>": {"path": "bin/<name>", "version": "...",
#                              "sha256": "...", ...}}}
#
# Sources:
# - tracon-agent: a static build (--agent), or a release's asset checked
#   against that release's own .sha256 (--agent-release), as the native
#   harness image does.
# - claude: the official Claude Code release archive at the version and
#   digests pinned in containers/harness-claude/Containerfile, so the bundle
#   and the image cannot drift apart. --claude-archive supplies the archive
#   from disk; it is held to the same digest.
# - tracon-sandbox: the in-sandbox supervisor, a static build (--sandbox) or
#   a release's asset checked the same way as tracon-agent's
#   (--sandbox-release). Optional, because releases before it shipped have no
#   such asset; the manifest lists only what is there.
#
# The bundle version defaults to the workspace version: tracon-agent and
# tracon-sandbox speak to the node that ships with them.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)

die() { echo "harness-bundle: $*" >&2; exit 1; }

arch='' agent='' agent_release='' sandbox='' sandbox_release='' claude_archive='' version='' out=''
while [ $# -gt 0 ]; do
  case "$1" in
    --arch) arch=${2:?}; shift 2 ;;
    --agent) agent=${2:?}; shift 2 ;;
    --agent-release) agent_release=${2:?}; shift 2 ;;
    --sandbox) sandbox=${2:?}; shift 2 ;;
    --sandbox-release) sandbox_release=${2:?}; shift 2 ;;
    --claude-archive) claude_archive=${2:?}; shift 2 ;;
    --version) version=${2:?}; shift 2 ;;
    --out) out=${2:?}; shift 2 ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument $1" ;;
  esac
done

case "$arch" in
  amd64) triple=x86_64-unknown-linux-musl; claude_asset=x64; elf=x86-64 ;;
  arm64) triple=aarch64-unknown-linux-musl; claude_asset=arm64; elf=aarch64 ;;
  *) die "--arch must be amd64 or arm64" ;;
esac
if [ -n "$agent" ] && [ -n "$agent_release" ]; then
  die "pass one of --agent and --agent-release"
fi
[ -n "$agent" ] || [ -n "$agent_release" ] || die "pass --agent PATH or --agent-release VERSION"
if [ -n "$sandbox" ] && [ -n "$sandbox_release" ]; then
  die "pass one of --sandbox and --sandbox-release"
fi

if [ -z "$version" ]; then
  version=$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version = "\(.*\)"$/\1/p}' "$root/Cargo.toml")
fi
# Every value written into the manifest passes through one of these patterns,
# which is what lets it be built without a JSON encoder.
semver='^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]+)?$'
[[ "$version" =~ $semver ]] || die "bundle version '$version' is not a version"
agent_version=${agent_release:-$version}
[[ "$agent_version" =~ $semver ]] || die "tracon-agent version '$agent_version' is not a version"
sandbox_version=${sandbox_release:-$version}
[[ "$sandbox_version" =~ $semver ]] || die "tracon-sandbox version '$sandbox_version' is not a version"

containerfile="$root/containers/harness-claude/Containerfile"
pinned() { sed -n "s/^ARG $1=\(.*\)$/\1/p" "$containerfile"; }
claude_version=$(pinned CLAUDE_VERSION)
case "$arch" in
  amd64) claude_digest=$(pinned CLAUDE_LINUX_X64_SHA256) ;;
  arm64) claude_digest=$(pinned CLAUDE_LINUX_ARM64_SHA256) ;;
esac
[[ "$claude_version" =~ $semver ]] || die "no CLAUDE_VERSION in $containerfile"
[[ "$claude_digest" =~ ^[0-9a-f]{64}$ ]] || die "no Claude Code digest for $arch in $containerfile"

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
else
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
fi
check_digest() {
  local got
  got=$(sha256 "$1")
  [ "$got" = "$2" ] || die "$1: sha256 $got, expected $2"
}

# The bundle must be byte-for-byte reproducible from the same inputs, which
# needs GNU tar's --sort and --mtime.
tar=${TAR:-}
if [ -z "$tar" ]; then
  for candidate in gtar tar; do
    if command -v "$candidate" >/dev/null 2>&1 && "$candidate" --version 2>/dev/null | grep -q 'GNU tar'; then
      tar=$candidate
      break
    fi
  done
fi
[ -n "$tar" ] || die "GNU tar is needed (gtar on macOS); set TAR to point at it"

# A binary for the wrong architecture, or one that wants a libc the base image
# may not have, would only fail inside a session. `file` is optional; CI has it.
check_static() {
  command -v file >/dev/null 2>&1 || return 0
  local what
  what=$(file -L "$1")
  echo "$what" | grep -q "$elf" || die "$1 is not built for $arch: $what"
  echo "$what" | grep -qE 'statically linked|static-pie linked' || die "$1 is not static: $what"
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

out=${out:-$root/target/harness-bundle}
mkdir -p "$out"
out=$(cd "$out" && pwd)
dest="$out/$version/$arch"
rm -rf "$dest"
mkdir -p "$dest/bin"

# Fetches <name>-<triple> from a release, held to the .sha256 published beside
# it, into $fetched. Not a command substitution: errexit doesn't reach in there.
release_asset() {
  local asset="$1-$triple"
  local base="https://github.com/cosmicspork/tracon/releases/download/v$2"
  curl -fsSL -o "$work/$asset" "$base/$asset"
  curl -fsSL -o "$work/$asset.sha256" "$base/$asset.sha256"
  check_digest "$work/$asset" "$(cut -d' ' -f1 "$work/$asset.sha256")"
  fetched="$work/$asset"
}

if [ -n "$agent_release" ]; then
  release_asset tracon-agent "$agent_release"
  agent=$fetched
fi
[ -f "$agent" ] || die "no tracon-agent at $agent"
check_static "$agent"
install -m 0755 "$agent" "$dest/bin/tracon-agent"

claude_url="https://github.com/anthropics/claude-code/releases/download/v$claude_version/claude-linux-$claude_asset.tar.gz"
if [ -z "$claude_archive" ]; then
  claude_archive="$work/claude.tar.gz"
  curl -fsSL -o "$claude_archive" "$claude_url"
fi
check_digest "$claude_archive" "$claude_digest"
mkdir -p "$work/claude"
tar -xzf "$claude_archive" -C "$work/claude" claude
command -v file >/dev/null 2>&1 && { file "$work/claude/claude" | grep -q "$elf" || die "the Claude Code archive is not $arch"; }
install -m 0755 "$work/claude/claude" "$dest/bin/claude"

if [ -n "$sandbox_release" ]; then
  release_asset tracon-sandbox "$sandbox_release"
  sandbox=$fetched
fi
if [ -n "$sandbox" ]; then
  [ -f "$sandbox" ] || die "no tracon-sandbox at $sandbox"
  check_static "$sandbox"
  install -m 0755 "$sandbox" "$dest/bin/tracon-sandbox"
fi

{
  printf '{\n  "schema": 1,\n  "version": "%s",\n  "arch": "%s",\n  "components": {\n' "$version" "$arch"
  printf '    "claude": {\n'
  printf '      "path": "bin/claude",\n      "version": "%s",\n      "sha256": "%s",\n' \
    "$claude_version" "$(sha256 "$dest/bin/claude")"
  printf '      "source": "%s",\n      "source_sha256": "%s"\n    },\n' "$claude_url" "$claude_digest"
  printf '    "tracon-agent": {\n'
  printf '      "path": "bin/tracon-agent",\n      "version": "%s",\n      "sha256": "%s"\n    }' \
    "$agent_version" "$(sha256 "$dest/bin/tracon-agent")"
  if [ -n "$sandbox" ]; then
    printf ',\n    "tracon-sandbox": {\n'
    printf '      "path": "bin/tracon-sandbox",\n      "version": "%s",\n      "sha256": "%s"\n    }' \
      "$sandbox_version" "$(sha256 "$dest/bin/tracon-sandbox")"
  fi
  printf '\n  }\n}\n'
} > "$dest/manifest.json"

epoch=${SOURCE_DATE_EPOCH:-$(git -C "$root" log -1 --format=%ct 2>/dev/null || echo 0)}
tarball="tracon-bundle-$version-$arch.tar.gz"
"$tar" --sort=name --mtime="@$epoch" --owner=0 --group=0 --numeric-owner \
  --format=posix --pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime \
  -C "$out" -cf - "$version/$arch" | gzip -n -9 > "$out/$tarball"
(cd "$out" && printf '%s  %s\n' "$(sha256 "$tarball")" "$tarball" > "$tarball.sha256")

cat "$dest/manifest.json"
echo "$out/$tarball"
