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
| `CONTRACT_VERSION` | 4 | Wire version, reported at `GET /v0/info`. Additive; moving it rotates nothing. Every node and the hub move together: a frame or enrollment of another version is refused. |
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
`event`, `queue`, `reviews`, `questions`, `node`, `command`, `ack`, `events_request`,
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
0x1f ‖ created_ms ‖ 0x1f ‖ title))` — pinned in `sync/src/work.rs`. Sync schema step 5
added a nullable `brief_slug` to that row and step 9 dropped it, both with no contract
change: a peer leaves out a field it does not have, an omitted field keeps whatever the
receiving replica holds rather than clearing it, and a field the receiver has no column
for is ignored.
Commands are discriminated on `op`: `create`, `prompt`, `answer`, `kill`, `verdict`,
and (version 3) `provider_connect`, `provider_code`, `provider_disconnect` — a peer's
providers driven from another node's interface, executed by the owner exactly as a
local request; `provider_connect` acks with the sign-in URL.
A `verdict` command may carry an optional `patch`: a unified diff the operator edited
by hand, applied by the agent on the owning node. It may also carry `head_sha` and
`revision_id`: the commit and the review revision the deciding operator inspected. The
revision is the identity the owning node binds the decision to, compared as it records
it — the commit cannot stand in for it, because a resubmission may carry the same
`head_sha` with different requirements or prose. Both are additive and defaulted, so a
node on an older build reads the rest of the verdict unchanged, a peer that names no
revision is held to the commit alone as before, and the contract version is unmoved. Version 3 also adds an optional `providers` field to the `node` payload's
row (absent on older builds); a new command op fails whole-payload deserialization on
an older node, which drops the frame — the sender times out rather than erring, hence
the version bump.
A `command` payload may carry `not_after_ms`, the sender's wall-clock deadline in
milliseconds: the owner refuses a command received past it (allowing for clock skew)
rather than running it after the sender gave up. It is additive and defaulted, so a
command without one runs whenever it lands, an older owner ignores the field, and the
contract version is unmoved. A node that cannot parse a direct `command` payload acks
its `cmd_id` with an error naming its older build instead of dropping it silently.
A `recipe_offer` command `{ recipe, from_channel, note?, not_after_ms }` offers the
receiver a repository recipe on a channel both are members of, which the receiver
checks by its own record. `recipe` is `{ repo, dockerfile?, context?, checks?, prepare,
egress, session_egress, timeout_secs? }`, `repo` a forge identity (`host/owner/name`);
a receiver refuses any other field, and any string in it, or the note, that carries a
control, format or separator character. It is staged for the receiver's operator, never
applied on receipt, and acks `{ offer_id, state, expires_ms, superseded }`. The op is
additive and the contract version is unmoved: a node that predates it answers that it
runs an older build.
A `questions` payload `{ waiting }` lists the sender's open operator questions on a
session in the channel, possibly none: a peer closes the ones it mirrored that the list
no longer names, and a `snapshot` carries the same list as an optional `questions`
field. An `answer_question` command `{ question_id, choice?, text? }` answers one on the
node that asked it, which runs it only for a co-member of the question's channel and
releases the session waiting on it. All three are additive and the contract version is
unmoved: an older node cannot read a `questions` payload and drops it, ignores the
snapshot field, and answers the command that it runs an older build.
A peer administers another node only through `[mesh] admin_peers` on the owner and
an admin statement (below). `provider_connect`, `provider_code` and
`provider_disconnect` gain an optional `statement`; an owner on this build refuses one
without it from a peer, and an older owner ignores the field. `config_read` (no fields)
reads the owner's `node.toml` as its own `GET /api/config` shows it, for an admin peer
only. `config_patch { expected_hash, ops, statement, not_after_ms }` edits it, `ops`
in `PATCH /api/config`'s shape; the owner runs it only for an admin peer, with a
statement for exactly this command on itself, and never past `not_after_ms`. Both ops
are additive and the contract version is unmoved: a node that predates them answers
that it runs an older build.
Version 4 lets a review row and a queued card name no session: a harness the operator
runs themselves calls with none. `session_id` on a `reviews` row and on a `queue` card
may be `null`, a review row gains an optional `lane` (the caller's display label), and
a session-less card is grouped by its `intent.channel`. A version 3 node fails to
deserialize the null, so the two cannot share a mesh.
A peer mirrors a session-less card by the channel it was listed on and sends an
`answer` for it to the card's node, which decides and runs it for a co-member of the
channel; an answer that is an admin action there is refused, as for any peer. A
`snapshot`'s `waiting` lists every card the sender's `queue` would, approvals and
session-less cards among them, so a peer expires only what the sender no longer holds.
A listed card whose id is one of the receiver's own approvals is refused as impersonation.
A sender removed from a channel (today, removed from the hub) lists nothing there again,
so when the hub's member list no longer has it on that channel the receiver expires the
cards it mirrored from it there, session-less or not, and refuses a late answer to one
rather than relaying it.

## Admin statements (`admin.rs`)

```json
{ "issuer": "<node id>", "target": "<node id>", "op_hash": "<hex sha256>",
  "summary": "On beta: ...", "asserted_at_ms": 0, "not_after_ms": 0,
  "session": "<hex12>" | "local", "nonce": "<hex16 bytes>", "sig": "<hex ed25519>" }
```

What one node's operator confirmed (with that node's passkey, or at its machine) for
an admin command on another node, signed by the confirming node's key.

- `op_hash = SHA256("tracon/v0/admin-op" ‖ len+target ‖ len+canonical(command))`,
  where `command` is the command as it serializes (`op` included) without its
  top-level `statement` and `not_after_ms`, and `canonical` is JSON with every
  object's keys in byte order and no whitespace.
- `sig = Ed25519(issuer, "tracon/v0/admin-statement" ‖ len+issuer ‖ len+target ‖
  len+op_hash ‖ len+summary ‖ i64_be(asserted_at_ms) ‖ i64_be(not_after_ms) ‖
  len+session ‖ len+nonce)`.
- `not_after_ms - asserted_at_ms` is at most two minutes; `summary` at most 1024
  characters.

The receiver checks that the issuer is the frame's sender and is named in its `[mesh]
admin_peers`, that the signature verifies, that `target` is itself and `op_hash` the
command it is about to run, that the statement is fresh (30 seconds' leeway for clock
skew), and spends `nonce` once.

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
| `admin-statement.json` | op hash, signed bytes and signature of a fixed statement |
