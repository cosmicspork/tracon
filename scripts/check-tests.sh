#!/bin/sh
# Every integration test binary must include the shared support module, which
# is what keeps `cargo test` out of the operator's real state directory. A
# file that forgets it links the library without the cfg(test) guard.
set -eu
cd "$(dirname "$0")/.."
bad=0
for f in node/tests/*.rs; do
  if ! grep -q '^#\[path = "support/mod.rs"\]' "$f"; then
    echo "$f: does not include node/tests/support/mod.rs" >&2
    bad=1
  fi
done
exit $bad
