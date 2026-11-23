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
| `CONTRACT_VERSION` | 5 | Wire version, reported at `GET /v0/info`. Additive; moving it rotates nothing. Every node and the hub move together: a frame or enrollment of another version is refused. |
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

Presence is the hub's alone: a node counts a peer reachable while the hub is up and lists the peer `connected`, and no frame a peer sends changes that. A `hello` is the node's description, sent when it changes (the boundary check's time aside), at least every 30 minutes, and again after the hub returns.

Payload kinds (JSON, discriminated on `kind`): `hello`, `snapshot`, `session`,
`event`, `queue`, `review`, `review_update` (direct only), `questions`, `node`,
`command`, `ack`, `events_request`,
`events_batch`, `key_handoff` (direct only), `policy_bundle` (direct only),
`credential_handoff` (direct only; broker rows for the recipient), `changes`,
`changes_request` (direct only), `changes_batch` (direct only), `repo_images`.

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
A `repo_images` payload `{ images }` on a member channel (never `@mesh`) lists image
statements the sender signed for repository images it built and pushed to a registry:
`{ repo, recipe, platform, image, signer, signed_ms, sig }`, `repo` a forge identity,
`recipe` the 64-hex recipe hash the node keys its build on, `platform` `linux/<arch>`,
`image` `name@sha256:<64 hex>`, `signer` the sender's node id, and `sig` Ed25519 by
`signer` over `"tracon/v0/image-statement" ‖ len‖repo ‖ len‖recipe ‖ len‖platform ‖
len‖image ‖ len‖signer ‖ signed_ms(i64 BE)` (`spec/vectors/image-statement.json`). A
receiver drops the payload if any statement is not the sender's own or does not verify,
and takes an image only by the digest signed, from a signer that still shares the
channel. A `snapshot` cadence repeats the sender's statements. The kind is additive and
the contract version is unmoved: an older node cannot read it and drops it.
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
A `start_reviewer` command `{ review_id, revision_id?, statement? }` starts a fresh
reviewer on a review the owner holds, as its own operator would. It spends the owner's
models, so it is an admin command like `config_patch`: run only for an admin peer, with
a statement for exactly this command on itself. The owner refuses it when `revision_id`
is no longer the review's latest revision, and returns any refusal of the start itself
verbatim. The op is additive and the contract version is unmoved: a node that predates
it answers that it runs an older build.
An `admin_request` command `{ method, path, body?, statement? }` runs one of the
owner's own admin routes for an admin peer, as its interface would: `path` is the
route's path with no query, `body` its JSON body. The owner runs only the routes it
lets a peer reach (plugin update, rollback and restart; authority grants; stopping
and reopening a channel to external harnesses and its `notify` binding; offering a
repository recipe and accepting one; restarting the node; and reads of the lists they
act on), only for an admin peer, and every one but a read only with a statement for
exactly this command on itself. It answers `{ status, body }`, the route's own
answer. The op is additive and the contract version is unmoved: a node that predates
it answers that it runs an older build.
Version 4 lets a review row and a queued card name no session: a harness the operator
runs themselves calls with none. `session_id` on a review row and on a `queue` card
may be `null`, a review row gains an optional `lane` (the caller's display label), and
a session-less card is grouped by its `intent.channel`. A version 3 node fails to
deserialize the null, so the two cannot share a mesh.
Version 5 replaces the `reviews` payload, which listed every open review with its
diff and description each time the sender's queue was republished, with `review`
`{ event }`: one change to one of the sender's reviews, on the review's channel.
`event` is tagged on `event`: `opened` and `revised` carry `review`, the owner's
metadata row (`id`, `session_id`, `node_id`, `channel`, `kind`, `title`, `state`,
`provider`, `target`, `base_ref`, `head_sha`, `added`, `removed`, `files`, `lane`,
`publication_failed`, `claimed_ms`, `claimers`, `created_ms`, `updated_ms`, and `body`
for a report only); `claimed` and `unclaimed` carry `{ review_id, claimers,
claimed_ms }`; `closed` carries `outcome` `{ id, state, verdict_reason,
publish_result, updated_ms }`. A `snapshot`'s `reviews` lists the same metadata rows.
The `review_detail` command is gone. A `review_watch` command `{ request }`, where the
request is `{ review_id, channel, owner, renew? }`, watches a review the receiver
owns, on a channel both share: it answers with the owner's whole detail, the diff
included (cut at a line under 2 MiB, with `diff_truncated: { bytes, shown }`), and the
owner then sends the watcher `review_update` `{ review_id, detail }` directly whenever
that detail changes (with `review.diff` null and `diff_omitted: true` when the watcher
already holds that head's diff). A watch lapses two minutes after it was last renewed
(`renew: true`, answered `{ watching }`), when the watcher is unreachable, or when the
review closes; one on a review that left the queue answers the detail once and
watches nothing. `review_unwatch` `{ request }` ends a watch. `review_read`
`{ request }` answers `{ review, outcome }`, the metadata row and, once the review
left the queue, its outcome. A version 4 node cannot read these, so the two cannot
share a mesh.
A peer mirrors a session-less card by the channel it was listed on and sends an
`answer` for it to the card's node, which decides and runs it for a co-member of the
channel. An `answer` that is an admin action on the card's node (allowing a repository
setup proposal, a Dockerfile trial or a plugin install or update, or saving an egress
host for the repository or a fetch pattern to the channel's allow list) also needs the sender named in that node's `[mesh]
admin_peers` and an optional `statement` field the sender's operator confirmed for
exactly this answer (below); without both it is refused. The field is additive and the
contract version is unmoved: an older owner ignores it and refuses such an answer as
before. A
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
| `GET /v0/events` | member | SSE pokes, no payload: `frames` (pull), `members` (a member connected or disconnected; read the list again), `sync` (pokes were lost; do both). Holding one is what makes a member connected |
| `GET /v0/members` | member | routing metadata, and each member's presence as the hub sees it: `connected` (holds an events stream) and `last_seen_ms` (its last authenticated request as a member since the hub started). Presence is never stored with the member |
| `PUT /v0/enroll/{code}` | member | open a slot |
| `POST /v0/enroll/{code}` | none, rate-limited; `binding_sig` must prove the pair | fill it (public keys and a name); returns `{inviter}`, the node id that opened the slot |
| `GET /v0/enroll/{code}` | slot creator | fetch and delete |
| `POST /v0/claims` | member | `{key, version, ttl_secs}`: first member to claim a version of an opaque key wins (`201`); others get `409 {holder, version}` until it lapses, and `409 {version}` for an older version |
| `POST /v0/admit`, `DELETE /v0/admit/{id}`, `DELETE /v0/admit/{id}/channels/{channel}` | member; writing a sealing key needs the target's `binding_sig` | membership |
| `GET /v0/relay/listen` | node member; `404` without a relay listener | SSE: `ready {host}`, then `open {id}` per relayed connection (`relay.rs`) |
| `GET /v0/relay/streams/{id}` | the node the connection was offered to, once | WebSocket carrying its raw bytes; an empty binary message ends a direction |
| `GET /v0/relay/connect` | none; `404` without a relay | a sandbox's WebSocket: binary messages (at most 64 KiB) carry its TLS to a node, routed by SNI as on the listener; an empty one ends a direction. Anything not routed gets a close and nothing else; one client may hold 8 connections in handshake, and each has 3 s to send its ClientHello. `GET /v0/info` lists it as `sandbox_relay.connect` |

The relay listener itself (`TRACON_HUB_RELAY_ADDR`) is not HTTP: it reads a TLS
ClientHello's SNI, `<first 32 hex of node id>.<relay domain>`, and forwards the
connection's bytes unchanged. `/v0/relay/connect` does the same with the bytes of
a WebSocket's binary messages. `GET /v0/info` reports `sandbox_relay: {domain}`.

## Generated names (`names.rs`)

Not a wire format, but pinned the same way: the node, the CLI and the interface
(`spa/src/lib/names.ts`) must derive the same display name for a record with
nothing stored or synced.

```
h    = splitmix64_finalizer(FNV-1a-64("tracon/name/v1" ‖ 0x1f ‖ kind ‖ 0x1f ‖ id))
name = kind "-" adjectives[h mod A] "-" nouns[(h / A) mod N] "-" zero_pad2((h / (A·N)) mod 100)
```

- `id` is the full id as stored (UTF-8), never a prefix: a UUIDv7's first 12 hex
  characters are its timestamp.
- The finalizer is splitmix64's mix step without the golden-ratio increment:
  `z ^= z >> 30; z *= 0xbf58476d1ce4e5b9; z ^= z >> 27; z *= 0x94d049bb133111eb; z ^= z >> 31`.
- `A` and `N` are the lengths of the lists in `proto/src/names-v1.json`, which both
  sides embed. The lists are frozen: a changed list is `v2`, with a new domain.
- Kinds: `session`, `review`, `report`, `approval`, `manifest`, `agent`, `task`,
  `transfer`, `check`, `node`.

A name is a label, never a key; two records of a kind may share one.

The operator can rename a session, review, report, approval, manifest, transfer
or check. Those names are the replicated `entity_name` table (row id
`<kind>:<entity id>`, last writer wins by HLC like any record); a revert is a
tombstone. A record with no row reads by its generated name, and the generated
name still resolves after a rename. Resolution (`GET /api/resolve`, CLI id
arguments, tool id arguments) tries, in order: an operator name (ignoring case),
a generated name, the full id, then an id prefix of at least 6 characters; the
first step that finds anything answers, and several answers are an ambiguity
listed by name and id, never a guess.

## Vectors

| File | Pins |
|---|---|
| `key-derivation.json` | seed → both public keys, node id, credential-store key |
| `envelope.json` | seal, wrap, box bytes for fixed nonces and ephemerals |
| `auth.json` | descriptor bytes and signatures |
| `keyring.json` | container bytes for genesis + one rotation, and a handoff |
| `frame.json` | canonical bytes and ids for channel and direct headers |
| `admin-statement.json` | op hash, signed bytes and signature of a fixed statement |
| `names.json` | list lengths, hashes and generated names for fixed ids, two of them UUIDv7s from one millisecond |
