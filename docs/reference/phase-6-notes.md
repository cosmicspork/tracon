# Phase 6 notes

Written 2026-08-29, while building it. What the roadmap does not have room for:
the decisions, the runbooks, and the things that only showed up when it ran.

> **Superseded in part.** The pager bridge, the `notify.sink`/`notify.node` bindings
> and the cluster runbook below were replaced in Phase 8 by Web Push from every node
> to its own devices; see ARCHITECTURE "Notification sinks". Kept as the record of
> what was built and why.

## The gap the phase opened with

The exit criterion is directing a task from the phone with both laptops closed, so
the phone has to reach the always-on node. The corpus said two things that could not
both be acted on: the phone "reads over the hub" (`ARCHITECTURE.md`, four times), and
"the hub never talks to a browser". Meanwhile the operator API had no authentication
at all — the `local_only` middleware is a DNS-rebinding guard, not a credential.

So the phase started by answering that: the node grew operator auth, and the phone
reaches a node directly over an HTTPS ingress. Building a browser-facing surface on
the hub, plus an auth model for it, to reach a node that is already reachable would
have been the larger change. The hub still never talks to a browser, and the
hub-mediated path stays unbuilt because nothing needs it.

## Auth, as built

- Loopback is unchanged, and there is a test whose only job is to keep it that way.
  The CLI, `just dev`, and `kubectl port-forward` all arrive as loopback.
- With no token issued, everything else is refused outright, with the command that
  would open the door in the message. Deny-safe: a request with no peer address
  counts as remote.
- A cookie rather than a bearer because `EventSource` cannot set headers and the
  phone lives on `/api/stream`. `SameSite=Lax`, not `Strict`: opening a review from
  a push notification is a top-level cross-site navigation and Strict drops the
  cookie on exactly that.
- Server-side session rows, not signed tokens, because rotation and logout have to
  bite immediately — and a denylist for signed tokens is a table anyway.
- Only SHA-256 is stored, of the token and of each cookie. `tracon auth issue` sends
  the node the hash, never the token: the plaintext does not cross the wire even
  over loopback.

Two things only the socket run showed, neither visible to an in-process test:

- The CLI could not reach a remote node at all — it never sent the token. `TRACON_TOKEN`
  now does that.
- `curl` will not store a `Secure` cookie over plain HTTP. That is correct for the
  real path (TLS terminates at the ingress, and browsers treat `localhost` as a
  secure context regardless), but it means a plain-HTTP LAN node is not a supported
  way to use the cookie.

## Notifications

The load-bearing choice is that the notifier subscribes to the **bus**, not to
`Manager::publish_queue`. A peer's approval is mirrored into this node's tables and
published *untapped* — which reaches subscribers but never the manager. A notifier
hooked to the manager would have silently missed every peer's queue, which is exactly
the case the phase exists for: the other laptop raised it, this node is the one awake.

Deliberately not done:

- **No announcement of the standing queue at startup.** A redeploy or a waking laptop
  is not news, and re-announcing trains you to swipe pushes away. Anything a peer
  raised while this node was down still arrives — it was not in the store to be
  primed, so it diffs as new when the mirror lands it.
- **No page when a review returns to `new` after a release.** `release_review` moves
  claimed → new, so an id-only diff would buzz every time you opened a review and
  walked away. Reviews are tracked by state, and only `revising → new` (the agent
  resubmitting) pages again.

Pushes are hints; the queue is the truth. A failed send is logged and retried once,
then dropped. Sends are spawned behind a semaphore so the subscriber loop is always
in `recv()` — a lagging subscriber costs every SSE client its position.

## The patch builder

`spa/src/lib/patch.ts` is ~100 lines rather than a dependency, because the only thing
that matters is whether `git apply` accepts the output — so the tests run real git in
a temporary repository, plus 400 seeded-random edits over a duplicate-heavy alphabet.

The fuzz earned its place immediately: hunks that closed at exactly `CONTEXT`
unchanged lines could emit overlapping context, which git rejects. The streaming
approach was the wrong shape; it now builds an explicit edit script and groups changes
into hunks whose contexts cannot overlap.

Two more bugs came only from running it end to end:

- `review::git()` trims, so `cat-file blob` was dropping a file's trailing newline.
  The editor then built a patch declaring "no newline at end of file" and would have
  silently stripped it.
- The API `trim()`ed the incoming patch, removing its final newline. `git apply` calls
  such a patch **corrupt**. A patch's whitespace is content.

A file whose base cannot be rebuilt from the stored diff stays read-only rather than
being opened against a guess.

## Building the wrapper on an immutable host

Bazzite has the webkit2gtk and gtk *runtime* libraries but no development headers and
no pkgconfig, and layering them with `rpm-ostree` re-applies on every image update. A
container is the lower-friction path, and the produced binary runs on the host because
the versions match:

```bash
distrobox create --name tracon-build --image registry.fedoraproject.org/fedora:44 --yes
distrobox enter tracon-build -- sudo dnf install -y \
  webkit2gtk4.1-devel gtk3-devel libappindicator-gtk3-devel \
  rust cargo clippy rustfmt
just wrapper          # or: just wrapper-check
```

Running it on this host wants `GDK_BACKEND=x11` (the session is Wayland) and
`WEBKIT_DISABLE_COMPOSITING_MODE=1`.

Notes: Tauri requires **RGBA** icons and headless Chromium writes RGB, so the icons are
converted after rasterising. `wrapper/gen/schemas/` is tauri-build output and is
ignored. The wrapper is a separate cargo workspace (`exclude = ["wrapper"]`) so
`cargo test` at the root never needs any of this — checked, not assumed.

Not built: bundling and signing (`bundle.active: false`), and macOS is not in CI. The
plist and the `launchctl` path are written and reviewed but only exercised on a Mac.

## Switchboard

It installs **nothing** on Linux. `systemctl --user list-unit-files` on this host shows
`tracon.service` and `pager-bridge.service` and no switchboard anything; it is a macOS
menu-bar app that toggles launchd agents. So "Switchboard's units move" is a macOS-only
migration, and on Linux there was nothing to move — `tracon service install` simply
adopted the hand-written unit that was already there.

Its three roles: supervising the notebook server (dies with notebook), toggling launchd
agents from a menu bar (superseded by the tray, on macOS only), and the display-linked
theme switcher.

**The theme switcher is deliberately killed.** It forces macOS Light mode while an
external display is connected. Nothing in tracon depends on it; it is macOS-only
(System Events, with the automation permission that implies); and rehoming it into the
wrapper would mean a mac-only code path maintained forever for a behaviour unrelated to
directing agent work. If it is missed, it belongs in dotfiles as a small launchd agent,
not in tracon.

Retiring the repo is the operator's, and last: after the mac's pager-bridge LaunchAgent
has moved off it and the wrapper has been used there. Nothing is retired until its
replacement has been in real use.

## Cluster runbook

Merging the homelab PR deploys, so it lands only after `tracon 0.5.0` and `pager 0.6.0`
exist — the ingress must not stand in front of a node image with no auth guard.

Then, in order:

1. The cluster bridge mints its identity on first start, so its key cannot be in the
   manifests ahead of time. Read it off the pod and append it to the relay's
   `PAGER_BRIDGE_PUBKEY` (comma-separated; the laptop's stays):
   `kubectl exec deploy/pager-bridge -n pager -- pager-bridge id`
   Until then the cluster bridge gets a 401 and only its own pushes are lost.
2. Pair the phone to the cluster bridge from `kubectl logs` — the same QR flow as the
   laptop's. Keep both pairings; each bridge seals to its own devices.
3. On the lab node: `tracon auth issue`, then log in at `https://tracon-node.0x69.xyz`
   and install the PWA from there.
4. `tracon channel bind personal notify.sink=pager notify.node=<lab node id>` (and
   `client`); `tracon channel bind work notify.sink=tray`.
5. Then the exit criterion, for real: close both laptops, wait for a push, tap it, and
   approve a review from the phone.

## Still open

Carried from Phase 4: the snapshot run against Spaces, and the hub memory limit
(128Mi → 256Mi). From Phase 5: one real plan → execute → review chain through the
gateway, with the check-container timing recorded here.
