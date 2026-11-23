#!/usr/bin/env bash
# Run a database service bundle the way a sandbox's supervisor runs it, on
# the stock base's own base image, and fail unless it answers on loopback
# only, keeps what it wrote in a kept data directory across a stop and a
# start, and starts empty without one.
#
#   scripts/database-bundle-smoke.sh postgres|redis <bundle-dir>
#
# <bundle-dir> is one <version>/<arch> of scripts/database-bundle.sh, for
# this host's architecture. The container mirrors a sandbox: the bundle
# read-only at /opt/tracon-services/<service>, no network, no capabilities,
# no new privileges, the base's unprivileged user. The kept directory is a
# host directory at /var/lib/tracon-services/<service>, as the node sends a
# repository's volume in. SMOKE_PREFIX (default `tracon-`) starts the
# containers' names.
set -euo pipefail

service=${1:?usage: database-bundle-smoke.sh postgres|redis <bundle-dir>}
bundle=${2:?usage: database-bundle-smoke.sh postgres|redis <bundle-dir>}
bundle=$(cd "$bundle" && pwd)
root=$(cd "$(dirname "$0")/.." && pwd)
image=$(sed -nE 's|^FROM ([^ ]+@sha256:[0-9a-f]{64}).*|\1|p' "$root/containers/sandbox-base/Containerfile")
[[ -n $image ]] || { echo "no pinned FROM line in containers/sandbox-base/Containerfile" >&2; exit 1; }
case "$service" in
  postgres) port=5432 ;;
  redis) port=6379 ;;
  *) echo "service must be postgres or redis" >&2; exit 1 ;;
esac
name=${SMOKE_PREFIX:-tracon-}$service-bundle-smoke-$$
target=/var/lib/tracon-services/$service
kept=$(mktemp -d)
chmod 0777 "$kept"
timeout=60

cleanup() {
  podman rm -f -t 0 --ignore "$name" >/dev/null 2>&1 || true
  podman unshare rm -rf "$kept" 2>/dev/null || rm -rf "$kept"
}
trap cleanup EXIT

start() {
  local mounts=(--mount "type=bind,src=$bundle,dst=/opt/tracon-services/$service,ro,relabel=shared")
  if [[ ${1:-} == kept ]]; then
    mounts+=(--mount "type=bind,src=$kept,dst=$target,relabel=shared")
  fi
  podman run -d --name "$name" \
    --network none \
    --cap-drop=ALL --security-opt=no-new-privileges \
    --user 65532:65532 -e HOME=/tmp -e "TRACON_SERVICE_DATA=$target" \
    "${mounts[@]}" \
    "$image" "/opt/tracon-services/$service/run" >/dev/null
  for _ in $(seq "$timeout"); do
    if podman exec "$name" bash -c "exec 3<>/dev/tcp/127.0.0.1/$port" 2>/dev/null; then
      return 0
    fi
    if [[ -z $(podman ps -q --filter "name=^$name\$") ]]; then
      echo "$service exited before it was ready" >&2
      podman logs "$name" >&2 || true
      exit 1
    fi
    sleep 1
  done
  echo "nothing listened on 127.0.0.1:$port within ${timeout}s" >&2
  podman logs "$name" >&2 || true
  exit 1
}

# The supervisor's stop: SIGTERM, and the container goes once `run` exits.
stop() {
  podman stop -t 30 "$name" >/dev/null
  podman rm -f --ignore "$name" >/dev/null
}

client() {
  case "$service" in
    postgres) podman exec "$name" "/opt/tracon-services/postgres/bin/psql" -h 127.0.0.1 -U postgres -tAc "$1" ;;
    # shellcheck disable=SC2086 # the command is words
    redis) podman exec "$name" "/opt/tracon-services/redis/bin/redis-cli" -h 127.0.0.1 $1 ;;
  esac
}
write() {
  case "$service" in
    postgres) client "CREATE TABLE kept (v text); INSERT INTO kept VALUES ('smoke')" >/dev/null ;;
    redis) client "SET kept smoke" >/dev/null ;;
  esac
}
read_back() {
  case "$service" in
    postgres) client "SELECT v FROM kept" 2>/dev/null || true ;;
    redis) client "GET kept" ;;
  esac
}

start kept
echo "ready on 127.0.0.1:$port with a kept data directory"

# shellcheck disable=SC2016 # expanded by the bash inside the container
listeners=$(podman exec "$name" bash -c 'for f in /proc/net/tcp /proc/net/tcp6; do
  while read -r _ local _ state _; do [ "$state" = 0A ] && echo "$local"; done < <(tail -n +2 "$f"); done')
exposed=$(grep -vE '^(0100007F|00000000000000000000000001000000):' <<<"$listeners" || true)
if [[ -n $exposed ]]; then
  echo "listening beyond loopback (hex address:port from /proc/net):" >&2
  echo "$exposed" >&2
  exit 1
fi
echo "listening on loopback only"

uid=$(podman exec "$name" id -u)
[[ $uid != 0 ]] || { echo "$service runs as root" >&2; exit 1; }

write
stop
start kept
got=$(read_back)
[[ $got == smoke ]] || { echo "the kept data did not survive a stop and a start (got '$got')" >&2; podman logs "$name" >&2; exit 1; }
echo "kept what it wrote across a stop and a start"
stop

start
got=$(read_back)
[[ -z $got ]] || { echo "without a kept directory it did not start empty (got '$got')" >&2; exit 1; }
echo "started empty without a kept directory"
stop
