#!/usr/bin/env bash
# Assemble the `browser` service bundle for one architecture: headless Chrome
# and everything it loads beyond glibc, which a sandbox's supervisor runs from
# a read-only copy on the stock base image.
#
#   scripts/browser-bundle.sh --arch amd64|arm64 [--out DIR]
#
# Layout, under --out (default target/browser-bundle):
#
#   <version>/<arch>/run                   starts Chrome, CDP on 127.0.0.1:9222
#   <version>/<arch>/headless-shell/       the upstream build, as shipped
#   <version>/<arch>/lib/                  the libraries it needs beyond glibc
#   <version>/<arch>/fonts/                DejaVu and a fonts.conf naming them
#   <version>/<arch>/manifest.json
#   tracon-browser-<version>-<arch>.tar.gz          entries rooted at <version>/<arch>/
#   tracon-browser-<version>-<arch>.tar.gz.sha256
#
# <version> is Chrome's. A catalogue entry runs the release's, which the node
# fetches and checks itself (node/src/service_bundle.rs reads <version> from
# the same FROM line at build time), or points at one <version>/<arch>:
#
#   [[service]]
#   name = "browser"
#   bundle = "release"                    # or "/path/to/<version>/<arch>"
#   port = 9222
#   ready = "/json/version"
#
# Source: the chromedp/headless-shell image named, by tag and index digest, on
# the FROM line of containers/browser/Containerfile, which is only that pin
# (Dependabot moves it; node/build.rs reads it too). It is a Debian
# image; its own libraries are copied rather than relying on the base image's,
# and every one is held to the glibc of the stock base (Ubuntu 24.04, 2.39),
# so a Debian bump that needs a newer glibc fails here and not in a session.
# Nothing is executed for the target architecture: libraries are found by
# reading ELF headers, so both bundles build on one host.
#
# Needs podman, readelf (binutils) and GNU tar.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)

die() { echo "browser-bundle: $*" >&2; exit 1; }

arch='' out=''
while [ $# -gt 0 ]; do
  case "$1" in
    --arch) arch=${2:?}; shift 2 ;;
    --out) out=${2:?}; shift 2 ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument $1" ;;
  esac
done

case "$arch" in
  amd64) multiarch=x86_64-linux-gnu; machine='X86-64' ;;
  arm64) multiarch=aarch64-linux-gnu; machine='AArch64' ;;
  *) die "--arch must be amd64 or arm64" ;;
esac

# The stock base's glibc: nothing in the bundle may need a newer one.
glibc_max=2.39
# Provided by every base image and loaded from there, never from a bundle: the
# loader must match the libc it loads.
system_libs=' libc.so.6 libm.so.6 libdl.so.2 libpthread.so.0 librt.so.1 libresolv.so.2 libutil.so.1 ld-linux-x86-64.so.2 ld-linux-aarch64.so.1 libgcc_s.so.1 '
# Loaded by NSS with dlopen, so no ELF header names them.
nss_modules='libsoftokn3.so libfreebl3.so libfreeblpriv3.so libnssckbi.so libnssdbm3.so'

containerfile="$root/containers/browser/Containerfile"
image=$(sed -nE 's|^FROM (docker\.io/chromedp/headless-shell:[^@ ]+@sha256:[0-9a-f]{64}).*|\1|p' "$containerfile")
[ -n "$image" ] || die "no pinned headless-shell FROM line in $containerfile"
version=${image#*:}
version=${version%@*}
[[ "$version" =~ ^[0-9]+(\.[0-9]+){3}$ ]] || die "headless-shell tag '$version' is not a Chrome version"

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

# The image's files for this architecture, without running anything in it.
podman pull --quiet --platform "linux/$arch" "$image" >/dev/null
container=$(podman create --platform "linux/$arch" "$image")
rootfs="$work/rootfs"
mkdir -p "$rootfs"
podman export "$container" | "$tar" -x -C "$rootfs" \
  --wildcards 'headless-shell/*' "usr/lib/$multiarch/*" 'usr/share/fonts/truetype/dejavu/*'
podman rm -f "$container" >/dev/null
container=''
libdir="$rootfs/usr/lib/$multiarch"
[ -x "$rootfs/headless-shell/headless-shell" ] || die "$image has no /headless-shell/headless-shell"

out=${out:-$root/target/browser-bundle}
mkdir -p "$out"
out=$(cd "$out" && pwd)
dest="$out/$version/$arch"
rm -rf "$dest"
mkdir -p "$dest/lib" "$dest/fonts/dejavu"

cp -a "$rootfs/headless-shell" "$dest/headless-shell"
readelf -h "$dest/headless-shell/headless-shell" | grep -q "Machine:.*$machine" \
  || die "the image's headless-shell is not $arch"

needed() { readelf -d --wide "$1" 2>/dev/null | sed -nE 's/.*\(NEEDED\).*\[(.*)\]$/\1/p'; }
is_elf() { [ "$(head -c 4 "$1" | od -An -c | tr -d ' ')" = '177ELF' ]; }

# Every library the browser and what it loads need, beyond glibc and beyond
# what headless-shell/ already carries, copied (links followed) into lib/.
for module in $nss_modules; do
  [ -e "$libdir/$module" ] || continue
  cp -L "$libdir/$module" "$dest/lib/$module"
  [ -e "$libdir/${module%.so}.chk" ] && cp -L "$libdir/${module%.so}.chk" "$dest/lib/"
done
queue=("$dest/headless-shell/headless-shell" "$dest"/headless-shell/*.so* "$dest"/lib/*.so*)
while [ ${#queue[@]} -gt 0 ]; do
  file=${queue[0]}
  queue=("${queue[@]:1}")
  { [ -f "$file" ] && is_elf "$file"; } || continue
  for lib in $(needed "$file"); do
    case "$system_libs" in *" $lib "*) continue ;; esac
    [ -e "$dest/headless-shell/$lib" ] || [ -e "$dest/lib/$lib" ] && continue
    [ -e "$libdir/$lib" ] || die "$(basename "$file") needs $lib, which the image does not have"
    cp -L "$libdir/$lib" "$dest/lib/$lib"
    queue+=("$dest/lib/$lib")
  done
done

# Held to the base's glibc.
newest=$(for file in "$dest/headless-shell/headless-shell" "$dest"/headless-shell/*.so* "$dest"/lib/*.so*; do
  is_elf "$file" && readelf -V --wide "$file" 2>/dev/null
done | sed -nE 's/.*Name: GLIBC_([0-9]+\.[0-9]+).*/\1/p' | sort -t. -k1,1n -k2,2n | tail -1)
[ -n "$newest" ] || die "found no glibc version requirement; is readelf working?"
if [ "$(printf '%s\n%s\n' "$newest" "$glibc_max" | sort -t. -k1,1n -k2,2n | tail -1)" != "$glibc_max" ]; then
  die "the bundle needs glibc $newest; the stock base has $glibc_max"
fi

cp -L "$rootfs"/usr/share/fonts/truetype/dejavu/*.ttf "$dest/fonts/dejavu/"
cat > "$dest/fonts/fonts.conf" <<'EOF'
<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
<!-- The bundle's own fonts, found relative to this file: the base image has none. -->
<fontconfig>
  <dir prefix="relative">dejavu</dir>
  <cachedir prefix="xdg">fontconfig</cachedir>
  <alias><family>sans-serif</family><prefer><family>DejaVu Sans</family></prefer></alias>
  <alias><family>serif</family><prefer><family>DejaVu Serif</family></prefer></alias>
  <alias><family>monospace</family><prefer><family>DejaVu Sans Mono</family></prefer></alias>
</fontconfig>
EOF

# --no-sandbox: Chrome's sandbox needs namespaces a capability-less sandbox
# cannot make; the sandbox is the boundary. --disable-dev-shm-usage: a
# container's /dev/shm can be small, which Chrome otherwise warns about and
# can crash on. Loopback only: services are reached on the sandbox's loopback
# and nowhere else.
cat > "$dest/run" <<'EOF'
#!/bin/sh
# Headless Chrome from this bundle, answering CDP on 127.0.0.1:9222 only.
# Arguments are passed on to Chrome.
set -eu
here=$(dirname "$(readlink -f "$0")")
export LD_LIBRARY_PATH="$here/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export FONTCONFIG_FILE="$here/fonts/fonts.conf"
profile=$(mktemp -d "${TMPDIR:-/tmp}/tracon-browser.XXXXXX")
exec "$here/headless-shell/headless-shell" \
  --no-sandbox \
  --disable-dev-shm-usage \
  --use-gl=angle \
  --use-angle=swiftshader \
  --user-data-dir="$profile" \
  --remote-debugging-address=127.0.0.1 \
  --remote-debugging-port=9222 \
  "$@"
EOF
chmod 0755 "$dest/run"

# Every value in the manifest passes a pattern, which is what lets it be
# written without a JSON encoder.
{
  printf '{\n  "schema": 1,\n  "service": "browser",\n  "version": "%s",\n  "arch": "%s",\n' "$version" "$arch"
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
tarball="tracon-browser-$version-$arch.tar.gz"
"$tar" --sort=name --mtime="@$epoch" --owner=0 --group=0 --numeric-owner \
  --format=posix --pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime \
  -C "$out" -cf - "$version/$arch" | gzip -n -9 > "$out/$tarball"
(cd "$out" && printf '%s  %s\n' "$(sha256 "$tarball")" "$tarball" > "$tarball.sha256")

echo "$dest"
echo "$out/$tarball"
