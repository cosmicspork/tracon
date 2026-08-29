#!/bin/sh
# This repository stands on its own: it names no other project of the
# operator's (except consulta, which it absorbed), no personal host, cluster or
# domain. The list is the enforcement; CHANGELOG.md is generated and exempt.
set -eu
cd "$(dirname "$0")/.."
pattern='homelab|pager|kritee|svastha|notebook|switchboard|dotfiles|0x69\.xyz|bazzite|digitalocean|do-nyc1|~/src/docs|/home/jd|joshbowen'
hits="$(git grep -niIE "$pattern" -- . ':!CHANGELOG.md' ':!scripts/scrub-check.sh' | grep -vE -- '--no-pager' || true)"
if [ -n "$hits" ]; then
  echo "scrub-check: references to other projects or personal hosts:" >&2
  echo "$hits" >&2
  exit 1
fi
