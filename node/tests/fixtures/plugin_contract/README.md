# Plugin contract vectors

The caller-context test vectors (contract v1, `params._meta.tracon`) of the two
plugins that implement it, copied verbatim:

- `sql-review.json`: `cosmicspork/sql-review@9ec963c5`, `tests/contract_vectors.json`
- `jira-review.json`: `cosmicspork/jira-review@e8e81356`, `tests/contract_vectors.json`

What the vectors check, and how a plugin verifies a context, is in
[docs/reference/plugins.md](../../../../docs/reference/plugins.md#the-caller-context-_metatracon).

`node/tests/plugin_contract.rs` checks that the node signs exactly what they
verify, and that the tests' model of a plugin refuses each tampered case at the
check it names. When the wire format changes, update both plugins and both
files here together, naming the new commits above.

Each file's `valid_preview_token` case is the node's own, not the plugin's:
its `valid` call approved from a card whose `tracon/preview` returned a token,
so the context carries `preview_token` (and an `approval_id`), re-signed with
the file's `seed` and nonce by `generate_preview_token_vectors` in
`plugin_contract.rs` (`--ignored`). Once a plugin's `contract_vectors.json`
carries `valid_preview_token` of its own, replace its file here verbatim and
name the new commit.
