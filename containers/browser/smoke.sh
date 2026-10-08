#!/usr/bin/env bash
# Start the browser image the way the node starts a catalogue service, and fail
# unless it answers CDP on the session's loopback and listens nowhere else.
#
#   containers/browser/smoke.sh <image>
#
# The podman arguments mirror `PodmanSpec::sidecar_args` and `probe_args` in
# node/src/runner/podman.rs; keep them in step.
set -euo pipefail

image=${1:?usage: smoke.sh <image>}
run=tracon-browser-smoke-$$
session=$run-session
sidecar=$run-browser
port=9222
timeout=60

# The sidecar first: the session's namespace cannot go while a container is in it.
cleanup() {
  podman rm -f -t 0 --ignore "$sidecar" >/dev/null 2>&1 || true
  podman rm -f -t 0 --ignore "$session" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# The session container's network namespace, on podman's default network the
# way a session's is on the internal one.
podman run -d --rm --name "$session" --entrypoint sleep "$image" infinity >/dev/null

podman run -d --rm --name "$sidecar" \
  --network "container:$session" \
  --cap-drop=ALL --security-opt=no-new-privileges --init \
  -e HTTPS_PROXY=http://127.0.0.1:1 -e HTTP_PROXY=http://127.0.0.1:1 \
  -e NO_PROXY=localhost,127.0.0.1 -e no_proxy=localhost,127.0.0.1 \
  "$image" >/dev/null

# Bash's /dev/tcp rather than curl: the image carries bash, and the probe must
# not go through the proxy.
in_namespace() {
  podman run --rm --network "container:$session" \
    --cap-drop=ALL --security-opt=no-new-privileges \
    --entrypoint bash "$image" -c "$1"
}
get() {
  in_namespace "exec 3<>/dev/tcp/127.0.0.1/$port &&
    printf 'GET $1 HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n' >&3 && { timeout 3 cat <&3 || true; }"
}

version=
for _ in $(seq "$timeout"); do
  if response=$(get /json/version 2>/dev/null) && [[ $response == "HTTP/1.1 200"* ]]; then
    version=$(grep -o '"Browser": *"[^"]*"' <<<"$response" || true)
    break
  fi
  if [[ -z $(podman ps -q --filter "name=^$sidecar\$") ]]; then
    echo "the browser exited before it was ready" >&2
    exit 1
  fi
  sleep 1
done
if [[ -z $version ]]; then
  echo "no CDP answer on 127.0.0.1:$port within ${timeout}s" >&2
  podman logs "$sidecar" >&2 || true
  exit 1
fi
echo "ready: $version"

# A target can be made and listed, so the browser does more than answer.
in_namespace "exec 3<>/dev/tcp/127.0.0.1/$port &&
  printf 'PUT /json/new?about:blank HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n' >&3 && { timeout 3 cat <&3 || true; }" \
  | grep -q '"type": *"page"' || { echo "could not open a page over CDP" >&2; exit 1; }
echo "opened a page"

# Every listening socket in the namespace: the session's own container listens
# on nothing, so anything here is the browser's. Loopback only.
# shellcheck disable=SC2016 # expanded by the bash inside the namespace
listeners=$(in_namespace 'for f in /proc/net/tcp /proc/net/tcp6; do
  awk "NR > 1 && \$4 == \"0A\" { print \$2 }" "$f"; done')
exposed=$(grep -vE '^(0100007F|00000000000000000000000001000000):' <<<"$listeners" || true)
if [[ -n $exposed ]]; then
  echo "listening beyond loopback (hex address:port from /proc/net):" >&2
  echo "$exposed" >&2
  exit 1
fi
echo "listening on loopback only: $(tr '\n' ' ' <<<"$listeners")"

# Not root, so a bug in Chrome lands as an unprivileged user.
uid=$(podman exec "$sidecar" id -u)
if [[ $uid == 0 ]]; then
  echo "the browser runs as root" >&2
  exit 1
fi
echo "runs as uid $uid"
