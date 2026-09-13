// Rewrite /opt/tracon/toolchain.json in place, adding the digest and size of
// every binary the profile names. The profile is committed alongside the
// Containerfile and is what the node renders its `lsp`/`formatter` config
// from; the digests are the image's own record of what that rendering will
// actually execute, and exist so a node (or an operator) can tell one built
// image from another without re-reading it.
//
// Run in the final stage, after every tool is in place: a digest of something
// that was copied from a builder stage is only true once it has been copied.
const crypto = require("node:crypto")
const fs = require("node:fs")

const path = "/opt/tracon/toolchain.json"
const manifest = JSON.parse(fs.readFileSync(path, "utf8"))

function digest(file) {
  // Follows symlinks on purpose: the named path is what OpenCode spawns, and
  // the bytes behind it are what runs.
  const real = fs.realpathSync(file)
  const bytes = fs.readFileSync(real)
  return {
    path: file,
    resolved: real,
    size: bytes.length,
    sha256: crypto.createHash("sha256").update(bytes).digest("hex"),
  }
}

const binaries = []
for (const tool of [...manifest.lsp, ...manifest.formatter]) {
  const file = tool.command[0]
  const entry = digest(file)
  binaries.push({ id: tool.id, version: tool.version, ...entry })
}
// The tsserver bundle is not spawned, but it is named in `initialization` and
// a missing one makes the TypeScript server useless, so it is recorded too.
for (const tool of manifest.lsp) {
  const file = tool.initialization?.tsserver?.path
  if (file) binaries.push({ id: `${tool.id}:tsserver`, version: tool.version, ...digest(file) })
}

manifest.binaries = binaries
fs.writeFileSync(path, JSON.stringify(manifest, null, 2) + "\n")
