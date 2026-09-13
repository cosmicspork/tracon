# Gate A — OpenCode v1.18.30: configuration, ambient content, skills, plugins, MCP, LSP, state, network

Source tree examined: `<opencode-src>`
Version asserted by `packages/opencode/package.json:3` → `"version": "1.18.30"`.
All paths below are relative to that tree unless absolute. Line numbers are from `cat -n` on the
checked-out file.

Plan sections this answers: the migration plan (`plan-tracon-opencode-migration`) lines 131–143
("Skills, configuration, plugins and LSP") and lines 189–207 ("State isolation, upgrades and legacy
cutover").

---

## 0. Architectural facts that frame everything else

**The server is multi-instance and the instance is chosen by the client, per request.**
`packages/opencode/src/cli/cmd/serve.ts:10-12` — comment: *"Server loads instances per-request via
x-opencode-directory header — no need for an ambient project InstanceContext at startup."*
`instance: false` at line 12.
The directory actually used is resolved in
`packages/opencode/src/server/routes/instance/httpapi/middleware/workspace-routing.ts:87`:

```ts
return url.searchParams.get("directory") || request.headers["x-opencode-directory"] || process.cwd()
```

Consequence for tracon: config discovery is **not** a one-shot startup event. Any authenticated
client can name an arbitrary filesystem directory and cause a fresh
`Config.loadInstanceState` for it — including project config, `.opencode/`, `AGENTS.md`, agents,
commands, plugins and an npm install in that directory. Launch-time env is therefore the only
reliable control surface; "we only start the server in the sandboxed worktree" is **not** sufficient.
Tracon's gateway must additionally pin/validate `?directory=` and `x-opencode-directory`.

Server auth is `OPENCODE_SERVER_PASSWORD` / `OPENCODE_SERVER_USERNAME`
(`packages/core/src/flag/flag.ts:32-33`); `serve` prints a warning and runs **unsecured** if the
password is unset (`packages/opencode/src/cli/cmd/serve.ts:15-17`).

**Two flag mechanisms exist and they are not the same object.**
- `packages/core/src/flag/flag.ts` — a plain `process.env` reader (`Flag.*`). Some entries are
  captured at module load (lines 16–51) and some are getters evaluated per access (lines 54–77).
- `packages/opencode/src/effect/runtime-flags.ts` — an Effect `ConfigService` (`RuntimeFlags.*`)
  carrying a second, larger set of `OPENCODE_DISABLE_*` switches (lines 16–57).
A node-written launch manifest must set env for **both**; neither can be set through a config file.

---

## 1. Configuration discovery — exhaustive

### 1.1 The global config directory is NOT moved by `OPENCODE_CONFIG_DIR`

`packages/core/src/global.ts:1-31`:

```ts
import { xdgData, xdgCache, xdgConfig, xdgState } from "xdg-basedir"
const app = "opencode"
const data   = path.join(xdgData!,   app)   // line 11
const cache  = path.join(xdgCache!,  app)   // line 12
const config = path.join(xdgConfig!, app)   // line 13
const state  = path.join(xdgState!,  app)   // line 14
const tmp    = path.join(os.tmpdir(), app)  // line 15
get home() { return process.env.OPENCODE_TEST_HOME ?? os.homedir() }  // lines 18-20
```

`Global.Path.config` is therefore always `${XDG_CONFIG_HOME:-$HOME/.config}/opencode`.

There *is* a second accessor, `Global.make()` at `packages/core/src/global.ts:59-72`, whose `config`
field is `Flag.OPENCODE_CONFIG_DIR ?? Path.config` (line 64). **But the config loader does not use
it for the global file read.** `packages/opencode/src/config/config.ts:272-274` reads
`path.join(Global.Path.config, ...)` — the raw XDG path, not the service. Line 413 likewise merges
under the source label `Global.Path.config`.

**Settled:** `OPENCODE_CONFIG_DIR` *adds* a directory (see §1.3 step 6); it does **not** relocate or
suppress the global config file. Only `XDG_CONFIG_HOME` (and `HOME`, via `xdg-basedir`'s fallback)
moves the global config. This is exactly the trap the plan warns about at line 137.

### 1.2 Global config file candidates and self-writes

`packages/opencode/src/config/config.ts:140-148` (`globalConfigFile()`), candidates in order:
`opencode.jsonc`, `opencode.json`, `config.json`, all under `Global.Path.config`.

`loadGlobal()` at lines 260–293 merges **all three** unconditionally (lines 272–274), in the order
`config.json` → `opencode.json` → `opencode.jsonc` (later wins).

Two **writes** happen here:
- Lines 264–271: if none of `OPENCODE_CONFIG`, `OPENCODE_CONFIG_DIR`, `OPENCODE_CONFIG_CONTENT` is
  set and the global file is missing, OpenCode **creates** `$XDG_CONFIG_HOME/opencode/opencode.jsonc`
  seeded with `{"$schema": "https://opencode.ai/config.json"}`. Setting any one of those three env
  vars suppresses the seed write (useful: a read-only global config dir is then tolerable).
- Lines 276–290: legacy TOML migration — if `$config/config` exists it is `import()`-ed as TOML,
  merged, **written back** to `config.json`, and the original **unlinked**. Only fires if that file
  exists.

Separately, `loadConfig()` at lines 245–249 **rewrites any config file it loads** that lacks
`$schema`, injecting `"$schema": "https://opencode.ai/config.json"` into the file text. The write is
`.pipe(Effect.catch(() => Effect.void))`, so a read-only mount is survivable — but a node-written
manifest should include `$schema` itself to avoid the attempted write entirely.

### 1.3 Full merge order in `Config.loadInstanceState` (`packages/opencode/src/config/config.ts:328-609`)

Later merges win (`mergeConfigConcatArrays`, lines 46–52; `instructions` arrays are **unioned**, not
replaced — line 49).

| # | Source | Lines | Gate |
|---|---|---|---|
| 1 | **Remote `.well-known/opencode`** for every stored auth entry of `type: "wellknown"` | 370–410 | Only if `auth.json` holds a wellknown entry. No env kill-switch. |
| 1b | The **`remote_config.url`** named by that well-known doc (a second HTTP fetch, with `{env:}`-substituted headers) | 377–395, 65–100 | same |
| 2 | Global config (`config.json`, `opencode.json`, `opencode.jsonc` under `$XDG_CONFIG_HOME/opencode`) | 412–413 → 260–293 | `XDG_CONFIG_HOME` / `HOME` only |
| 3 | **`OPENCODE_CONFIG`** — a single extra file path | 415–418 | env |
| 4 | **Project config files** `opencode.jsonc` / `opencode.json`, walked **up the tree** | 420–424 | `OPENCODE_DISABLE_PROJECT_CONFIG` |
| 5 | For each dir in `ConfigPaths.directories()`: `opencode.json` + `opencode.jsonc` inside it | 438–448 | partially (see below) |
| 5b | For each such dir: `.gitignore` **write**, `npm install @opencode-ai/plugin` **(network)**, commands, agents, modes, auto-discovered plugins | 450–479 | not gated |
| 6 | **`OPENCODE_CONFIG_CONTENT`** — inline JSON(C) from env | 482–490 | env |
| 7 | **Console/org remote config** `${account.url}/api/config` | 495–528 | only if an active account with `active_org_id` exists in the account store |
| 8 | **System managed config dir**: `/etc/opencode` (Linux), `/Library/Application Support/opencode` (macOS), `%ProgramData%\opencode` (Windows) — `opencode.json`/`opencode.jsonc` | 530–536; `packages/opencode/src/config/managed.ts:20-33` | `OPENCODE_TEST_MANAGED_CONFIG_DIR` overrides the dir |
| 9 | **macOS MDM managed preferences** `/Library/Managed Preferences/[user/]ai.opencode.managed.plist` via `plutil` — *"override everything"* | 538–548; `managed.ts:43-68` | darwin-only; no-op on Linux |
| 10 | **`OPENCODE_PERMISSION`** — JSON merged into `config.permission` | 559–565 | env |

Step 4's walk: `ConfigPaths.files` at `packages/opencode/src/config/paths.ts:10-21` calls
`FSUtil.up({ targets: ["opencode.jsonc","opencode.json"], start: directory, stop: worktree })`.
`FSUtil.up` (`packages/core/src/fs-util.ts:168-182`) walks parents and **breaks only on
`stop === current` or filesystem root** (lines 176–179). So yes — it walks parent directories, and
if `worktree` is undefined or is not an ancestor of `directory`, it walks all the way to `/`.

Step 5's directory list, `ConfigPaths.directories` (`packages/opencode/src/config/paths.ts:23-41`):

```ts
unique([
  Global.Path.config,                                              // line 26  — always
  ...(!Flag.OPENCODE_DISABLE_PROJECT_CONFIG                         // lines 27-33
        ? yield* afs.up({ targets: [".opencode"], start: directory, stop: worktree })
        : []),
  ...(yield* afs.up({ targets: [".opencode"],                       // lines 34-38 — NOT GATED
        start: Global.Path.home, stop: Global.Path.home })),
  ...(Flag.OPENCODE_CONFIG_DIR ? [Flag.OPENCODE_CONFIG_DIR] : []),  // line 39
])
```

**`$HOME/.opencode` is discovered unconditionally** — `OPENCODE_DISABLE_PROJECT_CONFIG` does not
suppress it. It is only neutralised by a clean per-session `HOME`.

### 1.4 `OPENCODE_DISABLE_PROJECT_CONFIG` — what it actually covers

Defined as a live getter at `packages/core/src/flag/flag.ts:54-56` (comment at 52–53 confirms it is
read at access time, not module load).

| Suppressed | Not suppressed |
|---|---|
| Project `opencode.json(c)` upward walk (`config.ts:420`) | Global config (`config.ts:412`, and upstream test `packages/opencode/test/config/config.test.ts:2027-2037` asserts this) |
| Project `.opencode/` dirs → their config, commands, agents, modes, plugins, skills (`paths.ts:27-33`) | `$HOME/.opencode` (`paths.ts:34-38`) |
| Project `AGENTS.md` for the system prompt (`packages/opencode/src/session/instruction.ts:81`, `123`) | `$config/AGENTS.md`, `$HOME/.claude/CLAUDE.md` (`instruction.ts:60-63`) |
| Ambient `AGENTS.md` system-context (`packages/core/src/instruction-context.ts:48-54`) | **Nested `AGENTS.md`/`CLAUDE.md` attached at runtime by the `read` tool** (`instruction.ts:179-221`) — see §2.2 |
| Relative `instructions` globs re-rooted to `global.config` (`instruction.ts:81-89`) | `OPENCODE_CONFIG`, `OPENCODE_CONFIG_CONTENT`, managed dirs, remote config |

Upstream tests that pin this behaviour: `packages/opencode/test/config/config.test.ts:1995-2037`
("skips project config files when flag is set", "skips project `.opencode/` directories when flag is
set", "still loads global config when flag is set") and
`packages/core/test/instruction-context.test.ts`.

### 1.5 `.env` files

**OpenCode does not load `.env` files.** A grep for `dotenv`, `loadEnvFile`, `parseEnv`, `".env"`
across `packages/*/src` returns only: a file-picker exclusion list
(`packages/app/src/constants/file-picker.ts:22`), an attachment exclusion list
(`packages/session-ui/src/v2/components/prompt-input/attachments.ts:27`) and default **permission**
rules that put `*.env` behind `ask` (`packages/opencode/src/agent/agent.ts:132`,
`packages/core/src/plugin/agent.ts:115`). No dotenv dependency, no env-file parsing. Clean negative.

Env *does* reach config through `{env:VAR}` substitution:
`packages/opencode/src/config/variable.ts:36-38` — `text.replace(/\{env:([^}]+)\}/g, ...)` reading
`input.env?.[varName] ?? process.env[varName]`. And through `{file:path}` substitution (lines
40–89), which reads arbitrary files relative to the config file's directory, supports `~/`
expansion (lines 62–64) and absolute paths (line 66). **A project-supplied config file can therefore
exfiltrate any readable file into config values** — one more reason `OPENCODE_DISABLE_PROJECT_CONFIG`
must be set, not just relied on.

### 1.6 Verdict on "single node-written config as the ONLY source"

**Not natively supported. There is no `--config-only` / `OPENCODE_NO_DISCOVERY` switch.** The best
available is **custom path plus env vars that neutralise the others**, and it requires *all* of:

```
XDG_CONFIG_HOME=<per-session, empty or read-only>   # neutralises global config (§1.1)
XDG_DATA_HOME  =<per-session, empty>                # empties auth.json → no .well-known fetch (§1.3 #1)
XDG_CACHE_HOME =<per-session>
XDG_STATE_HOME =<per-session>
HOME           =<per-session, no .opencode, no .claude>   # neutralises paths.ts:34-38 and instruction.ts:62
OPENCODE_DISABLE_PROJECT_CONFIG=true                # §1.4
OPENCODE_CONFIG=<read-only manifest path>           # the single intended source
```

…plus these residual sources that **no env var disables**:

- `/etc/opencode/opencode.json{,c}` — `managed.ts:27`. Overridable only via
  `OPENCODE_TEST_MANAGED_CONFIG_DIR` (`managed.ts:32`), an explicitly test-named variable. Relying
  on it is exactly the "guessed environment variable" the plan forbids (line 137). In a Podman/K8s
  runner the honest control is *don't create `/etc/opencode` in the image* — which is a packaging
  control, and verifiable.
- macOS managed preferences (`managed.ts:43-68`) — darwin-only, irrelevant to a Linux runner.
- Remote well-known / console-org config — gated on credentials in the per-session
  `XDG_DATA_HOME`/account store, so an empty per-session data dir neutralises them **in practice**,
  but there is no explicit disable flag.

**Status: supportable by env + config + image packaging, with an auditable residue.** The claim to
make in the design record is "no ambient *discovery*, given a hermetic image and per-session
HOME/XDG", not "config is single-source by construction".

---

## 2. Ambient content picked up from a checked-out repository

### 2.1 Instruction files

`packages/opencode/src/session/instruction.ts:60-68`:

```ts
const globalFiles = [
  path.join(global.config, "AGENTS.md"),                                        // 61
  ...(!flags.disableClaudeCodePrompt ? [path.join(global.home, ".claude", "CLAUDE.md")] : []),  // 62
]
const instructionFiles = [
  "AGENTS.md",                                                                   // 65
  ...(!flags.disableClaudeCodePrompt ? ["CLAUDE.md"] : []),                       // 66
  "CONTEXT.md", // deprecated                                                     // 67
]
```

Search order for the system prompt (`systemPaths`, lines 110–153):
1. First existing entry of `globalFiles` — `break` at line 119 (first-wins).
2. If `!OPENCODE_DISABLE_PROJECT_CONFIG` (line 123): `findUp` each of `AGENTS.md`, `CLAUDE.md`,
   `CONTEXT.md` from `ctx.directory` up to `ctx.worktree`; **first file name that matches anywhere
   wins and the loop breaks** (lines 124–132; comment at 122: *"The first project-level match wins so
   we don't stack AGENTS.md/CLAUDE.md from every ancestor"*).
3. `config.instructions` globs (lines 135–150) — `~/` expanded to `global.home` (line 138), absolute
   paths globbed in place (141–145), relative paths globbed **up the tree** via `FSUtil.globUp`
   (`instruction.ts:79-89` → `packages/core/src/fs-util.ts:184-198`). When
   `OPENCODE_DISABLE_PROJECT_CONFIG` is set, relative globs are re-rooted to `global.config` (lines
   86–88).

**`.cursorrules`, `.cursor/rules`, `.github/copilot-instructions.md` are NOT read.** Grep across
`packages/core/src` and `packages/opencode/src` for those names returns nothing. Only
`AGENTS.md` / `CLAUDE.md` / `CONTEXT.md`.

Disable:
- `AGENTS.md`/`CLAUDE.md`/`CONTEXT.md` from the repo → `OPENCODE_DISABLE_PROJECT_CONFIG=true`.
- `CLAUDE.md` (project and `$HOME/.claude/CLAUDE.md`) → `OPENCODE_DISABLE_CLAUDE_CODE_PROMPT=true`
  or the broader `OPENCODE_DISABLE_CLAUDE_CODE=true`
  (`packages/opencode/src/effect/runtime-flags.ts:23-26`).
- `$config/AGENTS.md` → only by controlling `XDG_CONFIG_HOME`.

### 2.2 `AGENTS.md` attached at runtime by the `read` tool — NOT gated

`packages/opencode/src/session/instruction.ts:179-221` (`resolve`). Comment at line 193:
*"Walk upward from the file being read and attach nearby instruction files once per message."*
Loop condition at line 194: `while (current.startsWith(root) && current !== root)`.

This path has **no `OPENCODE_DISABLE_PROJECT_CONFIG` check**. Reading `src/foo/bar.ts` pulls
`src/foo/AGENTS.md` (or `CLAUDE.md`, via `find` at lines 171–177 using the same
`instructionFiles` list) into the conversation. Only `OPENCODE_DISABLE_CLAUDE_CODE_PROMPT` removes
the `CLAUDE.md` half; the `AGENTS.md` half is unconditional.

**This is the sharpest ambient-content finding.** Per the plan's line 139 ("Instruction content
grants no permission"), it is not a privilege escalation — but it *is* unsuppressible ambient repo
content reaching the model, and tracon should state that explicitly rather than claim ambient repo
instructions are disabled.

Second, separate mechanism: `packages/core/src/instruction-context.ts:40-74` registers a
`core/instructions` system-context source that reads `AGENTS.md` up-tree. This one **is** gated —
line 48: `Flag.OPENCODE_DISABLE_PROJECT_CONFIG || !insideProject ? [] : yield* fs.up({targets:
["AGENTS.md"], start, stop})`. It still always reads `join(global.config, "AGENTS.md")` (line 58).

### 2.3 `.opencode/` subdirectory contents

Loaded for **every** directory returned by `ConfigPaths.directories()`
(`packages/opencode/src/config/config.ts:438-480`):

| Subdir | Glob | Loader | Notes |
|---|---|---|---|
| `agent/` or `agents/` | `{agent,agents}/**/*.md` | `packages/opencode/src/config/agent.ts:13-18` | `symlink: true`, `dot: true` |
| `mode/` or `modes/` | `{mode,modes}/*.md` | `config/agent.ts:36-41` | |
| `command/` or `commands/` | `{command,commands}/**/*.md` | `packages/opencode/src/config/command.ts:15-20` | a malformed file **throws** (line 36) |
| `plugin/` or `plugins/` | `{plugin,plugins}/*.{ts,js}` | `packages/opencode/src/config/plugin.ts:21-28` | auto-registered as `file://` specs |
| `skills/` | see §3 | | |
| `tool/`, `tools/` | see §4 | | |

**`symlink: true` on all three globs** (`config/agent.ts:17`, `config/command.ts:19`,
`config/plugin.ts:25`) — symlinks are followed, so the plan's requirement to "validate package
paths/symlinks" (line 133) is load-bearing: a repo can symlink `.opencode/plugins/x.ts` out of the
worktree.

Disable: `OPENCODE_DISABLE_PROJECT_CONFIG=true` removes the *project* `.opencode` dirs from the list;
`$HOME/.opencode` and `$XDG_CONFIG_HOME/opencode` remain and must be controlled by per-session
HOME/XDG.

### 2.4 Writes and installs into every config directory — read-only-mount hazard

`packages/opencode/src/config/config.ts:450`:

```ts
yield* ensureGitignore(dir).pipe(Effect.orDie)
```

`ensureGitignore` (lines 309–326) does `fs.ensureDir(dir)` then writes a `.gitignore` containing
`node_modules`, `package.json`, `package-lock.json`, `bun.lock`, `.gitignore` (lines 314–318). The
write catches only `PermissionDenied` (lines 320–323); `ensureDir` itself
(`packages/core/src/fs-util.ts:116-125`) catches only `AlreadyExists`. **An `EROFS` from a read-only
bind mount is neither**, and `Effect.orDie` at line 450 makes it a defect → the instance load dies
(`config.ts:616` `loadInstanceState(ctx).pipe(Effect.orDie)`).

**Action for tracon: mount the config directory read-only only after verifying this path, or
pre-create `.gitignore` in the image and test the read-only mount explicitly in Gate C.** This is a
concrete, testable failure mode, not a theoretical one.

`packages/opencode/src/config/config.ts:452-471` then runs, **for every config directory**:

```ts
npmSvc.install(dir, { add: [{ name: "@opencode-ai/plugin",
                              version: InstallationLocal ? undefined : InstallationVersion }] })
```

forked detached (line 469), failures logged as `"background dependency install failed"` warnings
(lines 463–467). So offline is survivable but it *is* an unconditional package-install attempt per
config dir at instance load. See §4 and §8.

### 2.5 `package.json` and `.mcp.json`

- **`package.json` scripts are never read or executed.** `package.json` is read only for
  formatter selection (`packages/opencode/src/format/formatter.ts:72`, `:95`), LSP root detection
  (`packages/opencode/src/lsp/server.ts:233`, `:1549`), plugin entrypoint resolution
  (`packages/opencode/src/plugin/shared.ts:184,191,219`) and plugin version metadata
  (`packages/opencode/src/plugin/meta.ts:78`).
- **`.mcp.json` is not supported.** Grep across `packages/*/src` for `mcp.json` returns no
  filesystem-discovery hit; MCP servers come **only** from the `mcp` key of the merged config
  (`packages/core/src/v1/config/config.ts:113`). Good: MCP is fully controlled by whoever controls
  config, i.e. `OPENCODE_DISABLE_PROJECT_CONFIG` + a node-written manifest.

### 2.6 Full config key surface (for manifest authoring)

`packages/core/src/v1/config/config.ts` — `Info` fields at lines 36–169, including
`shell:36`, `server:38`, `command:41`, `skills:44`, `plugin:56`, `share:57`, `autoshare:61`,
`autoupdate:64`, `disabled_providers:68`, `enabled_providers:71`, `model:74`, `small_model:77`,
`default_agent:80`, `agent:96`, `provider:110`, `mcp:113`, `formatter:116`, `lsp:120`,
`instructions:124`, `permission:128`, `tools:129`, `attachment:130`, `enterprise:133`,
`tool_output:136`, `compaction:149`, `experimental:169`.
Well-known remote config schema fields `config` / `remote_config` at lines 23–24.


---

## 3. Skills

### 3.0 There are TWO skill implementations in this tree

| | **v1 — what the CLI/`serve` actually uses** | **v2 — core, newer, partially wired** |
|---|---|---|
| Service | `packages/opencode/src/skill/index.ts` | `packages/core/src/skill.ts` |
| URL discovery | `packages/opencode/src/skill/discovery.ts` | `packages/core/src/skill/discovery.ts` |
| Tool | `packages/opencode/src/tool/skill.ts` | `packages/core/src/tool/skill.ts` |
| Prompt injection | `packages/opencode/src/session/system.ts:107-120` | `packages/core/src/skill/guidance.ts:16-32` |
| Registration | `packages/opencode/src/tool/registry.ts:221,244` | `packages/core/src/tool/builtins.ts:12,42`; `packages/core/src/location-services.ts:70` |

Both are live (v2 backs `packages/server/src/handlers/skill.ts:6-8` and the v2 session runner).
**Tracon must pin behaviour to v1 for v1.18.30 and re-diff on every upgrade** — they differ in
security-relevant ways (§3.5).

### 3.1 Format

v1 validation is *only* this (`packages/opencode/src/skill/index.ts:53-59`):

```ts
function isSkillFrontmatter(data: unknown): data is { name: string; description?: string } {
  return isRecord(data) && typeof data.name === "string" &&
    (data.description === undefined || typeof data.description === "string")
}
```

- Recognised fields: **`name` (required string), `description` (optional string)**. Nothing else.
- The published docs (`packages/web/src/content/docs/skills.mdx:39-63`) claim `license`,
  `compatibility`, `metadata`, a `^[a-z0-9]+(-[a-z0-9]+)*$` name regex, a 1–64 char limit and a
  name-must-match-directory rule. **None is enforced.** `InvalidError` / `NameMismatchError` are
  declared at `packages/opencode/src/skill/index.ts:61-71` and referenced nowhere.
- v2 adds `slash: boolean` and allows `name` to be omitted (inferred from filename):
  `packages/core/src/skill.ts:33-38`, `:88-94`.
- Parser: `gray-matter` via `packages/core/src/config/markdown.ts:4-10`, with a lenient
  unquoted-colon retry at `:22-36`.
- **No depth limit, no file-size limit.** `packages/core/src/util/glob.ts:5-21` has no
  `maxDepth`/`deep` option. Patterns use `**` (`packages/opencode/src/skill/index.ts:23-25`).
- **Symlinks are followed** in every scan: `symlink: true` hardcoded at
  `packages/opencode/src/skill/index.ts:155` and `packages/core/src/skill.ts:79`.
- Assets/scripts: supported as a **sampled listing only** (max 10 files), never auto-read or
  executed — `packages/opencode/src/tool/skill.ts:34-60` (`limit: 10`, `follow: false`),
  v2 `packages/core/src/tool/skill.ts:15,84-91` (`FILE_LIMIT = 10`).

### 3.2 Discovery paths, in order (v1 — `packages/opencode/src/skill/index.ts:173-233`)

1. `~/.claude/skills/**/SKILL.md` — Claude-compat, global (`:21,:187,:190-194`)
2. `~/.agents/skills/**/SKILL.md` — global (`:22,:188`)
3. Walk up from cwd to worktree root: `<dir>/.claude/skills/**/SKILL.md` and
   `<dir>/.agents/skills/**/SKILL.md` at every level (`:196-202`)
4. Every dir from `config.directories()` × `{skill,skills}/**/SKILL.md` (`:205-208`) — i.e.
   `$XDG_CONFIG_HOME/opencode`, project `.opencode` dirs, `$HOME/.opencode`, `$OPENCODE_CONFIG_DIR`
   (see §1.3)
5. `config.skills.paths[]` × `**/SKILL.md`, `~/` expanded, relative resolved against cwd, **no
   containment check** (`:210-220`)
6. `config.skills.urls[]` → **HTTP pull** → cache dirs × `**/SKILL.md` (`:222-227`)

Config schema: `packages/core/src/v1/config/skills.ts:5-12` — `{ paths?: string[], urls?: string[] }`.
Referenced from `packages/core/src/v1/config/config.ts:44`.

**No npm-package skill source exists** in v1. (v2 plugins can register sources via a `skill.transform`
hook — `packages/plugin/src/v2/effect/skill.ts:4-11`, `packages/core/src/plugin/internal.ts:113,117`.)

### 3.3 Disabling

`packages/opencode/src/effect/runtime-flags.ts:21,27-30`, consumed at
`packages/opencode/src/skill/index.ts:266-267`, applied at `:186-187`:

| Env var | Effect |
|---|---|
| `OPENCODE_DISABLE_EXTERNAL_SKILLS=true` | drops `.claude/skills` **and** `.agents/skills`, global and project |
| `OPENCODE_DISABLE_CLAUDE_CODE_SKILLS=true` (or `OPENCODE_DISABLE_CLAUDE_CODE=true`) | drops only `.claude` |
| `OPENCODE_DISABLE_PROJECT_CONFIG=true` | drops project `.opencode` skill dirs (via `config.directories()`) — **not** `.claude`/`.agents` |

**There is no flag that disables skill discovery entirely, and none that restricts it to a single
directory.** The only full off-switch is at the tool layer — `tools: { skill: false }` or
`permission.skill: { "*": "deny" }` (`packages/opencode/src/session/system.ts:108`,
`packages/opencode/src/skill/index.ts:314`) — and **discovery, including network fetching, still
runs**.

### 3.4 Name conflicts — last-wins, warning only, and racy

`packages/opencode/src/skill/index.ts:125-139`: a duplicate logs
`Effect.logWarning("duplicate skill name", {...})` and then **overwrites**. Never an error.

Worse, the winner is nondeterministic: `packages/opencode/src/skill/index.ts:240-243` loads matches
with `concurrency: "unbounded"`, and `add` awaits an async `readFile`. So which of two same-named
skills wins varies run to run. A `~/.claude/skills/x/SKILL.md` can silently shadow a project
`.opencode/skills/x/SKILL.md`, or not.

v2 is deterministic (registration order, then `files.toSorted()`) —
`packages/core/src/skill.ts:81,112-117`.

**Direct conflict with plan line 133 ("reject duplicate skill names").** Upstream does not reject;
tracon must reject at manifest-build time, in the node, before launch, and must additionally
guarantee only one skill root exists in the runner (see §3.3 — it cannot be enforced from inside).

### 3.5 Read-only, outside-the-project skill bundles — YES

Absolute paths and `~/` paths in `skills.paths` are accepted with no containment check
(`packages/opencode/src/skill/index.ts:211-219`; the only guard is `fsys.isDir` at `:214`). Loading
is glob → `readFile` → parse; **nothing is written into a skill directory**, no lock, no index.

The only writes are into the **global cache** for URL-sourced skills:
`packages/opencode/src/skill/discovery.ts:35` → `path.join(Global.Path.cache, "skills")`, per-skill
root at `:79`, version stamp `.opencode-version` at `:80,105`, atomic swap via `.tmp-<uuid>` /
`.old-<uuid>` at `:94-96,106-120`. `Global.Path.cache` is XDG cache
(`packages/core/src/global.ts:12`).

**So: a read-only, node-provisioned skill bundle mounted anywhere and named in `skills.paths` works.**
This satisfies the plan's line 133/141 requirement.

Caveat worth recording: discovered skill directories are **auto-whitelisted into the agent's
permission defaults** — `packages/opencode/src/agent/agent.ts:101,108-125` builds
`whitelistedDirs` from `skill.dirs()` and sets `external_directory` to `allow` for them. That
whitelist includes the HTTP-download cache dirs.

### 3.6 Network — yes, and it cannot be disabled

`config.skills.urls[]` triggers unauthenticated HTTP(S) GETs during discovery state init, before any
user interaction: `packages/opencode/src/skill/index.ts:223` → `discovery.pull(url)`
(`packages/opencode/src/skill/discovery.ts:49`).
- `:50-52` index URL `new URL("index.json", base).href`
- `:56-63` GET + decode of `Index = { skills: [{ name, files[], version? }] }` (`:13-21`)
- `:34` wrapped in `withTransientReadRetry` → automatic retries
- `:88-92`, `:98-102` per-file GET → `fs.writeWithDirs(dest, ...)`

**v1's `pull()` has no path-traversal or origin hardening.** `skill.name` and each `file` go straight
into `path.join` (`:79`, `:90`) with only a `files.includes("SKILL.md")` check (`:67-73`). An
`index.json` with `{"name": "../../../x", "files": ["SKILL.md","../../../../.bashrc"]}` writes outside
the cache root, and `files: ["https://other.example/x.md"]` fetches cross-origin.
v2's rewrite **is** hardened — `packages/core/src/skill/discovery.ts:15-24` (`isSafeSegment`),
`:26-53` (`isSafeRelativePath`, post-`decodeURIComponent`), `:124,141` (`FSUtil.contains`), `:138`
(same-origin), `:148-150` (drop whole skill if any file unsafe); tests at
`packages/core/test/skill-discovery.test.ts:46,57,68,79`.

**Nothing disables the fetch.** `OPENCODE_DISABLE_EXTERNAL_SKILLS` does **not** cover
`skills.urls` — inspect `packages/opencode/src/skill/index.ts:186-227`: the `disableExternalSkills`
branch closes at `:203`, the URL loop at `:222` is unconditional. `OPENCODE_PURE` is not consulted.
The only control is **not putting `skills.urls` in the config** — which, given
`OPENCODE_DISABLE_PROJECT_CONFIG` + a node-written manifest + network denial, is adequate defence in
depth but is a *config* control, not a *capability* control.

### 3.7 Invocation and execution

- One fixed tool, `skill`, taking `{ name: string }` — `packages/opencode/src/tool/skill.ts:8-12`;
  registered unconditionally at `packages/opencode/src/tool/registry.ts:221,244`. No per-skill tools.
- Permission: `ctx.ask({ permission: "skill", patterns: [params.name], always: [params.name] })` at
  `packages/opencode/src/tool/skill.ts:27-33`. **Ordering bug:** `skill.require(params.name)` runs
  **before** the ask (`:23-25`), and the `NotFoundError` message enumerates every skill name
  including denied ones (`packages/opencode/src/skill/index.ts:77-79`) — a denied skill's existence
  leaks via probing.
- Skill **scripts are not executed by the skill tool** — only listed. Execution requires a separate
  `bash` call under normal bash permissions.
- **But skills are also registered as slash commands** — `packages/opencode/src/command/index.ts:134-151`
  (`source: "skill"` at `:140`), and command templates are shell-interpolated:
  `packages/opencode/src/config/markdown.ts:6` `SHELL_REGEX = /!`([^`]+)`/g`, executed at `:12-14`.
  **A `SKILL.md` body containing `` !`cmd` `` becomes shell execution on `/skillname`, on a path that
  never reaches the `skill` permission check.** This is the sharpest skill finding: skill *content*
  is executable, so the plan's line 139 ("Instruction content grants no permission") must be applied
  to skill bodies too, and the manifest builder should reject `` !` `` in skill bodies or accept that
  skills are code.
- Prompt injection: `packages/opencode/src/skill/index.ts:321-346`; `location` is HTML-escaped at
  `:333`, **`name` and `description` are not** — frontmatter can inject markup into the system prompt.
  Skills with no `description` are filtered out of the listing (`:322-323`) but remain invocable.

---

## 7. Process and state layout

### 7.1 Directory layout and the env vars that move each

`packages/core/src/global.ts:10-31`:

| Slot | Path expression | Default (Linux) | Moved by |
|---|---|---|---|
| `home` | `OPENCODE_TEST_HOME ?? os.homedir()` (`:18-20`) | `$HOME` | `OPENCODE_TEST_HOME`, `HOME` |
| `data` | `xdgData/opencode` (`:11`) | `~/.local/share/opencode` | `XDG_DATA_HOME`, else `HOME` |
| `config` | `xdgConfig/opencode` (`:13`) — service variant adds `OPENCODE_CONFIG_DIR ??` (`:64`) | `~/.config/opencode` | `XDG_CONFIG_HOME`, `HOME` (**see §1.1 caveat**) |
| `cache` | `xdgCache/opencode` (`:12`) | `~/.cache/opencode` | `XDG_CACHE_HOME`, `HOME` |
| `state` | `xdgState/opencode` (`:14`) | `~/.local/state/opencode` | `XDG_STATE_HOME`, `HOME` |
| `log` | `data/log` (`:23`) | | follows `data` |
| `repos` | `data/repos` (`:24`) | | follows `data` |
| `bin` | `cache/bin` (`:22`) | `~/.cache/opencode/bin` | follows `cache` |
| `tmp` | `os.tmpdir()/opencode` (`:15`) | `/tmp/opencode` | `TMPDIR` |
| locks | `state/locks` (`packages/core/src/util/flock.ts:19-22`) | | follows `state` |

**`OPENCODE_TEST_HOME` moves only `home`.** The XDG-derived dirs come from `xdg-basedir`, which reads
`XDG_*` (falling back to `os.homedir()`) at **import time**. Upstream's own test harness confirms
this: `packages/opencode/test/preload.ts:1-2,34-37` sets all four `XDG_*` vars *before any src
import*, precisely because `OPENCODE_TEST_HOME` is insufficient.

`packages/core/src/global.ts:35-43` does a blocking top-level-await `mkdir` of all seven dirs as an
**import side effect** — so all of data/config/state/tmp/log/bin/repos must be writable at process
start. A fully read-only `XDG_CONFIG_HOME` therefore fails at import, before any of the config logic
in §1. **Mount the read-only manifest elsewhere and point `OPENCODE_CONFIG` at it; give
`XDG_CONFIG_HOME` a writable-but-empty per-session tmpfs.**

Non-DB state on disk:
- `$data/auth.json` — provider/wellknown credentials (`packages/opencode/src/auth/index.ts:10`)
- `$data/mcp-auth.json` — MCP OAuth tokens (`packages/opencode/src/mcp/auth.ts:37`)
- `$data/snapshot/<projectID>/<hash(worktree)>` — bare git repos
  (`packages/opencode/src/snapshot/index.ts:71`, `packages/core/src/snapshot.ts:98`)
- `$data/plans` (`packages/opencode/src/session/session.ts:334`), `$data/tool-output`
  (`packages/core/src/plugin/agent.ts:11`), `$data/repos` (repository cache)
- `$state/model.json` (`packages/opencode/src/cli/cmd/run/variant.shared.ts:19`);
  `$state/plugin-meta.json` (`packages/opencode/src/plugin/meta.ts:49`, overridable by
  `OPENCODE_PLUGIN_META_FILE`)
- `$cache/skills/` (URL skill cache), `$cache/bin/` (ripgrep etc.), `$cache/packages/<pkg>` (npm)
- Legacy JSON KV store `packages/opencode/src/storage/storage.ts:63-65`

### 7.2 The SQLite database

**Path** — `packages/core/src/database/database.ts:43-55`:

```ts
export function path() {
  if (Flag.OPENCODE_DB) {
    if (Flag.OPENCODE_DB === ":memory:" || isAbsolute(Flag.OPENCODE_DB)) return Flag.OPENCODE_DB
    return join(Global.Path.data, Flag.OPENCODE_DB)
  }
  if (["latest","beta","prod"].includes(InstallationChannel)
      || process.env.OPENCODE_DISABLE_CHANNEL_DB === "1"
      || process.env.OPENCODE_DISABLE_CHANNEL_DB === "true")
    return join(Global.Path.data, "opencode.db")
  return join(Global.Path.data, `opencode-${InstallationChannel.replace(/[^a-zA-Z0-9._-]/g,"-")}.db`)
}
```

Default: `$XDG_DATA_HOME/opencode/opencode.db`. Moved by **`OPENCODE_DB`** (absolute path,
`:memory:`, or bare filename relative to `$data`; declared `packages/core/src/flag/flag.ts:47`),
**`OPENCODE_DISABLE_CHANNEL_DB`**, and indirectly `XDG_DATA_HOME`.
**`path()` is called at module-import time** (`database.ts:57` `layerFromPath(path())`), so
`OPENCODE_DB` must be in the environment at process start.

**PRAGMAs** — `packages/core/src/database/database.ts:27-32`, on every open, before migrations:
`journal_mode = WAL`, `synchronous = NORMAL`, `busy_timeout = 5000`, `cache_size = -64000`,
`foreign_keys = ON`, `wal_checkpoint(PASSIVE)`.
Driver also sets WAL independently: `packages/core/src/database/sqlite.bun.ts:158-164` (opened
`readwrite: true, create: true`), `packages/core/src/database/sqlite.node.ts:151-159`.
`layerFromPath` passes only `{ filename }` (`database.ts:40`), so `config.timeout` is never set on
the node driver — only the `PRAGMA busy_timeout = 5000` applies.
Each client serializes queries behind a 1-permit semaphore (`sqlite.bun.ts:121-130`,
`sqlite.node.ts:115-124`) — **in-process only**.

**Migrations** — not drizzle-kit at runtime. 38 hand-written TS modules eagerly imported into an
ordered array: `packages/core/src/database/migration.gen.ts:1-44`, files in
`packages/core/src/database/migration/`. Applied **on every open** at `database.ts:33` →
`DatabaseMigration.apply(db)`. Version table is `migration (id TEXT PRIMARY KEY, time_completed
INTEGER NOT NULL)` — an **applied-ID ledger, not a monotonic schema version**
(`packages/core/src/database/migration.ts:18-41`, table created at `:30`/`:46`).

**Downgrade hazard — this is the one that matters for plan line 193/195.**
`applyOnly` (`packages/core/src/database/migration.ts:43-107`) iterates only the *binary's own*
`migrations` array and skips ids already in `completed`. Rows whose ids the old binary has never
heard of are never examined. There is **no max-known-migration check, no `user_version`, no schema
fingerprint, and no error**. An older binary opening a newer DB proceeds silently, queries a schema
it does not understand, and **can still write to it**. Recent migrations with destructive names
(`20260622170816_reset_v2_session_state`, `20260622142730_simplify_session_context_epoch`,
`20260622202450_simplify_session_input`) make this a data-loss shape, not just an error shape.
The only structural mitigation is the channel-suffixed filename (`database.ts:48-54`) — but
`latest`, `beta` and `prod` **all share `opencode.db`**, so a beta→stable downgrade hits the
unguarded path directly.
The one hard failure that does exist is `migration.ts:25` — `Effect.die("Database is not empty and
has no session table")` — which fires only on a foreign/corrupt file, never on version mismatch.
`Effect.orDie` at `database.ts:36` makes any migration failure an unrecoverable process defect.

**Concurrency — two processes CAN open the same DB and nothing prevents it.** No flock, lockfile or
single-instance guard around the database. Coordination is purely SQLite WAL + `busy_timeout=5000`.
The `Semaphore.make(1)` in the drivers and the `lock = Semaphore.makeUnsafe(1)` at `migration.ts:11`
are both in-process. Two processes starting simultaneously against an unmigrated DB both run
`applyOnly` concurrently and rely on SQLite transactions not to collide.
`Flock` (`packages/core/src/util/flock.ts`) is a separate advisory `mkdir`-based lock rooted at
`$state/locks` (`:19-22`, defaults `staleMs: 60_000`, `timeoutMs: 300_000` at `:25-30`) and is
**never used for the database** — only for models.dev cache
(`packages/core/src/models-dev.ts:226,241`), npm installs (`packages/core/src/npm.ts:78`),
repository cache (`packages/core/src/repository-cache.ts:137`), MCP auth
(`packages/opencode/src/mcp/auth.ts:63`) and plugin meta
(`packages/opencode/src/plugin/meta.ts:147,171,185`).

**This directly confirms plan line 193's requirement is NOT met natively.** "Prevent two processes or
versions opening the same state for mutation" must be enforced by tracon (per-session DB path via
`OPENCODE_DB` + per-session `XDG_DATA_HOME` + node-side single-writer fencing), not by OpenCode.

**Tables** — `packages/core/src/database/schema.gen.ts`:
`workspace:8`, `data_migration:21`, `account_state:27`/`account:35`/`control_account:47`,
`credential:60`, `event_sequence:73`/`event:80`, **`permission`:90**, `project_directory:101`/
`project:112`, **`message`:128** (`id, session_id, time_created, time_updated, data TEXT` — payload
is a JSON blob in `data`), **`part`:138** (`id, message_id, session_id, …, data TEXT`),
`session_context_epoch:149`, `session_input:158`, `session_message:170`, **`session`:182** (incl.
`cost`, `tokens_*`, `share_url`, `permission`, `agent`, `model`, `time_archived`), `todo:216`,
`session_share:229`.

**Permission "always" grants** live in the **`permission`** table
(`packages/core/src/database/schema.gen.ts:90`; columns `id, project_id, action, resource,
time_created, time_updated`; FK to `project` ON DELETE CASCADE). Written by
`packages/core/src/permission/saved.ts:54-62` (`PermissionSaved.add`), read at `:42-52`. Scoped by
`project_id`, keyed `(action, resource)`. Table definition `packages/core/src/permission/sql.ts`.
Action vocabulary is `ask` / `allow` / `deny` (`packages/core/src/v1/config/permission.ts:5`), with
known keys `read, edit, glob, grep, list, bash, task, external_directory, todowrite, question,
webfetch, websearch, lsp, doom_loop, skill` plus an open record for MCP/custom tools
(`packages/core/src/v1/config/permission.ts:17-36`). Rule precedence uses `findLast` over the merged
ruleset with a **default of `"ask"`** (`packages/opencode/src/permission/index.ts:28-38`).

**Backup/export — there is NO backup or DB-snapshot command.** In
`packages/opencode/src/cli/cmd/`:
- `db.ts:8-43` — `opencode db [query]` runs arbitrary SQL; with no query it `spawn("sqlite3",
  [Database.path()])` (`:38`). `opencode db path` at `:45-52`.
- `export.ts:11-58` — exports a single **session transcript**, redacting file contents/paths. Not a
  DB backup.
- `import.ts:117,132-149` — imports a session from a share URL or file.
- `stats.ts` — local read-only aggregation.
No `backup.ts`, no `snapshot.ts`. The Bun driver exposes `native.serialize()`
(`packages/core/src/database/sqlite.bun.ts:104-110`) but nothing in the CLI calls it.

**Consequence for plan line 195** ("Upgrade state on a clone after quiescing writers, using a
SQLite/WAL-consistent backup procedure verified against the selected release"): tracon must implement
this itself — quiesce, `VACUUM INTO` or `sqlite3 .backup` against the per-session file, verify, then
migrate the clone. There is nothing upstream to call.

---

## 8. Outbound network calls at startup and runtime (non-inference)

| # | Call | Host / URL | Code | When | Disable |
|---|---|---|---|---|---|
| 1 | **models.dev catalogue** | `https://models.opencode.ai/api.json` | `packages/core/src/models-dev.ts:160,176` | **startup + every 60 min**, forked fiber (`:254-257`) | `OPENCODE_DISABLE_MODELS_FETCH`; redirect via `OPENCODE_MODELS_URL`; bypass entirely via `OPENCODE_MODELS_PATH` (local file, `:184`) |
| 2 | Update check — curl install | `https://opencode.ai/install` | `packages/opencode/src/installation/index.ts:147` | on `upgrade` only | `OPENCODE_DISABLE_AUTOUPDATE` / config `autoupdate:false` |
| 3 | Update check — brew | `https://formulae.brew.sh/api/formula/opencode.json` | `installation/index.ts:219` | TUI start (`cli/tui/worker.ts:61`) | same |
| 4 | Update check — npm | `<registry>/opencode-ai/<channel>` (default `https://registry.npmjs.org`) | `installation/index.ts:231-234`; `packages/core/src/npm-config.ts:37` | TUI start | same |
| 5 | Update check — choco | `https://community.chocolatey.org/api/v2/Packages?...` | `installation/index.ts:240` | TUI start | same |
| 6 | Update check — scoop | `https://raw.githubusercontent.com/ScoopInstaller/Main/master/bucket/opencode.json` | `installation/index.ts:250` | TUI start | same |
| 7 | Update check — fallback | `https://api.github.com/repos/anomalyco/opencode/releases/latest` | `installation/index.ts:258` | TUI start, unknown install method | same |
| 8 | OTEL logs | `${OTEL_EXPORTER_OTLP_ENDPOINT}/v1/logs` | `packages/core/src/observability/otlp.ts:50-53` | startup, only if endpoint set | opt-**in**; leave `OTEL_EXPORTER_OTLP_ENDPOINT` unset |
| 9 | OTEL traces | `${OTEL_EXPORTER_OTLP_ENDPOINT}/v1/traces` | `observability/otlp.ts:55-77` | same | same |
| 10 | **Web UI asset proxy** | `https://app.opencode.ai` | `packages/opencode/src/server/shared/ui.ts:9,88-93`; wired `packages/opencode/src/server/routes/instance/httpapi/server.ts:200` | on GET to UI routes, **only if no embedded bundle** | **no flag turns the fallback off**; `OPENCODE_DISABLE_EMBEDDED_WEB_UI` makes it *more* likely (`ui.ts:44-45`) |
| 11 | **ripgrep binary download** | `https://github.com/BurntSushi/ripgrep/releases/download/<ver>/<file>` | `packages/core/src/ripgrep/binary.ts:105,109-116` | lazy, first grep, if no system `rg` and no `$cache/bin/rg` | **unconditional — no flag.** Avoid by shipping `rg` on `PATH` or pre-seeding `$cache/bin/rg` (`binary.ts:93-98`) |
| 12 | **LSP server downloads** | github.com, api.github.com, eclipse.org, download-cdn.jetbrains.com, api.releases.hashicorp.com | `packages/opencode/src/lsp/server.ts:183,552,600,976,1207,1297,1330,1405,1632,1705,1877` | lazy, on LSP spawn for a language | **`OPENCODE_DISABLE_LSP_DOWNLOAD`** (`packages/opencode/src/effect/runtime-flags.ts:22`; checked at `lsp/server.ts:152,182,370,404,493,550,598,692,708`) |
| 13 | **Plugin / provider npm install** | npm registry via `@npmcli/arborist` (`ignoreScripts: true`) | `packages/core/src/npm.ts:78-113,115-136`; callers `packages/opencode/src/plugin/shared.ts:211`, `packages/opencode/src/provider/provider.ts:1846`, `packages/core/src/config/plugin/external.ts:77` | lazy, when a configured plugin/provider package is not in `$cache/packages/<pkg>`; **plus the unconditional per-config-dir install at `packages/opencode/src/config/config.ts:452-471`** | `OPENCODE_PURE` for config plugins (§4); `OPENCODE_DISABLE_DEFAULT_PLUGINS` for built-ins; the per-config-dir install is **not gated** (fails as a logged warning) |
| 14 | **Skill fetch from URL** | user-supplied `<url>/index.json` + per-file | `packages/opencode/src/skill/discovery.ts:40,56`; triggered `packages/opencode/src/skill/index.ts:222-223` | at skill init, if `skills.urls` set | **only by not setting `skills.urls`** — `OPENCODE_DISABLE_EXTERNAL_SKILLS` does NOT cover it (§3.6) |
| 15 | Share — create | `POST <base>/api/share` | `packages/opencode/src/share/share-next.ts:314` | explicit share, or auto | `OPENCODE_DISABLE_SHARE=true|1` (`share-next.ts:23`); config `share: "disabled"` (`packages/opencode/src/share/session.ts:28`) |
| 16 | Share — incremental sync | `POST <base>/api/share/<id>/sync` | `share-next.ts:259` | every message/part while shared, 1s debounce (`:141-145`) | same |
| 17 | Share — delete | `DELETE <base>/api/share/<id>` | `share-next.ts:350` | unshare / session delete | same |
| — | Share base | `config.enterprise.url ?? "https://opncd.ai"`, or the account console origin | `share-next.ts:210,221` | — | `enterprise.url` config key |
| 18–20 | Account device login / token refresh / user+orgs | `<server>/auth/device/code`, `/auth/device/token`, `/api/user`, `/api/orgs` | `packages/opencode/src/account/account.ts:390,419,220,300,287` | `opencode account login`, refresh | on demand; default server `https://opencode.ai/console` (`packages/opencode/src/cli/cmd/account.ts:18`) |
| 21 | **Org remote config** | `GET <account.url>/api/config` (bearer + `x-org-id`) | `account.ts:370-373` | **every config load**, when an account with an active org exists | logged out (empty per-session `$data`) = no call |
| 22 | **`.well-known` remote config** | `<url>/.well-known/opencode`, then a **second, server-specified URL** | `packages/opencode/src/config/config.ts:373-390` | **every config load**, per `auth` entry of `type: "wellknown"` | only fires if such an auth entry exists in `$data/auth.json` |
| 23–28 | Provider OAuth (GitHub Copilot, OpenAI/ChatGPT, xAI, DO-cloud, Snowflake, Google Vertex ADC + GCP metadata) | github.com, auth.openai.com, auth.x.ai, the DO-cloud API host, `<acct>.snowflakecomputing.com`, googleapis.com | `packages/opencode/src/plugin/github-copilot/copilot.ts:21-22,234,264`; `packages/core/src/plugin/provider/openai.ts:15`; `packages/opencode/src/plugin/xai.ts:7,15`; `packages/opencode/src/plugin/do-cloud (vendor plugin) .ts:9-10`; `packages/opencode/src/plugin/snowflake-cortex.ts:102,106,137`; `packages/core/src/plugin/provider/google-vertex.ts:46` | on demand | on demand only |
| 29 | Exa web-search MCP | `https://mcp.exa.ai/mcp` | `packages/core/src/tool/websearch.ts:20`; `packages/opencode/src/tool/mcp-websearch.ts:5-6` | model tool call | **off by default**; enabled by `OPENCODE_ENABLE_EXA` / `OPENCODE_EXPERIMENTAL_EXA` / `OPENCODE_EXPERIMENTAL` (`runtime-flags.ts:31-35`) |
| 30 | Parallel web-search MCP | `https://search.parallel.ai/mcp` | `websearch.ts:21`; `mcp-websearch.ts:7` | model tool call | off by default; `OPENCODE_ENABLE_PARALLEL` / `OPENCODE_EXPERIMENTAL_PARALLEL` |
| 31 | `webfetch` tool | arbitrary http(s) chosen by the model | `packages/core/src/tool/webfetch.ts:86` | model tool call | `tools`/`permission` config (`webfetch: "deny"`) |
| 32 | Remote MCP servers | user-configured | `packages/opencode/src/mcp/index.ts`; auth `packages/opencode/src/mcp/auth.ts`; client metadata `packages/opencode/src/mcp/oauth-provider.ts:47` | on MCP connect | `mcp` config key |
| 33 | Repository clone/fetch | `https://github.com/<path>.git` or `https://<host>/<path>.git` into `$data/repos` | `packages/core/src/repository.ts:176,192`; `packages/core/src/repository-cache.ts:178,190,195` | repo-backed sessions/workspaces | `OPENCODE_REPO_CLONE_GITHUB_BASE_URL` re-points the base (`repository.ts:174-177`) |
| 34–36 | `opencode github` / `opencode import` CLI calls | api.opencode.ai, api.github.com, share URL | `packages/opencode/src/cli/cmd/github.handler.ts:325,698,1596`; `packages/opencode/src/cli/cmd/import.ts:132-134` | CLI only, not `serve` | CLI-only |
| 37 | Desktop changelog | `https://opencode.ai/changelog.json` | `packages/app/src/context/highlights.tsx:10` | desktop/web app, **not the server** | — |

### 8.1 Notable conclusions

- **There is no telemetry, analytics, phone-home or crash reporter.** The only observability egress is
  OTLP, strictly opt-in via `OTEL_EXPORTER_OTLP_ENDPOINT`; both `loggers()` (`otlp.ts:51`) and
  `tracingLayer()` (`otlp.ts:56`) early-return empty when it is unset. `opencode stats` is local SQL.
  Clean negative — record it.
- **The only automatic startup fetch is models.dev**, and it repeats every 60 minutes
  (`packages/core/src/models-dev.ts:254-257`). It *is* disableable, but note
  `OPENCODE_DISABLE_MODELS_FETCH` is read once at module load via `truthy()`
  (`packages/core/src/flag/flag.ts:29`), not lazily — it must be in the launch environment.
  `OPENCODE_MODELS_PATH` pointing at a node-provisioned catalogue file is the stronger control: it
  gives a pinned model catalogue instead of merely an absent one.
- **Two egress paths have no off switch and are STOP-candidates or packaging requirements:**
  1. **ripgrep download** (`packages/core/src/ripgrep/binary.ts:105`) — no flag at all, unlike LSP.
     Mitigation is packaging: ship `rg` on `PATH` or pre-seed `$cache/bin/rg` (`binary.ts:93-98`).
     Acceptable as an image control, but it must be *verified in the image*, not assumed.
  2. **Web UI fallback to `https://app.opencode.ai`** (`packages/opencode/src/server/shared/ui.ts:88-93`)
     — proxies every UI-route request upstream **forwarding client headers** when no embedded bundle
     exists, and `OPENCODE_DISABLE_EMBEDDED_WEB_UI` *forces* that state (`ui.ts:44-45`). The emitted
     CSP includes `connect-src *` (`ui.ts:12`). This is exactly the case the plan calls out at line
     147. Mitigation must be (a) verify the selected artifact embeds the full UI bundle, and (b)
     block/intercept the egress at the gateway — not a flag.
- **`OPENCODE_PURE` is not an offline switch.** It is consulted only by the plugin loader
  (`packages/opencode/src/plugin/index.ts:181`, `packages/opencode/src/plugin/tui/runtime.ts:1089`).
  Do not present it as a hermetic-mode flag.

---

## 6. LSP and formatters

Registry: `packages/opencode/src/lsp/server.ts` (1983 lines). Every `export const X: Info` is
auto-registered by `Object.values(LSPServer)` at `packages/opencode/src/lsp/lsp.ts:154-156`.
There is no `packages/core/src/lsp/`, and **no `BunProc` / `bun x` anywhere in v1.18.30** — npm
installs go through `@npmcli/arborist`.

### 6.1 LSP is OFF by default — good news for the plan's "make enablement explicit" (line 143)

`cfg.lsp` is `Schema.optional` (`packages/core/src/config.ts:75`) and
`packages/opencode/src/lsp/lsp.ts:151` short-circuits with *"all LSPs are disabled"* when it is
absent. Confirmed by `packages/web/src/content/docs/lsp.mdx:51`. Formatters likewise:
`packages/opencode/src/format/index.ts:120` `if (!cfg.formatter)`, key at
`packages/core/src/config.ts:72-74`.

**So the default posture is already "nothing starts, nothing downloads".** Enablement is explicit
per-server in the node-written manifest, which is exactly what the plan wants.

### 6.2 Bundled server list (39 servers)

Ids and spawned commands, `packages/opencode/src/lsp/server.ts`: `deno`:88, `typescript`:115,
`vue`:144, `eslint`:173, `oxlint`:224, `biome`:296, `gopls`:358, `ruby-lsp`:392, `ty`:424 (gated on
`OPENCODE_EXPERIMENTAL_LSP_TY`, `:437`), `pyright`:485, `elixir-ls`:529, `zls`:585, `csharp`:687,
`razor`:703, `fsharp`:823, `sourcekit-lsp`:856, `rust`:890, `clangd`:935, `svelte`:1069,
`astro`:1096, `jdtls`:1147 (needs Java ≥ 21, `:1197`), `kotlin-ls`:1273, `yaml-ls`:1361,
`lua-ls`:1387, `php intelephense`:1515, `prisma`:1546, `dart`:1563, `ocaml-lsp`:1580, `bash`:1596,
`terraform`:1622, `texlab`:1695, `dockerfile`:1774, `gleam`:1800, `clojure-lsp`:1817, `nixd`:1837,
`tinymist`:1867, `haskell-language-server`:1951, `julials`:1968.
Canonical id list duplicated at `packages/core/src/v1/config/lsp.ts:22-61`.
`ty` and `pyright` are mutually exclusive (`packages/opencode/src/lsp/lsp.ts:98-108`).

`which()` always appends the opencode bin dir to PATH — `packages/core/src/util/which.ts:6-7`:
`const full = base ? base + path.delimiter + Global.Path.bin : Global.Path.bin`.

### 6.3 Auto-download — what, from where, into where

Binary install root: `Global.Path.bin` = `$XDG_CACHE_HOME/opencode/bin`
(`packages/core/src/global.ts:12,22`, mkdir'd eagerly at `:41`).
npm install root: `packages/core/src/npm.ts:79`
`const directory = (pkg) => path.join(global.cache, "packages", sanitize(pkg))` →
`$XDG_CACHE_HOME/opencode/packages/<pkg>/node_modules/.bin/<bin>` (`npm.ts:193-194,222-235`).
Install is an **Arborist reify with `ignoreScripts: true`, `binLinks: true`** (`npm.ts:86-108`),
serialized behind a flock `npm-install:<dir>` (`npm.ts:82`). Default registry
`https://registry.npmjs.org` (`packages/core/src/npm-config.ts:37`).

**npm-installed servers** (`Npm.which`): typescript-language-server (`server.ts:125`),
`@vue/language-server`:153, biome:340, pyright:494, svelte-language-server:1078,
`@astrojs/language-server`:1111, yaml-language-server:1370, intelephense:1524,
bash-language-server:1605, dockerfile-language-server-nodejs:1783.

**Raw `fetch` tarball/binary downloads (no timeout, no checksum):** eslint from
`https://github.com/microsoft/vscode-eslint/archive/refs/heads/main.zip` (`server.ts:183`) — and it
then runs **`npm install` AND `npm run compile`** in the extracted tree (`:206-208`); elixir-ls
(`:552`, then `mix deps.get / compile / elixir_ls.release2` at `:571-573`); zls (`:600,647`); clangd
(`:976,1018`); jdtls from eclipse.org (`:1206-1207`); kotlin-ls from api.github.com then
`download-cdn.jetbrains.com` (`:1297,1330`); lua-ls (`:1405`); terraform-ls from
`api.releases.hashicorp.com` (`:1632`); texlab (`:1705`); tinymist (`:1877`).
Extraction shells out to `unzip`/`Expand-Archive` (`packages/opencode/src/util/archive.ts:4-15`) and
`tar` (`server.ts:663,1045,1216,1479,1750,1929`).

**Package-manager installs:** `go install golang.org/x/tools/gopls@latest` with `GOBIN=Global.Path.bin`
(`server.ts:372-373`); `gem install rubocop --bindir <bin>` (`:405`);
`dotnet tool install fsautocomplete --tool-path <bin>` (`:835`);
`dotnet tool install --global roslyn-language-server --prerelease` (`:756`).

**Never auto-installed:** deno, oxlint, rust-analyzer, sourcekit-lsp, prisma, dart, ocaml-lsp, gleam,
clojure-lsp, nixd, haskell-language-server, julials, ty.

### 6.4 Disabling downloads and pointing at pre-installed binaries

**`OPENCODE_DISABLE_LSP_DOWNLOAD` is the only network kill-switch** —
`packages/opencode/src/effect/runtime-flags.ts:22`, checked at 24 sites in `server.ts`
(152, 182, 370, 404, 493, 550, 598, 692, 708, 755, 834, 974, 1077, 1110, 1204, 1295, 1369, 1403,
1523, 1604, 1630, 1703, 1782, 1875). There is **no `OPENCODE_DISABLE_LSP`**; total disablement is
config-only (omit `lsp`, or `"lsp": false`).

`lsp` schema — `packages/core/src/config/lsp.ts:5-18`:

```ts
export const Disabled = Schema.Struct({ disabled: Schema.Literal(true) })          // 5-7
export class Server extends Schema.Class<Server>("ConfigV2.LSP.Server")({
  command: Schema.String.pipe(Schema.Array),                                        // 10
  extensions: Schema.String.pipe(Schema.Array, Schema.optional),                    // 11
  disabled: Schema.Boolean.pipe(Schema.optional),                                   // 12
  env: Schema.Record(Schema.String, Schema.String).pipe(Schema.optional),           // 13
  initialization: Schema.Record(Schema.String, Schema.Unknown).pipe(Schema.optional),// 14
}) {}
export const Entry = Schema.Union([Disabled, Server])                               // 17
export const Info  = Schema.Union([Schema.Boolean, Schema.Record(Schema.String, Entry)]) // 18
```

V1 variant with the builtin-id list and the "custom servers must declare `extensions`" filter:
`packages/core/src/v1/config/lsp.ts:9-18,63-78`.

Override logic — `packages/opencode/src/lsp/lsp.ts:160-181`: re-declaring a builtin id with
`command` **replaces its `spawn`**, so e.g. `"lsp": {"gopls": {"command": ["/usr/bin/gopls"]}}`
points at a pre-installed binary and bypasses download entirely. Two gotchas:
- it **discards the builtin's `initialization`** unless you supply it — e.g. overriding `typescript`
  silently drops the `tsserver.path` init that `server.ts:135-139` provides (`lsp.ts:173-179`);
- the builtin `root` detector *is* preserved (`lsp.ts:167`).

**Recommended tracon posture:** `OPENCODE_DISABLE_LSP_DOWNLOAD=true` **and** an explicit `lsp` map
naming absolute image-baked binary paths per server, with `initialization` copied from the builtin.
Belt and braces, and it satisfies plan line 143 without relying on a default.

### 6.5 Missing server + denied network — it can hang, and failure is silent

**No hang guard on install/download.** No `AbortSignal.timeout` / `signal:` anywhere in `server.ts`;
`fetch()` is bare. `npm install` (`server.ts:207`), `mix compile` (`:572`), `go install` (`:372`),
`dotnet tool install` (`:756`), `gem install` (`:405`) have **no timeout** —
`Options.timeout` in `packages/opencode/src/util/process.ts:81-83` only escalates SIGTERM→SIGKILL
*after an AbortSignal fires*, and no signal is passed.

The spawn loop is **serial and awaited inline** (`packages/opencode/src/lsp/lsp.ts:254-289`,
`const client = await task` inside the per-server `for`), and the `edit`/`write` tools await
`lsp.touchFile(...)` synchronously (`packages/opencode/src/tool/edit.ts:197`,
`packages/opencode/src/tool/write.ts:75`; only `read` forks it,
`packages/opencode/src/tool/read.ts:119`).

**Therefore: on a blackholed (DROP) network, the first edit of a `.ts` file can block indefinitely.**
On a refused (RST/ENOTFOUND) network `fetch` rejects fast and the server is silently skipped.
This is a direct hit on plan line 143 ("Missing servers fail visibly, not by hanging on network
downloads"). **The runner's egress denial must REJECT, not DROP** — or LSP downloads must be disabled
outright via `OPENCODE_DISABLE_LSP_DOWNLOAD`. Record this as a required runner property.

Failure is **silent and sticky** — `lsp.ts:220-228` adds the key to `s.broken` on any spawn failure
and `lsp.ts:259` skips broken keys forever (cleared only on instance dispose). There is **no
`logError` on a failed spawn**, and every `Archive.extractZip(...).catch((error) => false)` discards
`error` (`server.ts:191-193,658-660,1039-1041,1341-1343,1474-1476,1743-1745,1924-1926`).
**Plan line 143 requires "Show disabled/unavailable/starting/running/failed status" — upstream does
not surface enough to build that faithfully.** Tracon either contributes an upstream status/event
surface (a bounded contribution) or presents only disabled/running and states the limitation.

Timeout constants — `packages/opencode/src/lsp/client.ts:13-18`:
`DIAGNOSTICS_DEBOUNCE_MS = 150`, `DIAGNOSTICS_DOCUMENT_WAIT_TIMEOUT_MS = 5_000`,
`DIAGNOSTICS_FULL_WAIT_TIMEOUT_MS = 10_000`, `DIAGNOSTICS_REQUEST_TIMEOUT_MS = 3_000`,
**`INITIALIZE_TIMEOUT_MS = 45_000`**. Applied at `client.ts:211-255` (initialize → `InitializeError`
→ `lsp.ts:236-240` `broken.add` + `Process.stop`), `:294-302`, `:329-339`, `:508`, `:530`.
`withTimeout` is a plain `Promise.race` (`packages/opencode/src/util/timeout.ts:1-13`) and **does not
kill the child** — a server that never answers `initialize` stalls 45s then gets `Process.stop`'d.
The npm path has its own 5-minute lock wait (`packages/core/src/util/effect-flock.ts:43-44`,
`STALE_MS = 60_000`, `TIMEOUT_MS = 5 * 60_000`).

### 6.6 Formatters

26 built-ins in `packages/opencode/src/format/formatter.ts`: `gofmt`:18, `mix`:28, `prettier`:38
(gated on a `prettier` dep in package.json, `:78`), `oxfmt`:87 (gated on
`OPENCODE_EXPERIMENTAL_OXFMT`, `:94`), `biome`:110 (gated on `biome.json(c)`, `:144`), `zig`:156,
`clang-format`:166, `ktlint`:179, `ruff`:189, `air`:218, `uv`:236 (suppressed if ruff enabled,
`:240`), `rubocop`:249, `standardrb`:259, `htmlbeautifier`:269, `dart`:279, `ocamlformat`:289,
`terraform`:300, `latexindent`:310, `gleam`:320, `shfmt`:330, `nixfmt`:340, `rustfmt`:350,
`pint`:360, `ormolu`:376, `cljfmt`:386, `dfmt`:396.

**Auto-installed formatters: only `prettier`, `oxfmt`, `@biomejs/biome`** — same arborist path
(`formatter.ts:79,102,148`) into `$XDG_CACHE_HOME/opencode/packages/<pkg>`. All others are `which()`
lookups. **These three are NOT covered by `OPENCODE_DISABLE_LSP_DOWNLOAD`** — the only control is not
enabling `formatter`, or overriding `command` with an absolute path.

**Selection is not first-match — ALL matching enabled formatters run, in registration order**
(`packages/opencode/src/format/index.ts:57-71` returns an array; `:80-112` iterates). `$FILE`
substitution at `:82`.

Schema — `packages/core/src/config/formatter.ts:5-12`: `{ disabled?, command?: string[],
environment?: Record<string,string>, extensions?: string[] }`, `Info = Boolean | Record<string,Entry>`.
Merge at `index.ts:134-157`; disabling either `ruff` or `uv` disables both (`:139-144`).

**When they run:** synchronously **after the file write, before diagnostics**, in exactly three
tools — `packages/opencode/src/tool/write.ts:65`, `packages/opencode/src/tool/edit.ts:112` and `:156`,
`packages/opencode/src/tool/apply_patch.ts:253`. Not watcher-driven. Formatter processes are spawned
`stdin/stdout/stderr: "ignore"` with **no timeout/abort** (`index.ts:86-93`) — **a hanging formatter
hangs the edit tool.**
Caching bug worth knowing: `index.ts:45` `if (cmd === false || cmd === undefined)` means a formatter
that resolved to "unavailable" is **re-probed on every format call** (including `which`, `findUp`, and
potentially an npm install), not cached.

### 6.7 Process lifecycle — orphan risk is REAL

Two spawn paths with different semantics:

- **LSP servers**: `packages/opencode/src/lsp/launch.ts:11-16` → `Process.spawn` → `cross-spawn`
  **without `detached`** (`packages/opencode/src/util/process.ts:63-69`). Children share opencode's
  process group and are not group-killable. Teardown `client.ts:640-644` calls
  `Process.stop(input.server.process)` → `packages/opencode/src/util/process.ts:149-163`: a bare
  `proc.kill()` (SIGTERM, single pid) on POSIX; `taskkill /T /F` only on Windows. **No SIGKILL
  escalation, no wait for exit, no group kill.**
- **Formatters**: `AppProcess.run` → `packages/core/src/cross-spawn-spawner.ts:424`
  `detached: command.options.detached ?? process.platform !== "win32"`, release finalizer `:428-447`,
  `killGroup` `:292-312` (`process.kill(-proc.pid!, signal)`) with `killOne` fallback `:314-322`.
  These *are* handled correctly.

Shutdown chain: LSP finalizer `packages/opencode/src/lsp/lsp.ts:198-202` fires only when the
per-instance `ScopedCache` entry is invalidated
(`packages/opencode/src/effect/instance-state.ts:38` → `instance-registry.ts:10-12` →
`packages/opencode/src/project/instance-store.ts:166-192`).

**Concrete orphan risks:**
1. **`opencode serve` installs no `SIGINT`/`SIGTERM`/`SIGHUP` handler.**
   `packages/opencode/src/cli/cmd/serve.ts:22` is just `yield* Effect.never`. A grep for signal
   handlers over `packages/opencode/src` + `packages/core/src` finds only
   `runtime.lifecycle.ts:265,290` (SIGINT for the `run` prompt draft), `mcp/index.ts:546`,
   `core/shell.ts:48,54` and `util/process.ts:79`. So `kill <pid>` / a Kubernetes `SIGTERM` /
   `podman stop` terminates opencode with **zero finalizers run** → every LSP child is reparented to
   init.
2. `packages/opencode/src/index.ts:137-141` forces this even on clean exit:
   ```ts
   } finally {
     // Some subprocesses don't react properly to SIGTERM and similar signals.
     // … Explicitly exit to avoid any hanging subprocesses.
     process.exit()
   }
   ```
   `process.exit()` in a `finally` aborts pending async finalizers.
3. `Process.stop` never escalates to SIGKILL and never awaits `proc.exited`.
4. The `spawning` map (`lsp.ts:116,267-282`) has no cancellation — shutdown during an in-flight
   `go install` / `npm run compile` orphans that build too.

**This satisfies plan line 143's "terminate child processes with the runner" ONLY because the
container/pod boundary does it.** The runner must use a PID-namespace container with a real init (or
`--init` / `shareProcessNamespace` + explicit cleanup) and rely on the container teardown, **not** on
OpenCode's shutdown. Record it as a runner requirement, not an OpenCode property.

### 6.8 LSP state on disk

`$XDG_CACHE_HOME/opencode/bin/{vscode-eslint, elixir-ls-master, jdtls, kotlin-ls, clangd_<tag>+clangd
symlink, lua-language-server-<arch>-<platform>, zls, texlab, tinymist, terraform-ls, gopls, rubocop,
fsautocomplete}` (`server.ts:180,536-542,1200,1290,1049-1059,1461,668,1755,1934,1670,382,414,845`);
`$XDG_CACHE_HOME/opencode/packages/<pkg>/node_modules/.bin/` (`packages/core/src/npm.ts:79,193-194`);
`$DOTNET_CLI_HOME|~/.dotnet/tools/roslyn-language-server` (`server.ts:777-784`);
`fs.mkdtemp(os.tmpdir()/opencode-jdtls-data)` — **a fresh temp dir per jdtls spawn, never deleted**
(`server.ts:1246`); npm install locks at `$XDG_STATE_HOME/opencode/locks/<hash>.lock/`
(`packages/core/src/util/effect-flock.ts:102,172-173,257`).
Download artifacts (`.zip`/`.tar.gz`) are `fs.rm`'d after extraction but **leak if extraction throws**
(`server.ts:186,554,1659,1208,1333,652,1455,1738,1918`).
Implicitly outside opencode's control: clangd `--background-index` writes `.cache/clangd/` **into the
project worktree** (`server.ts:940`); gopls/rust-analyzer use their own caches.
No diagnostic/symbol cache is persisted — in-memory `Map`s only (`client.ts:139-141,268`).

---

## 4. Plugins and custom tools

### 4.1 Declaration forms

v1 schema (the one used at runtime) — `packages/core/src/v1/config/plugin.ts:5-9`:

```ts
export const Options = Schema.Record(Schema.String, Schema.Unknown)
export const Spec = Schema.Union([Schema.String, Schema.mutable(Schema.Tuple([Schema.String, Options]))])
```

Wired at `packages/core/src/v1/config/config.ts:56`. Each entry is `"spec"` or `["spec", {options}]`.
(v2 uses a different key `plugins` with `{package, options?}` — `packages/core/src/config/plugin.ts:5-13`,
`packages/core/src/config.ts:102`.)

Form dispatch, `packages/opencode/src/plugin/shared.ts`:
- `isPathPluginSpec` `:171-173` — `file://`, `.`-prefixed, or absolute (incl. Windows drive, `:67-69`)
- `pluginSource` `:56-59` — path-like ⇒ `"file"`, **everything else ⇒ `"npm"`**
- `parsePluginSpecifier` `:22-34` via `npm-package-arg`
- `resolvePluginTarget` `:207-213` — bare name rewritten to `name@latest`, then `Npm.add(pkg)`

| Form | Handling |
|---|---|
| bare npm name | → `name@latest`, npm install (`shared.ts:210`) |
| `name@version` / range / npm alias | passed to `Npm.add` |
| `file://…` | `resolvePathPluginTarget` `:175-192` — dir with `package.json`, else first of `index.{ts,tsx,js,mjs,cjs}` (`INDEX_FILES` `:54`), else throws |
| absolute / Windows path | same path branch |
| relative (`./x.ts`) | path branch, resolved **relative to the declaring config file** (`packages/opencode/src/config/plugin.ts:42-60`) |
| **http(s) URL** | **not special-cased** — falls to the npm branch; `npa` classifies it `remote` and it is handed to Arborist as a remote tarball. **There is no HTTP fetch-and-eval path for plugins.** |

Dedup across merged configs: last-wins by npm package name or exact `file://` URL
(`packages/opencode/src/config/plugin.ts:64-77`).

### 4.2 Auto-discovery

`packages/opencode/src/config/plugin.ts:18-30` — glob `{plugin,plugins}/*.{ts,js}`, `dot: true`,
**`symlink: true`**, non-recursive. Called per config directory at
`packages/opencode/src/config/config.ts:478-479` over the `ConfigPaths.directories()` list (§1.3).

`OPENCODE_DISABLE_PROJECT_CONFIG=true` suppresses **only** project-tree `.opencode` dirs
(`packages/opencode/src/config/paths.ts:27-33`) and project `opencode.json[c]`
(`config.ts:420-424`). `$XDG_CONFIG_HOME/opencode`, `$HOME/.opencode` and `$OPENCODE_CONFIG_DIR` are
still scanned and their plugins still load.

### 4.3 Installation — arborist in-process, not `bun install`

**No `bun install`, no `npm` binary, no subprocess.** `@npmcli/arborist` is imported dynamically and
run in-process — `packages/core/src/npm.ts:79-113`:

```ts
const directory = (pkg: string) => path.join(global.cache, "packages", sanitize(pkg))   // 79
yield* flock.acquire(`npm-install:${input.dir}`)                                        // 82
const { Arborist } = yield* Effect.promise(() => import("@npmcli/arborist"))            // 83
const npmOptions = yield* NpmConfig.load(input.dir)
const arborist = new Arborist({ ...npmOptions, path: input.dir, binLinks: true,
                                progress: false, savePrefix: "", ignoreScripts: true })  // 86-92
arborist.reify({ ...npmOptions, add, save: true, saveType: "prod" })
```

**`ignoreScripts: true`** — install scripts of plugin dependencies do not run. Good.

Target directory: `$XDG_CACHE_HOME/opencode/packages/<spec>/node_modules/<name>`, where `<spec>`
includes the version (`foo@latest`, `foo@1.2.3`). `global.cache` from
`packages/core/src/global.ts:12`. `sanitize` (`npm.ts:43-48`) only rewrites characters on win32.

**Second install site, unconditional at config load** —
`packages/opencode/src/config/config.ts:450-471`: for **every** `ConfigPaths.directories()` entry it
`ensureGitignore(dir)` (which **creates the directory**, `config.ts:310`, and writes a `.gitignore`,
`:313-325`) then forks a detached `Npm.install(dir, { add: [{ name: "@opencode-ai/plugin", version:
InstallationVersion }] })`. Not gated by `OPENCODE_PURE`.

**Registry/auth env: the full npm config stack is honoured.**
`packages/core/src/npm-config.ts:12-32` builds `@npmcli/config` with `cwd: dir, env: {...process.env}`
and spreads `config.flat` into both the constructor and `reify()`. So builtin npmrc, `/etc/npmrc`,
`~/.npmrc`, `<dir>/.npmrc`, and every `npm_config_*` / `NPM_CONFIG_*` var apply (registry,
`//host/:_authToken`, `cache`, `proxy`, `strict-ssl`, `offline`, `prefer-offline`, `fetch-retries`).
Default registry `https://registry.npmjs.org` (`npm-config.ts:34-40`) — though that function is used
only by the updater (`packages/opencode/src/installation/index.ts:230`), not by plugin installs.

`OPENCODE_PLUGIN_META_FILE` is **not** an install/registry knob — it only relocates the load-bookkeeping
JSON (`packages/opencode/src/plugin/meta.ts:48-50`; default `$XDG_STATE_HOME/opencode/plugin-meta.json`).

**Offline behaviour: degrade, not hard-fail — but it can stall.**
- Per-dir `@opencode-ai/plugin` install: forked detached, failure is a warning
  (`config.ts:461-470`, `"background dependency install failed"`).
- Per-plugin install failure: `packages/opencode/src/plugin/loader.ts:96-101` returns
  `{ok:false, stage:"install"}`, the plugin is dropped and a session error event is published
  (`packages/opencode/src/plugin/index.ts:198-202`); filtered at `loader.ts:232-235`.
- **Hang bound: 5 minutes.** The install flock has `STALE_MS = 60_000`, `TIMEOUT_MS = 5 * 60_000`
  (`packages/core/src/util/effect-flock.ts:43-53`). `Config.waitForDependencies()`
  (`config.ts:632-636`) is awaited **before loading plugins** (`plugin/index.ts:184`) and **before
  loading custom tools** (`packages/opencode/src/tool/registry.ts:187`), so a blackholed registry can
  stall the first prompt for that window. Same DROP-vs-REJECT lesson as §6.5.

### 4.4 Pre-populated cache satisfies resolution fully offline — YES

`packages/core/src/npm.ts:125-127`:

```ts
if (yield* afs.existsSafe(path.join(dir, "node_modules", name))) {
  return resolveEntryPoint(name, path.join(dir, "node_modules", name))
}
```

A pure existence check — no version verification, no registry contact. Pre-creating
`$XDG_CACHE_HOME/opencode/packages/<pkg>@<ver>/node_modules/<pkg>` (and `<pkg>@latest/...` for bare
names) makes resolution fully offline. This is exactly the plan's line 141 "verified read-only
bundles only when dependencies are fully resolved before launch" — **it works, and the image can bake
it.** Note the flip side: **no integrity check**, so the cache contents are trusted purely on the
strength of how the image built them.

`Npm.install` short-circuits twice more: `npm.ts:140-144` returns immediately if the dir is **not
writable** (so making `.opencode` read-only is a clean kill switch for the per-dir install), and
`npm.ts:148-157` / `:159-187` skip when `node_modules` exists and no declared dep is missing from
`package-lock.json`.

**There is no frozen-lockfile or offline flag in opencode's own code.** Grep for
`frozen|offline|prefer-offline` across `packages/core/src` + `packages/opencode/src` returns only
OAuth `offline_access` scopes. The only lever is npm's own `offline=true` / `prefer-offline=true` via
`.npmrc` or `npm_config_offline=true`, which flows through `NpmConfig.load` → `config.flat` →
Arborist (`npm.ts:85-101`). **That is a legitimate, documented npm mechanism, not a guessed env var —
but it is npm's contract, not OpenCode's, and should be stated as such.**

### 4.5 Disabling plugins — `OPENCODE_PURE` is the answer, with caveats

| Lever | Effect | Evidence |
|---|---|---|
| **`OPENCODE_PURE=1`** (or CLI `--pure`) | **Disables all external/config plugins** | `packages/opencode/src/effect/runtime-flags.ts:18`; server `packages/opencode/src/plugin/index.ts:181` `const plugins = flags.pure ? [] : (cfg.plugin_origins ?? [])`; TUI `packages/opencode/src/plugin/tui/runtime.ts:1089`; CLI→env `packages/opencode/src/index.ts:62-71` |
| `OPENCODE_DISABLE_DEFAULT_PLUGINS=1` | Disables the ~12 built-in auth/provider plugins only | `runtime-flags.ts:19`; `plugin/index.ts:170` |
| `OPENCODE_DISABLE_PROJECT_CONFIG=1` | Drops project-scoped plugins only; globals survive | `paths.ts:27`, `config.ts:420` |

**`OPENCODE_PURE` does NOT disable:** the background `@opencode-ai/plugin` npm install in every
config dir (`config.ts:452`, no pure guard), `.opencode` directory creation (`config.ts:450`/`:310`),
or **`.opencode/tool/` custom tools** (`packages/opencode/src/tool/registry.ts:183-197`, no pure
guard). There is **no `OPENCODE_DISABLE_PLUGIN*` variable** anywhere in the repo, and **no config key**
to disable plugins.

### 4.6 What a plugin can do in-process

Loaded with a raw `await import(row.entry)` (`packages/opencode/src/plugin/loader.ts:139`) —
**same process, same JS runtime, no sandbox, no isolate.** Initialised **eagerly** at instance
bootstrap (`packages/opencode/src/project/bootstrap.ts:38` `yield* plugin.init()`, with the comment
at `:37-38`: *"Plugin can mutate config so it has to be initialized before anything else"*).

`PluginInput` (`packages/plugin/src/index.ts:56-66`) hands each plugin a full authenticated SDK
client, `project`, `directory`, `worktree`, `serverUrl`, and **`$: Bun.$`** (arbitrary shell) —
constructed at `packages/opencode/src/plugin/index.ts:153-168`, with `$` bound at `:167` and
`headers: ServerAuth.headers()` at `:149`. **A plugin therefore holds the server's own credentials
and a shell.** This is precisely the plan's line 141 position: executable customization runs in the
untrusted runner and must be contained by the node gateway and the sandbox, never trusted.

Hook list — `packages/plugin/src/index.ts:222-335`:
`dispose`:223, `event`:224, `config`:225, `tool` (map of tool definitions):226-228, `auth`:229,
`provider`:230, `chat.message`:234-243, `chat.params`:247-256, `chat.headers`:257-260,
**`permission.ask`:261**, `command.execute.before`:262-265, `tool.execute.before`:266-269,
`shell.env`:270-273, `tool.execute.after`:274-281,
`experimental.chat.messages.transform`:282-290, `experimental.chat.system.transform`:291-296,
`experimental.provider.small_model`:297, `experimental.session.compacting`:305-308,
`experimental.compaction.autocontinue`:316-326, `experimental.text.complete`:327-330,
`tool.definition`:334 (rewrite the description and JSON schema of **any** tool).

**Finding: `permission.ask` is DECLARED but NEVER INVOKED in v1.18.30.**
`grep -rn 'trigger("permission' --include=*.ts` returns zero hits; the only non-test occurrence of
the string is the type declaration at `packages/plugin/src/index.ts:261`. Every live trigger site is
enumerable (`agent/agent.ts:381`; `session/tools.ts:106,121,175,208,258,291,338,373,402,420`;
`session/prompt.ts:307,389,554,999,1255,1460`; `session/processor.ts:530`;
`session/compaction.ts:373,379,500`; `session/llm/request.ts:69,114,134`; `tool/code-mode.ts:141,180`;
`tool/registry.ts:318`; `provider/provider.ts:1953`; `plugin/pty-environment.ts:18`;
`server/.../handlers/pty.ts:71`) and none is `permission.ask`.
**Do not design a tracon permission-broker plugin around this hook on v1.18.30.** Either contribute
it upstream (a small, bounded contribution) or broker permissions at the gateway/HTTP layer.

A second Effect-based **v2 plugin API** exists (`PluginContext` — `agent`, `aisdk` model
interception, `catalog`, `command`, `integration`, `plugin.add/remove`, `reference`, `skill`) at
`packages/core/src/plugin/host.ts:29-218`, loaded from the `plugins` key at
`packages/core/src/config/plugin/external.ts:73-88` (also `await import(entrypoint)`, `:80`).

### 4.7 Custom tools in `.opencode/tool/` — the gap

`packages/opencode/src/tool/registry.ts:183-197`:

```ts
const dirs = yield* config.directories()
const matches = dirs.flatMap((dir) =>
  Glob.scanSync("{tool,tools}/*.{js,ts}", { cwd: dir, absolute: true, dot: true, symlink: true }))
if (matches.length) yield* config.waitForDependencies()
for (const match of matches) {
  const namespace = path.basename(match, path.extname(match))
  const mod = yield* Effect.promise(() => import(pathToFileURL(match).href))
  ...
}
```

- Both `tool/` and `tools/`, `.js` and `.ts`, non-recursive, symlinks followed, dot-prefixed files included;
  same `config.directories()` list as plugins.
- Naming: `default` export ⇒ filename; named export `X` in `foo.ts` ⇒ `foo_X`.
- Shape duck-typed at `registry.ts:355-357` (`args`, `description`, `execute`).
- **Executed in-process** via dynamic `import()` (`:192`), called at `:154`. No sandbox.
- **Not gated by `OPENCODE_PURE`.**
- **Permission is NOT automatic.** `packages/opencode/src/session/tools.ts:102-132` fires
  `tool.execute.before`, calls `item.execute(args, ctx)` at `:111`, then `tool.execute.after` —
  **there is no `ctx.ask(...)`**. Contrast MCP tools at `session/tools.ts:408`, which do ask. A custom
  tool prompts only if it *voluntarily* calls `context.ask(...)` (bridged at `registry.ts:148-153` →
  `session/tools.ts:81-89`). A custom tool can be *hidden* by an explicit `deny` + `*` rule on its id
  (`packages/opencode/src/permission/index.ts:204-214`, applied at
  `packages/opencode/src/session/llm/request.ts:208-214`), but that is opt-in denial.

**Consequence:** `OPENCODE_DISABLE_PROJECT_CONFIG=true` is what keeps repo-supplied
`.opencode/tool/*.ts` out; `OPENCODE_PURE` does **not**. Global and `$HOME/.opencode` tool dirs must
be kept empty by per-session HOME/XDG. Any node-approved custom tool that needs a permission prompt
must call `context.ask` itself.

---

## 5. MCP

### 5.1 Config schema

`mcp` is a record of name → server, `packages/core/src/v1/config/config.ts:113-115`:

```ts
mcp: Schema.optional(
  Schema.Record(Schema.String, Schema.Union([ConfigMCPV1.Info, Schema.Struct({ enabled: Schema.Boolean })])))
```

`packages/core/src/v1/config/mcp.ts`:
- **Local / stdio** `:6-23`: `type: "local"`, `command: string[]` (argv; `command[0]` is the binary),
  `cwd?`, `environment?: Record<string,string>`, `enabled?`, `timeout?: PositiveInt`
- **Remote** `:44-59`: `type: "remote"`, `url: string`, `enabled?`, **`headers?: Record<string,string>`**,
  `oauth?: OAuth | false`, `timeout?: PositiveInt`
- **OAuth** `:26-41`: `clientId`, `clientSecret`, `scope`, `callbackPort` (default 19876),
  `redirectUri` (default `http://127.0.0.1:19876/mcp/oauth/callback`)
- Union discriminated on `type` at `:62`

(v2 variant with `disabled` instead of `enabled` and `mcp.servers`: `packages/core/src/config/mcp.ts:19-61`;
lowering at `packages/opencode/src/config/v2-compat.ts:23-54,245-311,362-377`.)

### 5.2 HTTPS + bearer token — YES, this is exactly tracon's `/mcp/{session_id}` case

`packages/opencode/src/mcp/index.ts:269-284`:

```ts
const transports = [
  { name: "StreamableHTTP", transport: new StreamableHTTPClientTransport(url, {
      authProvider, requestInit: mcp.headers ? { headers: mcp.headers } : undefined }) },
  { name: "SSE", transport: new SSEClientTransport(url, {
      authProvider, requestInit: mcp.headers ? { headers: mcp.headers } : undefined }) },
]
```

Also on the OAuth reconnect path `:846-849` and in `opencode mcp debug`
(`packages/opencode/src/cli/cmd/mcp.ts:736-742`).

Config shape tracon should emit:

```json
{"mcp": {"tracon": {"type": "remote",
                    "url": "https://gateway/mcp/<session_id>",
                    "headers": {"Authorization": "Bearer <token>"},
                    "oauth": false}}}
```

**Set `oauth: false`** (`mcp/index.ts:240-241,251-267`) — otherwise an `McpOAuthProvider` is attached
alongside the headers and a 401 triggers a browser OAuth flow. Verified upstream by
`packages/opencode/test/mcp/headers.test.ts:42-60`, which asserts `authorization: Bearer test-token`
on every request.

Transport order is **StreamableHTTP first, SSE fallback** (`mcp/index.ts:269-284`, loop `:289-332`);
the loop breaks early on `needs_auth` / `needs_client_registration` (`:331`).

### 5.3 Protocol version and SDK

**`@modelcontextprotocol/sdk` pinned to `1.29.0`** — `packages/opencode/package.json:83`;
lock `bun.lock:597,1831`; **patched** via `package.json:159` →
`patches/@modelcontextprotocol%2Fsdk@1.29.0.patch`.

**OpenCode does not hardcode or override any protocol version string.** It sends SDK 1.29.0's
`LATEST_PROTOCOL_VERSION` and relies on the SDK's `SUPPORTED_PROTOCOL_VERSIONS` negotiation. The patch
hoists the handshake into `_initialize(transport, options)` so it can be replayed on session expiry
(adds `transport.setProtocolVersion(result.protocolVersion)` and
`transport.onsessionexpired = async () => { await this._initialize(transport) }`). The only place
OpenCode names the constant is the debug command, and it imports it
(`packages/opencode/src/cli/cmd/mcp.ts:747`). `node_modules` is absent from this checkout, so the
literal date string is determined entirely by the 1.29.0 tarball — **tracon's compatibility manifest
must pin and record the resolved version string from the actual artifact, not from source.**

Client identity and capabilities — `packages/opencode/src/mcp/index.ts:39-50,75-81`:
`new Client({ name: "opencode", version: InstallationVersion }, { capabilities: { roots: {} } })`.
`sampling`, `elicitation` and `tasks` are explicitly commented out (`:41-48`). A `roots/list` handler
returns the instance directory as a `file://` URI (`:77-79`).

### 5.4 Timeouts — three distinct numbers, and the docs are wrong

- **`DEFAULT_TIMEOUT = 30_000`** — defined twice (`packages/opencode/src/mcp/index.ts:38`,
  `packages/opencode/src/mcp/catalog.ts:11`). Applies to **connect/initialize**
  (`mcp/index.ts:286` remote, `:359` local, enforced by `withTimeout(client.connect(t), timeout)` at
  `:226`; helper `packages/opencode/src/util/timeout.ts:1-13`) and to **`tools/list`**
  (`catalog.ts:38-40`).
- **Tool execution: no OpenCode default.** `packages/opencode/src/mcp/index.ts:661-664`:
  ```ts
  function requestTimeout(s, name, configured, fallback?) {
    const staticTimeout = configured && isMcpConfigured(configured) ? configured.timeout : undefined
    return s.config[name]?.timeout ?? staticTimeout ?? fallback
  }
  ```
  with `fallback = cfg.experimental?.mcp_timeout` (`:672`). If neither is set this is `undefined` and
  is passed straight to `client.callTool(..., { timeout })` at `packages/opencode/src/mcp/catalog.ts:53-67`:
  ```ts
  await client.callTool({ name, arguments: … }, CallToolResultSchema, {
    resetTimeoutOnProgress: true, signal: options.abortSignal, timeout, onprogress: () => {} })
  ```
  ⇒ the **SDK's `DEFAULT_REQUEST_TIMEOUT_MSEC` (60 s in the MCP TS SDK)** governs, and
  `resetTimeoutOnProgress: true` with a forced progress token means a server emitting progress can
  extend it **indefinitely** — there is no `maxTotalTimeout`.

**Answer to the `review_status` question:** the omp-era 20 s cap was sized against a 30 s client
timeout. Under OpenCode the effective tool-call ceiling is **60 s (SDK default)**, explicitly settable
via `experimental.mcp_timeout` (ms) or per-server `mcp.<name>.timeout`. Better still, tracon's MCP
server can emit **progress notifications**, which reset the timer (`catalog.ts:55`) and remove the cap
entirely. **Recommendation: set `experimental.mcp_timeout` explicitly in the manifest rather than
inheriting an SDK default, and keep the tool's own wait comfortably under it.**

⚠️ **The config doc strings are stale/wrong.** `packages/core/src/v1/config/mcp.ts:20-22` and `:56-58`
both say *"Defaults to 5000 (5 seconds) if not specified"*. The code path is 30 000 for connect/list
and SDK-default for calls. Do not size anything from the documentation.

Pagination guard on listings: `MAX_LIST_PAGES = 1_000` with duplicate-cursor detection
(`packages/opencode/src/mcp/catalog.ts:12,18-36`).

### 5.5 MCP tool calls ARE subject to the permission system

Unlike custom tools (§4.7), every MCP tool call asks —
`packages/opencode/src/session/tools.ts:390-419`, with the check at **`:408`**:

```ts
yield* ctx.ask({ permission: key, metadata: {}, patterns: ["*"], always: ["*"] })
```

`ctx.ask` routes to `Permission.ask` with the merged agent + session ruleset
(`session/tools.ts:81-89`). The permission id is the flattened tool name
`sanitize(server) + "_" + sanitize(toolName)` (`packages/opencode/src/mcp/catalog.ts:117-119`), so
rules are written as `"tracon_review_status": "allow"` or a wildcard `"tracon_*"`.
Default when no rule matches is **`ask`** (`packages/opencode/src/permission/index.ts:28-38`);
resolution at `:67-107` (deny ⇒ `DeniedError`, `:75-79`). Because `always: ["*"]`, answering "always"
allow-lists **the entire tool** for the session (`permission/index.ts:145-151`) — note it is `["*"]`,
not the specific arguments, so "always" is coarse.
Code-mode path asks identically (`packages/opencode/src/tool/code-mode.ts:147`).
MCP **resource** tools use structured `mcp:<server>:<uri>` patterns
(`session/tools.ts:173-174,256-257,346-347`) mapped onto the `read` permission
(`permission/index.ts:206,209`).
A `deny` + `*` rule removes the tool from the model's schema entirely
(`packages/opencode/src/tool/registry.ts:286`, `packages/opencode/src/tool/code-mode.ts:210`, via
`Permission.visibleTools` at `permission/index.ts:216-219`).

### 5.6 Startup and failure

**Not started at process startup — lazily, per-instance (per working directory), on first MCP access.**
The connection block is inside `InstanceState.make` (`packages/opencode/src/mcp/index.ts:492-560`),
and `InstanceState` is a `ScopedCache` whose lookup runs on first `get`
(`packages/opencode/src/effect/instance-state.ts:26-50`). MCP is deliberately **absent** from the
eager bootstrap list (`packages/opencode/src/project/bootstrap.ts:41-45` covers
`lsp, shareNext, format, vcs, snapshot, project`, plus `plugin.init()` at `:38`). First touch is
session tool assembly (`session/tools.ts:136,390`), command listing
(`packages/opencode/src/command/index.ts:62`), the system prompt
(`packages/opencode/src/session/system.ts:65`), or the `/mcp` HTTP routes.

Once triggered, **all configured servers connect concurrently and unconditionally** except explicitly
disabled ones — `mcp/index.ts:505-529`, `{ concurrency: "unbounded" }` at `:528`. Entries without a
`type` are logged as errors and skipped (`:509-512`); `enabled: false` ⇒ status `"disabled"`
(`:514-517`, early return at `:374-376`).

**Failure is non-fatal and contained** — `mcp/index.ts:408-414` catches the cause and yields
`{status: {status: "failed", error}}`; local-transport failures at `:365-368`; remote failures produce
`failed` / `needs_auth` / `needs_client_registration` at `:292-327,334-337`. A failed server logs
`"server unavailable"` (`:385`) and contributes no tools. Statuses: `connected | disabled | failed |
needs_auth | needs_client_registration` (`:83-107`).
Resilience: transport closed on failure via `acquireUseRelease` (`:220-231`); `client.onclose`
demotes to `failed: "Connection closed"` and republishes `ToolsChanged` (`:442-455`);
`notifications/tools/list_changed` re-lists (`:462-471`); server log notifications forwarded to
OpenCode's logger (`:457-459,474-490`).
**On scope teardown, stdio children are killed by walking `pgrep -P` descendants and SIGTERM-ing them
before `client.close()`** (`:531-556`, `:418-440`) — better hygiene than the LSP path (§6.7), but
still dependent on finalizers running, which `serve` does not guarantee on SIGTERM.
Stdio env: full `process.env` inherited plus `mcp.environment` overlay, with `BUN_BE_BUN=1` when the
command is literally `opencode` (`:347-357`). **Note the full-env inheritance** — a stdio MCP server
configured by a project config would inherit every secret in the runner's environment. One more
reason `OPENCODE_DISABLE_PROJECT_CONFIG` is mandatory.

---

## 9. Verdict table against the plan's requirements

Legend — **N** = supported natively; **E** = supportable by env + config (+ image packaging);
**U** = needs an upstream change / is a STOP-candidate or an explicit design decision.

| # | Plan requirement | Verdict | Evidence | What it costs |
|---|---|---|---|---|
| 1 | **Single-source config** — one node-written file is the only source (plan L137) | **E**, with an auditable residue | §1.3, §1.6; `packages/opencode/src/config/config.ts:328-609`; `packages/opencode/src/config/paths.ts:23-41`; `packages/core/src/global.ts:11-14` | Requires `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME`, `XDG_STATE_HOME`, `HOME`, `OPENCODE_DISABLE_PROJECT_CONFIG=true`, `OPENCODE_CONFIG=<path>`. `/etc/opencode` has no env control (only the test-named `OPENCODE_TEST_MANAGED_CONFIG_DIR`) → **image must not contain `/etc/opencode`**. Remote well-known/console config is credential-gated, not flag-gated. **Claim "no ambient discovery given a hermetic image", not "single-source by construction".** |
| 2 | **No ambient repo config** (plan L137) | **E for config; U for one instruction path** | §2.1–2.5; `packages/opencode/src/session/instruction.ts:81,123`; `packages/core/src/instruction-context.ts:48`; upstream tests `packages/opencode/test/config/config.test.ts:1995-2037` | `OPENCODE_DISABLE_PROJECT_CONFIG=true` covers project `opencode.json[c]`, `.opencode/*`, `AGENTS.md` system prompt, `instructions` globs. **Gap: `Instruction.resolve` (`instruction.ts:179-221`) attaches nested `AGENTS.md`/`CLAUDE.md` whenever the `read` tool reads a file, with no flag check.** `OPENCODE_DISABLE_CLAUDE_CODE=true` removes the `CLAUDE.md` half only. Either accept and document it (instruction content grants no permission — plan L139) or contribute the flag check upstream. |
| 3 | **Skills from a read-only bundle** (plan L133, L141) | **N** for loading; **U** for duplicate rejection and for a discovery off-switch | §3.5 (`packages/opencode/src/skill/index.ts:211-219` — absolute paths accepted, nothing written); §3.4 (`:125-139` warn-and-overwrite, `:240-243` racy); §3.3 (no total off-switch); §3.6 (`skills.urls` fetch has no flag) | Read-only outside-worktree bundles work today. **Upstream does NOT reject duplicate names (plan L133 requires it) and the winner is nondeterministic** → tracon must reject at manifest-build time in the node. `OPENCODE_DISABLE_EXTERNAL_SKILLS=true` + `OPENCODE_DISABLE_CLAUDE_CODE=true` + `OPENCODE_DISABLE_PROJECT_CONFIG=true` + clean HOME/XDG leaves only the manifest's `skills.paths`. Also: **skill bodies become shell-interpolated slash commands** (`packages/opencode/src/command/index.ts:134-151`; `packages/opencode/src/config/markdown.ts:6,12-14`) — treat skill content as code. |
| 4 | **No runtime plugin install** (plan L141) | **E**, with two residual installs | §4.3–4.5; `packages/opencode/src/plugin/index.ts:181`; `packages/core/src/npm.ts:125-127,140-144`; `packages/opencode/src/config/config.ts:452-471`; `packages/opencode/src/tool/registry.ts:183-197` | `OPENCODE_PURE=1` disables all config plugins. **It does NOT disable** the per-config-dir `@opencode-ai/plugin` install (`config.ts:452`) or `.opencode/tool/` custom tools (`registry.ts:183`). Residuals are closed by: `OPENCODE_DISABLE_PROJECT_CONFIG=true` (project tool dirs), clean HOME/XDG (global tool dirs), a **read-only or pre-populated** config dir (`npm.ts:140-144` skips a non-writable dir; `:125-127` is a bare existence check, so a baked cache resolves offline), plus network denial. `ignoreScripts: true` (`npm.ts:91`) means dep install scripts never run. No integrity check on the baked cache — the image is the trust root. |
| 5 | **No LSP download** (plan L143) | **N/E** — genuinely good | §6.1 (LSP off by default, `packages/opencode/src/lsp/lsp.ts:151`), §6.4 (`OPENCODE_DISABLE_LSP_DOWNLOAD`, `runtime-flags.ts:22`, 24 call sites) | Default is already "nothing runs". Set `OPENCODE_DISABLE_LSP_DOWNLOAD=true` **and** declare each server with an absolute image-baked `command` (copying the builtin `initialization`, since an override discards it — `lsp.ts:173-179`). **Caveat: `prettier`/`oxfmt`/`@biomejs/biome` formatter auto-installs are NOT covered by that flag** (`packages/opencode/src/format/formatter.ts:79,102,148`) — override their `command` too. |
| 5b | **Missing servers fail visibly, not by hanging** (plan L143) | **U / runner property** | §6.5; no `AbortSignal` in `packages/opencode/src/lsp/server.ts`; serial awaited spawn `lsp.ts:254-289`; `edit.ts:197`, `write.ts:75` await inline; silent `broken` set `lsp.ts:220-228,259` | **On a DROP-style egress denial the first edit of a `.ts` file can block indefinitely.** Runner egress must **REJECT (RST/ENOTFOUND), not DROP** — plus `OPENCODE_DISABLE_LSP_DOWNLOAD=true`. The same applies to the 5-minute npm flock window (`packages/core/src/util/effect-flock.ts:43-53`). Upstream also emits **no error log** on spawn failure, so plan L143's "show disabled/unavailable/starting/running/failed" cannot be built faithfully without an upstream status/event surface — a bounded contribution, or an explicit scope cut. |
| 5c | **Terminate child processes with the runner** (plan L143) | **U / runner property** | §6.7; `packages/opencode/src/cli/cmd/serve.ts:22` (`Effect.never`, no signal handlers); `packages/opencode/src/index.ts:137-141` (`process.exit()` in `finally`); `packages/opencode/src/util/process.ts:149-163` (SIGTERM, no escalation, no group kill); LSP spawned **without** `detached` (`util/process.ts:63-69`) | `opencode serve` installs **no SIGINT/SIGTERM/SIGHUP handler**, so a K8s/Podman stop runs zero finalizers and orphans every LSP child. Formatters are fine (`packages/core/src/cross-spawn-spawner.ts:424,292-312` detach + group-kill); MCP stdio children are walked with `pgrep -P` (`mcp/index.ts:531-556`) but only if finalizers run. **Must rely on the container PID namespace + a real init**, not on OpenCode. Record as a runner requirement. |
| 6 | **Per-session state isolation** (plan L191) | **E** | §7.1–7.2; `packages/core/src/global.ts:10-31`; `packages/core/src/database/database.ts:43-55`; `packages/opencode/test/preload.ts:1-2,34-37` | Per-session `HOME` + all four `XDG_*` + `OPENCODE_DB`. **All must be in the launch environment** — `global.ts:35-43` mkdirs at import and `database.ts:57` resolves the DB path at import. `OPENCODE_TEST_HOME` alone is insufficient (upstream's own harness sets the XDG vars instead). `XDG_CONFIG_HOME` must be **writable-but-empty**, not read-only (import-time mkdir); mount the read-only manifest elsewhere and point `OPENCODE_CONFIG` at it. |
| 6b | **Prevent two processes/versions opening the same state** (plan L193) | **U — must be built by tracon** | §7.2; no flock on the DB anywhere; `packages/core/src/util/flock.ts` is used for models.dev/npm/repos/MCP-auth/plugin-meta only; in-process semaphores at `sqlite.bun.ts:121-130`, `migration.ts:11` | **Upstream provides nothing.** Two processes can open the same DB; coordination is only WAL + `busy_timeout=5000` (`database.ts:27-32`). Tracon must own single-writer fencing at the node, keyed to the per-session DB path. |
| 7 | **Safe backup / migration, verified restore with the older runtime** (plan L195) | **U — must be built by tracon; downgrade is unsafe** | §7.2; `packages/core/src/database/migration.ts:18-41,43-107`; `packages/opencode/src/cli/cmd/{db,export,import,stats}.ts` | **No backup/snapshot command exists.** `opencode export` is a redacted session transcript, not a DB backup; `opencode db` is a raw SQL shell. Tracon must implement quiesce → `VACUUM INTO` / `.backup` → verify → migrate-on-clone itself. **Worse: an older binary opening a newer DB proceeds silently and can write** — the `migration` table is an applied-ID ledger with no max-known check, no `user_version`, no fingerprint, no error. `latest`/`beta`/`prod` all share `opencode.db` (`database.ts:48-54`). **Plan L195's "Never promise transparent downgrade" is therefore mandatory, not merely prudent**, and tracon must gate restore on a recorded state-schema generation of its own. |
| 8 | **No non-inference egress** (plan L141, L147) | **E**, except two paths | §8 (37 enumerated calls); §8.1 | Disableable: models.dev (`OPENCODE_DISABLE_MODELS_FETCH`, better `OPENCODE_MODELS_PATH`), updates (`OPENCODE_DISABLE_AUTOUPDATE`), LSP (`OPENCODE_DISABLE_LSP_DOWNLOAD`), share (`OPENCODE_DISABLE_SHARE` + `share: "disabled"`), Exa/Parallel (off by default), OTEL (opt-in). **No telemetry of any kind — clean negative.** **Two with no flag: (a) ripgrep download (`packages/core/src/ripgrep/binary.ts:105`) → bake `rg` on PATH or pre-seed `$cache/bin/rg`; (b) web-UI fallback to `https://app.opencode.ai` (`packages/opencode/src/server/shared/ui.ts:88-93`, forwards client headers, CSP `connect-src *` at `:12`) — `OPENCODE_DISABLE_EMBEDDED_WEB_UI` makes it *more* likely, not less.** (b) is the plan's L147 case: verify the artifact embeds the full UI bundle and intercept the egress at the gateway. |
| 9 | **`OPENCODE_PURE` as a hermetic-mode master switch** | **Does not exist — do not claim it** | `packages/core/src/flag/flag.ts:66-68`; only consumers are `packages/opencode/src/plugin/index.ts:181` and `packages/opencode/src/plugin/tui/runtime.ts:1089` | It disables config plugins and nothing else. |
| 10 | **MCP client to tracon's `/mcp/{session_id}` with a bearer token** | **N** | §5.2; `packages/opencode/src/mcp/index.ts:269-284`; test `packages/opencode/test/mcp/headers.test.ts:42-60` | Works today. Use `type: "remote"`, `headers.Authorization`, and **`oauth: false`**. Set `experimental.mcp_timeout` explicitly (the effective default for calls is the SDK's 60 s, not the documented 5 s — §5.4), or emit progress notifications to reset the timer. MCP tool calls **are** permission-checked (§5.5); custom `.opencode/tool/` tools are **not** (§4.7). |

### 9.1 STOP-candidates, ranked

Nothing here requires "a large long-lived fork" (plan L233), so **Gate A does not stop**. Ranked
residual risks:

1. **Downgrade/migration safety (row 7).** The only one with a silent-data-loss shape. Mitigation is
   entirely tracon-side and must be designed before Gate C, not discovered in it.
2. **Single-writer fencing (row 6b).** Also entirely tracon-side; cheap, but it must be explicit.
3. **LSP hang on a DROP-style network (row 5b)** and the **5-minute npm flock window (row 4)**.
   Mitigated by REJECT-style egress denial + `OPENCODE_DISABLE_LSP_DOWNLOAD`. Verify in Gate C's
   network-denied tests — the plan already calls for exactly that (L243).
4. **Child-process orphaning on SIGTERM (row 5c).** Mitigated by the container boundary; must be
   asserted as a runner property, with a test.
5. **Web UI upstream fallback (row 8b).** Packaging + gateway, per plan L147. Verify the asset
   inventory of the actual artifact.
6. **Duplicate skill names not rejected, and racy (row 3).** Node-side manifest validation.
7. **`permission.ask` plugin hook is dead code (§4.6).** Only matters if tracon planned to broker
   permissions in-process. Broker at the gateway instead, or contribute the trigger upstream.
8. **Nested `AGENTS.md` attached by the `read` tool (row 2).** Document honestly; do not claim
   ambient repo instructions are fully suppressed.

### 9.2 The minimum launch environment, as settled by code

```
HOME=<per-session, empty>                  # no .opencode, no .claude
XDG_CONFIG_HOME=<per-session, writable-but-empty>   # import-time mkdir; NOT read-only
XDG_DATA_HOME=<per-session>                # empty auth.json ⇒ no .well-known remote config
XDG_CACHE_HOME=<per-session, may be seeded read-mostly with npm cache + rg>
XDG_STATE_HOME=<per-session>
OPENCODE_CONFIG=<read-only manifest, outside the worktree, with "$schema" present>
OPENCODE_DB=<per-session absolute path>
OPENCODE_DISABLE_PROJECT_CONFIG=true
OPENCODE_PURE=1
OPENCODE_DISABLE_DEFAULT_PLUGINS=true      # only if no built-in provider auth is needed
OPENCODE_DISABLE_EXTERNAL_SKILLS=true
OPENCODE_DISABLE_CLAUDE_CODE=true          # covers CLAUDE.md prompt + .claude skills
OPENCODE_DISABLE_LSP_DOWNLOAD=true
OPENCODE_DISABLE_MODELS_FETCH=true         # or OPENCODE_MODELS_PATH=<pinned catalogue>
OPENCODE_DISABLE_AUTOUPDATE=true
OPENCODE_DISABLE_SHARE=true
OPENCODE_SERVER_PASSWORD=<per-session>     # serve runs UNSECURED without it
```

Plus, as **image/runner** properties (not env):
- no `/etc/opencode`
- `rg` on `PATH` (or `$XDG_CACHE_HOME/opencode/bin/rg` pre-seeded)
- LSP/formatter binaries baked, named by absolute path in the manifest's `lsp`/`formatter` keys
- `$XDG_CACHE_HOME/opencode/packages/<pkg>@<ver>/node_modules/<pkg>` pre-populated for any approved plugin
- the selected artifact verified to embed the full web UI bundle
- egress denied with **REJECT**, not DROP
- PID namespace with a real init, and teardown that reaps orphans
- gateway pins/validates `?directory=` and `x-opencode-directory` (§0)
