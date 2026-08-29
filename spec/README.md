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
| `CONTRACT_VERSION` | 2 | Wire version, reported at `GET /v0/info`. Additive; moving it rotates nothing. Every node and the hub move together: a frame or enrollment of another version is refused. |
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
Commands are discriminated on `op`: `create`, `prompt`, `answer`, `kill`, `verdict`.
A `verdict` command may carry an optional `patch`: a unified diff the operator edited
by hand, applied by the agent on the owning node. Additive and defaulted, so a node on
an older build reads the rest of the verdict unchanged and the contract version is
unmoved.

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
| `POST /v0/enroll/{code}` | none, rate-limited | fill it (public keys and a name) |
| `GET /v0/enroll/{code}` | slot creator | fetch and delete |
| `POST /v0/admit`, `DELETE /v0/admit/{id}` | member | membership |

## Vectors

| File | Pins |
|---|---|
| `key-derivation.json` | seed → both public keys, node id, credential-store key |
| `envelope.json` | seal, wrap, box bytes for fixed nonces and ephemerals |
| `auth.json` | descriptor bytes and signatures |
| `keyring.json` | container bytes for genesis + one rotation, and a handoff |
| `frame.json` | canonical bytes and ids for channel and direct headers |
