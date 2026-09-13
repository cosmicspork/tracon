#!/bin/sh
# Build OpenCode's native web UI from the pinned tag and vendor it as a
# release-time artefact.
#
# The bundle is *not* checked into git: it is 37 MB of build output, and a
# checked-in copy would be a second, unverifiable source of truth for what the
# node serves. What is checked in is this recipe and `DIGEST` — the tree digest
# of the artefact it produces. The node refuses to serve a tree whose digest is
# not that one (`node/src/http/ui.rs`), so the bundle is pinned by content the
# same way the harness binary is pinned by its tarball's sha256.
#
# Why build it at all rather than lift the assets out of the pinned `opencode`
# binary: the binary embeds them in `bunfs` behind `opencode-web-ui.gen.ts`,
# reachable only by running the binary's own catch-all — the same catch-all
# that falls back to `app.opencode.ai` when the import fails
# (`docs/reference/opencode-v1.18.30/api-ui.md` §6, finding 3). tracon serves
# `/` itself and never proxies that route, so it needs the tree, not the route.
#
#   ./containers/opencode-ui/build.sh --src <opencode checkout at v1.18.30>
#
# Options:
#   --src DIR   an existing checkout of anomalyco/opencode at the pinned tag.
#               Required: this recipe does not fetch, so the source it builds
#               from is the operator's to attest.
#   --out DIR   where to install the tree (default: the node's state dir,
#               $XDG_STATE_HOME/tracon/opencode-ui, which is where the node
#               looks unless `[ui] opencode_bundle_dir` says otherwise).
#   --tar DIR   also write opencode-ui-<version>.tar.gz here.
#   --check     print the digest and compare it with DIGEST; install nothing.
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
version=$(sed -n 's/^version=//p' "$here/PINNED")
src=
out=
tar_dir=
check=0

while [ $# -gt 0 ]; do
  case "$1" in
    --src) src=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    --tar) tar_dir=$2; shift 2 ;;
    --check) check=1; shift ;;
    -h|--help) sed -n '2,33p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [ -z "$src" ]; then
  echo "--src <opencode checkout at v$version> is required" >&2
  exit 2
fi
if [ ! -f "$src/packages/app/package.json" ]; then
  echo "$src does not look like an opencode checkout (no packages/app)" >&2
  exit 2
fi

got=$(sed -n 's/^  "version": "\(.*\)",$/\1/p' "$src/packages/app/package.json" | head -1)
if [ "$got" != "$version" ]; then
  echo "checkout is packages/app $got, not the pinned $version" >&2
  exit 2
fi

if [ -z "$out" ]; then
  out=${XDG_STATE_HOME:-$HOME/.local/state}/tracon/opencode-ui
fi

# `--ignore-scripts`: the workspace's postinstall builds native modules for
# `packages/core` (node-pty, tree-sitter grammars) that the browser bundle does
# not link and that need a node-gyp toolchain this recipe should not require.
# Nothing the web build reads is produced by a lifecycle script.
echo "==> bun install (frozen lockfile, no lifecycle scripts)" >&2
(cd "$src" && bun install --frozen-lockfile --ignore-scripts >&2)

echo "==> vite build packages/app" >&2
(cd "$src" && bun run --cwd packages/app build >&2)

dist=$src/packages/app/dist
[ -f "$dist/index.html" ] || { echo "no $dist/index.html after the build" >&2; exit 1; }

# Upstream's own release drops the sourcemaps before embedding
# (`packages/opencode/script/build.ts:33`); shipping them would publish the
# whole app's source on the UI origin for no benefit. 48 MB of the 85 MB.
find "$dist" -name '*.map' -delete
# Cloudflare Pages header rules. Meaningless here and confusing to serve.
rm -f "$dist/_headers"

# The whole point of serving this ourselves (finding 3). A build that acquired
# a reference to the upstream host must not be vendored.
echo "==> checking for upstream hosts" >&2
if grep -rIl -e 'app\.opencode\.ai' "$dist" >/dev/null 2>&1; then
  echo "the built tree references app.opencode.ai; refusing to vendor it" >&2
  grep -rIl -e 'app\.opencode\.ai' "$dist" >&2
  exit 1
fi

# A tree digest in exactly the form `node/src/http/ui.rs` recomputes over what
# it is about to serve: for each file in path order, the path, a NUL, the
# bytes, a NUL.
digest=$(
  cd "$dist" && find . -type f | sed 's|^\./||' | LC_ALL=C sort | while IFS= read -r p; do
    printf '%s\0' "$p"
    cat "$p"
    printf '\0'
  done | sha256sum | cut -d' ' -f1
)

echo "opencode-ui v$version"
echo "files   $(find "$dist" -type f | wc -l | tr -d ' ')"
echo "bytes   $(du -sb "$dist" | cut -f1)"
echo "digest  $digest"

pinned=$(cat "$here/DIGEST" 2>/dev/null || true)
if [ -n "$pinned" ] && [ "$pinned" != "$digest" ]; then
  echo >&2
  echo "DIGEST says $pinned" >&2
  echo "this build  $digest" >&2
  echo "The vendored artefact is pinned by content. A differing digest means a" >&2
  echo "different source, a different toolchain, or a non-reproducible build --" >&2
  echo "settle which before changing DIGEST." >&2
  exit 1
fi

[ "$check" -eq 1 ] && exit 0

echo "==> installing to $out" >&2
rm -rf "$out"
mkdir -p "$(dirname "$out")"
cp -a "$dist" "$out"

if [ -n "$tar_dir" ]; then
  mkdir -p "$tar_dir"
  # Deterministic: sorted names, no owner, a fixed mtime, gzip without the
  # timestamp -- so the tarball's own sha256 is a function of the tree.
  (cd "$dist" && tar --sort=name --owner=0 --group=0 --numeric-owner \
      --mtime='UTC 2020-01-01' -cf - .) | gzip -n -9 \
      > "$tar_dir/opencode-ui-v$version.tar.gz"
  echo "tarball $tar_dir/opencode-ui-v$version.tar.gz"
  echo "tar.gz  $(sha256sum "$tar_dir/opencode-ui-v$version.tar.gz" | cut -d' ' -f1)"
fi
