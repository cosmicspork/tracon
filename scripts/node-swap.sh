#!/usr/bin/env bash
# Run an unreleased build as this machine's own node, and put the release back.
#
#   node-swap.sh dev [ref]          build ref, back up state, run it as the node
#   node-swap.sh release [restore]  run the release again, optionally from the backup
#
# The dev build runs beside the installed binary rather than in place of it:
# the desktop app owns that path and reinstalls it whenever its version
# differs from the one the app carries, so a build copied over it would be
# silently replaced. On Linux the user unit is pointed at the build through a
# systemd drop-in; on macOS the LaunchAgent's plist is, with the release's
# plist kept beside the backups until `release` puts it back.
#
# TRACON_UNIT (default tracon.service), TRACON_LABEL (default
# com.tracon.node) and TRACON_URL (default http://127.0.0.1:7420) point it at
# another node; TRACON_DEV_TREE is where the build's worktree lives.
set -euo pipefail

url=${TRACON_URL:-http://127.0.0.1:7420}
tree=${TRACON_DEV_TREE:-${XDG_CACHE_HOME:-$HOME/.cache}/tracon/node-dev}
repo=$(cd "$(dirname "$0")/.." && pwd)

say() { echo "node-swap: $*"; }
die() {
  echo "node-swap: $*" >&2
  exit 1
}

command -v jq >/dev/null || die "needs jq"

case $(uname -s) in
  Linux)
    command -v systemctl >/dev/null || die "needs systemd --user"
    unit=${TRACON_UNIT:-tracon.service}
    fragment=$(systemctl --user show -P FragmentPath "$unit")
    [ -n "$fragment" ] || die "no user unit $unit; \`tracon service install\` writes it"
    dropin="$fragment.d/node-dev.conf"
    logs="journalctl --user -u $unit"

    # The node takes its directories from the unit's environment; the setup
    # and boundary commands run here must see the same ones.
    for kv in $(systemctl --user show -P Environment "$unit"); do
      case $kv in TRACON_*=*) export "${kv?}" ;; esac
    done
    state_dir=${TRACON_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/tracon}
    config_dir=${TRACON_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/tracon}

    copy_tree() { cp -a --reflink=auto "$1" "$2"; }
    service_stop() { systemctl --user stop "$unit"; }
    service_start() { systemctl --user start "$unit"; }
    dev_installed() { [ -e "$dropin" ]; }

    # The unit's own command line, with any drop-in removed first.
    release_argv() {
      systemctl --user show -P ExecStart "$unit" | sed -n 's/.*argv\[\]=\([^;]*\) ;.*/\1/p'
    }
    clear_dev() {
      rm -f "$dropin"
      rmdir "$(dirname "$dropin")" 2>/dev/null || true
      systemctl --user daemon-reload
    }
    install_dev() {
      mkdir -p "$(dirname "$dropin")"
      cat >"$dropin" <<EOF
# Written by tracon's \`just node-dev\`; \`just node-release\` removes it.
# $(git -C "$tree" log -1 --format='%H %s')
[Service]
ExecStart=
ExecStart=$1 $2
EOF
      systemctl --user daemon-reload
    }
    ;;
  Darwin)
    label=${TRACON_LABEL:-com.tracon.node}
    plist="$HOME/Library/LaunchAgents/$label.plist"
    [ -f "$plist" ] || die "no LaunchAgent $plist; \`tracon service install\` writes it"
    logs="$(/usr/libexec/PlistBuddy -c 'Print :StandardErrorPath' "$plist" 2>/dev/null || echo "the node's log")"

    plist_env() {
      plutil -extract EnvironmentVariables json -o - "$plist" 2>/dev/null |
        jq -r 'to_entries[] | select(.key | startswith("TRACON_")) | "\(.key)=\(.value)"'
    }
    while IFS= read -r kv; do
      [ -n "$kv" ] && export "${kv?}"
    done < <(plist_env)
    # `dirs` has no state directory on macOS, so state and config share one.
    state_dir=${TRACON_STATE_DIR:-$HOME/Library/Application Support/tracon}
    config_dir=${TRACON_CONFIG_DIR:-$HOME/Library/Application Support/tracon}
    # Outside LaunchAgents, where launchd would load it as a second agent.
    release_plist="$(dirname "$state_dir")/tracon-backups/release.plist"

    copy_tree() { cp -cpR "$1" "$2" 2>/dev/null || cp -pR "$1" "$2"; }
    service_stop() {
      launchctl bootout "gui/$UID/$label" 2>/dev/null || true
      for _ in $(seq 30); do
        launchctl print "gui/$UID/$label" >/dev/null 2>&1 || return 0
        sleep 1
      done
      die "launchd still has $label after 30s"
    }
    service_start() { launchctl bootstrap "gui/$UID" "$plist"; }
    dev_installed() { [ -e "$release_plist" ]; }

    release_argv() {
      local from=$plist
      dev_installed && from=$release_plist
      plutil -extract ProgramArguments json -o - "$from" | jq -r 'join(" ")'
    }
    clear_dev() {
      if dev_installed; then
        cp "$release_plist" "$plist"
        rm -f "$release_plist"
      fi
    }
    install_dev() {
      local args_json
      mkdir -p "$(dirname "$release_plist")"
      cp "$plist" "$release_plist"
      args_json=$(jq -cn --arg bin "$1" --arg args "$2" '[$bin] + ($args | split(" ") | map(select(. != "")))')
      plutil -replace ProgramArguments -json "$args_json" "$plist"
    }
    ;;
  *) die "needs systemd --user (Linux) or launchd (macOS)" ;;
esac
backups="$(dirname "$state_dir")/tracon-backups"

# Set while the node is stopped, so a failure says how to get it back.
stopped=0
trap '[ "$stopped" = 0 ] || echo "node-swap: the node is stopped; \`just node-release\` starts the release again (add \`restore\` to restore the state backup too)" >&2' EXIT

# Sessions the node supervises. External harnesses only lose their MCP
# connection over a restart, so they do not hold it up.
managed_running() {
  curl -fsS --max-time 3 "$url/api/queue" 2>/dev/null |
    jq '[.running[]? | select(.state != "closed" and .harness_id != "external")] | length' ||
    echo 0
}

stop_node() {
  local n
  n=$(managed_running)
  [ "$n" = 0 ] || die "$n session(s) running on the node; stopping it ends them, so end them first"
  service_stop
  stopped=1
}

start_node() {
  service_start
  stopped=0
  for _ in $(seq 60); do
    curl -fsS --max-time 2 "$url/api/health" >/dev/null 2>&1 && return 0
    sleep 1
  done
  die "the node did not answer at $url; see $logs"
}

dev() {
  local ref=${1:-origin/main} rev argv args="" release_bin dev_bin from_release=1
  dev_installed && from_release=0

  git -C "$repo" fetch -q origin
  rev=$(git -C "$repo" rev-parse --verify "$ref^{commit}")
  if [ -e "$tree/.git" ]; then
    git -C "$tree" checkout -q --detach --force "$rev"
  else
    git -C "$repo" worktree prune
    mkdir -p "$(dirname "$tree")"
    git -C "$repo" worktree add -q --detach "$tree" "$rev"
  fi
  if ! command -v cargo >/dev/null && command -v brew >/dev/null; then
    PATH="$(brew --prefix rustup)/bin:$PATH"
  fi
  # Rebuilding the SPA alone recompiles the node (it embeds spa/dist), so an
  # unchanged commit reuses the last build.
  if [ "$(cat "$tree/target/node-dev-rev" 2>/dev/null)" = "$rev" ] && [ -x "$tree/target/release/tracon" ]; then
    say "reusing the build of $(git -C "$tree" log -1 --format='%h %s')"
  else
    say "building $(git -C "$tree" log -1 --format='%h %s') in $tree"
    (cd "$tree/spa" && bun install --frozen-lockfile && bun run build)
    (cd "$tree" && cargo build --release --bin tracon)
    echo "$rev" >"$tree/target/node-dev-rev"
  fi

  stop_node
  clear_dev
  argv=$(release_argv)
  [ -n "$argv" ] || die "could not read the node's command line"
  release_bin=${argv%% *}
  [[ $argv == *" "* ]] && args=${argv#* }
  dev_bin="$(dirname "$release_bin")/tracon-dev"
  # Staged and renamed: a file a process is running cannot be overwritten.
  cp "$tree/target/release/tracon" "$dev_bin.new"
  mv -f "$dev_bin.new" "$dev_bin"

  # Only the release's state is worth restoring; replacing one dev build
  # with another keeps the backup taken before the first.
  if [ "$from_release" = 1 ]; then
    local stamp dest
    stamp=$(date +%Y%m%d-%H%M%S)
    dest="$backups/$stamp"
    mkdir -p "$dest"
    copy_tree "$state_dir" "$dest/state"
    [ "$config_dir" = "$state_dir" ] || copy_tree "$config_dir" "$dest/config"
    "$release_bin" --version >"$dest/version"
    ln -sfn "$stamp" "$backups/latest"
    say "state backed up to $dest"
  fi

  "$dev_bin" setup
  install_dev "$dev_bin" "$args"
  start_node
  "$dev_bin" check-boundary --deep ||
    die "the boundary check failed; the node is running the dev build, \`just node-release\` goes back"
  say "the node runs $(git -C "$tree" log -1 --format=%h) from $dev_bin; \`just node-release\` goes back"
}

release() {
  local mode=${1:-} src aside argv bin
  case $mode in "" | restore) ;; *) die "usage: node-swap.sh release [restore]" ;; esac
  if ! dev_installed && [ -z "$mode" ]; then
    say "the node already runs its release binary"
    return
  fi

  stop_node
  clear_dev
  if [ "$mode" = restore ]; then
    src=$(readlink -f "$backups/latest" 2>/dev/null) || true
    [ -n "$src" ] && [ -d "$src/state" ] || die "no backup under $backups"
    # Kept, not deleted: what the dev build wrote may be wanted afterwards.
    aside="$backups/replaced-$(date +%Y%m%d-%H%M%S)"
    mkdir -p "$aside"
    mv "$state_dir" "$aside/state"
    copy_tree "$src/state" "$state_dir"
    if [ "$config_dir" != "$state_dir" ]; then
      mv "$config_dir" "$aside/config"
      copy_tree "$src/config" "$config_dir"
    fi
    say "restored $src; what the dev build left is in $aside"
  fi

  argv=$(release_argv)
  bin=${argv%% *}
  # The dev build's setup may have rebuilt the gateway from its own
  # definitions; the release refuses to run harnesses until they match again.
  "$bin" setup
  start_node
  "$bin" check-boundary --deep
  say "the node runs $("$bin" --version)"
}

case ${1:-} in
  dev) dev "${2:-}" ;;
  release) release "${2:-}" ;;
  *) die "usage: node-swap.sh dev [ref] | release [restore]" ;;
esac
