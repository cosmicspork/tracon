#!/usr/bin/env bash
# Run the browser service bundle the way a sandbox's supervisor runs it, on
# the stock base's own base image, and fail unless it answers CDP on loopback
# and listens nowhere else.
#
#   containers/browser/bundle-smoke.sh <bundle-dir>
#
# <bundle-dir> is one <version>/<arch> of scripts/browser-bundle.sh, for this
# host's architecture. The image is the FROM line of
# containers/sandbox-base/Containerfile: Ubuntu with nothing added, so a
# library the bundle forgot shows here rather than being found in the base.
# The container mirrors a sandbox: the bundle read-only at
# /opt/tracon-services/browser, no capabilities, no new privileges, the base's
# unprivileged user. SMOKE_PREFIX (default `tracon-`) starts the container's
# name.
set -euo pipefail

bundle=${1:?usage: bundle-smoke.sh <bundle-dir>}
bundle=$(cd "$bundle" && pwd)
root=$(cd "$(dirname "$0")/../.." && pwd)
image=$(sed -nE 's|^FROM ([^ ]+@sha256:[0-9a-f]{64}).*|\1|p' "$root/containers/sandbox-base/Containerfile")
[[ -n $image ]] || { echo "no pinned FROM line in containers/sandbox-base/Containerfile" >&2; exit 1; }
name=${SMOKE_PREFIX:-tracon-}browser-bundle-smoke-$$
port=9222
timeout=60

cleanup() { podman rm -f -t 0 --ignore "$name" >/dev/null 2>&1 || true; }
trap cleanup EXIT

podman run -d --rm --name "$name" \
  --network none \
  --cap-drop=ALL --security-opt=no-new-privileges \
  --user 65532:65532 -e HOME=/tmp \
  --mount "type=bind,src=$bundle,dst=/opt/tracon-services/browser,ro,relabel=shared" \
  "$image" /opt/tracon-services/browser/run >/dev/null

# Bash's /dev/tcp in the container itself: the base has bash and no curl.
in_sandbox() { podman exec "$name" bash -c "$1"; }
get() {
  in_sandbox "exec 3<>/dev/tcp/127.0.0.1/$port &&
    printf '$1 HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n' >&3 && { timeout 5 cat <&3 || true; }"
}

version=
for _ in $(seq "$timeout"); do
  if response=$(get 'GET /json/version' 2>/dev/null) && [[ $response == "HTTP/1.1 200"* ]]; then
    version=$(grep -o '"Browser": *"[^"]*"' <<<"$response" || true)
    break
  fi
  if [[ -z $(podman ps -q --filter "name=^$name\$") ]]; then
    echo "the browser exited before it was ready" >&2
    podman logs "$name" >&2 || true
    exit 1
  fi
  sleep 1
done
if [[ -z $version ]]; then
  echo "no CDP answer on 127.0.0.1:$port within ${timeout}s" >&2
  podman logs "$name" >&2 || true
  exit 1
fi
echo "ready: $version"

get 'PUT /json/new?about:blank' | grep -q '"type": *"page"' \
  || { echo "could not open a page over CDP" >&2; podman logs "$name" >&2; exit 1; }
echo "opened a page"

# shellcheck disable=SC2016 # expanded by the bash inside the container
listeners=$(in_sandbox 'for f in /proc/net/tcp /proc/net/tcp6; do
  while read -r _ local _ state _; do [ "$state" = 0A ] && echo "$local"; done < <(tail -n +2 "$f"); done')
exposed=$(grep -vE '^(0100007F|00000000000000000000000001000000):' <<<"$listeners" || true)
if [[ -n $exposed ]]; then
  echo "listening beyond loopback (hex address:port from /proc/net):" >&2
  echo "$exposed" >&2
  exit 1
fi
echo "listening on loopback only: $(tr '\n' ' ' <<<"$listeners")"

uid=$(in_sandbox 'id -u')
[[ $uid != 0 ]] || { echo "the browser runs as root" >&2; exit 1; }
echo "runs as uid $uid"
