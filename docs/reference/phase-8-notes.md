# Phase 8 notes

Written 2026-08-29, during the work. What the roadmap does not have room for.

## Web Push without a relay

The `web-push` crate was the obvious dependency and is unusable here: its HTTP
clients are curl or hyper-with-native-tls, and its `ece` dependency has only an
OpenSSL backend. A static musl node cannot carry OpenSSL. RFC 8291 and RFC 8292
are about two hundred lines over `p256`, `aes-gcm` and `hkdf`, and the RFC's own
Appendix A vector is a unit test that passes byte for byte, so that is what the
node does. The shapes worth having in front of you:

- **Payload encryption (RFC 8291).** ECDH between an ephemeral P-256 key and the
  device's `p256dh`; `HKDF(salt = auth, ikm = ecdh)` expanded with
  `"WebPush: info\0" ‖ ua_public ‖ as_public` to a 32-byte IKM; then
  `HKDF(salt, ikm)` expanded with `"Content-Encoding: aes128gcm\0"` (16 bytes,
  the key) and `"Content-Encoding: nonce\0"` (12 bytes). One AES-128-GCM record
  of `plaintext ‖ 0x02`. The body is `salt(16) ‖ rs(4, BE, 4096) ‖ 65 ‖
  as_public(65) ‖ ciphertext`.
- **VAPID (RFC 8292).** `Authorization: vapid t=<jwt>, k=<b64url pubkey>`. The
  JWT is ES256 with the signature as raw `r ‖ s`, never DER; `aud` is the
  push endpoint's scheme and host; `exp` at most a day out (the node uses twelve
  hours); `sub` a `mailto:` or `https:` — Apple checks its shape, which is what
  `[notify] contact` is for.
- **Headers.** `Content-Encoding: aes128gcm`, `TTL`, `Urgency: high`, and `Topic`
  — at most 32 URL-safe characters, so the node hashes the tag down to one.
  Topic lets the push service collapse undelivered pushes that replace each
  other; the tag does the same on the phone once they arrive.

**Every push must show something.** iOS revokes a subscription after a few pushes
whose service worker showed nothing, so the worker shows a generic banner for a
payload it cannot parse rather than nothing.

**No mesh change was needed.** The notifier reads the bus, and peers' queues,
reviews and promotions already arrive mirrored and untapped. The only thing that
made one node deliver was the `notify.node` check; deleting it *is* the fan-out.
A phone subscribed at two nodes hears from both, and the same `tag` from both
makes the second replace the first.

**A loopback endpoint may be plain http.** Real push services are https and the
node refuses anything else — except a loopback host, which only ever means a
test's fake service. That is what lets the integration tests run the real
encryption and the real headers through a real socket.

## The test suite and the operator's state

`node/tests/support/state.rs` explains the mechanism; `scripts/check-tests.sh`
enforces the include. Two things worth knowing beyond that:

- A `remove_dir_all` in a test fixture is not paranoia. `tracon-it-state-<pid>`
  was created but never cleared, so a run that was killed and a recycled pid
  handed the next run a populated credential store.
- `type_name_of_val` inside an async test yields `…::name::{{closure}}::here`,
  so a "use the enclosing function's name" macro that takes the last path
  segment gives every async test the same name. The first run of exactly that
  macro shared one fixture directory across nineteen tests, which is the failure
  it was written to prevent. Skip the `{{closure}}` segments.

## Shipping the desktop app

- The sidecar is named by the *wrapper's* target triple
  (`binaries/tracon-x86_64-unknown-linux-gnu`), not the node's; a static musl
  binary under a gnu name is fine. `externalBin` is passed with `--config` at
  bundle time rather than written into `tauri.conf.json`, so `cargo build` in
  `wrapper/` never needs the sidecar present.
- The wrapper looks beside its own executable first, then `~/.local/bin`, then
  PATH — so a bundle uses the node it carries even when an older install is
  around.
- The native musl toolchain produces `static-pie linked`, which `file` does not
  call `statically linked`; the assertion accepts both.
- AppImage bundling wants `librsvg2` development files even though nothing here
  renders SVG; linuxdeploy's gtk plugin asks for its `.pc` file and fails
  without it.
- The wrapper's version was frozen at `0.1.0` because release-please only knew
  about the workspace `Cargo.toml`; it now bumps `wrapper/Cargo.toml` and
  `wrapper/tauri.conf.json` too, and the lockfile refresh covers the wrapper.

## Release assets, as of 0.6.0

| Asset | |
|---|---|
| `tracon-x86_64-unknown-linux-musl`, `tracon-hub-x86_64-unknown-linux-musl` | static Linux |
| `tracon-aarch64-apple-darwin` | Apple Silicon |
| `checksums.txt` and one `.sha256` per binary | what `install.sh` verifies |
| `tracon_<v>_amd64.AppImage`, `tracon_<v>_amd64.deb`, `tracon_<v>_aarch64.dmg` | the desktop app, node inside |
| `ghcr.io/cosmicspork/tracon-{hub,node,harness,harness-claude}:<v>` | images |
