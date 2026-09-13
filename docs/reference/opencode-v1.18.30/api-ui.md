# Gate A — OpenCode v1.18.30: native API surface, permissions, PTY, web UI, release artifact

Source inspected: `<opencode-src>` (v1.18.30).
Paths below are relative to that root. Line numbers are from the files as they exist there.

**Which package `opencode serve` actually runs.** `packages/opencode` owns the CLI and the
listener. `packages/server` is **not** launched standalone by `serve` — its
`createRoutes()`/`webHandler()` (`packages/server/src/routes.ts:39-68`) serve other hosts, and
its `openapiPath: "/openapi.json"` (`routes.ts:54`) is therefore **not** reachable from
`opencode serve`. But `packages/server`'s *API definition* **is** mounted inside the opencode
listener: `packages/opencode/src/server/routes/instance/httpapi/server.ts:177-181` builds
`serverRoutes = HttpApiBuilder.layer(Api)` from `@opencode-ai/server/api`
(`packages/server/src/api.ts:5` → `makeDefaultApi`, `packages/protocol/src/api.ts:78`).

So one process serves **two** surfaces:

- **v1 / legacy**, bare paths (`/session`, `/pty`, `/config`, `/global/*`, `/auth/*`, `/tui/*`…),
  declared in `packages/opencode/src/server/routes/instance/httpapi/groups/*.ts`;
- **v2**, under `/api/*`, declared in `packages/protocol/src/groups/*.ts`, implemented in
  `packages/server/src/handlers/*.ts`.

Composition: `packages/opencode/src/server/routes/instance/httpapi/api.ts:54-94`.
**No path prefixes are injected**: `HttpApi.addHttpApi()` merges endpoints verbatim, so every
path written in a group file is the final absolute path.

`packages/opencode/src/server/routes/instance/httpapi/public.ts:530-537` is **not** a second
mount — `PublicApi = OpenCodeHttpApi.annotateMerge(...)`, i.e. OpenAPI-spec generation for
`GET /doc` (`.../server.ts:188-192`). **"Public API" ≡ the entire surface. There is no
internal-only route set and no route that is hidden from an authenticated caller.**

---

## 1. Server surface

### Entry point

| Fact | Evidence |
|---|---|
| CLI registration | `packages/opencode/src/index.ts:14,93` |
| `serve` handler | `packages/opencode/src/cli/cmd/serve.ts:6-24` |
| Listener | `packages/opencode/src/server/server.ts:73-98` (`Server.listen`) |
| Node server | `packages/opencode/src/server/server.ts:200` (`createServer()`), `:214` (`NodeHttpServer.layer(() => server, { port, host, gracefulShutdownTimeout: "1 second" })`) |
| Route tree | `.../httpapi/server.ts:271-313` (`createRoutes`) |
| `--version` output | yargs `.version("version", "show version number", InstallationVersion)` — `packages/opencode/src/index.ts:51`. Prints **the bare version string and nothing else** (e.g. `1.18.30`). `InstallationVersion` is the build-time `OPENCODE_VERSION` define, falling back to the literal `"local"` (`packages/core/src/installation/version.ts:6`). |

`serve` runs with `instance: false` (`serve.ts:12`) — no ambient project is loaded at startup;
**every request chooses its own project directory** (see the implicit-authorization flag below).

### Flags (exact spellings)

Declared once in `packages/opencode/src/cli/network.ts:6-33` and shared by `serve` and `web`:

| Flag | Default | Note |
|---|---|---|
| `--port` | `0` | `0` = "try 4096 first, then any free port" (`server.ts:117-122`) |
| `--hostname` | `"127.0.0.1"` | |
| `--mdns` | `false` | describe: *"enable mDNS service discovery (defaults hostname to 0.0.0.0)"* |
| `--mdns-domain` | `"opencode.local"` | |
| `--cors` (array) | `[]` | extra allowed CORS origins |

Resolution (`network.ts:62-80`): an explicitly-typed CLI arg wins; else global config
`server.port` / `server.hostname` / `server.mdns` / `server.mdnsDomain` / `server.cors`; else the
default. **`network.ts:70-74`: if mDNS is enabled and `config.server.hostname` is unset, the
hostname is silently rewritten to `0.0.0.0`.**

There is **no** `--unix`, `--socket`, `--tls`, `--auth`, `--password`, or `--no-ui` flag.

### Default bind / loopback / Unix socket

- **Default bind: `127.0.0.1`, port 4096 (falling back to a random free port).**
- Loopback can be forced and is sticky: an explicit `--hostname 127.0.0.1` always beats config
  (`network.ts:64,70-71`). **OK.**
- **No Unix-domain-socket support.** Only `{ port, host }` reach `NodeHttpServer.layer`
  (`server.ts:214`); no code path accepts a socket path. Loopback TCP is the only local-only
  transport. This is a limitation, not a blocker.

### Authentication

- **Env vars only, no flags:** `OPENCODE_SERVER_PASSWORD`, `OPENCODE_SERVER_USERNAME`
  (default username `"opencode"`) — `packages/server/src/auth.ts:31-32`; mirrored at
  `packages/core/src/flag/flag.ts:32-33`.
- **If `OPENCODE_SERVER_PASSWORD` is unset or empty the server is entirely unauthenticated.**
  `ServerAuth.required()` (`packages/server/src/auth.ts:40-42`) returns false and every auth
  middleware short-circuits to an identity function
  (`.../httpapi/middleware/authorization.ts:104,122,138`;
  `packages/server/src/middleware/authorization.ts:289`). `serve` only prints
  `Warning: OPENCODE_SERVER_PASSWORD is not set; server is unsecured.` (`serve.ts:15-17`).
- Scheme: HTTP **Basic**, `WWW-Authenticate: Basic realm="Secure Area"`
  (`.../httpapi/middleware/authorization.ts:14,80`).
- **Second credential channel: `?auth_token=<base64("user:pass")>` query parameter**
  (`AUTH_TOKEN_QUERY = "auth_token"`, `.../httpapi/middleware/authorization.ts:12,77-83`;
  identically `packages/server/src/middleware/authorization.ts:9,29-36`). This puts a reusable,
  non-expiring credential in URLs — logs, referrers, history. Directly contrary to the plan's
  "never place bearer credentials in query strings, referrers, localStorage or logs".
- Auth bypasses:
  - `PUBLIC_UI_PATHS`: `GET /site.webmanifest`, `/web-app-manifest-192x192.png`,
    `/web-app-manifest-512x512.png` (`packages/opencode/src/server/shared/public-ui.ts:4-12`,
    used at `.../middleware/authorization.ts:110`).
  - PTY WebSocket connect carrying `?ticket=` (`shared/pty-ticket.ts:13-15` →
    `.../middleware/authorization.ts:143`; `packages/server/src/middleware/authorization.ts:48`).
- **No cookies, no token-issuance endpoint, no CSRF token, no session binding.** Basic
  credentials are the only durable authority the server understands.

### CORS / origin

`packages/server/src/cors.ts:11-20` — `isAllowedCorsOrigin` returns true for:

- **an absent `Origin` header** (line 12) — non-browser callers are never origin-checked;
- **any** `http://localhost:<port>` (13) and **any** `http://127.0.0.1:<port>` (14);
- `oc://renderer` (15); `tauri://localhost`, `http://tauri.localhost`, `https://tauri.localhost` (16-17);
- **any** `https://*.opencode.ai` — `/^https:\/\/([a-z0-9-]+\.)*opencode\.ai$/` (3, 18);
- anything in `--cors` / `config.server.cors` (19).

`isAllowedRequestOrigin` (22-26) additionally accepts same-host. Applied globally with
`maxAge: 86_400` at `.../httpapi/server.ts:121-128`.

**Consequence:** OpenCode's own origin checking is not a boundary a tracon gateway may lean on.
Every localhost port and every `*.opencode.ai` subdomain is a trusted origin, and an
`Origin`-less request always passes.

### mDNS / discovery

- Off by default (`network.ts:17-21`). Publishes via `bonjour-service`
  (`packages/opencode/src/server/mdns.ts:6-34`) as service type `http`, name `opencode-<port>`,
  host `opencode.local` (or `--mdns-domain`), `txt { path: "/" }`.
- **Disable:** omit `--mdns` and leave `server.mdns` unset in global config (the default).
- **Second guard:** `setupMdns` (`server.ts:155-170`) refuses to publish when the hostname is
  `127.0.0.1`, `localhost`, or `::1`, logging `"mDNS enabled but hostname is loopback; skipping
  mDNS publish"`. Loopback binding makes mDNS unreachable regardless. **OK.**

### Self-update, telemetry, startup egress

| Behaviour | Verdict | Evidence |
|---|---|---|
| Auto-update on launch | **Not reached by `serve`.** `upgrade()` has exactly one call site, the TUI worker. | `packages/opencode/src/cli/upgrade.ts:8`; sole caller `packages/opencode/src/cli/tui/worker.ts:61` |
| Belt-and-braces disable | `OPENCODE_DISABLE_AUTOUPDATE=1` (or `autoupdate:false`) | `cli/upgrade.ts:10`, `packages/core/src/flag/flag.ts:23` |
| **models.dev catalog fetch** | **YES — one immediate fetch at startup, then every 60 min**, because `ModelsDev.node` is in the server service graph (`.../httpapi/server.ts:218`). Disable with `OPENCODE_DISABLE_MODELS_FETCH=true`. Override the URL with `OPENCODE_MODELS_URL` / pre-seed with `OPENCODE_MODELS_PATH`. | `packages/core/src/models-dev.ts:255-258`, `:222`; `packages/core/src/flag/flag.ts:29,45-46` |
| OTLP logs / traces | **Opt-in only** — nothing is exported unless `OTEL_EXPORTER_OTLP_ENDPOINT` is set | `packages/core/src/observability/otlp.ts:7,51,56` |
| Product analytics / crash reporting (server) | none found | — |
| Session share upload | only via `POST /session/:id/share`; posts to `https://opncd.ai` (or `enterprise.url`); refuses when config `share: "disabled"` | `packages/opencode/src/share/share-next.ts:210`; `packages/opencode/src/share/session.ts:28` |
| Update-check endpoints used by `POST /global/upgrade` | `https://opencode.ai/install`, `https://api.github.com/repos/anomalyco/opencode/releases/latest`, brew/chocolatey/scoop | `packages/opencode/src/installation/index.ts:147,219,240,250,258` |
| **UI fallback proxy to `https://app.opencode.ai`** | the one genuinely dangerous egress — see §6 | `packages/opencode/src/server/shared/ui.ts:9,40-42,88-93` |

Net: with `OPENCODE_DISABLE_MODELS_FETCH=true`, no `OTEL_*`, `share:"disabled"`, and the
embedded UI present, `opencode serve` makes **no outbound call at startup**. Provider inference
traffic is of course separate.

---

## 2. API / route / event matrix

### Mount trees and their middleware (`.../httpapi/server.ts:141-203`)

| Tree | Contents | Middleware |
|---|---|---|
| `rootApiRoutes` (141-145) | `ControlApi`, `ControlPlaneApi`, `GlobalApi` | Authorization **only** — *no* instance context, *no* workspace routing |
| `eventApiRoutes` (146-149) | `GET /event` (SSE) | Authorization + WorkspaceRouting + InstanceContext |
| `ptyConnectApiRoutes` (150-153) | `GET /pty/:ptyID/connect` (WS) | **PtyConnect**Authorization (ticket-bypassable) + WorkspaceRouting + InstanceContext |
| `instanceRoutes` (154-176) | config, experimental, file, instance, mcp, project, project-copy, pty, question, permission, provider, session, sync, tui, workspace | Authorization + WorkspaceRouting + InstanceContext |
| `serverRoutes` (177-181) | the whole `/api/*` v2 surface | Authorization + Location / SessionLocation |
| `docRoute` (190-192) | `GET /doc` | router-level auth |
| `uiRoute` (194-203) | `* /*` catch-all | router-level auth, with the `PUBLIC_UI_PATHS` bypass |

### Scope resolution — and the implicit-authorization flag

**v1 tree** — `.../httpapi/middleware/workspace-routing.ts:86-88`:

```ts
function defaultDirectory(request, url): string {
  return url.searchParams.get("directory") || request.headers["x-opencode-directory"] || process.cwd()
}
```

then `.../middleware/instance-context.ts:29`: `store.load({ directory: decode(route.directory) })`.

**v2 `/api/*` tree** — `packages/server/src/location.ts:29-39`:

```ts
const workspaceID = query.get("location[workspace]") || request.headers["x-opencode-workspace"]
const directory   = query.get("location[directory]")
  || (request.headers["x-opencode-directory"] ? decode(...) : process.cwd())
```

Session-scoped routes are safer: the v1 router extracts `sessionID` from
`/session/:id/...` and `/experimental/session/:id/background`
(`shared/workspace-routing.ts:20-29`) and the session's **stored** directory/workspace override
the query (`middleware/workspace-routing.ts:182`); the v2 middleware reads the directory from
the session's DB row (`packages/server/src/middleware/session-location.ts:42-63`).

> ### FLAG — `directory` is an implicit authorization
>
> A `?directory=` query param, a `?location[directory]=` query param, or an
> `x-opencode-directory` header on **any** authenticated request selects *and instantiates*
> an arbitrary host directory as an OpenCode project. There is no allowlist, no containment
> check, no per-directory credential, no audit distinction. **One Basic credential = full
> authority over every path the server process can reach.** Additional body-borne directory
> selectors: `POST /sync/replay` (`directory` in the body),
> `POST /experimental/control-plane/move-session` (target dir in the body),
> `DELETE /experimental/worktree` (`directory` in the body),
> `POST /api/session` (`location` in the payload, `packages/protocol/src/groups/session.ts:129-135`).
> A tracon gateway must **rewrite and pin the directory on every request and reject any
> caller-supplied value**, including in bodies — observing it is not enough.

Secondary selector `?workspace=<wrk_…>` / `x-opencode-workspace`: when it resolves to a
**remote** workspace, the middleware **proxies the entire request — WebSocket upgrades
included — to that workspace's URL** (`middleware/workspace-routing.ts:113-146`, WS at `:130`),
stripping `directory`/`workspace` (`shared/workspace-routing.ts:31-45`). That is an outbound
request primitive reachable from an authenticated HTTP call. Classify **forbidden** unless
workspaces are disabled (`OPENCODE_EXPERIMENTAL_WORKSPACES`, `OPENCODE_WORKSPACE_ID`;
`packages/core/src/flag/flag.ts:49-50`).

### v1 route matrix

Class: **R** readable · **M** mediated (needs a tracon policy decision) · **F** forbidden.
Scope: `dir` = `?directory`/`x-opencode-directory` · `sess` = session-row derived ·
`body` = a directory/target in the request body · `—` = global, no scoping middleware.
`Q` = the `?directory=&workspace=` pair. Declaring paths are relative to
`packages/opencode/src/server/routes/instance/httpapi/`.

| M | Path | Class | Scope | Declared | Handler |
|---|---|---|---|---|---|
| GET | `/config` | R | dir | groups/config.ts:16 | handlers/config.ts:14 |
| PATCH | `/config` | **F** config write (= plugin-install vector) | dir | groups/config.ts:26 | handlers/config.ts:18 |
| GET | `/config/providers` | R | dir | groups/config.ts:38 | handlers/config.ts:24 |
| PUT | `/auth/:providerID` | **F** provider credential write | — | groups/control.ts:39 | handlers/control.ts:13 |
| DELETE | `/auth/:providerID` | **F** provider credential delete | — | groups/control.ts:51 | handlers/control.ts:21 |
| POST | `/log` | M | — (`directory`/`workspace` accepted but inert) | groups/control.ts:62 | handlers/control.ts:28 |
| POST | `/experimental/control-plane/move-session` | **F** | body | groups/control-plane.ts:22 | handlers/control-plane.ts:12 |
| GET | `/global/health` | R (`{healthy:true, version}`) | — | groups/global.ts:79 | handlers/global.ts:66 |
| GET | `/global/event` | R — **SSE, unscoped firehose across every instance** | — | groups/global.ts:88 | handlers/global.ts:70,120 |
| GET | `/global/config` | R | — | groups/global.ts:97 | handlers/global.ts:74 |
| PATCH | `/global/config` | **F** global config write (also disposes every instance) | — | groups/global.ts:106 | handlers/global.ts:78 |
| POST | `/global/dispose` | M | — | groups/global.ts:117 | handlers/global.ts:84 |
| POST | `/global/upgrade` | **F** self-update (installs a new binary) | — | groups/global.ts:126 | handlers/global.ts:89 |
| GET | `/event` | R — **SSE**, filtered to instance directory | dir+ws | groups/event.ts:14 | handlers/event.ts:92 (body :25) |
| GET | `/experimental/capabilities` | R | dir | groups/experimental.ts:108 | handlers/experimental.ts:39 |
| GET | `/experimental/console` | **F** account metadata | dir | groups/experimental.ts:118 | handlers/experimental.ts:43 |
| GET | `/experimental/console/orgs` | **F** enumerates logged-in accounts | dir | groups/experimental.ts:129 | handlers/experimental.ts:60 |
| POST | `/experimental/console/switch` | **F** persists active account/org | dir | groups/experimental.ts:140 | handlers/experimental.ts:85 |
| GET | `/experimental/tool`, `/experimental/tool/ids` | R | dir | groups/experimental.ts:152,164 | handlers/experimental.ts:94,107 |
| GET | `/experimental/worktree` | R | dir | groups/experimental.ts:176 | handlers/experimental.ts:111 |
| POST | `/experimental/worktree` | **M** — runs configured startup scripts (command exec) | dir | groups/experimental.ts:187 | handlers/experimental.ts:116 |
| DELETE | `/experimental/worktree` | **M** — `directory` in body; deletes branch | body | groups/experimental.ts:200 | handlers/experimental.ts:122 |
| POST | `/experimental/worktree/reset` | **M** git reset | dir | groups/experimental.ts:212 | handlers/experimental.ts:131 |
| GET | `/experimental/session` | R (cross-project list) | dir/global | groups/experimental.ts:224 | handlers/experimental.ts:138 |
| POST | `/experimental/session/:sessionID/background` | **M** subagent/background run | sess | groups/experimental.ts:235 | handlers/experimental.ts:159 |
| GET | `/experimental/resource` | R (MCP resources) | dir | groups/experimental.ts:248 | handlers/experimental.ts:174 |
| GET | `/find` | R (ripgrep over tree) | dir | groups/file.ts:108 | handlers/file.ts:27 |
| GET | `/find/file` | R | dir | groups/file.ts:118 | handlers/file.ts:43 |
| GET | `/find/symbol` | R (LSP) | dir | groups/file.ts:128 | handlers/file.ts:62 |
| GET | `/file` | R — `?path=` arbitrary | dir | groups/file.ts:138 | handlers/file.ts:66 |
| GET | `/file/content` | R — `?path=` arbitrary read | dir | groups/file.ts:148 | handlers/file.ts:96 |
| GET | `/file/status` | R | dir | groups/file.ts:158 | handlers/file.ts:127 |
| POST | `/instance/dispose` | M | dir | groups/instance.ts:62 | handlers/instance.ts:24 |
| GET | `/path` | R — leaks home/state/config/worktree paths | dir | groups/instance.ts:72 | handlers/instance.ts:29 |
| GET | `/vcs`, `/vcs/status`, `/vcs/diff`, `/vcs/diff/raw` | R (`/vcs/diff/raw` ≈ bulk export) | dir | groups/instance.ts:83,94,104,114 | handlers/instance.ts:40,47,51,57 |
| POST | `/vcs/apply` | **M** writes the working tree | dir | groups/instance.ts:127 | handlers/instance.ts:61 |
| GET | `/command`, `/agent`, `/skill`, `/lsp`, `/formatter` | R | dir | groups/instance.ts:139,149,159,169,179 | handlers/instance.ts:76,80,84,88,92 |
| GET | `/mcp` | R | dir | groups/mcp.ts:45 | handlers/mcp.ts:12 |
| POST | `/mcp` | **F** registers an MCP server from the body (stdio ⇒ arbitrary command+args persisted to config) | dir | groups/mcp.ts:55 | handlers/mcp.ts:16 |
| POST | `/mcp/:name/auth` | **F** MCP OAuth start | dir | groups/mcp.ts:67 | handlers/mcp.ts:23 |
| POST | `/mcp/:name/auth/callback` | **F** stores MCP OAuth creds | dir | groups/mcp.ts:79 | handlers/mcp.ts:36 |
| POST | `/mcp/:name/auth/authenticate` | **F** opens a browser, waits for callback | dir | groups/mcp.ts:93 | handlers/mcp.ts:51 |
| DELETE | `/mcp/:name/auth` | **F** | dir | groups/mcp.ts:105 | handlers/mcp.ts:64 |
| POST | `/mcp/:name/connect` | **F** spawns a local stdio MCP process | dir | groups/mcp.ts:117 | handlers/mcp.ts:75 |
| POST | `/mcp/:name/disconnect` | M | dir | groups/mcp.ts:128 | handlers/mcp.ts:88 |
| GET | `/permission` | R | dir | groups/permission.ts:21 | handlers/permission.ts:12 |
| POST | `/permission/:requestID/reply` | **M** approves tool execution | dir | groups/permission.ts:31 | handlers/permission.ts:16 |
| GET | `/project`, `/project/current`, `/project/:id/directories` | R | dir | groups/project.ts:22,32,65 | handlers/project.ts:15,19,52 |
| POST | `/project/git/init` | **M** runs `git init` in the resolved dir | dir | groups/project.ts:42 | handlers/project.ts:23 |
| PATCH | `/project/:projectID` | M | dir | groups/project.ts:52 | handlers/project.ts:36 |
| POST | `/experimental/project/:projectID/copy/generate-name` | M (invokes an LLM) | dir | groups/project-copy.ts:15 | handlers/project-copy.ts:25,62 |
| GET | `/provider` | R | dir | groups/provider.ts:38 | handlers/provider.ts:42 |
| GET | `/provider/auth` | **F** lists provider auth methods/state | dir | groups/provider.ts:48 | handlers/provider.ts:64 |
| POST | `/provider/:providerID/oauth/authorize` | **F** provider login | dir | groups/provider.ts:58 | handlers/provider.ts:68,81 |
| POST | `/provider/:providerID/oauth/callback` | **F** writes provider credentials | dir | groups/provider.ts:71 | handlers/provider.ts:96 |
| GET | `/pty/shells` | R | dir | groups/pty.ts:44 | handlers/pty.ts:60 |
| GET | `/pty` | R | dir | groups/pty.ts:54 | handlers/pty.ts:64 |
| POST | `/pty` | **F unless capability-gated** — spawns a process, arbitrary `command`/`args`/`cwd`/`env` | dir | groups/pty.ts:64 | handlers/pty.ts:69 |
| GET/PUT/DELETE | `/pty/:ptyID` | M | dir | groups/pty.ts:76,88,101 | handlers/pty.ts:84,105,129 |
| POST | `/pty/:ptyID/connect-token` | **M** mints an auth-bypass ticket | dir | groups/pty.ts:113 | handlers/pty.ts:144 |
| GET | `/pty/:ptyID/connect` | **M** — **WebSocket**, full terminal I/O; ticket bypasses Basic auth | dir | groups/pty.ts:144 (`PtyConnectApi`) | handlers/pty.ts:181 |
| GET | `/question` | R | dir | groups/question.ts:22 | handlers/question.ts:12 |
| POST | `/question/:requestID/reply` \| `/reject` | **M** | dir | groups/question.ts:32,45 | handlers/question.ts:16,38 |
| GET | `/session` | R | dir (forced local, `shared/workspace-routing.ts:5-9`) | groups/session.ts:111 | handlers/session.ts:64 |
| GET | `/session/status` | R | dir (forced **forward** to remote workspace) | groups/session.ts:121 | handlers/session.ts:77 |
| GET | `/session/:sessionID` | R | sess | groups/session.ts:132 | handlers/session.ts:85 |
| GET | `/session/:sessionID/children` | R (lineage) | sess | groups/session.ts:144 | handlers/session.ts:89 |
| GET | `/session/:sessionID/todo` | R | sess | groups/session.ts:156 | handlers/session.ts:94 |
| GET | `/session/:sessionID/diff` | R | sess | groups/session.ts:168 | handlers/session.ts:99 |
| GET | `/session/:sessionID/message` | R | sess | groups/session.ts:179 | handlers/session.ts:106 |
| GET | `/session/:sessionID/message/:messageID` | R | sess | groups/session.ts:191 | handlers/session.ts:147 |
| POST | `/session` | **M** create/lineage | dir | groups/session.ts:203 | handlers/session.ts:159,422 |
| DELETE | `/session/:sessionID` | **M** | sess | groups/session.ts:215 | handlers/session.ts:178 |
| PATCH | `/session/:sessionID` | **F/M** — body can **rewrite the session's permission ruleset** | sess | groups/session.ts:227 | handlers/session.ts:183 |
| POST | `/session/:sessionID/fork` | **M** lineage | sess | groups/session.ts:240 | handlers/session.ts:218,425 |
| POST | `/session/:sessionID/abort` | **M** | sess | groups/session.ts:253 | handlers/session.ts:232 |
| POST | `/session/:sessionID/init` | **M** writes AGENTS.md | sess | groups/session.ts:265 | handlers/session.ts:237 |
| POST | `/session/:sessionID/share` | **F** share/publish | sess | groups/session.ts:279 | handlers/session.ts:259 |
| DELETE | `/session/:sessionID/share` | **F** | sess | groups/session.ts:291 | handlers/session.ts:265 |
| POST | `/session/:sessionID/summarize` | M | sess | groups/session.ts:303 | handlers/session.ts:273 |
| POST | `/session/:sessionID/message` (prompt) | **M** — runs the agent ⇒ tool/command execution | sess | groups/session.ts:316 | handlers/session.ts:295 |
| POST | `/session/:sessionID/prompt_async` | **M** fire-and-forget | sess | groups/session.ts:329 | handlers/session.ts:311 |
| POST | `/session/:sessionID/command` | **M** | sess | groups/session.ts:343 | handlers/session.ts:331 |
| POST | `/session/:sessionID/shell` | **M** executes a shell command | sess | groups/session.ts:356 | handlers/session.ts:341 |
| POST | `/session/:sessionID/revert` | **M** mutates files | sess | groups/session.ts:369 | handlers/session.ts:349 |
| POST | `/session/:sessionID/unrevert` | **M** | sess | groups/session.ts:383 | handlers/session.ts:357 |
| POST | `/session/:sessionID/permissions/:permissionID` | **M** (deprecated) approves tool execution | sess | groups/session.ts:395 | handlers/session.ts:362 |
| DELETE | `/session/:sessionID/message/:messageID` | **M** history edit | sess | groups/session.ts:409 | handlers/session.ts:380 |
| DELETE | `/session/:sessionID/message/:messageID/part/:partID` | **M** | sess | groups/session.ts:422 | handlers/session.ts:389 |
| PATCH | `/session/:sessionID/message/:messageID/part/:partID` | **M** | sess | groups/session.ts:433 | handlers/session.ts:397 |
| POST | `/sync/start` | **F** | dir | groups/sync.ts:49 | handlers/sync.ts:27 |
| POST | `/sync/replay` | **F** — injects raw `{aggregateID,seq,type,data}` events; `directory` in body | body | groups/sync.ts:59 | handlers/sync.ts:34 |
| POST | `/sync/steal` | **F** | dir+body | groups/sync.ts:71 | handlers/sync.ts:61 |
| POST | `/sync/history` | **F** | dir | groups/sync.ts:83 | handlers/sync.ts:72 |
| POST | `/tui/append-prompt` | M | dir | groups/tui.ts:56 | handlers/tui.ts:34 |
| POST | `/tui/open-help` \| `open-sessions` \| `open-themes` \| `open-models` | M | dir | groups/tui.ts:68,78,88,98 | handlers/tui.ts:41,46,51,56 |
| POST | `/tui/submit-prompt` | **M** submits the queued prompt ⇒ agent run | dir | groups/tui.ts:108 | handlers/tui.ts:61 |
| POST | `/tui/clear-prompt` | M | dir | groups/tui.ts:118 | handlers/tui.ts:66 |
| POST | `/tui/execute-command` | **F** drives an arbitrary TUI command (aliases at handlers/tui.ts:11) | dir | groups/tui.ts:128 | handlers/tui.ts:71 |
| POST | `/tui/show-toast` | M | dir | groups/tui.ts:140 | handlers/tui.ts:79 |
| POST | `/tui/publish` | M | dir | groups/tui.ts:151 | handlers/tui.ts:86 |
| POST | `/tui/select-session` | M | dir+body | groups/tui.ts:163 | handlers/tui.ts:98 |
| GET | `/tui/control/next` | M (long-poll queue read) | dir | groups/tui.ts:175 | handlers/tui.ts:107 |
| POST | `/tui/control/response` | **F** — body is `Schema.Unknown`, unvalidated | dir | groups/tui.ts:185 | handlers/tui.ts:111 |
| GET | `/experimental/workspace/adapter`, `/experimental/workspace`, `/experimental/workspace/status` | **F** (workspaces off) | dir (forced local) | groups/workspace.ts:53,63,95 | handlers/workspace.ts:16,21,55 |
| POST | `/experimental/workspace` | **F** provisions a workspace/sandbox | dir (local) | groups/workspace.ts:73 | handlers/workspace.ts:25 |
| POST | `/experimental/workspace/sync-list` | **F** | dir (local) | groups/workspace.ts:85 | handlers/workspace.ts:51 |
| DELETE | `/experimental/workspace/:id` | **F** | dir (local) | groups/workspace.ts:105 | handlers/workspace.ts:60 |
| POST | `/experimental/workspace/warp` | **F** moves a session + working-tree changes across workspaces | body | groups/workspace.ts:117 | handlers/workspace.ts:64 |
| GET | `/doc` | R (OpenAPI JSON of the whole surface) | — | `.../server.ts:190` | — |
| `*` | `/*` | UI catch-all — §6 | — | `.../server.ts:194-203` | `shared/ui.ts` |

### v2 `/api/*` route matrix

Declared in `packages/protocol/src/groups/`, handled in `packages/server/src/handlers/`.

| M | Path | Class | Scope | Declared | Handler |
|---|---|---|---|---|---|
| GET | `/api/health` | R | — | health.ts:5 | health.ts:6 |
| GET | `/api/location` | R | loc | location.ts:30 | location.ts:7 |
| GET | `/api/agent` | R | loc | agent.ts:8 | agent.ts:8 |
| GET | `/api/command` | R | loc | command.ts:9 | command.ts:7 |
| GET | `/api/skill` | R | loc | skill.ts:9 | skill.ts:7 |
| GET | `/api/reference` | R | loc | reference.ts:9 | reference.ts:7 |
| GET | `/api/model` | R | loc | model.ts:10 | model.ts:9 |
| GET | `/api/provider`, `/api/provider/:providerID` | R | loc | provider.ts:10,25 | provider.ts:11,18 |
| PATCH | `/api/credential/:credentialID` | **F** credential write | loc | credential.ts:8 | credential.ts:8 |
| DELETE | `/api/credential/:credentialID` | **F** | loc | credential.ts:24 | credential.ts:15 |
| GET | `/api/integration`, `/api/integration/:id` | **F** provider auth state | loc | integration.ts:12,26 | integration.ts:22,29 |
| POST | `/api/integration/:id/connect/key` | **F** provider auth (API key) | loc | integration.ts:41 | integration.ts:36 |
| POST | `/api/integration/:id/connect/oauth` | **F** provider auth (OAuth) | loc | integration.ts:61 | integration.ts:50 |
| GET | `/api/integration/attempt/:attemptID` | **F** | loc | integration.ts:82 | integration.ts:66 |
| POST | `/api/integration/attempt/:attemptID/complete` | **F** | loc | integration.ts:97 | integration.ts:73 |
| DELETE | `/api/integration/attempt/:attemptID` | **F** | loc | integration.ts:114 | integration.ts:95 |
| GET | `/api/fs/read/*` | R — path is a wildcard segment | loc | fs.ts:22 | fs.ts:12 (raw) |
| GET | `/api/fs/list`, `/api/fs/find` | R | loc | fs.ts:36,50 | fs.ts:22,30 |
| GET | `/api/event` | R — **SSE**, no replay | server-wide | event.ts:35 | event.ts:23 (raw) |
| GET | `/api/session` | R (cursor-paged) | `?directory`/`?project`/`?workspace` | session.ts:109 | session.ts:24 |
| POST | `/api/session` | **M** — payload carries caller-chosen `id` *and* `location` | body | session.ts:129 | session.ts:67 |
| GET | `/api/session/active` | R | — | session.ts:146 | session.ts:80 |
| GET | `/api/session/:sessionID` | R | sess | session.ts:158 | session.ts:90 |
| POST | `/api/session/:sessionID/agent` | **M** settings change | sess | session.ts:173 | session.ts:107 |
| POST | `/api/session/:sessionID/model` | **M** settings change | sess | session.ts:189 | session.ts:123 |
| POST | `/api/session/:sessionID/prompt` | **M** | sess | session.ts:205 | session.ts:139 |
| POST | `/api/session/:sessionID/compact` | **M** | sess | session.ts:226 | session.ts:172 |
| POST | `/api/session/:sessionID/wait` | R/M | sess | session.ts:241 | session.ts:196 |
| POST | `/api/session/:sessionID/revert/stage` \| `/clear` \| `/commit` | **M** workspace mutation | sess | session.ts:256,272,281 | session.ts:220,259,288 |
| GET | `/api/session/:sessionID/context` | R | sess | session.ts:292 | session.ts:304 |
| GET | `/api/session/:sessionID/history?after=&limit=` | **R — durable paged replay** (limit ≤ 100, session.ts:87) | sess | session.ts:307 | session.ts:332 |
| GET | `/api/session/:sessionID/event?after=<seq>` | **R — SSE with durable replay then live** | sess | session.ts:327 | session.ts:357 |
| POST | `/api/session/:sessionID/interrupt` | **M** | sess | session.ts:345 | session.ts:365 |
| GET | `/api/session/:sessionID/message` | R | sess | message.ts:26 | message.ts:31 |
| GET | `/api/session/:sessionID/message/:messageID` | R | sess | session.ts:360 | session.ts:372 |
| GET | `/api/permission/request` | R | loc | permission.ts:23 | permission.ts:17 |
| GET | `/api/permission/saved` | R — **the persisted "always" grants** | loc | permission.ts:37 | permission.ts:23 |
| DELETE | `/api/permission/saved/:id` | **M** revokes a persisted grant | loc | permission.ts:49 | permission.ts:52 |
| POST | `/api/session/:sessionID/permission` | **M** creates a permission request | sess | permission.ts:63 | permission.ts:59 |
| GET | `/api/session/:sessionID/permission`, `/permission/:requestID` | R | sess | permission.ts:89,104 | permission.ts:67,79 |
| POST | `/api/session/:sessionID/permission/:requestID/reply` | **M** | sess | permission.ts:119 | permission.ts:90 |
| GET | `/api/question/request` | R | loc | question.ts:20 | question.ts:26 |
| GET | `/api/session/:sessionID/question` | R | sess | question.ts:37 | question.ts:32 |
| POST | `/api/session/:sessionID/question/:requestID/reply` \| `/reject` | **M** | sess | question.ts:52,68 | question.ts:39,50 |
| GET | `/api/pty` | R | loc | pty.ts:23 | pty.ts:32 |
| POST | `/api/pty` | **F unless capability-gated** — spawns a process | loc | pty.ts:37 | pty.ts:38 |
| GET/PUT/DELETE | `/api/pty/:ptyID` | M | loc | pty.ts:52,68,85 | pty.ts:57,75,98 |
| POST | `/api/pty/:ptyID/connect-token` | **M** | loc | pty.ts:101 | pty.ts:115 |
| GET | `/api/pty/:ptyID/connect` | **M** — **WebSocket** | loc | pty.ts:119 | pty.ts:140 (raw) |
| POST/DELETE | `/experimental/project/:projectID/copy`, POST `.../copy/refresh` | **M** workspace mutation | loc | project-copy.ts:25,36,47 | project-copy.ts:12,25,32 |

> **Note.** `ProjectCopyGroup`'s `root` is `/experimental/project/:projectID/copy`
> (`packages/protocol/src/groups/project-copy.ts:7`) — **without** the `/api` prefix every other
> v2 group uses. It therefore shares a namespace with the legacy
> `POST /experimental/project/:projectID/copy/generate-name`. Confirm against the live `/doc`
> output when pinning the matrix; a fail-closed gateway should treat this prefix specially.

### Streaming endpoints

Only two transports. There is **no general-purpose WebSocket** — `OPENCODE_EXPERIMENTAL_WEBSOCKETS`
exists (`packages/opencode/src/effect/runtime-flags.ts:55`) but no served route consumes it.

| Endpoint | Kind | Replay |
|---|---|---|
| `GET /event` | SSE | none |
| `GET /global/event` | SSE, unscoped | none |
| `GET /api/event` | SSE | none |
| `GET /api/session/:sessionID/event?after=` | SSE | **durable, by aggregate seq** |
| `GET /pty/:ptyID/connect`, `GET /api/pty/:ptyID/connect` | WebSocket | `?cursor=` output replay |

### Event type inventory

From `define({ type: … })` across `packages/schema/src/*.ts`, assembled by
`packages/schema/src/event-manifest.ts:57-80`:

`server.connected`, `server.heartbeat` (synthesized, `handlers/event.ts:63-66`),
`server.instance.disposed`, `global.disposed`,
`session.created`, `session.updated`, `session.deleted`, `session.idle`, `session.error`,
`session.diff`, `session.status`, `session.compacted`,
`message.updated`, `message.removed`, `message.part.updated`, `message.part.delta`,
`message.part.removed`,
`permission.asked`, `permission.replied`, `permission.v2.asked`, `permission.v2.replied`,
`question.asked`/`replied`/`rejected` and `question.v2.asked`/`replied`/`rejected`,
`todo.updated`, `file.edited`, `file.watcher.updated`,
`pty.created`/`updated`/`exited`/`deleted`, `lsp.updated`,
`mcp.tools.changed`, `mcp.browser.open.failed`, `plugin.added`,
`project.updated`, `project.directories.updated`, `vcs.branch.updated`,
`workspace.ready`/`failed`/`status`, `worktree.ready`/`failed`,
`installation.update-available`, `installation.updated`, `ide.installed`,
`models-dev.refreshed`, `catalog.updated`, `integration.updated`,
`integration.connection.updated`, `reference.updated`, `command.executed`,
`tui.command.execute`, `tui.prompt.append`, `tui.session.select`, `tui.toast.show`,
plus the **v2 durable session stream** `session.next.{prompted, prompt.admitted, step.started,
step.ended, step.failed, text.started, text.delta, text.ended, reasoning.started,
reasoning.delta, reasoning.ended, tool.input.started, tool.input.delta, tool.input.ended,
tool.called, tool.progress, tool.success, tool.failed, shell.started, shell.ended,
agent.switched, model.switched, context.updated, compaction.started, compaction.delta,
compaction.ended, revert.staged, revert.cleared, revert.committed, retried, moved, synthetic}`.

---

## 3. Session identity and lineage

### ID shapes

All IDs are `<prefix>_<26 chars>`: 12 hex chars of millisecond timestamp (+ a per-ms counter,
bit-inverted for descending) followed by 14 random base62 chars —
`packages/schema/src/identifier.ts:14-30`.

| Entity | Prefix | Order | Declared |
|---|---|---|---|
| Session | `ses_` | **descending** | `packages/schema/src/session-id.ts:5-14` |
| Message (v1) | `msg_` | ascending | `packages/schema/src/v1/session.ts:17-20` |
| Message (v2) | `msg_` | — | `packages/schema/src/session-message.ts` |
| Part | `prt_` | ascending | `packages/schema/src/v1/session.ts:23-26` |
| Permission | `per_` | ascending | `packages/schema/src/v1/permission.ts:10-13`; `packages/schema/src/permission.ts` |
| Question | `que_` | — | `packages/schema/src/question.ts` |
| PTY | `pty_` | — | `packages/schema/src/pty.ts` |
| Event | `evt_` | — | `packages/schema/src/event.ts` |
| Workspace | `wrk_` | — | `packages/schema/src/workspace-id.ts` |

**Tool calls have no OpenCode-owned ID namespace.** A tool call is `{ messageID, callID }`
where `callID` is a **provider-supplied string** (`packages/schema/src/v1/permission.ts:34`).
Treat it as untrusted text, never as a tracon identity — map it, do not adopt it.

### Lineage

- `SessionInfo.parentID` (v1) — `packages/schema/src/v1/session.ts:550`;
  `Session.Info.parentID` (v2) — `packages/schema/src/session.ts:21`.
- Children: `GET /session/:sessionID/children` (`groups/session.ts:144`).
- Fork: `POST /session/:sessionID/fork` (`groups/session.ts:240`).
- Subagent / background: `POST /experimental/session/:sessionID/background`
  (`groups/experimental.ts:235`). The v1 router recognises this path
  (`shared/workspace-routing.ts:20-29`), so a background subagent inherits the **parent
  session's** directory, not the caller's — useful for tracon lineage.
- **`POST /api/session` accepts a caller-supplied `id`**
  (`packages/protocol/src/groups/session.ts:131`). Good for idempotent creation by the tracon
  controller; also means the native UI can choose IDs — pin it at the gateway.
- Events carrying these ids: `session.created/updated/deleted` carry `{ sessionID, info }` with
  `parentID` inside `info` (`packages/schema/src/v1/session.ts:571+`); `message.*` carry
  `sessionID` + `messageID`; `message.part.*` add `partID`; `permission.asked` carries
  `{ id, sessionID, permission, patterns, metadata, always, tool:{messageID,callID} }`
  (`packages/schema/src/v1/permission.ts:27-35,61`); `permission.replied` carries
  `{ sessionID, requestID, reply }` (`:62-65`).

### Replay vs live-only — the reconciliation question

- **`GET /event`, `GET /global/event`, `GET /api/event` have NO durable replay.**
  `handlers/event.ts:12-19` builds every SSE frame with **`id: undefined`** — the SSE `id:`
  field is never emitted, so `Last-Event-ID` cannot work. The stream is a live tap on an
  unbounded in-process queue registered at connect time (`handlers/event.ts:31-33`); anything
  published while disconnected is **lost**. `server.connected` is sent first
  (`handlers/event.ts:70`); a 10 s `server.heartbeat` is the only liveness signal (`:63-66`).
  Events are filtered to the instance directory and workspace (`:34-41`).
  **Do not assume any ordering or delivery guarantee here beyond per-connection FIFO.**
- **Durable replay exists, but only per-session and only on the v2 surface:**
  - `GET /api/session/:sessionID/event?after=<seq>` — *"Replay durable events after an aggregate
    sequence, then continue with new durable events"* (`packages/protocol/src/groups/session.ts:327-343`).
  - `GET /api/session/:sessionID/history?after=&limit=` — finite paged read, `{data, hasMore}`
    (`:307-325`; limit ≤ 100 at `:87`). Its own description warns *"Newly committed events may
    appear on later pages."*
  - Durable events carry `{ aggregateID, seq, version }` (`packages/protocol/src/groups/event.ts:11`).
- **Snapshot endpoints for reconciliation:** `GET /session`, `GET /api/session` (cursor-paged),
  `GET /api/session/:sessionID`, `GET /api/session/:sessionID/context`,
  `GET /session/:sessionID/message`, `GET /api/session/:sessionID/message[/:messageID]`,
  `GET /permission` + `GET /api/permission/request`, `GET /api/permission/saved`,
  `GET /question` + `GET /api/question/request`, `GET /session/status`,
  `GET /api/session/active`, `GET /pty` / `GET /api/pty`, `GET /session/:sessionID/diff`.
- **Closing the snapshot/stream race is achievable only on the v2 durable path**: subscribe
  `/api/session/:id/event?after=<last known seq>` first, then snapshot, then apply. On the v1
  `/event` stream there is no sequence to anchor to — tracon must treat it as best-effort
  notification and drive correctness from the durable per-session stream plus snapshots.

---

## 4. Permissions

Two parallel permission systems live in the same binary.

### v1 — `packages/opencode/src/permission/index.ts`

- Raised from the session processor: `packages/opencode/src/session/processor.ts:372`
  calls `permission.ask({...})`.
- `ask` (`permission/index.ts:67-107`) evaluates the ruleset first (`evaluate`, `:28-38`,
  last-match-wins wildcard over `{permission, pattern}`):
  - `deny` → fails immediately, **no event** (`:76-79`);
  - **`allow` → continues immediately, no request, NO EVENT** (`:80-84`);
  - `ask` → mints `per_…`, publishes `permission.asked` (`:86-100`), and blocks on a Deferred.
- Answered by `POST /permission/:requestID/reply` (`groups/permission.ts:31`,
  `handlers/permission.ts:16`) or the deprecated
  `POST /session/:sessionID/permissions/:permissionID` (`groups/session.ts:395`).
  Payload `{ reply: "once" | "always" | "reject", message?: string }`
  (`packages/schema/src/v1/permission.ts:38-44`).
- **"always" (v1) is in-memory, per-loaded-instance, never persisted.** `reply` pushes
  `{permission, pattern, action:"allow"}` onto `state.approved`
  (`permission/index.ts:145-151`) — an array on the per-instance `InstanceState` built at
  `:46-65` — then auto-resolves every other pending request in the same session that the new
  rule now permits (`:153-166`). Nothing touches disk or the DB. It dies with the instance.
- `reject` **with** a `message` becomes a `CorrectedError` (feedback to the model); `reject`
  **without** one rejects **every other pending request in that session** (`:129-138`).

### v2 — `packages/core/src/permission.ts`

- `assert` (`:197-218`): evaluates agent-configured rules + `savedRules()`;
  `deny` → `BlockedError`; **`allow` → returns at `:206` with no request and no event**;
  `ask` → creates and awaits.
- `reply`: on `"always"` **and** a non-empty `request.save`, it calls
  `saved.add({ projectID, action, resources })` (`:250-256`), which **persists rows to the
  SQLite `PermissionTable`** (`packages/core/src/permission/saved.ts:54-69`;
  `packages/core/src/permission/sql.ts`). Those rows feed every later evaluation for the
  project (`:116,131-135,159`). Visible/removable via `GET /api/permission/saved` and
  `DELETE /api/permission/saved/:id`.
- Events: `permission.v2.asked` / `permission.v2.replied`.

### Can an external controller gate tool execution *before* it runs?

**No — not over HTTP. The only pre-execution hook runs in-process, as a plugin.**

- Pre-execution interception points, both plugin hooks:
  - `"tool.execute.before"(input:{tool, sessionID, callID}, output:{args})` —
    `packages/plugin/src/index.ts:266-270`. Invoked at
    `packages/opencode/src/session/tools.ts:107,176,259,339,403`,
    `packages/opencode/src/session/prompt.ts:308`,
    `packages/opencode/src/tool/code-mode.ts:142`. It can mutate `args` and can throw to abort —
    but it executes **inside the OpenCode process**, i.e. inside the untrusted runner.
  - `"permission.ask"(input: Permission, output:{status:"ask"|"deny"|"allow"})` —
    `packages/plugin/src/index.ts:261`. Also in-process.
- The HTTP permission routes are **notify-then-answer**, and they fire **only** for actions the
  ruleset classifies as `ask`. Anything configured `allow` executes with **no event at all** —
  an external controller is never told, let alone asked.
- Therefore a tracon gate must be built from: (a) a policy that leaves **every** permission at
  `ask` so every action produces `permission.asked` and blocks on an HTTP reply — noting that
  `PATCH /session/:sessionID` can rewrite that ruleset (`groups/session.ts:227`) and so must be
  forbidden; plus (b) a pinned plugin in the runner image as a cooperating (not enforcing)
  hook; plus (c) filesystem/network containment as the actual boundary. This matches the plan's
  own position that "a plugin can subvert OpenCode-local behavior".
- **"Always" grants bypass the external controller by construction.** A v1 `always` persists
  only in RAM but silently widens the in-process ruleset for the rest of the instance's life; a
  v2 `always` writes a permanent DB row scoped to the *project*, surviving restarts. The plan's
  requirement — "do not forward an upstream 'always' grant as authority to broaden policy" — is
  satisfiable only if the gateway **rewrites every `reply: "always"` to `reply: "once"`** and
  records the broadening as a tracon-side decision instead.

---

## 5. PTY / shell

Two equivalent surfaces: v1 `/pty/*` (`groups/pty.ts`, `handlers/pty.ts`) and v2 `/api/pty/*`
(`packages/protocol/src/groups/pty.ts`, `packages/server/src/handlers/pty.ts`).

- **Create:** `POST /pty` with `{ command?, args?, cwd?, title?, env? }`
  (`packages/schema/src/pty.ts:40-46`). `handlers/pty.ts:69-82` defaults `cwd` to the instance
  directory, merges the plugin `shell.env` hook, and calls
  `spawn(command, args, { name: "xterm-256color", cwd, env })`
  (`packages/core/src/pty.ts:182-183`).
  **There is no permission check anywhere in this path** — no reference to `Permission` exists
  in `packages/core/src/pty.ts` or `packages/core/src/pty/*`. An authenticated HTTP caller gets
  arbitrary command execution as the server user, in any directory it names.
  Classify **forbidden unless explicitly capability-gated per workspace**.
- **Connect:** `GET /pty/:ptyID/connect` — WebSocket upgrade (`handlers/pty.ts:181,207`),
  with `?cursor=<int>` for output replay (`:202-206`); the cursor is validated as a safe
  integer ≥ −1.
- **Authorization of the WebSocket.** Browsers cannot set an `Authorization` header on a WS
  upgrade, so OpenCode mints a ticket:
  - `POST /pty/:ptyID/connect-token` requires the header `x-opencode-ticket: 1`
    (`shared/pty-ticket.ts:2-3`) **and** an allowed `Origin` (`handlers/pty.ts:144-150`,
    `validOrigin` at `:28-30`). Returns `{ ticket: <uuid v4>, expires_in: 60 }`
    (`packages/core/src/pty/ticket.ts:9,43-47`).
  - Tickets are held in an in-memory `Cache`, **TTL 60 s, capacity 10 000, single-use**
    (`Cache.invalidateWhen`, `ticket.ts:48-50`), scoped to `{ ptyID, directory, workspaceID }`
    (`ticket.ts:14-18,27-31`; captured at `handlers/pty.ts:32-36`).
  - The connect path **skips Basic auth entirely when `?ticket=` is present**
    (`shared/pty-ticket.ts:13-15` → `.../middleware/authorization.ts:143`), after which the
    handler checks origin and consumes the ticket (`handlers/pty.ts:195-201`).
    When no ticket is present, **`?auth_token=` is still accepted** as the credential.
- This is the closest OpenCode gets to the plan's "short-lived, single-use capability": it is
  single-use, 60 s, and bound to `{ptyID, directory, workspace}` — but **not** bound to an
  owner, an audience, or a specific browser session, and its origin check accepts any
  `http://localhost:*` (`packages/server/src/cors.ts:13`).

---

## 6. Native web UI

### How the server serves the UI

`packages/opencode/src/server/shared/ui.ts`, mounted as the catch-all
`router.add("*", "/*", …)` at `.../httpapi/server.ts:194-203`.

```ts
// shared/ui.ts:44-49
export function embeddedUI(disableEmbeddedWebUi: boolean) {
  if (disableEmbeddedWebUi) return Promise.resolve(null)
  return (embeddedUIPromise ??=
    import("opencode-web-ui.gen.ts").then((m) => m.default as Record<string,string>).catch(() => null))
}
```

```ts
// shared/ui.ts:78-93 (abridged)
const embeddedWebUI = yield* Effect.promise(() => embeddedUI(services.disableEmbeddedWebUi))
if (embeddedWebUI) return yield* serveEmbeddedUIEffect(path, services.fs, embeddedWebUI)
const response = yield* services.client.execute(
  HttpClientRequest.make(request.method)(upstreamURL(path), { … }))   // ← proxies app.opencode.ai
```

- **Embedded-asset path.** `opencode-web-ui.gen.ts` is generated at build time
  (`packages/opencode/script/build.ts:26-48`), injected into the Bun single-file compile via
  `files: { "opencode-web-ui.gen.ts": embeddedFileMap }` and as an extra entrypoint
  (`build.ts:184,190`). It maps published path → a `type: "file"` import, so the assets live in
  the binary's `bunfs`. Lookup: `embeddedWebUI[path.replace(/^\//,"")] ?? embeddedWebUI["index.html"]`
  (`shared/ui.ts:69`) — an SPA fallback to `index.html` for unknown paths.
- **Fallback to `app.opencode.ai`.** `UI_UPSTREAM = new URL("https://app.opencode.ai")`
  (`shared/ui.ts:9`), `upstreamURL()` (`:40-42`).
  **Exact condition:** the fallback runs whenever `embeddedUI()` resolves to `null`, i.e. either
  (a) `OPENCODE_DISABLE_EMBEDDED_WEB_UI` is truthy
  (`packages/opencode/src/effect/runtime-flags.ts:20`), or (b) **the dynamic import throws and
  is swallowed by `.catch(() => null)`** (`shared/ui.ts:48`). It then proxies **the request
  method and body**, not just GETs (`requestBody`, `:24-28`).
  - **There is NO flag that disables the fallback.** `OPENCODE_DISABLE_EMBEDDED_WEB_UI=true`
    *selects* the fallback — it is what `packages/desktop/src/main/index.ts:123` sets, because
    the desktop app ships its own UI. Setting it in a tracon deployment would be exactly
    backwards.
  - **The only ways to prevent it:** (i) verify the embedded bundle is present in the pinned
    artifact so the import never fails, and (ii) deny egress to `app.opencode.ai` at the network
    layer / intercept `/*` at the tracon gateway and serve the pinned bundle itself. A missing
    asset then becomes a hard 404/failure rather than a silent fetch of unpinned upstream
    JavaScript — which is what the plan demands.
- **CSP the server itself emits** (`shared/ui.ts:11-13`):
  ```
  default-src 'self'; script-src 'self' 'wasm-unsafe-eval' ['sha256-<theme-preload>'];
  style-src 'self' 'unsafe-inline'; img-src 'self' data: https: blob:;
  font-src 'self' data:; media-src 'self' data:; connect-src * data: blob:
  ```
  `cspForHtml()` (`:19-22`) computes a sha256 over the inline `id="oc-theme-preload-script"`
  block and adds it to `script-src`. **`connect-src *` is unrestricted** and `img-src` allows
  all `https:` — the plan's "do not inherit an unrestricted connect-src policy" applies
  literally here. tracon must replace this header, not pass it through.
- **Unauthenticated asset paths:** `GET /site.webmanifest`,
  `/web-app-manifest-192x192.png`, `/web-app-manifest-512x512.png`
  (`shared/public-ui.ts:4-12`) bypass Basic auth so the manifest can install.

### `packages/app` — the UI client

**Server URL discovery** — all in `packages/app/src/entry.tsx`:

- `getCurrentUrl()` (`:99-104`), in priority order:
  1. `:100` — if `location.hostname.includes("opencode.ai")` → hardcoded `http://localhost:4096`
     (this is a *read-only hostname check*, not a redirect);
  2. `:101-102` — dev only: `http://${VITE_OPENCODE_SERVER_HOST ?? "localhost"}:${VITE_OPENCODE_SERVER_PORT ?? "4096"}`;
  3. `:103` — production: **`location.origin`**.
- `getDefaultUrl()` (`:106-110`) lets a localStorage override
  (`opencode.settings.dat:defaultServerUrl`, key at `:15`) win for the *default selected* server,
  but the injected server object at `:156-163` always uses `getCurrentUrl()`.
- User-entered servers: `packages/app/src/components/dialog-select-server.tsx`,
  `.../settings-v2/dialog-server-v2.tsx`; normalised by `normalizeServerUrl()`
  (`packages/app/src/context/server.tsx:23-28`).
- **No config is injected into `index.html`** — the only build-time HTML edit is inlining the
  theme preload script (`packages/app/vite.js:41-49`).
- Protocol is **probed at runtime**: `packages/app/src/utils/server-protocol.ts:28-34` tries
  `/global/health` (v1) then `/api/health` (v2).
- **⇒ A same-origin reverse proxy is the intended production configuration.** `location.origin`
  is the default; no CORS, no third-party cookies, no cross-origin anything is required.

**Authentication** — HTTP **Basic**, no cookies:

- `packages/app/src/utils/server.ts:6-8` `authTokenFromCredentials()` → `btoa("user:pass")`;
  `:10-19` `authFromToken()`; `:27-32,50-57` both SDK clients set
  `Authorization: Basic <b64>` when `server.password` is set. Same at
  `packages/app/src/utils/server-health.ts:91-95`, `.../server-protocol.ts:6-11`.
- **Web bootstrap:** `entry.tsx:154` reads `?auth_token=` from the URL, and `:112-117`
  `clearAuthToken()` strips it via `history.replaceState` after reading. The token is a
  **base64 `user:pass`** — a long-lived reusable credential, not a one-shot capability.
- WebSocket: `packages/app/src/utils/terminal-websocket-url.ts:24-33` — a `ticket` param
  (v2, obtained via `pty.connectToken` with `x-opencode-ticket: 1`,
  `packages/app/src/components/terminal.tsx:559-574`), or `auth_token=<basic b64>` for v1 when
  not same-origin.
- **No bearer tokens, no auth cookies, no `credentials:` fetch option anywhere.**

**Storage** (this is where the plan's "never place credentials in localStorage" is violated):

| Store | Key | Content |
|---|---|---|
| localStorage | **`opencode.global.dat:server`** | `StoredServer[]` whose `http` object contains `username` and **`password` in cleartext** — type `packages/app/src/context/server.tsx:185-195`, written `:293`, persisted `:255-267` |
| localStorage | `opencode.settings.dat:defaultServerUrl` | `entry.tsx:15,33-56` |
| localStorage | `opencode.global.dat:language` | `packages/app/src/context/language.tsx:143` |
| localStorage | `opencode-theme-id`, `opencode-color-scheme`, `opencode-theme-css-light`, `opencode-theme-css-dark` | `packages/ui/src/theme/context.tsx:14-19`; also `packages/app/public/oc-theme-preload.js:2,12,27` |
| IndexedDB | DB `opencode-drafts`, stores `documents`, `blobs` | prompt drafts + pasted/attached blobs — `packages/app/src/utils/draft-store.ts:98-101` |
| Cookie | `oc_locale=…; Path=/; Max-Age=31536000; SameSite=Lax` | not a credential, not HttpOnly — `packages/app/src/context/language.tsx:38-40,212` |
| sessionStorage | — | **none** |

Persist key prefixes: `packages/app/src/utils/persist.ts:26-29,383-386,388-432,473`.

**Transport**

- **SSE-style streaming over `fetch` (not `EventSource`)** —
  `packages/app/src/context/server-sdk.tsx:275-290` (`/global/event` v1, `/api/event` v2),
  reconnect loop `:263-311` with `RECONNECT_DELAY_MS = 250` (`:220`), stop/start on
  `pagehide`/`pageshow` (`:325-328`). Because it is `fetch` and not `EventSource`, it **can**
  carry the `Authorization` header — which is why Basic works at all.
- **WebSocket** (terminal only) — `packages/app/src/components/terminal.tsx:620-632`; URL from
  `packages/app/src/utils/terminal-websocket-url.ts:16-34` →
  `ws(s)://<base>/api/pty/<id>/connect` (v2) or `/pty/<id>/connect` (v1), `https:`→`wss:` at `:23`.
- **Polling** — server health every 10 s
  (`packages/app/src/utils/server-health.ts:115`; `packages/app/src/context/server.tsx:15`);
  `entry.tsx:173` passes `disableHealthCheck` for the web build.

**External origins the built app may contact** (CSP-relevant)

| Origin | Why | Evidence |
|---|---|---|
| `https://opencode.ai/changelog.json` | release-notes fetch | `packages/app/src/context/highlights.tsx:10,180` |
| `https://opencode.ai/…` images/videos | changelog `media.src` rendered as `<img>`/`<video>` | `packages/app/src/components/dialog-release-notes.tsx:131,137` |
| `https://opencode.ai/favicon-96x96-v3.png` | Notification API icon | `packages/app/src/entry.tsx:73` |
| Sentry | **only if `VITE_SENTRY_DSN` is set at build time** | `packages/app/src/entry.tsx:133-150`; `packages/app/src/env.d.ts:6-8`; upload plugin `vite.config.ts:5-21` |

**No PostHog, GA, Segment.** **Fonts are self-hosted** — `packages/app/src/index.css:6-17`
loads `/assets/JetBrainsMonoNerdFontMono-Regular.woff2` and `/assets/Inter.ttf` from
`packages/app/public/assets/`; there is **no `fonts.googleapis.com`/`gstatic`**, so
`font-src 'self'` suffices. Shiki highlighting is bundled, no CDN
(`packages/session-ui/src/components/markdown.worker.ts:7-8,144`;
`packages/ui/src/context/marked.tsx:2`). Every `opencode.ai/docs`, `/zen`, `github.com` mention
is an `openExternal`/`<ExternalLink>` new-tab target, not a fetch.

**Minimum workable CSP for the pinned UI** (tighter than what OpenCode emits):

```
default-src 'self';
script-src 'self' 'wasm-unsafe-eval' 'sha256-<oc-theme-preload hash>';
style-src 'self' 'unsafe-inline';
img-src 'self' data: blob:;          # widen only if remote message attachments must render
media-src 'self' data: blob:;
font-src 'self';
worker-src 'self' blob:;
connect-src 'self' wss://<ui-origin>;   # + https://opencode.ai only if release notes are wanted
frame-ancestors <the tracon PWA shell origin, if embedding>;
```

Rationale for each non-obvious directive:
- `'wasm-unsafe-eval'` — the terminal dynamically imports `ghostty-web` and calls
  `Ghostty.load()` (`packages/app/src/components/terminal.tsx:37-38`).
- inline-script hash — `packages/app/vite.js:41-49` replaces
  `<script id="oc-theme-preload-script" src="/oc-theme-preload.js">` (`index.html:22`) with an
  **inline** script at build time. `cspForHtml()` already computes this hash server-side
  (`shared/ui.ts:19-22`); reuse the same computation.
- `style-src 'unsafe-inline'` — theme CSS is injected as a `<style>` element from localStorage
  (`packages/ui/src/theme/context.tsx:118-141`; `oc-theme-preload.js:26-40`).
- `worker-src 'self' blob:` — two same-origin ES-module Web Workers:
  `packages/session-ui/src/components/markdown-worker.ts:120` and
  `packages/session-ui/src/pierre/worker.ts:10`; worker format set at `packages/app/vite.js:32-34`.
- `img-src blob: data:` — `packages/session-ui/src/v2/components/prompt-input/attachments.ts:228`
  (`URL.createObjectURL`), `packages/session-ui/src/pierre/media.ts:48-73`. Arbitrary-origin
  images can appear through the file viewer / message attachments
  (`packages/session-ui/src/components/message-part.tsx:1294`;
  `packages/session-ui/src/components/file-media.tsx:231,261`) — a deliberate trade-off.
- `connect-src` must include the `ws://`/`wss://` form of the UI origin for the terminal.

**Service worker / PWA**

- **No service worker, no workbox** anywhere in `app/src`, `ui/src`, `session-ui/src`.
  Nothing is cached by a SW. (So the plan's "keep SW caching limited to versioned shell assets"
  is trivially satisfied for the OpenCode UI itself, and any caching must come from tracon's own
  shell.)
- **A manifest exists and is root-absolute.** `packages/app/index.html:14` → `/site.webmanifest`
  (a symlink to `packages/ui/src/assets/favicon/site.webmanifest`) with
  `"id": "/"`, `"start_url": "/"`, `"scope": "/"`, `display: standalone`, icons
  `/web-app-manifest-192x192.png`, `/web-app-manifest-512x512.png`.
  **This conflicts with the tracon PWA manifest** (`spa/public/manifest.webmanifest`,
  also `scope: "/"`, `start_url: "/"`) if the two ever share an origin — and, per the plan, they
  must not. On a separate origin the two manifests are independent; the OpenCode UI would
  install as its own app unless the tracon shell embeds it rather than navigating to it.

**Mobile layout** — `packages/app/src/pages/session.tsx` (2391 lines)

| Thing | Line |
|---|---|
| **Breakpoint** `createMediaQuery("(min-width: 768px)")` → `isDesktop` | **448** |
| `mobileTab: "session" \| "changes"` store field | 116 |
| `mobileChanges` = `!isDesktop() && store.mobileTab === "changes"` | 668 |
| `wantsReview` mobile branch | 673 |
| `mobileTabs(compact, bottom)` tab bar | 2017-2049 (`Tabs.Trigger value="session"` 2025-2035; `value="changes"` 2036-2047) |
| `mobileTabsBottom` memo / render sites | 2050-2053 / 2065, 2245, 2259 |
| **Unified diff on mobile** `reviewContent({ diffStyle: "unified", … })` | **2072**, inside `<Match when={params.id && mobileChanges()}>` at 2069 |
| Desktop diff style (`layout.review.diffStyle()`, default `"split"`) | 1317-1318, 1369; split gate 475-476 |

Other 768 px breakpoints: `packages/app/src/components/session/session-header.tsx:167`,
`.../session-context-usage.tsx:55`, `packages/app/src/pages/session/terminal-panel-v2.tsx:39`,
`.../session-side-panel.tsx:95`; inverse `(max-width: 767px)` at
`packages/app/src/components/titlebar.tsx:75`, `.../settings-v2/general.tsx:281`.
`packages/session-ui/src/components/file.tsx:970` uses `(max-width: 640px)`.
This confirms the plan's reading: **responsive layout with Session/Changes tabs and unified
mobile diffs is real and intentional** — but it is layout evidence only, not certification of an
authenticated session, touch keyboard, terminal, or installed PWA.

**Build / subpath**

- `packages/app/package.json:19-21`: `dev: vite`, `build: vite build`, `serve: vite preview`.
- `packages/app/vite.config.ts`: plugins `[desktopPlugin, sentry]`;
  `build: { target: "esnext", sourcemap: true }`.
  **`base` is not set → defaults to `/`; `outDir` defaults to `dist/`; `assetsDir` to `assets/`.**
- **Hashed assets: yes** (Vite default `assets/[name]-[hash].[ext]`; `packages/app/public/_headers`
  targets `/assets/*.js|*.mjs|*.css`, confirming the layout).
- **Sourcemaps are emitted** (`sourcemap: true`) and are deleted only when the Sentry plugin is
  active (`vite.config.ts:18`). Without Sentry credentials, `.map` files ship — check the pinned
  artifact and decide whether to strip them.
- **Serving under a subpath is not supported as built.** Root-absolute references in
  `packages/app/index.html:9-19,22,27`, `packages/app/src/index.css:8,15` (font URLs), and the
  manifest's `start_url`/`scope`/icons. Serving at `/opencode/` would need `base` set plus CSS
  and manifest patches. **Use a dedicated origin at `/`** — which is what the plan already
  specifies.
- No top-level navigation or redirect to `opencode.ai` exists. The only navigation primitive is
  `openExternal()` (`entry.tsx:83-88`: `window.open(url, "_blank", "noopener,noreferrer")`,
  protocol restricted to `http:`/`https:`/`mailto:`), and the only `location` write in the whole
  app is `window.location.reload()` (`entry.tsx:91`).

---

## 7. Release artifact

From `packages/opencode/script/build.ts`:

- The Web UI is built first (`:26-48`: `bun run --cwd packages/app build`, globbing
  `packages/app/dist/**/*`, dropping `.map` files at `:33`) into `opencode-web-ui.gen.ts`,
  **unless `--skip-embed-web-ui` is passed** (`:24,50`).
- `Bun.build({ compile: { outfile: \`dist/${name}/bin/opencode\`, … } })` (`:163-202`) produces a
  **single self-contained executable**, with the UI map injected via
  `files: { "opencode-web-ui.gen.ts": embeddedFileMap }` (`:184`) and listed as an entrypoint
  (`:190`). Build-time defines include `OPENCODE_VERSION`, `OPENCODE_CHANNEL`, `OPENCODE_LIBC`
  (`:192-201`).
- `await $\`rm -rf ./dist/${name}/bin/tui\`` (`:217`) — no separate TUI binary ships.
- Packaging (`:235-244`): `tar -czf ../../${key}.tar.gz *` run **with cwd `dist/<name>/bin`**.

**⇒ `opencode-linux-x64.tar.gz` and `opencode-linux-x64-musl.tar.gz` each contain exactly one
file at the archive root: the executable `opencode`.** Confirmed by the installer, which does
`tar -xzf … -C "$tmp_dir"` then `mv "$tmp_dir/opencode" "$INSTALL_DIR"` (`install:337,343`).
The server binary and the web UI are the same file; **the UI is embedded, not fetched**,
provided the release was built without `--skip-embed-web-ui`.

Targets built include `linux-x64`, `linux-x64-baseline`, `linux-x64-musl`,
`linux-x64-musl-baseline`, `linux-arm64`, `linux-arm64-musl`, plus darwin/win32
(`build.ts:53-114`).

**Version reporting**

| Surface | Shape |
|---|---|
| `opencode --version` / `-v` | bare version string, e.g. `1.18.30` (`packages/opencode/src/index.ts:51`; value `packages/core/src/installation/version.ts:6`, literal `"local"` for an unbuilt tree) |
| `GET /global/health` | `{ "healthy": true, "version": "<InstallationVersion>" }` (`groups/global.ts:12-15,79`; handler `handlers/global.ts:66`) |
| `GET /api/health` | `{ "healthy": true }` — **no version** (`packages/protocol/src/groups/health.ts:5`; `packages/server/src/handlers/health.ts:6`) |
| `GET /doc` | full OpenAPI JSON of the whole surface (`.../server.ts:188-192`) |
| no `/version` route | — |

For pinning, `GET /global/health` is the right probe; `/api/health` is only a liveness check.

**Gate-A verification to run against the actual downloaded artifact** (not done here — this is a
source read): unpack the tarball, confirm a single `opencode` file, start it with egress to
`app.opencode.ai` blackholed, and confirm `GET /` returns the embedded `index.html` with a
`content-security-policy` header rather than failing — that proves `embeddedUI()` resolved
non-null and the fallback is unreachable. Then enumerate the served asset inventory and compare
it against `packages/app/dist/**` for the pinned commit.

---

## 8. Stop/go items

Judged against the plan's own bar: *"If meaningful use of the native UI requires bypassing
policy or an extensive maintained UI fork, stop."*

| # | Item | Verdict | Evidence / mitigation |
|---|---|---|---|
| 1 | **Bind loopback-only** | **OK** | Default `--hostname 127.0.0.1` (`cli/network.ts:12-16`); explicit flag always wins (`:64,70-71`). No Unix socket (`server/server.ts:214`) but not required. |
| 2 | **Disable mDNS** | **OK** | Off by default (`network.ts:17-21`); additionally refused on a loopback hostname (`server/server.ts:155-170`). |
| 3 | **No self-update at startup** | **OK** | `upgrade()` is called only from the TUI worker (`cli/tui/worker.ts:61`); belt-and-braces `OPENCODE_DISABLE_AUTOUPDATE`. `POST /global/upgrade` is a route — classify **forbidden**. |
| 4 | **No telemetry** | **OK** | OTLP is opt-in (`core/src/observability/otlp.ts:7,51,56`); no analytics. Server-side Sentry absent; the *UI* only initialises Sentry if `VITE_SENTRY_DSN` was set at build time (`packages/app/src/entry.tsx:133-150`) — verify it is unset in the pinned artifact. |
| 5 | **models.dev fetch at startup** | **OK with config** | `packages/core/src/models-dev.ts:255-258`. Set `OPENCODE_DISABLE_MODELS_FETCH=true` (+ `OPENCODE_MODELS_PATH` to pre-seed the catalog) and verify with egress blocked. Not a fork. |
| 6 | **UI fallback to `app.opencode.ai` cannot be disabled by a flag** | **OK but requires gateway work — track as a risk, not a STOP** | `shared/ui.ts:9,40-49,88-93`. `OPENCODE_DISABLE_EMBEDDED_WEB_UI` *selects* the fallback, it does not suppress it, and the fallback also triggers silently on a failed import (`.catch(() => null)`, `:48`). **Mitigation without a fork:** tracon serves `/*` itself from the pinned bundle and never proxies the catch-all to OpenCode, plus deny egress. The plan already requires exactly this. A one-line upstream contribution (`OPENCODE_DISABLE_UI_UPSTREAM` → 404 instead of proxy) would be a bounded, welcome PR. |
| 7 | **Unrestricted `connect-src *` in the server's CSP** | **OK** | `shared/ui.ts:12`. tracon must set its own CSP at the gateway and strip OpenCode's. A workable tightened policy is given in §6. Header replacement, not a fork. |
| 8 | **Credentials in URLs (`?auth_token=`) and in localStorage** | **OK with mediation — but this is the sharpest tension with the plan** | Server accepts `?auth_token` (`.../middleware/authorization.ts:12,77-83`); the app reads it from the query on bootstrap (`entry.tsx:154`) and **persists the cleartext password in `localStorage["opencode.global.dat:server"]`** (`packages/app/src/context/server.tsx:185-195,293`). The plan forbids both. **Mitigation:** run the UI same-origin behind the tracon gateway so `location.origin` is used (`entry.tsx:103`), have the gateway inject upstream Basic credentials server-side and require **no** password in the browser at all — the app then stores `password: undefined` and `Authorization` is never set client-side (`utils/server.ts:27-32`). Gateway-side auth is then tracon's own HttpOnly cookie. Verify the empty-password path end-to-end at Gate D; if the UI refuses to operate without a stored password, revisit. |
| 9 | **No server-side hook to gate tool execution before it runs** | **CONFIRMED LIMITATION — not a fork, but it changes the design** | Only in-process plugin hooks exist: `tool.execute.before` (`packages/plugin/src/index.ts:266-270`) and `permission.ask` (`:261`). HTTP permission routes are notify-then-answer and fire **only** when the ruleset says `ask`; `allow` emits **no event at all** (`packages/opencode/src/permission/index.ts:80-84`; `packages/core/src/permission.ts:206`). Per the plan's own rule — *"If a security-relevant event cannot be reconstructed, require an external gate or supported upstream hook before execution; do not substitute a best-effort event log for enforcement"* — tracon's enforcement must be **containment (fs/network/process isolation) plus an all-`ask` policy**, with the plugin hook as a cooperating convenience only. |
| 10 | **"Always" grants bypass the external controller** | **OK only if the gateway rewrites them** | v1 `always` mutates in-memory `state.approved` (`permission/index.ts:145-151`) and auto-resolves other pending requests (`:153-166`); v2 `always` **persists a project-scoped DB row** (`packages/core/src/permission.ts:250-256` → `permission/saved.ts:54-69`). Mitigation: the gateway must rewrite every `reply: "always"` to `"once"` on `POST /permission/:requestID/reply`, `POST /session/:id/permissions/:pid`, and `POST /api/session/:id/permission/:rid/reply`, and record the broadening tracon-side. Also **forbid `PATCH /session/:sessionID`** — its body can rewrite the session's permission ruleset outright (`groups/session.ts:227`). |
| 11 | **`directory` / `location[directory]` is an implicit authorization** | **OK with a strict gateway — but it is the single largest policy surface** | `middleware/workspace-routing.ts:86-88`; `middleware/instance-context.ts:29`; `packages/server/src/location.ts:29-39`; plus body-borne directories in `POST /sync/replay`, `POST /experimental/control-plane/move-session`, `DELETE /experimental/worktree`, `POST /api/session`. The gateway must pin the directory on every request **and inspect bodies**, and fail closed on unknown routes. |
| 12 | **PTY create is arbitrary command execution with no permission check** | **OK only as an explicit, workspace-scoped capability** | `POST /pty` / `POST /api/pty` → `spawn(command, args, {cwd, env})` (`packages/core/src/pty.ts:182-183`); no `Permission` reference in the PTY path. Matches what the plan already anticipates ("authorize an explicit workspace-scoped interactive capability"). Default-deny; allow only for an explicitly granted terminal capability. |
| 13 | **PTY WS ticket is not bound to owner/audience/session** | **OK with gateway-issued tickets** | 60 s, single-use, `{ptyID,directory,workspaceID}`-scoped (`packages/core/src/pty/ticket.ts:9,27-31,43-50`), but origin check accepts any `http://localhost:*` (`packages/server/src/cors.ts:13`). tracon should mint its own owner/audience-bound capability at the gateway and exchange it for the upstream ticket server-side. |
| 14 | **Auth is silently a no-op when `OPENCODE_SERVER_PASSWORD` is unset** | **OK — operational hazard, must be asserted** | `packages/server/src/auth.ts:40-42`; short-circuits at `.../middleware/authorization.ts:104,122,138`. Gate B must assert the variable is set and that an unauthenticated request is rejected; never rely on loopback alone. |
| 15 | **No durable replay on `/event`, `/global/event`, `/api/event`** | **OK — design around it** | SSE `id:` is always `undefined` (`handlers/event.ts:12-19`), so `Last-Event-ID` cannot work; events published while disconnected are lost. **But** `GET /api/session/:id/event?after=<seq>` and `GET /api/session/:id/history?after=` give real durable, sequenced replay (`packages/protocol/src/groups/session.ts:307-343`). tracon's ingestion must be anchored on the per-session durable stream + snapshots, treating the global streams as best-effort hints. This is exactly the plan's "do not assume SSE has durable replay or invent an ordering guarantee absent from upstream". |
| 16 | **UI cannot be served from a subpath** | **OK — use a dedicated origin** | `base` unset, root-absolute refs in `packages/app/index.html:9-19,22,27`, `src/index.css:8,15`, and the manifest. The plan already specifies a separate origin at `/`. |
| 17 | **OpenCode UI manifest claims `scope:"/"`, `start_url:"/"`** | **OK on a separate origin; gate the embed** | `packages/app/public/site.webmanifest`. On its own origin it cannot broaden or collide with tracon's `spa/public/manifest.webmanifest`. The in-scope shell + cross-origin embedded view remains a **candidate**, gated on `frame-ancestors` and on an authenticated transport that works with third-party cookies/storage blocked — unchanged from the plan. |
| 18 | **Legacy `/tui/*`, `/sync/*`, `/experimental/workspace/*`, `/experimental/console/*`, `/mcp/*`, `/auth/*`, `PATCH /global/config`, `POST /global/upgrade`, `*/share`** | **OK — deny by default** | All enumerated in §2 with file:line. The deny-by-default posture handles them; no fork needed. Note `POST /tui/control/response` takes an **unvalidated `Schema.Unknown` body** (`groups/tui.ts:185`). |
| 19 | **`ProjectCopyGroup` v2 routes lack the `/api` prefix** | **OK — matrix hygiene** | `packages/protocol/src/groups/project-copy.ts:7`. Confirm against live `/doc` when pinning; fail-closed prefix matching must not assume "v2 ⇒ `/api/`". |

**No STOP-candidate found.** The two items that came closest — the `app.opencode.ai` fallback
(#6) and the absence of an external pre-execution tool gate (#9) — are both resolvable inside
the architecture the plan already mandates (tracon serves the UI itself and denies egress;
enforcement comes from containment plus an all-`ask` policy rather than from an upstream hook).
Neither requires a maintained fork of `packages/app`. The residual items that most deserve an
explicit operator decision are **#9** (enforcement model) and **#8** (whether the UI is usable
with no password stored in the browser) — both are Gate D acceptance evidence, not blockers on
paper.
