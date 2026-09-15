# tracon mesh wire contract

The bytes that cross a machine boundary, defined once in `proto/` and pinned by
`vectors/*.json`. Every module in `proto/src/` has a `matches_spec_vectors` test;
regenerate with `cargo run -p tracon-proto --example vectors` and commit the result
alongside the change that made it necessary.

Shapes are borrowed from an earlier end-to-end-encrypted project's trust contract and
re-labelled. The crate is
vendored, not depended on, so this contract moves on its own cadence.

## Versions

| Constant | Value | Meaning |
|---|---|---|
| `CONTRACT_VERSION` | 3 | Wire version, reported at `GET /v0/info`. Additive; moving it rotates nothing. Every node and the hub move together: a frame or enrollment of another version is refused. |
| `CONTRACT_MAJOR` | 0 | Cryptographic era, embedded in every label below. Bumps only on a key-rotating break. |

Labels are `tracon/v{MAJOR}/{operation}`.

## Identity (`keys.rs`)

A node is one 32-byte random seed (`<state>/node-identity.seed`, hex, `0600`).

```
hk      = HKDF-SHA256(ikm = seed, salt = none)
x25519  = hk.expand("tracon/v0/x25519", 32)      sealing
ed25519 = hk.expand("tracon/v0/ed25519", 32)     signing
credstore = hk.expand("tracon/v0/credstore", 32) seals the node's credential store at rest
node_id = hex(ed25519 public key)
```

Fingerprint for human comparison during enrollment: first 16 hex characters of
`SHA256(ed25519 public)` in groups of four.

Key binding, signed by the node over its own two public keys:

```
binding = "tracon/v0/enroll-binding" ‖ ed25519 public(32) ‖ x25519 public(32)
binding_sig = ed25519(binding)
```

The fingerprint covers the node id alone, so the binding is what ties the
sealing key beside it to the same identity. Every enrollment fill and every
member record that writes a sealing key carries it, and nothing wraps a channel
keyring to a key without it.

## Sealing (`envelope.rs`)

AEAD is XChaCha20-Poly1305 (24-byte nonce, 16-byte tag).

- `Sealed` = `nonce(24) ‖ ciphertext+tag`, under a 32-byte `DataKey`, with AAD.
- ECIES to an X25519 recipient: ephemeral X25519 → DH → `HKDF-SHA256(ikm = shared,
  salt = eph_pub ‖ recipient_pub, info = label)` → 32-byte key → `Sealed`.
  Wire form `eph_pub(32) ‖ Sealed`.
  - `WrappedKey`: a `DataKey` sealed with label `wrap`, no AAD.
  - `SealedBox`: an arbitrary body sealed with label `box`, caller AAD.
  The two labels make a wrapped key unopenable as a box and vice versa.

## Channel keyrings (`keyring.rs`)

A channel's keys are epochs. Epoch ids are opaque 16-byte values; genesis is
all-zero. Container: `"trkr" ‖ 0x01 ‖ count(u32) ‖ [ id(16) ‖ created_at(i64) ‖
len(u32) ‖ WrappedKey ]…`, entries in ascending `(created_at, id)`, integers
big-endian. Merge is union by id. A handoff re-wraps every epoch to the grantee.

AAD for a frame sealed under a channel epoch: `channel ‖ 0x1f ‖ epoch_id`.

## Frames (`frame.rs`)

```json
{ "v": 1, "id": "<hex sha256>", "channel": "personal", "sender": "<node id>",
  "recipient": "<node id>" | absent, "sealing": {"mode":"channel","epoch":"<hex16>"} | {"mode":"direct"},
  "sent_ms": 0, "body": "<base64>", "sig": "<hex ed25519>" }
```

Canonical bytes: `u32_be(v) ‖ len+channel ‖ sender(32) ‖ (0x00 | 0x01‖recipient(32))
‖ (0x00 | 0x01‖epoch(16)) ‖ i64_be(sent_ms) ‖ len+body`, where `body` is the sealed
bytes (not the plaintext) and `len` is a big-endian u32.

- `id = SHA256("tracon/frame-id\0" ‖ canonical)`; version-independent tag.
- `sig = Ed25519(sender, "tracon/v0/frame" ‖ id)`.
- Channel sealing: `Sealed` under the epoch key, AAD as above.
- Direct sealing: `SealedBox` to the recipient's X25519 key, AAD
  `channel ‖ 0x1f ‖ sender(32) ‖ recipient(32)`.

Verification recomputes the id from the contents and never trusts the stored one.
The hub verifies before storing; peers verify before opening. `sent_ms` is
informational; ordering is the hub's per-channel sequence.

Channel names match `^[a-z0-9@._-]{1,64}$`. `@mesh` is the channel every node is
a member of. A frame may be at most 4 MiB serialized.

Payload kinds (JSON, discriminated on `kind`): `hello`, `snapshot`, `session`,
`event`, `queue`, `reviews`, `node`, `command`, `ack`, `events_request`,
`events_batch`, `key_handoff` (direct only), `policy_bundle` (direct only),
`credential_handoff` (direct only; broker rows for the recipient), `changes`,
`changes_request` (direct only), `changes_batch` (direct only).

A `changes` payload carries record-level changes from one site:
`{ table, op: upsert|delete, id, site, site_seq, hlc_ms, hlc_ctr, row }`. `site` must
equal the sender; `(site, site_seq)` makes a change idempotent; `(hlc_ms, hlc_ctr, site)`
is the last-writer-wins key; `row` is the whole record for an upsert and null for a
delete. Version 2 added these three kinds. The replicated tables are `document`,
`memory`, `promotion`, and (sync schema step 2, no contract change) `work_item`, whose
id is `hex(sha256("tracon/work-item" ‖ 0x1f ‖ channel ‖ 0x1f ‖ project ‖ 0x1f ‖ site ‖
0x1f ‖ created_ms ‖ 0x1f ‖ title))` — pinned in `sync/src/work.rs`.
Commands are discriminated on `op`: `create`, `prompt`, `answer`, `kill`, `verdict`,
and (version 3) `provider_connect`, `provider_code`, `provider_disconnect` — a peer's
providers driven from another node's interface, executed by the owner exactly as a
local request; `provider_connect` acks with the sign-in URL.
A `verdict` command may carry an optional `patch`: a unified diff the operator edited
by hand, applied by the agent on the owning node. Additive and defaulted, so a node on
an older build reads the rest of the verdict unchanged and the contract version is
unmoved. Version 3 also adds an optional `providers` field to the `node` payload's
row (absent on older builds); a new command op fails whole-payload deserialization on
an older node, which drops the frame — the sender times out rather than erring, hence
the version bump.

## Owner streams (`stream.rs`)

A second frame family, **relayed and never stored**, for a request a session's
owner must answer now: the mediated harness API, its event stream, and the
terminal. Durable frames are the wrong shape for these — a replayed `POST`
repeats a mutation, and a session's event stream would fill the hub's disk with
bytes nobody reads again — so streams get their own envelope, their own keys,
their own replay rule and their own hub routes.

```json
{ "v": 3, "channel": "personal", "sender": "<node id>", "recipient": "<node id>",
  "stream_id": "<hex16>", "epoch": "<hex16>", "seq": 0, "sent_ms": 0,
  "body": "<base64>", "sig": "<hex ed25519>" }
```

Everything but `body` is routing metadata the hub reads; `body` is the sealed
payload. `stream_id` is 16 random bytes. `seq` starts at 0 and increases by one
per frame **per sender**, so each direction has its own sequence.

Canonical bytes: `u32_be(v) ‖ len+channel ‖ sender(32) ‖ recipient(32) ‖
stream_id(16) ‖ epoch(16) ‖ u64_be(seq) ‖ i64_be(sent_ms)`, `len` a big-endian
u32. The canonical bytes are the AEAD associated data, so a frame the hub
re-labels onto another stream, channel, epoch, recipient or sequence number
fails to open. `sig = Ed25519(sender, "tracon/v0/stream" ‖ SHA256(canonical ‖
len+body))` — sealed then signed, as with durable frames.

### Keys and nonces

The per-stream key is derived from the channel epoch key, under a label no
durable operation uses, and separately for each direction:

```
salt = stream_id(16) ‖ 0x1f ‖ sender(32) ‖ recipient(32)
info = "tracon/v0/stream-key" ‖ 0x1f ‖ channel ‖ 0x1f ‖ epoch(16) ‖ 0x1f ‖ direction
key  = HKDF-SHA256(ikm = epoch key, salt, info)      direction ∈ {serving, owner}
```

`nonce = "trst" ‖ u64_be(seq) ‖ direction(1) ‖ 0x00 × 11` — a counter nonce
under a key that seals nothing else, which is **not** the durable frames'
random-nonce space. Both ends may therefore count from 0 without a `(key,
nonce)` pair ever repeating, and repeating a `seq` is a decryption failure
rather than a policy question. Replay protection is that plus strict
monotonicity per `(stream_id, sender)`: a repeat, a regression or a gap is
dropped. There is no dedupe table, because nothing is retained to dedupe
against.

### Payloads

Discriminated on `frame` (not `kind`, which `open` uses for the stream's own
kind):

| `frame` | Direction | Carries |
|---|---|---|
| `open` | serving → owner | the request metadata allowlist (below) |
| `head` | owner → serving | `status`, `headers`, `owner_epoch`, `uncertain` |
| `chunk` | either | `bytes` (base64), `fin` |
| `flow` | either | `credit`: how many more chunks the peer may send |
| `close` | either | `reason`, optional `detail` |
| `refused` | owner → serving | `reason`, optional `detail`; the stream is over |

An `open` names `protocol_version`, `session_id`, `kind` (`http`, `sse`, `ws`),
`method`, normalised `path`, optional `query`, an allowlisted `headers` subset,
`body_sha256` and `body_len`, an optional `operator` identity, and an optional
`owner_epoch` to be fenced on. The body itself travels as `chunk` frames and is
checked against the digest before the owner dispatches it. The header
allowlist is `accept`, `accept-language`, `content-type`, `last-event-id`,
`if-none-match`; the response allowlist is `content-type`, `cache-control`,
`etag`, `last-modified`, `vary`, `content-disposition`.

`refused` reasons: `unsupported_version`, `unknown_session`, `revoked_member`,
`owner_fenced`, `refused`, `busy`, `malformed`. `close` reasons: `done`,
`timeout`, `too_large`, `relay_dropped`, `owner_fenced`, `revoked`,
`peer_gone`, `local_error`.

`STREAM_PROTOCOL_VERSION` is 1 and moves independently of `CONTRACT_VERSION`.
An unknown `frame` variant fails the whole payload, so a peer on an older build
refuses by name rather than half-reading a stream; a peer that does not know
streams at all answers the routes with a 404. Neither needs the contract
version to move, which is why it has not.

### Limits

| Limit | Value |
|---|---|
| serialized stream envelope | 1 MiB |
| plaintext per `chunk` | 512 KiB |
| headers named by an `open` | 32 |
| credit window per direction | 16 chunks, one granted back per chunk consumed |
| relay buffer per stream | 32 frames, then `429 backpressure` |
| relay queue per connected member | 256 frames, then the stream is dropped |
| streams open per member (relay) | 32 |
| stream frames per member per minute | 3000 |
| idle stream forgotten by the relay after | 300 s |

The node adds its own, configurable under `[mesh]`:
`stream_open_timeout_secs` (30), `stream_idle_secs` (120),
`stream_max_body_bytes` (8 MiB), `stream_max_response_bytes` (256 MiB),
`stream_max_concurrent` (16).

### Hub routes

| Method/Path | Auth | Purpose |
|---|---|---|
| `POST /v0/streams` | sender and recipient both members of the frame's channel; key = sender | relay one sealed frame. `202`; `404 not_connected`, `429 backpressure`/`too_many_streams`/`rate_limited`, `409 relay_dropped`, `413` too large |
| `GET /v0/streams` | member | the member's live delivery connection: `stream` events carry sealed envelopes verbatim, `control` events carry the hub's own clear notices (`{stream_id, reason, detail}`) |
| `DELETE /v0/streams/{stream_id}` | member | this end is done; the relay forgets the stream and tells the other end |

The hub holds no key for a stream and derives none. It keeps a bounded
in-memory buffer per stream and per connection, a route (`a`, `b`, last-seen)
so the far end can be told when the near one goes, and nothing else — no
append, no retention, no replay. A dropped stream closes both ends with a
reason: the poster is told in the refusal, the peer in a `control` event,
because the hub cannot seal a `close` frame of its own.

## Hub requests (`auth.rs`)

Every authenticated request carries three headers:

| Header | Value |
|---|---|
| `tracon-public-key` | node id (hex Ed25519 public key) |
| `tracon-timestamp` | Unix seconds |
| `tracon-signature` | hex Ed25519 over the descriptor |

Descriptor: `"tracon/v0/relay-auth" ‖ len+method ‖ len+path_with_query ‖
SHA256(body) ‖ u64_be(timestamp)`.

The hub rejects a timestamp outside its skew window (default ±300 s) and, for
non-`GET` methods, a signature it has seen within that window: the signature is
the nonce.

## Hub endpoints

See `hub/` and `docs/ARCHITECTURE.md` (Mesh frames). Summary:

| Method/Path | Auth | Purpose |
|---|---|---|
| `GET /health`, `GET /v0/info` | none | probes; contract version and limits |
| `POST /v0/frames` | member of the frame's channel; key = sender | append; returns `{seq}` |
| `GET /v0/frames?channel&after&limit` | member | page; `410 {oldest}` when behind retention |
| `GET /v0/events` | member | SSE pokes, no payload |
| `GET /v0/members` | member | routing metadata |
| `PUT /v0/enroll/{code}` | member | open a slot |
| `POST /v0/enroll/{code}` | none, rate-limited; `binding_sig` must prove the pair | fill it (public keys and a name) |
| `GET /v0/enroll/{code}` | slot creator | fetch and delete |
| `POST /v0/claims` | member | `{key, version, ttl_secs}`: first member to claim a version of an opaque key wins (`201`); others get `409 {holder, version}` until it lapses, and `409 {version}` for an older version |
| `POST /v0/admit`, `DELETE /v0/admit/{id}` | member; writing a sealing key needs the target's `binding_sig` | membership |

## Vectors

| File | Pins |
|---|---|
| `key-derivation.json` | seed → both public keys, node id, credential-store key |
| `envelope.json` | seal, wrap, box bytes for fixed nonces and ephemerals |
| `auth.json` | descriptor bytes and signatures |
| `keyring.json` | container bytes for genesis + one rotation, and a handoff |
| `frame.json` | canonical bytes and ids for channel and direct headers |
