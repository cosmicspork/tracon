#!/usr/bin/env bash
# Assemble the `postgres` or `redis` service bundle for one architecture: the
# server, its clients and everything they load beyond glibc, which a
# sandbox's supervisor runs from a read-only copy on the stock base image.
#
#   scripts/database-bundle.sh --service postgres|redis --arch amd64|arm64 [--out DIR]
#
# Layout, under --out (default target/<service>-bundle):
#
#   <version>/<arch>/run            starts the server on 127.0.0.1 (5432, 6379)
#   <version>/<arch>/bin/           the clients, each wrapped to find lib/:
#                                   psql, pg_dump, pg_restore, pg_isready;
#                                   redis-cli
#   <version>/<arch>/lib/           the libraries they need beyond glibc
#   <version>/<arch>/pg/usr/...     postgres only: the server's tree as the
#                                   image lays it out, which Postgres finds
#                                   its share/ and lib/ in relative to itself
#   <version>/<arch>/libexec/       redis only: the server and the client
#   <version>/<arch>/manifest.json
#   tracon-<service>-<version>-<arch>.tar.gz          entries rooted at <version>/<arch>/
#   tracon-<service>-<version>-<arch>.tar.gz.sha256
#
# <version> is the server's, from the tag on the FROM line of
# containers/<service>/Containerfile, which is only that pin (Dependabot
# moves it within its major version). A catalogue entry points at one
# <version>/<arch>:
#
#   [[service]]
#   name = "postgres"                     # or "redis", port = 6379
#   bundle = "/srv/tracon/postgres/17.10/amd64"
#   port = 5432
#
# Where the data goes. `run` keeps it under $TRACON_SERVICE_DATA when that
# names a writable directory: the node mounts one there
# (/var/lib/tracon-services/<name>) when the session's repository keeps the
# service's data between sessions (`service_volumes` on its [[repo]] entry).
# Otherwise it keeps it in a directory of its own under $TMPDIR, which goes
# with the sandbox: the service starts empty in every session. Postgres runs
# a cluster initialised on first start with the superuser `postgres` and
# trust authentication, reached only on the sandbox's loopback, and stops
# with a fast shutdown, so a kept cluster comes back clean. Redis keeps an
# append-only file and snapshots in a kept directory, and nothing in one that
# is not.
#
# The images are Debian; their own libraries are copied rather than relying
# on the base image's, and every one is held to the glibc of the stock base
# (Ubuntu 24.04, 2.39). Nothing is executed for the target architecture:
# libraries are found by reading ELF headers, so both bundles build on one
# host.
#
# Needs podman, readelf (binutils) and GNU tar.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)

die() { echo "database-bundle: $*" >&2; exit 1; }

service='' arch='' out=''
while [ $# -gt 0 ]; do
  case "$1" in
    --service) service=${2:?}; shift 2 ;;
    --arch) arch=${2:?}; shift 2 ;;
    --out) out=${2:?}; shift 2 ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument $1" ;;
  esac
done

case "$service" in
  postgres|redis) ;;
  *) die "--service must be postgres or redis" ;;
esac
case "$arch" in
  amd64) multiarch=x86_64-linux-gnu; machine='X86-64' ;;
  arm64) multiarch=aarch64-linux-gnu; machine='AArch64' ;;
  *) die "--arch must be amd64 or arm64" ;;
esac

glibc_max=2.39
# Provided by every base image and loaded from there, never from a bundle:
# the loader must match the libc it loads.
system_libs=' libc.so.6 libm.so.6 libdl.so.2 libpthread.so.0 librt.so.1 libresolv.so.2 libutil.so.1 ld-linux-x86-64.so.2 ld-linux-aarch64.so.1 libgcc_s.so.1 '

containerfile="$root/containers/$service/Containerfile"
image=$(sed -nE "s|^FROM (docker\.io/library/$service:[^@ ]+@sha256:[0-9a-f]{64}).*|\1|p" "$containerfile")
[ -n "$image" ] || die "no pinned $service FROM line in $containerfile"
tag=${image#*:}
tag=${tag%@*}
version=${tag%%-*}
[[ "$version" =~ ^[0-9]+(\.[0-9]+){1,2}$ ]] || die "$service tag '$tag' does not start with a version"
major=${version%%.*}

for tool in podman readelf; do
  command -v "$tool" >/dev/null 2>&1 || die "$tool is needed"
done
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

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
else
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
fi

work=$(mktemp -d)
container=''
cleanup() {
  [ -z "$container" ] || podman rm -f "$container" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

case "$service" in
  postgres) wanted=("usr/lib/postgresql/$major/*" "usr/share/postgresql/*" "usr/lib/$multiarch/*" "lib/$multiarch/*") ;;
  redis) wanted=('usr/local/bin/redis-server' 'usr/local/bin/redis-cli' "usr/lib/$multiarch/*" "lib/$multiarch/*") ;;
esac

# The image's files for this architecture, without running anything in it.
podman pull --quiet --platform "linux/$arch" "$image" >/dev/null
container=$(podman create --platform "linux/$arch" "$image")
rootfs="$work/rootfs"
mkdir -p "$rootfs"
podman export "$container" | "$tar" -x -C "$rootfs" --wildcards "${wanted[@]}" 2>/dev/null || true
podman rm -f "$container" >/dev/null
container=''
libdirs=("$rootfs/usr/lib/$multiarch" "$rootfs/lib/$multiarch")

out=${out:-$root/target/$service-bundle}
mkdir -p "$out"
out=$(cd "$out" && pwd)
dest="$out/$version/$arch"
rm -rf "$dest"
mkdir -p "$dest/lib" "$dest/bin"

# A client, wrapped so it finds the bundle's libraries wherever it is.
wrap() {
  local name=$1 target=$2
  cat > "$dest/bin/$name" <<EOF
#!/bin/sh
here=\$(dirname "\$(dirname "\$(readlink -f "\$0")")")
export LD_LIBRARY_PATH="\$here/lib\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"
exec "\$here/$target" "\$@"
EOF
  chmod 0755 "$dest/bin/$name"
}

case "$service" in
  postgres)
    server="$rootfs/usr/lib/postgresql/$major/bin/postgres"
    [ -x "$server" ] || die "$image has no /usr/lib/postgresql/$major/bin/postgres"
    mkdir -p "$dest/pg/usr/lib/postgresql" "$dest/pg/usr/share/postgresql"
    cp -a "$rootfs/usr/lib/postgresql/$major" "$dest/pg/usr/lib/postgresql/$major"
    cp -a "$rootfs/usr/share/postgresql/$major" "$dest/pg/usr/share/postgresql/$major"
    # Links followed, so the tree is files and directories only. Each is
    # read where it is in the image: an absolute one against the image's
    # root, never this host's, and a relative one from its own directory
    # there, since what it names may be outside what the bundle copies.
    find "$dest/pg" -type l | while read -r link; do
      target=$(readlink "$link")
      case "$target" in
        /*) source="$rootfs$target" ;;
        *) source="$(dirname "$rootfs${link#"$dest/pg"}")/$target" ;;
      esac
      if [ -f "$source" ]; then
        cp --remove-destination "$source" "$link"
      else
        echo "database-bundle: dropping ${link#"$dest/"}, a link to $target, which the image does not have" >&2
        rm -f "$link"
      fi
    done
    # The image diverts Debian's postgresql.conf.sample to `.dpkg` and links
    # share/postgresql/<major>/ to its own copy one level up, edited to
    # listen on every address. initdb needs the sample beside the rest of
    # share/; upstream's is the one wanted, since `run` says where to listen.
    sample="$dest/pg/usr/share/postgresql/$major/postgresql.conf.sample"
    if [ -f "$rootfs/usr/share/postgresql/postgresql.conf.sample.dpkg" ]; then
      cp --remove-destination "$rootfs/usr/share/postgresql/postgresql.conf.sample.dpkg" "$sample"
    fi
    for needed in postgresql.conf.sample pg_hba.conf.sample pg_ident.conf.sample postgres.bki; do
      [ -f "$dest/pg/usr/share/postgresql/$major/$needed" ] \
        || die "the bundle has no share/postgresql/$major/$needed, which initdb needs"
    done
    for client in psql pg_dump pg_restore pg_isready; do
      [ -x "$dest/pg/usr/lib/postgresql/$major/bin/$client" ] && wrap "$client" "pg/usr/lib/postgresql/$major/bin/$client"
    done
    elves=("$dest"/pg/usr/lib/postgresql/"$major"/bin/* "$dest"/pg/usr/lib/postgresql/"$major"/lib/*.so)
    ;;
  redis)
    server="$rootfs/usr/local/bin/redis-server"
    [ -x "$server" ] || die "$image has no /usr/local/bin/redis-server"
    mkdir -p "$dest/libexec"
    cp -L "$server" "$dest/libexec/redis-server"
    cp -L "$rootfs/usr/local/bin/redis-cli" "$dest/libexec/redis-cli"
    wrap redis-cli libexec/redis-cli
    elves=("$dest"/libexec/*)
    ;;
esac
readelf -h "$server" | grep -q "Machine:.*$machine" || die "the image's $service is not $arch"

needed() { readelf -d --wide "$1" 2>/dev/null | sed -nE 's/.*\(NEEDED\).*\[(.*)\]$/\1/p'; }
is_elf() { [ "$(head -c 4 "$1" | od -An -c | tr -d ' ')" = '177ELF' ]; }
find_lib() {
  local dir
  for dir in "${libdirs[@]}"; do
    [ -e "$dir/$1" ] && { echo "$dir/$1"; return 0; }
  done
  return 1
}

# Every library the server, its clients and its modules need, beyond glibc,
# copied (links followed) into lib/.
queue=("${elves[@]}")
while [ ${#queue[@]} -gt 0 ]; do
  file=${queue[0]}
  queue=("${queue[@]:1}")
  { [ -f "$file" ] && is_elf "$file"; } || continue
  for lib in $(needed "$file"); do
    case "$system_libs" in *" $lib "*) continue ;; esac
    [ -e "$dest/lib/$lib" ] && continue
    found=$(find_lib "$lib") || die "$(basename "$file") needs $lib, which the image does not have"
    cp -L "$found" "$dest/lib/$lib"
    queue+=("$dest/lib/$lib")
  done
done

# Held to the base's glibc.
newest=$(for file in "${elves[@]}" "$dest"/lib/*.so*; do
  [ -f "$file" ] && is_elf "$file" && readelf -V --wide "$file" 2>/dev/null
done | sed -nE 's/.*Name: GLIBC_([0-9]+\.[0-9]+).*/\1/p' | sort -t. -k1,1n -k2,2n | tail -1)
[ -n "$newest" ] || die "found no glibc version requirement; is readelf working?"
if [ "$(printf '%s\n%s\n' "$newest" "$glibc_max" | sort -t. -k1,1n -k2,2n | tail -1)" != "$glibc_max" ]; then
  die "the bundle needs glibc $newest; the stock base has $glibc_max"
fi

case "$service" in
  postgres)
    cat > "$dest/run" <<EOF
#!/bin/sh
# Postgres from this bundle on 127.0.0.1:5432 only, superuser \`postgres\`,
# trust authentication. Its cluster is kept under \$TRACON_SERVICE_DATA when
# that is a writable directory (the repository keeps it between sessions),
# and otherwise in one that goes with the sandbox. Arguments are passed on
# to the server (\`-c name=value\`).
set -eu
here=\$(dirname "\$(readlink -f "\$0")")
export LD_LIBRARY_PATH="\$here/lib\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"
# GMT is the one zone Postgres knows without the system's zoneinfo, which
# the stock base does not carry.
export TZ=GMT
bin="\$here/pg/usr/lib/postgresql/$major/bin"
kept=\${TRACON_SERVICE_DATA:-}
if [ -n "\$kept" ] && [ -d "\$kept" ] && [ -w "\$kept" ]; then
  data="\$kept/pgdata"
else
  data="\$(mktemp -d "\${TMPDIR:-/tmp}/tracon-postgres.XXXXXX")/pgdata"
fi
mkdir -p "\$data"
# A kept cluster comes back with its modes as the node keeps them.
chmod 0700 "\$data"
if [ ! -s "\$data/PG_VERSION" ]; then
  "\$bin/initdb" -D "\$data" -U postgres --auth=trust --no-locale -E UTF8 --no-sync >/dev/null
fi
# One server runs on it, under this supervisor: a pid file left by one that
# did not stop cleanly is that one's.
rm -f "\$data/postmaster.pid"
sockets=\$(mktemp -d "\${TMPDIR:-/tmp}/tracon-postgres-socket.XXXXXX")
"\$bin/postgres" -D "\$data" \\
  -c listen_addresses=127.0.0.1 \\
  -c port=5432 \\
  -c unix_socket_directories="\$sockets" \\
  -c timezone=GMT \\
  -c log_timezone=GMT \\
  -c dynamic_shared_memory_type=mmap \\
  "\$@" &
server=\$!
# The supervisor's stop is a fast shutdown: open sessions are ended, and the
# cluster is left clean for the next one.
trap 'kill -INT "\$server" 2>/dev/null || true' TERM INT
set +e
wait "\$server"
status=\$?
while kill -0 "\$server" 2>/dev/null; do
  wait "\$server"
  status=\$?
done
exit "\$status"
EOF
    ;;
  redis)
    cat > "$dest/run" <<'EOF'
#!/bin/sh
# Redis from this bundle on 127.0.0.1:6379 only. Its data is kept under
# $TRACON_SERVICE_DATA when that is a writable directory (the repository
# keeps it between sessions: an append-only file and snapshots, written out
# when the supervisor stops it), and otherwise nowhere: it starts empty.
# Arguments are passed on to the server.
set -eu
here=$(dirname "$(readlink -f "$0")")
export LD_LIBRARY_PATH="$here/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
kept=${TRACON_SERVICE_DATA:-}
if [ -n "$kept" ] && [ -d "$kept" ] && [ -w "$kept" ]; then
  exec "$here/libexec/redis-server" --bind 127.0.0.1 --port 6379 --protected-mode yes \
    --dir "$kept" --appendonly yes --save 60 1 "$@"
fi
dir=$(mktemp -d "${TMPDIR:-/tmp}/tracon-redis.XXXXXX")
exec "$here/libexec/redis-server" --bind 127.0.0.1 --port 6379 --protected-mode yes \
  --dir "$dir" --appendonly no --save '' "$@"
EOF
    ;;
esac
chmod 0755 "$dest/run"

# Every value in the manifest passes a pattern, which is what lets it be
# written without a JSON encoder.
{
  printf '{\n  "schema": 1,\n  "service": "%s",\n  "version": "%s",\n  "arch": "%s",\n' "$service" "$version" "$arch"
  printf '  "source": "%s",\n  "run": "run",\n  "files": {\n' "$image"
  first=1
  while IFS= read -r path; do
    [[ "$path" =~ ^[A-Za-z0-9._/+-]+$ ]] || die "unexpected file name $path"
    [ "$first" = 1 ] || printf ',\n'
    first=0
    printf '    "%s": "%s"' "$path" "$(sha256 "$dest/$path")"
  done < <(cd "$dest" && find . -type f ! -name manifest.json | sed 's|^\./||' | LC_ALL=C sort)
  printf '\n  }\n}\n'
} > "$dest/manifest.json"

epoch=${SOURCE_DATE_EPOCH:-$(git -C "$root" log -1 --format=%ct 2>/dev/null || echo 0)}
tarball="tracon-$service-$version-$arch.tar.gz"
"$tar" --sort=name --mtime="@$epoch" --owner=0 --group=0 --numeric-owner \
  --format=posix --pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime \
  -C "$out" -cf - "$version/$arch" | gzip -n -9 > "$out/$tarball"
(cd "$out" && printf '%s  %s\n' "$(sha256 "$tarball")" "$tarball" > "$tarball.sha256")

echo "$dest"
echo "$out/$tarball"
