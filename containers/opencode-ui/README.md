# `opencode-ui` — the native interface, vendored

OpenCode's web UI, built from the pinned tag and served by tracon on an origin
of its own (`node/src/http/ui.rs`). Not a container image, despite living here:
it is a release-time artefact with the same shape as one — a recipe that is
checked in, an output that is not, and a digest that ties them together.

## Why tracon builds it instead of using the harness's copy

The pinned `opencode` binary already contains this bundle, embedded in `bunfs`
and served from a catch-all at `/*`. That catch-all is the problem. When the
embedded import resolves to `null` — because `OPENCODE_DISABLE_EMBEDDED_WEB_UI`
is set, *or because the dynamic import threw and was swallowed by
`.catch(() => null)`* — the same route proxies the request, method and body, to
`https://app.opencode.ai`. No flag disables that fallback; the flag that looks
like it would *selects* it. Under it the server sends a policy with
`connect-src *`.

(`docs/reference/opencode-v1.18.30/api-ui.md` §6 and §8 #6–7; manifest finding 3.)

So tracon serves `/` itself, from this tree, and never forwards the catch-all.
A missing asset is then a 404 on tracon's own origin rather than a silent fetch
of unpinned upstream JavaScript.

## Building it

```sh
./containers/opencode-ui/build.sh --src <opencode checkout at v1.18.30>
```

The recipe does not fetch the source: which checkout it builds from is the
operator's to attest, the same way the harness tarball's sha256 is. It runs

```sh
bun install --frozen-lockfile --ignore-scripts     # at the checkout root
bun run --cwd packages/app build                   # vite build
```

then drops `*.map` (upstream's own release does the same, at
`packages/opencode/script/build.ts:33`) and `_headers` (a Cloudflare Pages rule
file, meaningless here), refuses to vendor a tree that mentions
`app.opencode.ai`, prints the tree digest, and installs the result where the
node looks for it — `$XDG_STATE_HOME/tracon/opencode-ui`, or wherever
`[ui] opencode_bundle_dir` says.

`--ignore-scripts` is load-bearing: the workspace's lifecycle scripts build
native modules for `packages/core` (node-pty, tree-sitter grammars) that need a
node-gyp toolchain and that the browser bundle does not link. Nothing the web
build reads is produced by a lifecycle script.

`--tar <dir>` also writes a deterministic `opencode-ui-v1.18.30.tar.gz`
(sorted names, no owner, fixed mtime, `gzip -n`), for shipping the artefact
alongside a release rather than rebuilding it on every node.

## Getting it onto a node

Building it is the fallback, not the path. The release carries the tarball, and
three things install it:

```sh
tracon setup                                     # fetch the release asset and verify it
tracon setup --ui-bundle opencode-ui-v1.18.30.tar.gz   # the same, from a local file
```

`tracon setup` unpacks into a staging directory, hands it to the node's own
`Bundle::load`, and moves it into place only if the tree digest is the one in
`DIGEST` (`node/src/ui_bundle.rs`). A node that cannot get it serves no native
UI and says so; it never reaches for `app.opencode.ai`, which is the whole
point of finding 3.

`Dockerfile.node` carries the tree at `/opt/tracon/opencode-ui` with
`TRACON_OPENCODE_UI_DIR` pointing at it, because a pod's state directory is a
volume that would shadow anything installed under it. Drop the tarball into
this directory to build that image offline; otherwise it is fetched from the
release. Either way the image build asserts the tarball's sha256 *and*
recomputes the tree digest.

The release job (`opencode-ui` in `.github/workflows/release.yml`) checks out
the pinned upstream **commit**, runs this recipe, fails if the digest is not
`DIGEST`, attests the tarball with GitHub build provenance the way the desktop
assets are attested, and uploads it. No new secret: the source is a public tag
and the artefact is authenticated by provenance.

## The digest

`DIGEST` holds the tree digest of the artefact: for each file in path order,
the path, a NUL byte, the bytes, a NUL byte, hashed with SHA-256. `build.sh`
computes it in shell and `Bundle::load` recomputes it in Rust over the bytes it
is about to serve — so what is verified is what is served, not what was on disk
at some earlier moment. A tree that does not match is not served at all: the
origin answers 503 and says to rebuild.

| | |
|---|---|
| Upstream | `anomalyco/opencode`, tag `v1.18.30`, commit `3104c14` |
| Build | `bun install --frozen-lockfile --ignore-scripts` then `bun run --cwd packages/app build` |
| Toolchain | bun 1.3.14 (the workspace's `packageManager`), vite 7.1.4 |
| Files | 951 |
| Bytes | 36,049,284 (34.4 MiB), sourcemaps dropped (48 MB of the 85 MB the build emits) |
| Tree digest | `348cb604b71e6f4706f3c5ee43d0f2ff44f01fce9cd8c8623b1759d9334e5ae7` |
| `opencode-ui-v1.18.30.tar.gz` | `782ca629c49b1e2b620460b90c4d8ec7b1a2ad9bdc9783ba9227ad626cfb6696` |

The build is reproducible on one toolchain: two runs from the same checkout
produced the same digest. It is not claimed to be reproducible across bun or
vite versions, which is why the toolchain is in the table.

## Why it is not embedded in the node binary

34 MiB, 951 files. `spa/dist` is embedded next to it because it is small;
this is not. The tree is read from disk at startup, held in memory, and
verified by digest — which gives the same integrity guarantee as embedding
without a 34 MiB binary or a 951-file `rust-embed` compile.

## What is in it that tracon refuses at run time

The tree contains **no reference to `app.opencode.ai`** — that host lives in
the *server*, not in the app. It does contain absolute references that the
origin's CSP makes unreachable, and they are worth naming rather than
implying:

| In the tree | What it is | Why it cannot fire |
|---|---|---|
| `https://opencode.ai` (49) | release-notes fetch, the notification icon, docs links | `connect-src 'self'` and `img-src 'self' data: blob:`. The docs links are `openExternal` new-tab targets, not fetches |
| `http://localhost:4096` (64) | `getCurrentUrl()`'s branch for when `location.hostname` contains `opencode.ai`, plus dev-server defaults | the UI origin's hostname never contains `opencode.ai`, so the branch is dead; `import.meta.env.DEV` is false in this build |
| `https://api.myprovider.com` and its translations | placeholder text in the settings dialog | not a URL the app ever loads |
| `http://www.w3.org` | SVG namespaces | not a fetch |

No Sentry: `VITE_SENTRY_DSN` is unset in this build, so `Sentry.init` is never
reached (`packages/app/src/entry.tsx:133-150`). No service worker, no workbox,
anywhere in the tree.
