#!/bin/sh
# PID 1's only child, and the one thing that runs before the harness does.
#
# OpenCode installs `@opencode-ai/plugin` into every configuration directory it
# discovers, at config load, forked and detached, and neither `OPENCODE_PURE`
# nor any other flag turns that off (config-state.md 4.3, 4.5). The install
# short-circuits when the directory already holds a `node_modules` tree and a
# lockfile that declares the package, so this seeds exactly that from the image
# before the harness starts: the registry is never reached, on a machine where
# reaching it would fail anyway.
#
# The seed goes into the session's own tree rather than being baked at the
# final path because that tree is a per-session volume the node creates empty;
# nothing an image contains can already be inside it.
set -eu

seed=/opt/tracon/seed

# A copy that refuses to overwrite: a session that already has these (a restart
# against the same scratch volume) keeps what it has, and a read-only target is
# not an error — an unwritable config dir is itself a kill switch for the
# install this seeding exists to pre-empt (config-state.md 4.4).
if [ -n "${XDG_CONFIG_HOME:-}" ] && [ -d "$seed/config" ]; then
  target="$XDG_CONFIG_HOME/opencode"
  if [ ! -e "$target/node_modules" ]; then
    if ! (mkdir -p "$target" && cp -a "$seed/config/." "$target/"); then
      echo "tracon: could not seed $target; OpenCode will try the registry" >&2
    fi
  fi
fi

if [ -n "${XDG_CACHE_HOME:-}" ] && [ -d "$seed/packages" ]; then
  target="$XDG_CACHE_HOME/opencode/packages"
  if [ ! -e "$target/@opencode-ai" ]; then
    if ! (mkdir -p "$target" && cp -a "$seed/packages/." "$target/"); then
      echo "tracon: could not seed $target; OpenCode will try the registry" >&2
    fi
  fi
fi

# `exec`, so the harness — not this script — is the process the init reaps and
# the process a stop signal reaches.
[ "$#" -gt 0 ] || set -- opencode --version
exec "$@"
