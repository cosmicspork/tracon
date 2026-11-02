#!/usr/bin/env bash
# Smoke-test a sandbox base image with a harness bundle mounted the way a
# session mounts it: read-only at /opt/tracon, no network, as the image's own
# user. Checks the toolchain runs, the session directories are writable, and
# every binary in the bundle matches its manifest digest and starts.
#
#   containers/sandbox-base/smoke.sh IMAGE BUNDLE_DIR
#
# BUNDLE_DIR is one <version>/<arch> directory from scripts/harness-bundle.sh.
# ENGINE picks the container engine (default: podman, else docker).
set -euo pipefail

image=${1:?usage: smoke.sh IMAGE BUNDLE_DIR}
bundle=$(cd "${2:?usage: smoke.sh IMAGE BUNDLE_DIR}" && pwd)
engine=${ENGINE:-$(command -v podman >/dev/null 2>&1 && echo podman || echo docker)}

# `:Z` relabels for SELinux hosts; Docker without SELinux ignores it. The
# script runs inside the container, so it is single-quoted on purpose.
# shellcheck disable=SC2016
"$engine" run --rm --network none -v "$bundle:/opt/tracon:ro,Z" "$image" bash -euo pipefail -c '
  test "$(id -u):$(id -g)" = 65532:65532
  test "$PWD" = /work
  touch /work/.probe /cache/.probe "$HOME/.probe"
  if touch /opt/tracon/.probe 2>/dev/null; then echo "the bundle mount is writable" >&2; exit 1; fi

  git --version
  gcc --version | head -n1
  make --version | head -n1
  node --version
  npm --version
  python3 --version
  uv --version
  uv venv --quiet /tmp/venv
  php --version | head -n1
  modules=$(php -m)
  for ext in bcmath curl gd intl mbstring mysqli pdo_pgsql pdo_sqlite redis xml zip; do
    grep -qx "$ext" <<<"$modules" || { echo "php is missing $ext" >&2; exit 1; }
  done
  composer --version
  jq --version
  rg --version | head -n1
  sqlite3 --version

  cd /opt/tracon
  jq -e ".schema == 1 and .arch == \"$(dpkg --print-architecture)\"" manifest.json >/dev/null
  jq -r ".components | to_entries[] | \"\(.value.sha256)  \(.value.path)\"" manifest.json | sha256sum -c -
  for bin in bin/*; do
    jq -e --arg p "$bin" "[.components[].path] | index(\$p)" manifest.json >/dev/null \
      || { echo "$bin is not in the manifest" >&2; exit 1; }
  done
  tracon-agent --version
  claude --version
  if [ -e bin/tracon-sandbox ]; then tracon-sandbox --version; fi
'
echo "smoke test passed: $image with $bundle"
