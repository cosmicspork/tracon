# Plugin contract vectors

The caller-context test vectors (contract v1, `params._meta.tracon`) of the two
plugins that implement it, copied verbatim:

- `sql-review.json`: `cosmicspork/sql-review@9ec963c5`, `tests/contract_vectors.json`
- `jira-review.json`: `cosmicspork/jira-review@e8e81356`, `tests/contract_vectors.json`

`node/tests/plugin_contract.rs` checks that the node signs exactly what they
verify, and that the tests' model of a plugin refuses each tampered case at the
check it names. When the wire format changes, update both plugins and both
files here together, naming the new commits above.
