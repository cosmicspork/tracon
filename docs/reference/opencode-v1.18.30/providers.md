# Gate A — OpenCode v1.18.30 provider / auth inventory for the tracon model gateway

Candidate: `opencode` v1.18.30, `git describe` = `v1.18.30`, commit `3104c14 release: v1.18.30`
(`packages/opencode/package.json:3`). Runtime: Bun (`bun@1.3.14`, root `package.json:7`).
Comparison baseline: tracon `main` @ `74df5cc`.

OpenCode paths are relative to `<opencode-src>/`.
tracon paths are relative to the tracon repository root.

**Headline:** no STOP condition. The two paths the plan flagged as likeliest feasibility
failures both resolve favourably, and for the same structural reason — **OpenCode installs
its credential-holding request machinery only when an OAuth record exists in the runner's
auth store.** tracon never puts one there. Details in §3.5 and §8.

---

## 0. A structural fact that governs everything below: three stacks in one binary

| Stack | Code | Live in v1.18.30? |
|---|---|---|
| **v1 / ai-sdk** — provider list built in `packages/opencode/src/provider/provider.ts` (2072 lines), models resolved to `@ai-sdk/*` packages; session in `packages/opencode/src/session/*`; HTTP routes at `/session/...` | `packages/opencode/src/provider/provider.ts`, `packages/core/src/v1/config/provider.ts` | **Yes — the default and only fully-featured path.** |
| **v2 / Effect** — `packages/core/src/session/*`, `packages/core/src/plugin/provider/*`, credentials in SQLite, routes at `/api/session/...`, events `session.next.*` | `packages/core/src/` | Partially wired. **Cost accounting is broken here** (§6.2). |
| **native LLM** — hand-written protocol implementations replacing ai-sdk | `packages/llm/src/` | **No — gated behind `OPENCODE_EXPERIMENTAL_NATIVE_LLM`.** |

Evidence for the native gate: `packages/opencode/src/effect/runtime-flags.ts:54`
`experimentalNativeLlm: bool("OPENCODE_EXPERIMENTAL_NATIVE_LLM")`, defaulted `false` by
`const bool = (name) => Config.boolean(name).pipe(Config.withDefault(false))`
(`runtime-flags.ts:4`); sole production consumer `packages/opencode/src/session/llm.ts:226`
`if (flags.experimentalNativeLlm) {`; test asserts the default at
`packages/opencode/test/effect/runtime-flags.test.ts:62`.

Evidence that v1 is the served surface: the HTTP provider group imports `Provider` from
`@/provider/provider` (`packages/opencode/src/server/routes/instance/httpapi/groups/provider.ts:2`)
and the config API payload schema is `ConfigV1.Info` (`.../groups/config.ts:28`).

**Consequence for tracon:** pin v1/ai-sdk behaviour; put `OPENCODE_EXPERIMENTAL_NATIVE_LLM`
explicitly off in the launch manifest; read usage from the v1 surface, not `/api/...`. Three
stacks in one binary is itself an upgrade hazard — a release flipping a default silently
rewrites §2, §4 and §6. This belongs in the compatibility manifest as a pinned flag set, not
just a version string.

---

## 1. Provider config model

### 1.1 Schema (v1, the live one)

Top-level key `provider`, a map of provider id → info
(`packages/core/src/v1/config/config.ts:110`).

`ConfigProviderV1.Info` — `packages/core/src/v1/config/provider.ts:82-131`:

```
api        ?: string      // :83  base-URL template for models (supports ${VAR})
name       ?: string      // :84
env        ?: string[]    // :85  env var names that supply the API key
id         ?: string      // :86
npm        ?: string      // :87  AI SDK package, e.g. "@ai-sdk/openai-compatible"
whitelist  ?: string[]    // :88  only these model ids survive
blacklist  ?: string[]    // :89  these model ids are dropped
options    ?: {           // :90  OPEN struct — arbitrary extra keys allowed (:127)
  apiKey        ?: string           // :93
  baseURL       ?: string           // :94
  enterpriseUrl ?: string           // :95
  setCacheKey   ?: boolean          // :98
  timeout       ?: number | false   // :101
  headerTimeout ?: number | false   // :108  default 300000
  chunkTimeout  ?: number | false   // :117  default 300000; aborts on SSE chunk stall
  ...any                            // :127
}
models     ?: Record<string, Model> // :130
```

`Model` — `provider.ts:13-80`: `id`, `name`, `family`, `release_date`, `attachment`,
`reasoning`, `temperature`, `tool_call`, `interleaved`,
`cost{input,output,cache_read,cache_write,context_over_200k}`, `limit{context,input,output}`,
`modalities{input[],output[]}`, `experimental`, `status`
(`"alpha"|"beta"|"deprecated"|"active"`), `provider{npm,api}`, `options`,
**`headers: Record<string,string>`** (`:68`), `variants`.

Canonical example, verbatim from `packages/web/src/content/docs/providers.mdx:2632-2661`:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "myprovider": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "My AI Provider Display Name",
      "options": {
        "baseURL": "https://api.myprovider.com/v1",
        "apiKey": "{env:ANTHROPIC_API_KEY}",
        "headers": { "Authorization": "Bearer custom-token" }
      },
      "models": { "my-model-name": { "name": "My Model Display Name",
                    "limit": { "context": 200000, "output": 65536 } } }
    }
  }
}
```

`{env:VAR}` and `{file:path}` substitution: `packages/opencode/src/config/variable.ts:33-61`,
applied before parse at `packages/opencode/src/config/config.ts:233-239`.

### 1.2 Where config is read from — the full discovery order

`Config.loadInstanceState`, `packages/opencode/src/config/config.ts:328-548`. Merged with
`mergeConfigConcatArrays` → `mergeDeep(target, source)` (`config.ts:42-52`): **later sources
win for scalars; arrays are replaced, except `instructions` which is unioned** (`:48-50`).

| # | Source | Line | Suppressible? |
|---|---|---|---|
| 1 | For each `auth.json` entry of `type:"wellknown"`: **HTTP GET `<url>/.well-known/opencode`**, then a second GET of `remote_config.url` | `:370-410` | only by controlling the auth store |
| 2 | `$XDG_CONFIG_HOME/opencode/{config.json,opencode.json,opencode.jsonc}` (+ legacy TOML migration) | `:272-290` | redirect via `OPENCODE_CONFIG_DIR` (`packages/core/src/global.ts:64`) |
| 3 | `$OPENCODE_CONFIG` (single file) | `:415-418` | — |
| 4 | Project `opencode.json(c)`, walking **up** from cwd to worktree root | `:420-424`, `config/paths.ts:10-21` | `OPENCODE_DISABLE_PROJECT_CONFIG=1` |
| 5 | Every `.opencode/` walking up from cwd, **plus `$HOME/.opencode`**, plus `$OPENCODE_CONFIG_DIR`: `opencode.json(c)` + command/agent/plugin dirs | `:430-480`, `paths.ts:23-41` | cwd-walk only; **`$HOME/.opencode` is unconditional** (`paths.ts:34-38`) |
| 6 | `$OPENCODE_CONFIG_CONTENT` (inline JSON/JSONC) | `:482-490` | — |
| 7 | Active console org: **HTTP GET `<account.url>/api/config`** | `:492-528` | only by controlling the account DB |
| 8 | Managed dir — Linux **`/etc/opencode/opencode.json(c)`**, macOS `/Library/Application Support/opencode`, Windows `%ProgramData%\opencode` | `:530-536`, `config/managed.ts:20-33` | `OPENCODE_TEST_MANAGED_CONFIG_DIR` (test-only name) |
| 9 | macOS MDM plist `ai.opencode.managed` | `:538-548`, `managed.ts:43-58` | n/a on Linux |
| 10 | `$OPENCODE_PERMISSION` (permission subtree only) | `:559-565` | — |

Side effects to record in the manifest:

- Step 5 **writes a `.gitignore`** into every discovered `.opencode` dir (`:309-326`, called `:450`).
- Step 5 forks a background **npm install** of `@opencode-ai/plugin` into every discovered
  config dir (`:452-471`) — unconditional egress per session start, even with no plugins configured.
- `loadGlobal` **creates** `$XDG_CONFIG_HOME/opencode/opencode.json` if absent, but only when
  none of `OPENCODE_CONFIG` / `OPENCODE_CONFIG_DIR` / `OPENCODE_CONFIG_CONTENT` is set (`:264-271`).
- `loadConfig` **rewrites the user's file** to inject `"$schema"` if missing (`:245-249`).
- The `wellknown` auth type is the sharpest edge in the whole discovery path: it fetches a
  remote descriptor and, per `packages/opencode/src/cli/cmd/providers.ts:327-348`, **runs
  `wellknown.auth.command` as a subprocess** to mint the credential. A credential materialised
  by executing a command a remote server named has no place in a tracon runner. Assert no
  `wellknown` entry exists.

### 1.3 Enabled / disabled

`packages/core/src/v1/config/config.ts:68-73`: `disabled_providers: string[]`,
`enabled_providers: string[]`. Enforced at `packages/opencode/src/provider/provider.ts:1445-1452`:

```ts
const disabled = new Set(cfg.disabled_providers ?? [])
const enabled  = cfg.enabled_providers ? new Set(cfg.enabled_providers) : null
function isProviderAllowed(providerID) {
  if (enabled && !enabled.has(providerID)) return false
  if (disabled.has(providerID)) return false
  return true
}
```

`disabled_providers` beats `enabled_providers` (`packages/web/src/content/docs/config.mdx:843`;
test at `packages/opencode/test/provider/provider.test.ts:1019-1035`), and beats a set env var
(`provider.test.ts:609-615`). Same filter is duplicated in the HTTP list handler
(`.../httpapi/handlers/provider.ts:45-50`) and the `/connect` picker
(`packages/opencode/src/cli/cmd/providers.ts:361-368`) — keep all three in mind when auditing.

Asymmetry worth knowing: `disabled.has(...)` is checked at **every** accumulation stage
(`:1460, :1586, :1599, :1612, :1631`), but `isProviderAllowed` — the one that honours
`enabled_providers` — runs only in the final sweep at `:1673`. End state is the same, but a
provider excluded *only* by `enabled_providers` still has its plugin `auth.loader` executed at
`:1618` before deletion. **Use `disabled_providers` when the goal is to stop a loader running
at all; use `enabled_providers` as a belt-and-braces allowlist on top.**

A provider with zero surviving models is dropped (`:1715-1718`).

### 1.4 What makes a provider exist at all

Four independent sources, unioned (`provider.ts:1400-1655`); any one suffices:

1. **Catalogue** — every catalogue provider becomes a `database` entry (`:1404-1406`), inert until something below activates it.
2. **Config** — any key under `provider` (`:1482-1580`), extending a catalogue entry or creating one from nothing. **No credential required.**
3. **Env** — the first non-empty of the provider's `env` names supplies `provider.key` (`:1583-1593`). `provider.env` comes from the **catalogue**, copied at `:1343` — so `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` etc. are *not hard-coded in this repo*; they arrive in `api.json`. Overridable per provider via `provider.<id>.env`.
4. **`auth.json` / `OPENCODE_AUTH_CONTENT`** — a `type:"api"` entry supplies `provider.key` (`:1596-1606`); a plugin `auth.loader` supplies `options` (`:1608-1627`).

The `Env` service is a snapshot of `process.env` (`packages/opencode/src/env/index.ts:22`);
**no `.env` file is read** — no `dotenv`/`loadEnvFile` in `packages/core/src` or
`packages/opencode/src`, and the compiled binary sets `autoloadDotenv: false`
(`packages/opencode/script/build.ts:174`). tracon fully controls this channel.

### 1.5 Can tracon fully specify a provider from a single file, with no other discovery?

**Yes for the provider; no for discovery in general.** A provider is completely determined by
one config object (`npm` + `options.baseURL` + `options.apiKey` + `models`) — config providers
enter `database` unconditionally at `:1579`, and `resolveSDK` reads `options.baseURL`/`apiKey`
at `:1759-1781`.

"No other discovery" is not reachable by config alone. Best placement is
**`/etc/opencode/opencode.json`** (step 8), which merges *after* every project, home,
`OPENCODE_CONFIG` and `OPENCODE_CONFIG_CONTENT` source and therefore wins for `provider.*`,
`disabled_providers` and `model`. Combine with `OPENCODE_DISABLE_PROJECT_CONFIG=1`,
`OPENCODE_CONFIG_DIR=<read-only>`, a controlled `$HOME` with no `.opencode`, and
`OPENCODE_DISABLE_DEFAULT_PLUGINS=1` + `OPENCODE_PURE=1` (§3.6).

**Residual, not suppressible by any flag:** the `wellknown` remote-config fetch (step 1) and
the console-org `/api/config` fetch (step 7). Both are gated on auth/account *state*, not a
switch. tracon controls that state — but it is an invariant to assert and test, not a
configuration. This is exactly the plan's warning that "setting a custom config path alone is
not evidence that other discovery is disabled" (`plan:137`), and it holds.

---

## 2. Auth storage

### 2.1 Location and format (v1)

`packages/opencode/src/auth/index.ts:10`: `const file = path.join(Global.Path.data, "auth.json")`,
with `Global.Path.data = path.join(xdgData!, "opencode")` (`packages/core/src/global.ts:11`) →
`~/.local/share/opencode/auth.json`. Mode `0o600` (`:79, :88`).

Flat `Record<providerID, Info>`, tagged on `type` (`auth/index.ts:14-36`):

```ts
Oauth     { type: "oauth",     refresh, access, expires, accountId?, enterpriseUrl? }  // :14-21
Api       { type: "api",       key, metadata? }                                        // :23-27
WellKnown { type: "wellknown", key, token }                                            // :29-33
```

`export const OAUTH_DUMMY_KEY = "opencode-oauth-dummy-key"` (`:8`) — OpenCode already has the
placeholder-key concept, and its own xAI plugin states the pattern's intent verbatim
(`packages/opencode/src/plugin/xai.ts:218-219`): *"Dummy bearer keeps the AI SDK from bailing
on 'missing apiKey'; the real OAuth token is injected by the fetch override below."*

v2 stores credentials in **SQLite** instead — table `credential`
(`packages/core/src/credential/sql.ts:5-14`) in `join(Global.Path.data, "opencode.db")`
(`packages/core/src/database/database.ts:53-54`), with discriminator `"key"` rather than
`"api"` (`packages/schema/src/credential.ts:15-35`). Not the live path, but it is a second
credential store on disk that per-session state isolation must account for (`plan:191`).

### 2.2 `OPENCODE_AUTH_CONTENT` — and a trap

`auth/index.ts:58-67`:

```ts
const all = Effect.fn("Auth.all")(function* () {
  if (process.env.OPENCODE_AUTH_CONTENT) {
    try { return JSON.parse(process.env.OPENCODE_AUTH_CONTENT) } catch (err) {}
  }
  const data = yield* fsys.readJson(file)...
})
```

1. **tracon can supply the entire auth store from an env var, with no file on disk** — the
   cleanest fit for "the live runner holds no reusable provider credential" (`plan:116`).
2. **When it is set, the store is read-only and writes are silently lost.** `set()` (`:73-81`)
   and `remove()` (`:83-89`) still write `auth.json`, but `all()` returns before reading it.
   Good for tracon (no credential persistence in the runner); fatal for any in-process OAuth
   refresh that needs to persist.
3. Parse failure is swallowed (`catch (err) {}` at `:62`) and **falls through to the file** —
   malformed content fails *open*, not closed. Validate the JSON before launch.
4. Env-supplied auth is returned **undecoded** — the `Schema.decodeUnknownOption` filter applied
   to file contents (`:66`) is bypassed.

### 2.3 Is auth read at every request?

**No.** The provider table is built once per instance inside `InstanceState.make<State>`
(`provider.ts:1400-1730`); `auth.all()` runs once at `:1596`, each `auth.loader` once at
`:1618`. SDK instances are memoised by hash of `{providerID, npm, options}` (`:1788-1796`),
language models by `${providerID}/${model.id}` (`:1898-1899`).

Per-request credential freshness therefore comes only from a **custom `fetch`** in
`options.fetch`, which OpenCode wraps (`:1798-1829`, `fetchFn = customFetch ?? fetch` at `:1805`).
The loader receives `getAuth` as a **thunk** — `() => bridge.promise(auth.get(providerID))`
(`:1620`) — and `Auth.all` re-reads the store on each call, so a loader's fetch wrapper sees
live auth. That is how the OAuth plugins do request-time refresh despite the one-shot build.
`options.fetch` cannot come from JSON config (a function is not expressible); only a plugin can
set it — via `auth.loader`, or via the `config` hook mutating `config.provider[id].options.fetch`
before the provider table is read (`packages/opencode/src/plugin/index.ts:245-253`, and the
deliberate ordering comment at `provider.ts:1440-1444`).

### 2.4 Does a placeholder key + rewritten baseURL work?

**Yes, for any provider whose credential path is `options.apiKey` / `provider.key`.**
Precedence, `provider.ts:1759-1781`:

```ts
const baseURL = iife(() => {
  let url = typeof options["baseURL"] === "string" && options["baseURL"] !== ""
    ? options["baseURL"] : model.api.url      // :1760-1761  config baseURL WINS over catalogue
  ...
})
if (baseURL !== undefined) options["baseURL"] = baseURL                                  // :1780
if (options["apiKey"] === undefined && provider.key) options["apiKey"] = provider.key    // :1781
```

Merge order into `provider.options` (`mergeProvider` = `mergeDeep(existing, patch)`, `:1427-1438`):
catalogue → env → `auth.json` api keys → **plugin `auth.loader` options** → built-in `custom()`
loaders → **user config `options` re-applied last** (`:1647-1655`). So **config `options` beat a
plugin loader's `options` key-for-key** — but cannot *remove* a loader-installed `options.fetch`,
because `mergeDeep` only adds.

Nothing in the built-in `custom()` table forces a base URL for `anthropic` or `openai`:

- `anthropic` (`provider.ts:177-184`): only
  `options.headers["anthropic-beta"] = "interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14"`.
- `openai` (`provider.ts:209-216`): `headerTimeout` + forces the Responses API (`sdk.responses(modelID)`).

Both are `autoload: false` — they only decorate an already-present provider (`:1637`).
Providers that *do* recompute a URL (`azure` `:258,:295`; `amazon-bedrock` `:362-365`;
`google-vertex-anthropic` `:566`; `cloudflare-*` `:877`; `snowflake-cortex` `:933`) are outside
tracon's matrix, and several explicitly back off when a baseURL is already set
(`:742`, `:779`: *"When baseURL is already configured (e.g. corporate config routing through a
proxy/gateway), skip"*). The xAI plugin makes the same choice deliberately
(`packages/opencode/src/plugin/xai.ts:220-222`: *"We intentionally do NOT set baseURL … overriding
here would silently route around a user-configured gateway"*). Upstream mostly respects gateways;
Codex is the exception (§3.3).

### 2.5 **Critical: OpenCode honours no base-URL environment variable**

```
grep -rn "ANTHROPIC_BASE_URL|OPENAI_BASE_URL|ANTHROPIC_AUTH_TOKEN|OPENAI_API_BASE" packages
→ (no matches)
```

tracon's `harness_wiring` pushes `ANTHROPIC_BASE_URL` + `ANTHROPIC_API_KEY` as env for
`SHAPE_ANTHROPIC` (`node/src/gateway/model.rs:131-134`). **The env half does not reach OpenCode.**
`ANTHROPIC_API_KEY` *does* work (catalogue `env` name, consumed at `provider.ts:1587`), but the
base URL must be written into `provider.anthropic.options.baseURL`. **Required adapter change.**

### 2.6 Header name per provider

OpenCode's own lowering table, `packages/core/src/v1/config/provider-options.ts:142-160`, maps
each `npm` package's options onto `{url, headers, body, settings}`:

| `npm` package | key header | other headers | base-URL option |
|---|---|---|---|
| `@ai-sdk/openai` | `Authorization: Bearer <apiKey>` (`:34`, `bearer()` `:221-223`) | `OpenAI-Organization` (`:35`), `OpenAI-Project` (`:36`) | `baseURL` (`:32`) |
| `@ai-sdk/anthropic`, `@ai-sdk/google-vertex/anthropic` | **`x-api-key: <apiKey>`** (`:67`) | `Authorization: Bearer <authToken>` if `options.authToken` (`:68`) | `baseURL` (`:65`) |
| `@ai-sdk/google`, `@ai-sdk/google-vertex` | `x-goog-api-key` (`:93`) | — | `baseURL` (`:92`) |
| `@ai-sdk/azure` | `api-key` (`:111`) | — | `baseURL` (`:110`) |
| `@ai-sdk/openai-compatible` + aliases (`cerebras`, `deepinfra`, `groq`, `mistral`, `togetherai`, `xai`, `@openrouter/ai-sdk-provider`, `ai-gateway-provider`, `venice-ai-sdk-provider`) | `Authorization: Bearer` (ai-sdk default; the lowerer passes `apiKey` through as a setting, `:128-131`) | user `options.headers` | `baseURL` (`:130`) |
| `@ai-sdk/amazon-bedrock` | SigV4 / `AWS_BEARER_TOKEN_BEDROCK` (`provider.ts:322-325`) | — | `endpoint` ?? `baseURL` (`provider.ts:363`) |

**Caveat, stated plainly:** `node_modules` is not vendored in this checkout, so the ai-sdk
packages' own bytes were not inspected. The table is OpenCode's own declaration of what those
packages do. It is corroborated by the native implementations, which agree exactly:
`packages/llm/src/providers/anthropic.ts:15-18`
(`…orElse(Auth.config("ANTHROPIC_API_KEY")).pipe(Auth.header("x-api-key"))`) and
`packages/llm/src/providers/openai.ts:25` (`AuthOptions.bearer(options, "OPENAI_API_KEY")`).
**Confirm against the wire in Gate B rather than treating this table as settled.**

Native defaults (`packages/llm/src/protocols/`), useful for predicting paths:

| protocol | `DEFAULT_BASE_URL` | `PATH` | fixed headers |
|---|---|---|---|
| `anthropic-messages.ts` | `https://api.anthropic.com/v1` (`:29`) | `/messages` (`:30`) | `anthropic-version: 2023-06-01` (`:852`) |
| `openai-responses.ts` | `https://api.openai.com/v1` (`:29`) | `/responses` (`:30`) | — |
| `openai-chat.ts` | `https://api.openai.com/v1` (`:28`) | `/chat/completions` (`:29`) | — |

**Gateway path compatibility — no gateway change needed.** With
`options.baseURL = http://tracon-gw:<port>/model/anthropic/v1`, OpenCode sends
`POST /model/anthropic/v1/messages`; `raw_tail` yields `v1/messages`, matching `ANTHROPIC_ROUTES`
(`node/src/gateway/model.rs:212`). With `…/model/openai/v1`, the Responses call yields
`v1/responses`, matching `OPENAI_ROUTES` (`model.rs:226`).

Header conflict, already handled: OpenCode injects
`anthropic-beta: interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14`
(`provider.ts:180-182`). tracon drops the incoming `anthropic-beta` only when
`injection.oauth_beta` (`model.rs:486`) and re-merges the caller's flags with
`oauth-2025-04-20` prepended (`model.rs:703-716`). OpenCode's flags survive on the subscription
path and pass through untouched on the API-key path. **No change needed.**

### 2.7 Auth-related HTTP surface tracon must mediate

- `GET /provider` (`groups/provider.ts:37`), `GET /provider/auth` (`:47`)
- `POST /provider/:providerID/oauth/authorize` (`:58`)
- `POST /provider/:providerID/oauth/callback` (`:70`) — writes `auth.json` via
  `ProviderAuth.callback` → `auth.set` (`packages/opencode/src/provider/auth.ts:203-220`)
- **`PUT /auth/:providerID`** with an `Auth.Info` payload
  (`.../httpapi/groups/control.ts:32,39-50`) — the route plugins use to persist refreshed
  tokens, and a direct credential-write primitive.
- **`PATCH /config`** with a full `ConfigV1.Info` payload (`groups/config.ts:26-28`) — a client
  can rewrite `provider.<id>.options.baseURL` and `.apiKey`, i.e. **re-point model traffic away
  from the gateway**. Handler `handlers/config.ts:18-22` → `Config.update`
  (`config/config.ts:638-650`), which writes `<instance directory>/config.json`.

All five belong in the plan's "Provider auth, plugin installation, global config → **Deny raw
runtime access**" row (`plan:102`). `PATCH /config` and `PUT /auth/:providerID` are the two most
easily overlooked, and the two with the most direct security consequence.

---

## 3. Subscription / OAuth plugins

### 3.1 Which OAuth plugins ship

`packages/opencode/src/plugin/index.ts:67-86`, `internalPlugins()`: `CodexAuthPlugin`
(OpenAI/ChatGPT), `CopilotAuthPlugin`, `ModalPlugin`, `GitlabAuthPlugin`, `PoeAuthPlugin`,
`CloudflareWorkersAuthPlugin`, `CloudflareAIGatewayAuthPlugin`, `AzureAuthPlugin`,
`the DO-cloud auth plugin`, `SnowflakeCortexAuthPlugin`, `XaiAuthPlugin`, `CerebrasPlugin`.

### 3.2 **Anthropic Claude Pro/Max: there is no plugin. It was removed upstream.**

```
grep -rn "claude.ai/oauth|console.anthropic.com|9d1c250a-e61b-44d9-88ed-5944d1962f5e|oauth-2025-04-20" packages
→ (no matches)
```

`packages/core/src/plugin/provider/anthropic.ts` (27 lines) contains only the `anthropic-beta`
transform and the `@ai-sdk/anthropic` factory. `packages/core/src/oauth/` contains **only
`page.ts`**, the shared callback HTML. The complete set of files with real OAuth endpoints is
`console/app/src/lib/salesforce.ts`, `core/src/plugin/provider/{openai,opencode}.ts`,
`opencode/src/account/account.ts`, and `opencode/src/plugin/{do-cloud,github-copilot/copilot,
openai/codex,snowflake-cortex,xai}.ts` — **no anthropic**.

Upstream says so explicitly, `packages/web/src/content/docs/providers.mdx:356-368`:

> There are plugins that allow you to use your Claude Pro/Max models with OpenCode. Anthropic
> explicitly prohibits this. **Previous versions of OpenCode came bundled with these plugins but
> that is no longer the case as of 1.3.0.** Other companies support freedom of choice with
> developer tooling - you can use the following subscriptions in OpenCode with zero setup:
> ChatGPT Plus, Github Copilot, Gitlab Duo

(The doc is internally inconsistent — step 2 at `:340` still shows a "Claude Pro/Max" option, and
the web UI retains a string `provider.connect.title.anthropicProMax` → `"Login with Claude Pro/Max"`
(`packages/app/src/i18n/en.ts:137`) rendered only when a method label contains "max"
(`packages/app/src/components/dialog-connect-provider.tsx:1130`). No code registers such a method.
Both are dead UI paths that a third-party plugin could light up.)

**This is the best finding for tracon.** Nothing in the runtime wants to hold, refresh or rewrite
around an Anthropic subscription token. tracon's gateway already does the whole job: `Bearer`
injection, `anthropic-beta: oauth-2025-04-20` merge, and the `CLAUDE_CODE_SYSTEM` system-prompt
shaping the token demands (`node/src/gateway/model.rs:43-86, 486, 496-502, 703-716`) — PR #166,
`cab972b`. OpenCode needs only a plain `anthropic` provider with a placeholder `apiKey` and a
gateway `options.baseURL`. **Verdict: gateway-compatible as-is, and simpler than the omp path.**
The plan's fear (`plan:125`) does not apply here at all.

Risk to record: this is a deliberate upstream *policy* position, not an oversight. It will not be
re-added, and client-side checks could plausibly appear. Diff this specifically on every upgrade.

### 3.3 OpenAI Codex / ChatGPT subscription — the one plugin that fights a gateway

`packages/opencode/src/plugin/openai/codex.ts`. Constants (`:10-16`):

```ts
const CLIENT_ID = "app_EMoamEEZ73f0CkXaXp7hrann"
const ISSUER = "https://auth.openai.com"
const CODEX_API_ENDPOINT = "https://chatgpt.com/backend-api/codex/responses"
const OAUTH_PORT = 1455
```

It binds to provider id **`"openai"`** (`codex.ts:328-329`, `auth: { provider: "openai" }`).

**Login.** Two OAuth methods plus a manual API key (`:553-556`):
- *Browser* (`:440-469`): PKCE S256, loopback server on port **1455**, redirect
  `http://localhost:1455/auth/callback`, authorize
  `https://auth.openai.com/oauth/authorize?response_type=code&client_id=…&scope=openid profile email offline_access&code_challenge_method=S256&id_token_add_organizations=true&codex_cli_simplified_flow=true&originator=opencode`
  (`:88-102`); token `POST https://auth.openai.com/oauth/token` (`:117-133`).
- *Headless* (`:470-552`): OpenAI's proprietary device flow —
  `POST https://auth.openai.com/api/accounts/deviceauth/usercode`, display
  `https://auth.openai.com/codex/device`, poll
  `POST https://auth.openai.com/api/accounts/deviceauth/token` (`:474-508`).

**Storage.** `auth.json["openai"] = { type:"oauth", access, refresh, expires, accountId }`;
`accountId` from the JWT claims `chatgpt_account_id` → `"https://api.openai.com/auth".chatgpt_account_id`
→ `organizations[0].id` (`:59-78`).

**Refresh: request-time, in-process, persisted.** Inside the loader's `fetch` (`:363-397`): if
`expires < Date.now()`, `POST ${issuer}/oauth/token` with `grant_type=refresh_token`, then
`client.auth.set({ path: { id: "openai" }, body: {...} })` — i.e. `PUT /auth/openai`. Single-flight
via `refreshPromise` (`:341`).

**Request rewriting** (`:413-436`, verified verbatim):

```ts
headers.set("authorization", `Bearer ${currentAuth.access}`)
if (authWithAccount.accountId) headers.set("ChatGPT-Account-Id", authWithAccount.accountId)

const parsed = requestInput instanceof URL ? requestInput
  : new URL(typeof requestInput === "string" ? requestInput : requestInput.url)
const rewrite = parsed.pathname.includes("/v1/responses") || parsed.pathname.includes("/chat/completions")
const url = rewrite ? new URL(codexApiEndpoint) : parsed
if (rewrite) {
  const residency = extractResidency(currentAuth.access)
  if (residency) headers.set("x-openai-internal-codex-residency", residency)
}
...
if (websocketFetch && parsed.pathname.endsWith("/responses")) return websocketFetch(url, requestInit)
return fetch(url, OpenAIWebSocketPool.withoutInternalHeaders(requestInit))
```

Plus `"chat.headers"` (`:559-568`): `originator: "opencode"`, `User-Agent: opencode/<ver> (<os>)`,
`session-id: <sessionID>`, and `x-opencode-title` for title generation. `"chat.params"` (`:569-573`)
clears `maxOutputTokens`. And when the openai provider is OAuth, the system prompt is **moved into
the Responses `instructions` field** and system messages are dropped from the array
(`packages/opencode/src/session/llm/request.ts:57, :99, :101-112`).

**Two real hazards, both consequences of the rewrite test being a *pathname substring* check
rather than a host check:**

1. `baseURL = "https://my-gateway/v1"` → path `/v1/responses` → **rewritten to chatgpt.com.**
   The gateway is silently bypassed. This is the failure mode the plan predicted (`plan:125`).
2. `baseURL = "https://my-gateway/openai"` → path `/openai/responses` → **no rewrite**, but the
   headers were set *before* the test, so the request reaches an arbitrary host **carrying the
   live ChatGPT subscription bearer token and `ChatGPT-Account-Id`**. Demonstrated by upstream's
   own test at `packages/opencode/test/plugin/codex.test.ts:219-254`.

Note both hazards need the *loader to be installed*, which needs an `oauth` record for provider id
`openai`. See §3.5 — that is the whole answer.

**WebSocket transport.** `experimentalWebSocketsEnabled({ enabled: flags.experimentalWebSockets })`
= `input.enabled || ["local","dev","beta"].includes(InstallationChannel)`
(`packages/opencode/src/plugin/index.ts:62-64, :70-73`). Protocol header
`openai-beta: responses_websockets=2026-02-06` (`ws.ts:11, :79-82`); URL derived by
`url.replace(/^http/, "ws")` (`ws.ts:40-42`).

**A build trap tracon must not walk into.**
`InstallationChannel = typeof OPENCODE_CHANNEL === "string" ? OPENCODE_CHANNEL : "local"`
(`packages/core/src/installation/version.ts:7`), and `"local"` **is in the enable list**. Official
releases define `OPENCODE_CHANNEL` from `Script.channel`, which is `"latest"` for a release build
(`packages/script/src/index.ts:26-31` — `OPENCODE_CHANNEL` → `"latest"` if `OPENCODE_BUMP` or a
non-`0.0.0-` `OPENCODE_VERSION` → else **the current git branch name**), so a shipped binary has
WebSockets off. **A binary tracon builds from source without setting `OPENCODE_CHANNEL` gets
`"local"` and turns Codex WebSockets on.** A WebSocket model transport cannot be allowlisted by
(method, path) at all — tracon's gateway is an axum HTTP router and would simply reject the
upgrade, so this surfaces as a confusing failure rather than a silent bypass. Either consume the
official release artefact or pin `OPENCODE_CHANNEL` at build time; keep
`OPENCODE_EXPERIMENTAL_WEBSOCKETS` unset regardless. tracon already does the equivalent for omp
(`PI_CODEX_WEBSOCKET=false`, `node/src/gateway/model.rs:135`).

### 3.4 GitHub Copilot (brief — outside the plan's matrix)

`packages/opencode/src/plugin/github-copilot/copilot.ts`. RFC 8628 device code against
`https://github.com/login/device/code` → `/login/oauth/access_token`, `client_id
"Ov23li8tweQw6odWQebz"`, scope `read:user` (`:9, :19-24, :234-245`). Stores the GitHub token in
**both** `access` and `refresh` with `expires: 0` (`:286-306`) — **there is no refresh**; the token
is replayed indefinitely. Loader returns `apiKey: ""` plus a fetch that sets `x-initiator`,
`User-Agent`, `Authorization: Bearer <info.refresh>`, `Openai-Intent: conversation-edits`, and
optionally `Copilot-Vision-Request` (`:160-174`), deletes `x-api-key` and lowercase `authorization`,
and **passes the URL through unchanged** (`:175-178`). Base URL comes from the models hook:
`https://api.githubcopilot.com` or `https://copilot-api.<enterprise>` (`:26-28`). Per-model protocol
can be openai-responses, openai-chat **or anthropic-messages** (`github-copilot/models.ts:95-115`).
`Copilot-Integration-Id` is **not** set anywhere in this tree. Same credential-leak shape as Codex:
the loader attaches the real GitHub token to whatever host `baseURL` names.

### 3.5 **The plugin `auth` hook contract — and why tracon is safe**

Contract (`packages/plugin/src/index.ts:88-163`, server mirror
`packages/opencode/src/provider/auth.ts:41-54, :163-221`):
`auth: { provider, loader?, methods: [{type:"oauth"|"api", label, prompts?, authorize}] }`;
`loader: (auth: () => Promise<Auth>, provider: Provider) => Promise<Record<string, any>>` — an
**untyped record** merged wholesale into provider `options`, so it may carry `apiKey`, `baseURL`,
`headers`, `fetch`, anything. `callback` returning `{key,…}` stores `type:"api"`; returning
`{refresh,access,expires,…}` stores `type:"oauth"` with extras spread in.

**The decisive gate** — `packages/opencode/src/provider/provider.ts:1614-1616`:

```ts
const stored = yield* auth.get(providerID).pipe(Effect.orDie)
if (!stored) continue
if (!plugin.auth.loader) continue
```

**If there is no auth record for that provider id, the loader never runs and no `fetch` wrapper is
ever installed.** And if the record is `type:"api"` rather than `"oauth"`, every loader
short-circuits by its own first line: `codex.ts:339`
(`if (auth.type !== "oauth") return websocketFetch ? { fetch: websocketFetch } : {}`),
`copilot.ts:98` (`return {}`), `xai.ts:211`, `azure.ts:58`, `snowflake-cortex.ts:288`.

Both hold for tracon by construction, and doubly so:

1. tracon's auth store contains no `oauth` record — it supplies a placeholder as `type:"api"`
   (or nothing, relying on `options.apiKey` in config).
2. tracon's Codex provider is named **`openai-codex`** (`node/src/config.rs:884`), while the Codex
   plugin binds to **`openai`** (`codex.ts:329`). Different id — the plugin does not even see it.

Mid-session safety is also handled upstream: both `codex.ts:363-365` and `copilot.ts:103-104`
re-read `getAuth()` on **every** request and pass through untouched if the type is no longer
`"oauth"`, so a credential swap takes effect without a restart.

**Answer to the plan's feasibility gate (`plan:125`):** OpenCode's OAuth plugins *do* insist on
holding and refreshing the token themselves and *do* set `Authorization` themselves, and Codex
*does* rewrite the URL to the real host — so a placeholder + base-URL override is defeated **while
an OAuth record exists**. tracon's existing invariant — real tokens stay in the node's broker,
never in the runner — is exactly what makes the question moot. **The boundary tracon already
enforces for its own reasons happens to be the boundary that makes OpenCode gateway-compatible.**

### 3.6 Turning the plugins off (defence in depth)

| Lever | Effect | Evidence |
|---|---|---|
| `OPENCODE_DISABLE_DEFAULT_PLUGINS=1` | **skips every internal plugin**, Codex and Copilot included | `plugin/index.ts:170`; flag `runtime-flags.ts:19` |
| `OPENCODE_PURE=1` / `--pure` | skips **external** plugins only (`cfg.plugin_origins`); internal ones still load — and so no external plugin npm installs | `plugin/index.ts:181`; `index.ts:62-71` |
| `disabled_providers: ["openai", …]` | loader not invoked (`provider.ts:1612`), provider deleted | `provider.ts:1445-1452` |

Set all three. §3.5 means none of them is load-bearing, which is the right place to be: the
security property rests on a state invariant tracon already enforces, and the flags are belt.

---

## 4. Custom fetch / request interception / proxy

### 4.1 Custom fetch

`options.fetch` is honoured and always wrapped (`provider.ts:1798-1829`):

```ts
const customFetch = options["fetch"]                             // :1798
const chunkTimeout  = options["chunkTimeout"]  ?? 300_000        // :1799
const headerTimeout = options["headerTimeout"] ?? 300_000        // :1800
options["fetch"] = async (input, init?: BunFetchRequestInit) => {
  const fetchFn = customFetch ?? fetch                           // :1805
  const res = await fetchFn(input, { ...opts, timeout: false })  // :1821-1825
  if (!chunkAbortCtl) return res
  return wrapSSE(res, chunkTimeout, chunkAbortCtl)               // :1828
}
```

Two injection points, both plugin-only: `auth.loader` (§3.5) and the `config` hook mutating
`config.provider[id].options.fetch` before the provider table is read
(`plugin/index.ts:245-253`; ordering guaranteed by `provider.ts:1440-1444`). Special case: for
`google-vertex` on a non-`openai-compatible` package, `options.fetch` is deleted outright (`:1751-1753`).

Hook surface that can mutate outgoing requests without a fetch override
(`packages/plugin/src/index.ts:222-335`): **`chat.headers`** (add/override HTTP headers;
trigger `session/llm/request.ts:134-146`), **`chat.params`** (temperature/topP/topK/
maxOutputTokens/provider body options; `request.ts:114-132`), `chat.message`,
`experimental.chat.messages.transform`, `experimental.chat.system.transform`, `tool.definition`.
**There is no `request` hook and no `fetch` hook.**

Two timeouts to set deliberately, since a gateway adds latency: `options.headerTimeout` and
`options.chunkTimeout`, both default 300 000 ms, the latter aborting on SSE chunk stall
(`packages/core/src/v1/config/provider.ts:108-125`).

### 4.2 Proxy env vars — **HTTP_PROXY/HTTPS_PROXY/NO_PROXY work; ALL_PROXY does not**

OpenCode implements almost nothing itself and relies on Bun's fetch. There is an official doc
page, `packages/web/src/content/docs/network.mdx` (57 lines, "Configure proxies and custom
certificates"), prescribing exactly `HTTPS_PROXY`, `HTTP_PROXY`, and
`NO_PROXY=localhost,127.0.0.1` — with a caution at `:25-27` that bypassing the proxy for the
local server is **required** to prevent routing loops. CA override is `NODE_EXTRA_CA_CERTS`
(`:54`).

Code corroboration:

- **No `undici`, `setGlobalDispatcher`, `ProxyAgent`, `node-fetch`, `https-proxy-agent` or
  `socks-proxy-agent` anywhere** in `packages/`, and no global `fetch` monkey-patch.
- The one explicit proxy implementation is `packages/opencode/src/util/proxy-env.ts` (a vendored
  `proxy-from-env`), whose `getProxyForUrl` reads `${protocol}_proxy || all_proxy` in both cases
  (`:36-48`) and implements `no_proxy` matching (`:50-66`). Its **only** consumer is the Codex
  WebSocket transport, `packages/opencode/src/plugin/openai/ws.ts:85-90`, prefaced by the comment
  that settles the question:

  > `// Bun does not apply HTTP(S)_PROXY to WebSockets unless the proxy is supplied explicitly.`

  i.e. upstream's own statement that Bun's **fetch** does apply them — and that WebSockets are the
  exception it had to work around. `ALL_PROXY` reaches only this WebSocket path, never fetch.
- The compiled binary is a **Bun single-file executable** with
  `execArgv: ["--user-agent=opencode/<ver>", "--use-system-ca", "--"]`
  (`packages/opencode/script/build.ts:179`, verified) — so the OS trust store is trusted by
  default and a TLS-inspecting proxy needs only its CA installed system-wide.
  `autoloadDotenv: false` (`:174`) means proxy vars must be real environment variables.
- Electron/desktop does more (`setGlobalProxyFromEnv`, `tls.setDefaultCACertificates`, forced
  loopback `NO_PROXY` — `packages/desktop/src/main/{index,sidecar}.ts`), but **the CLI does none
  of it**. Do not generalise desktop behaviour to the runner.
- npm traffic is separate and uses npm's own config (`packages/core/src/npm-config.ts:12-32`
  with `env: { ...process.env }`), so `.npmrc` / `npm_config_*` `registry`, `proxy`,
  `https-proxy`, `noproxy`, `cafile` apply there instead.

**So tracon's existing `HTTPS_PROXY`/`HTTP_PROXY`/`NO_PROXY` wiring
(`node/src/runner/podman.rs:85-87`, `node/src/runner/kube.rs:155-157`) does carry over** — with
the caveat that this is a documented-and-corroborated conclusion, not an observed one, and Bun's
behaviour is a runtime property that an upgrade can change. **Verify by observation in Gate B.**

Note tracon does not *need* proxy support for the model path: with `options.baseURL` pointing at
`http://tracon-gw:<port>/model/<provider>`, model traffic is plain HTTP to a tracon-controlled
host and never touches the proxy — which is also why `NO_PROXY=<gateway host>` is already correct.
The proxy matters for everything OpenCode does unprompted: the catalogue fetch (§7), the
`@opencode-ai/plugin` install (§1.2), `Npm.add` for non-bundled SDKs (§5.3), LSP downloads,
and the UI asset fallback. For those, **network isolation is the control and the proxy is defence
in depth** — which matches the plan's insistence on proving isolation rather than trusting a flag.

---

## 5. Self-hosted / open-weight

### 5.1 Explicit OpenAI-compatible endpoint

Upstream documents all three targets with exact base URLs, in
`packages/web/src/content/docs/providers.mdx`:

- **llama.cpp** (`:1430-1462`): `"llama.cpp"`, `npm: "@ai-sdk/openai-compatible"`,
  `"baseURL": "http://127.0.0.1:8080/v1"`, with `limit: { context, output }` per model.
- **LM Studio** (`:1499-1525`): `"lmstudio"`, `"baseURL": "http://127.0.0.1:1234/v1"`.
- **Ollama** (`:1692-1730`): `"ollama"`, `"baseURL": "http://localhost:11434/v1"`; note `:1729`
  advises raising `num_ctx` to 16k–32k if tool calls misbehave.
- Protocol choice (`:2616-2622`): `@ai-sdk/openai-compatible` for `/v1/chat/completions`,
  `@ai-sdk/openai` for `/v1/responses`. **Both are bundled** (§5.3) — no npm egress.

For tracon's llama.cpp router on `:8080`, the same object with `options.baseURL` pointing at the
**gateway** URL of a node-configured provider whose `upstream` is the llama.cpp endpoint —
preserving the plan's rule that local inference is reached "through a node-owned provider binding
to an exact configured service", never by giving the worker loopback access (`plan:127`).

No API key is needed (`provider.ts:1781` only fills `apiKey` if absent); upstream's own fixture
uses `apiKey: "not-needed"` (`packages/opencode/test/provider/provider.test.ts:886`).

`@ai-sdk/openai-compatible` gets `includeUsage` forced on (`provider.ts:1755-1757`):

```ts
if (model.api.npm.includes("@ai-sdk/openai-compatible") && options["includeUsage"] !== false)
  options["includeUsage"] = true
```

That is `stream_options.include_usage`. **tracon must not set `includeUsage: false`.** See §6.3.

### 5.2 Auto-probing of localhost model servers: **none**

Searched exhaustively. `ollama|lmstudio|lm-studio|llama\.cpp|:11434|:1234` across `packages/`
matches **only** docs, two cosmetic icon names
(`packages/ui/src/components/provider-icons/types.ts:39, :57`), two marketing links, and two test
fixtures. `localhost|127.0.0.1` in `core/src`, `llm/src`, `opencode/src/provider` and
`opencode/src/config` yields five hits, none a probe: a hostname heuristic
(`core/src/repository.ts:167`), the MCP OAuth redirect default
(`core/src/v1/config/mcp.ts:39`), and the OpenAI `/connect` loopback listener
(`core/src/plugin/provider/openai.ts:51,53,80`).

The only model auto-discovery in the provider build is GitLab's, hard-coded by name and fully
gated (`provider.ts:1657-1668`), autoloading only with a token and hitting
`GITLAB_INSTANCE_URL` or `https://gitlab.com` (`:621`). The `custom()` table
(`provider.ts:174-940`) contains no local-server entry at all.

**So tracon's omp mitigation has no OpenCode equivalent to port.** The omp adapter writes
`{"disabled": true}` for `["llama.cpp","ollama","lm-studio"]` because *"omp ships them: absence is
what enables them, so the config has to turn them off explicitly"*
(`node/src/adapter/omp.rs:81-89`, PR #173). In OpenCode absence means absence — a provider only
becomes active through one of the four sources in §1.4. Belt-and-braces is still cheap:
`disabled_providers` names them, or better, `enabled_providers` lists only what tracon wrote.

What *does* egress unprompted at startup, and must be contained: the catalogue fetch (§7), the
`@opencode-ai/plugin` install per config dir (§1.2), `Npm.add` for non-bundled SDKs (§5.3), LSP
downloads (`OPENCODE_DISABLE_LSP_DOWNLOAD`), self-update (`OPENCODE_DISABLE_AUTOUPDATE`), sharing
(`OPENCODE_DISABLE_SHARE`), and — if any AWS env var is present — the **Bedrock credential chain,
which can reach IMDS at `169.254.169.254`** (`provider.ts:307-345`). Add `amazon-bedrock` to
`disabled_providers` and keep AWS vars out of the runner env.

### 5.3 `npm` and runtime package installation

Bundled, no egress — `BUNDLED_PROVIDERS`, `provider.ts:113-140` (24 entries): `@ai-sdk/amazon-bedrock`
(+`/mantle`), `@ai-sdk/anthropic`, `@ai-sdk/azure`, `@ai-sdk/google`, `@ai-sdk/google-vertex`
(+`/anthropic`), `@ai-sdk/openai`, `@ai-sdk/openai-compatible`, `@openrouter/ai-sdk-provider`,
`@ai-sdk/xai`, `@ai-sdk/mistral`, `@ai-sdk/groq`, `@ai-sdk/deepinfra`, `@ai-sdk/cerebras`,
`@ai-sdk/cohere`, `@ai-sdk/gateway`, `@ai-sdk/togetherai`, `@ai-sdk/perplexity`, `@ai-sdk/vercel`,
`@ai-sdk/alibaba`, `gitlab-ai-provider`, `@ai-sdk/github-copilot`, `venice-ai-sdk-provider`.

**Anything else is installed from npm at model-resolution time** (`provider.ts:1842-1854`):

```ts
const installedPath = await (async () => {
  if (model.api.npm.startsWith("file://")) return model.api.npm
  const item = await Npm.add(model.api.npm)
  ...
})()
const mod = await import(importSpec)
const fn = mod[Object.keys(mod).find((key) => key.startsWith("create"))!]
```

The v2 path does the same (`packages/core/src/plugin/provider/dynamic.ts:14-16`). Install dir
`$XDG_CACHE_HOME/opencode/packages/<pkg>` (`packages/core/src/npm.ts:79`), idempotent once
present (`:125-127`), `ignoreScripts: true` (`:80-113`). The config default when `npm` is omitted
is `"@ai-sdk/openai-compatible"` (`provider.ts:1504`) — bundled, so the common case is safe.
**There is no offline/disable switch.** `file://` specifiers are explicitly supported
(`provider.ts:1843`), which is the right escape hatch for an image-baked SDK.

**tracon must constrain `provider.<id>.npm` to the bundled list (or a `file://` path) in its
reviewed-config validation.** An arbitrary value is both a package download and arbitrary
`create*` factory execution inside the runner — the plan's "no arbitrary runtime plugin install
or download" (`plan:141`) applied to providers.

---

## 6. Usage accounting

### 6.1 Recorded fields (v1 — the live path)

`packages/opencode/src/session/session.ts:337-395`, `Session.getUsage`:

```ts
const tokens = {
  total,                                        // :366  usage.totalTokens, may be undefined
  input:  adjustedInputTokens,                  // :368  inputTokens − cacheRead − cacheWrite
  output: safe(outputTokens - reasoningTokens), // :370
  reasoning: reasoningTokens,                   // :371
  cache: { write: cacheWriteInputTokens, read: cacheReadInputTokens },  // :373-375
}
```

Values are **non-overlapping**: reconstruct prompt size as `input + cache.read + cache.write`
and total output as `output + reasoning`. `cacheWriteInputTokens` is recovered from provider
metadata when the normalised field is absent — `anthropic.cacheCreationInputTokens`,
`vertex.cacheCreationInputTokens`, `bedrock.usage.cacheWriteInputTokens`,
`venice.usage.cacheCreationInputTokens` (`:345-358`). An OpenAI-compatible local server reports
none of these.

Accumulation (`packages/opencode/src/session/processor.ts:452-467`): `cost += usage.cost` across
steps, `tokens = usage.tokens` (last step wins), plus a `step-finish` **part** per step carrying
the same numbers. Persisted at `session.ts:98-104, :143`.

### 6.2 Cost — and a v2 defect

v1 (`session.ts:379-395`): tiered pricing by context size (`cost.tiers` with
`tier.type === "context"`, plus a legacy `experimentalOver200K` branch at 200 000 tokens), then
`Decimal` arithmetic over `input`, `output`, `cache.read`, `cache.write`, and **reasoning billed at
the output rate** (with an explicit upstream TODO acknowledging the approximation). GitHub Copilot
short-circuits to `copilot.totalNanoAiu / 1e11` (`:386-389`).

**v2 hardcodes `cost: 0`** — `packages/core/src/session/runner/llm.ts:338` publishes
`SessionEvent.Step.Ended` with a literal `cost: 0`, and `message-updater.ts:213` assigns it
straight onto the message. There is no cost computation anywhere under
`packages/core/src/session/`. The v2 projector also never updates session-level totals for
`Step.Ended` (`packages/core/src/session/projector.ts:380` runs message projection only), so a
pure-v2 session reports zeroed `session.cost`. **Read usage from the v1 surface** (§6.4).

Prices come from the catalogue or config. A config-declared model with no `cost` gets **zero**
(`provider.ts:1550-1555`). Note the v1 config merge also **drops tiering**: the merged `cost`
object has no `tiers`/`experimentalOver200K`, so a config-declared model cannot express
context-tier pricing.

### 6.3 Providers that omit usage: **silently zero**

The chain, verified end to end:

1. Protocol mappers all begin `if (!usage) return undefined` — `openai-chat.ts:392`,
   `openai-responses.ts:508`, `anthropic-messages.ts:574`, `gemini.ts:343`,
   `bedrock-converse.ts:444`.
2. v1 `processor.ts:454`: `usage: value.usage ?? new Usage({})`.
3. `getUsage` is `safe(input.usage.X ?? 0)` throughout (`session.ts:340-346`), with `finite()`
   mapping `NaN`/`Infinity` to 0 (`:338`).
4. v2 `publish-llm-event.ts:16-27` does the same via `safe(undefined) = 0`.

There is **no local tokenizer and no estimator** in the accounting path — the `length / 4`
heuristics in the repo are UI display (`packages/app/src/components/session/session-context-breakdown.ts:12`)
and output-truncation budgets (`session/prompt.ts:87`, `session/tools.ts:581`), and feed nothing.
So a local llama.cpp endpoint that omits `usage` produces messages recording 0 input, 0 output and
$0.00 — indistinguishable from a free model. Partial usage degrades field-by-field.

OpenCode does ask for it: `stream_options: { include_usage: true }` is unconditional on the native
path (`packages/llm/src/protocols/openai-chat.ts:360`, verified) and forced on the live ai-sdk path
for `@ai-sdk/openai-compatible` (`provider.ts:1755-1757`).

**This is precisely the plan's requirement — "unknown usage must not silently become zero while
claiming a hard budget guarantee" (`plan:127`) — and OpenCode does not satisfy it, and cannot be
made to by configuration.** The mitigation is already in tracon's architecture and should be
stated as the answer rather than logged as a gap: the gateway counts on the wire
(`UsageScanner`, `node/src/gateway/model.rs:722-790`, reading `input_tokens`/`prompt_tokens` and
the Anthropic `message_start` / OpenAI `response.completed` shapes — tests at `:1136-1150`) and
enforces the ceiling there (`:447-470`), not from harness-reported numbers. **tracon's counts are
authoritative; OpenCode's are display.**

The honest residual: if the *upstream* omits usage, the gateway's scanner sees nothing either and
tracon's own count is zero. For a self-hosted endpoint that is a property of the endpoint.
**Gate B must include: point the gateway at the llama.cpp router, run a turn, assert the gateway
recorded non-zero tokens.** If it cannot, mark that provider unmetered in the UI rather than
reporting zero — silently reporting zero against a claimed budget is the failure the plan names.

### 6.4 Reconciliation surface

Use the **v1** routes and events:

- `GET /session/:sessionID/message` → `Array<{ info, parts }>`; `info` carries required
  `cost`/`tokens`, `parts` include `step-finish` with per-step values
  (`.../httpapi/groups/session.ts:77-104, :179-190`).
- `GET /event` (SSE, `.../groups/event.ts:8,14-16`) → **`message.updated`** with
  `properties.info.cost` / `.tokens` (`packages/schema/src/v1/session.ts:597`), plus
  `message.part.updated` for `step-finish` and `session.updated` for the rollup.
- `GET /config/providers` → `{providers, default}` (`handlers/config.ts:24-30`).

The v2 `/api/...` surface and `session.next.step.ended` give correct token counts but always
`cost: 0` (§6.2). Join on `tokens.{input,output,reasoning,cache.read,cache.write}` and `cost`;
mapping OpenCode message ids to tracon usage rows is the ingestion component's job (`plan:108`).

---

## 7. Model catalogue

### 7.1 Source

`packages/core/src/models-dev.ts:160`:

```ts
const source = Flag.OPENCODE_MODELS_URL || "https://models.opencode.ai"
```

fetched as `${source}/api.json` (`:176`), 10 s timeout (`:180`), 2 jittered retries (`:146-152`),
`User-Agent: opencode/<channel>/<version>/<client>` (`:23`). **Not `models.dev`, despite the
module name** — that literal survives only in the build-time snapshot generator
(`packages/opencode/script/generate.ts:10`).

Cache (`:161-165`): `$XDG_CACHE_HOME/opencode/models.json`, or `models-<hash>.json` when
`OPENCODE_MODELS_URL` is overridden. Freshness TTL 5 min by mtime (`:166-174`), `Flock`-guarded
(`:167`), written atomically via temp-file + rename (`:204-206`).

### 7.2 When it fetches

`populate` (`:217-231`): **disk → embedded snapshot → network**.

```ts
const fromDisk = yield* loadFromDisk;  if (fromDisk) return fromDisk     // :218-219
const snapshot = yield* loadSnapshot;  if (snapshot) return snapshot     // :220-221
if (Flag.OPENCODE_DISABLE_MODELS_FETCH) return {}                        // :222
```

Separately, a **background refresh fiber** starts at layer construction and repeats hourly
(`:255-258`):

```ts
if (!Flag.OPENCODE_DISABLE_MODELS_FETCH && !process.argv.includes("--get-yargs-completions")) {
  yield* Effect.forkScoped(refresh().pipe(Effect.repeat(Schedule.spaced("60 minutes")), Effect.ignore))
}
```

**A populated disk cache does not stop the network fetch — only the flag does.** Note also that in
a dev checkout with no cache, no snapshot and the flag unset, a failed fetch becomes a defect via
`Effect.orDie` (`:230`) — i.e. a crash, not a degraded start.

### 7.3 Pinning and disabling

| Lever | Effect | Evidence |
|---|---|---|
| `OPENCODE_MODELS_PATH=<file>` | read the catalogue from an exact file; a read failure does **not** trigger a refetch or delete the file (guard on `OPENCODE_MODELS_PATH === undefined`) | `models-dev.ts:184-196`; `flag.ts:46` |
| `OPENCODE_DISABLE_MODELS_FETCH=1` | no startup fetch **and no hourly background fetch**; empty catalogue if disk and snapshot are empty | `models-dev.ts:222, :255`; `flag.ts:29` |
| `OPENCODE_MODELS_URL=<url>` | point at a tracon-served catalogue; also changes the cache filename | `models-dev.ts:161-165`; `flag.ts:45` |
| embedded snapshot `OPENCODE_MODELS_DEV` | compiled in at build time | `models-dev.ts:136, :198-200`; injected by `packages/opencode/script/build.ts:195`, `build-node.ts:23`, `packages/cli/script/build.ts:87` |
| `MODELS_DEV_API_JSON=<file>` | **build-time**: embed a local `api.json` as the snapshot | `packages/cli/script/generate.ts:3` |

**Set both `OPENCODE_MODELS_PATH` and `OPENCODE_DISABLE_MODELS_FETCH=1`.** Either alone is
insufficient: the path alone leaves the hourly fetch running; the flag alone falls back to the
build-time snapshot, a catalogue tracon did not choose and cannot update. If tracon builds its own
binary, `MODELS_DEV_API_JSON` makes the snapshot itself a reviewed artefact.

### 7.4 What tracon must feed it

With the fetch disabled and no pinned file, `populate` returns `{}` — but config-declared providers
still enter `database` with their config-declared `models` (`provider.ts:1482-1580`), and
`database[providerID] = parsed` is unconditional (`:1579`), so an unknown provider id creates a new
entry outright. **tracon can run with an empty catalogue and a picker containing exactly what it
wrote** — the strongest form of "the picker matches what the gateway will actually serve".

Config merges per-field over the catalogue (`config ?? existing ?? default`), it does not replace.
Fields to write so the UI is not misleading: `name`, `limit.context`, `limit.output`, `cost.input`,
`cost.output` (+`cache_read`/`cache_write`), `reasoning`, `attachment`, `modalities`, and `status`.
Defaults to know: `tool_call` defaults **true** (`provider.ts:1524`) — convenient for local servers;
`modalities.input.text`/`output.text` default true and all other modalities false (`:1526-1541`);
`status` defaults `"active"`, `"alpha"` is hidden unless `OPENCODE_ENABLE_EXPERIMENTAL_MODELS`, and
`"deprecated"` is always dropped (`:1693-1694`).

**Comparison with tracon PR #174** (`21b42c3`, *"fix(omp): list Codex models under openai-codex in
the probed catalogue"*): omp's catalogue is whatever its pinned build shipped, so tracon must
*filter* afterwards — `offerable()` plus the `CHATGPT_ACCOUNT_REFUSES` denylist
(`node/src/gateway/model.rs:155-186`), which its own comment calls *"a denylist keyed on the known
refusal rather than something derived"*. **OpenCode inverts this: tracon declares the catalogue
instead of probing and subtracting.** The denylist becomes unnecessary — those models simply are
not written. That is a real simplification and a genuine argument for the migration; it should be
captured as such, and the denylist **retired with the omp adapter rather than ported**.

---

## 8. Verdict table

| Provider path | Mechanism in OpenCode | Gateway-compatible? | What tracon must build | STOP-candidate? |
|---|---|---|---|---|
| **Hosted API key — `anthropic`** | `provider.anthropic.options.{baseURL,apiKey}`, `npm: @ai-sdk/anthropic` (bundled). Key as **`x-api-key`**. `custom().anthropic` adds only `anthropic-beta`, never a URL (`provider.ts:177-184`). `POST …/v1/messages` matches `ANTHROPIC_ROUTES`. | **Yes** | Write `options.baseURL = http://<gw>/model/anthropic/v1` + placeholder `apiKey`. **Stop relying on `ANTHROPIC_BASE_URL` env — OpenCode reads no base-URL env var (§2.5).** | No |
| **Hosted API key — `openai`** | `npm: @ai-sdk/openai` (bundled), `Authorization: Bearer`. `custom().openai` forces the **Responses** API (`provider.ts:212`) → `POST …/v1/responses`, on `OPENAI_ROUTES`. | **Yes** | `options.baseURL = …/model/openai/v1` + placeholder key; set `headerTimeout`/`chunkTimeout` deliberately (§4.1). | No |
| **Open-weight hosted** (groq/together/deepinfra/openrouter…) | `@ai-sdk/openai-compatible` or a bundled alias; `Authorization: Bearer`; `POST …/v1/chat/completions` on `OPENAI_ROUTES`. | **Yes** | Node-side provider entry per service, same config shape. Do not set `includeUsage: false`. | No |
| **Self-hosted OpenAI-compatible** (llama.cpp `:8080`, ollama, lm-studio) | `npm: @ai-sdk/openai-compatible`, explicit `options.baseURL`. `includeUsage` forced true (`provider.ts:1755-1757`). **No auto-probe exists** (§5.2). | **Yes** for transport. **Usage accounting is the real risk** (§6.3). | Node-owned binding to the exact service (never loopback in the runner). `enabled_providers` rather than an omp-style denylist. **Gate B must prove non-zero gateway token counts, or mark the provider unmetered.** | No — but *usage* is a gate, not a detail |
| **Anthropic subscription** (PR #166 / `cab972b`) | **No OpenCode plugin exists — removed upstream as of 1.3.0** (`providers.mdx:356-368`; zero code matches). Presents as an ordinary API-key `anthropic` provider. | **Yes, as-is.** tracon's gateway already injects `Bearer`, merges `anthropic-beta: oauth-2025-04-20`, and prepends `CLAUDE_CODE_SYSTEM` (`model.rs:43-86, 486, 496-502, 703-716`). OpenCode's own beta flags survive the merge. | **Nothing new.** Reuse the existing shaping unchanged. | No — this path gets *simpler* |
| **Codex subscription** (PR #174 / `21b42c3`) | `CodexAuthPlugin` binds provider id **`openai`** (`codex.ts:329`) and installs a token-holding, URL-rewriting `fetch` — **but only when an `oauth` record exists for that id** (`provider.ts:1614-1616`; `codex.ts:339`). tracon's provider is named `openai-codex` and its store holds no OAuth record, so the plugin is inert. | **Yes.** Treat `openai-codex` as an OpenAI-compatible provider at `…/model/openai-codex/v1`; the gateway injects `Authorization` + `chatgpt-account-id` (`broker/mod.rs:500-505`, `model.rs:700-702`) and the path `v1/responses` is on `OPENAI_CODEX_ROUTES` (`model.rs:238-247`). | Keep the provider id **≠ `openai`** and the auth store free of `oauth` records — both already true. Add `OPENCODE_DISABLE_DEFAULT_PLUGINS=1` as belt. Pin `OPENCODE_CHANNEL` at build time or use the release artefact (§3.3 WebSocket trap). Verify in Gate B that the ChatGPT backend accepts a request whose system prompt is *not* moved to `instructions` (§3.3). | **No** |

### The trap to write down

If tracon ever names its Codex provider **`openai`** *and* an `oauth` record exists for it, then a
`baseURL` of `…/v1` produces path `/v1/responses`, which `codex.ts:422` matches, and the request is
**silently rewritten to `https://chatgpt.com/backend-api/codex/responses`** — bypassing the gateway,
its allowlist, its ceiling and its usage counting entirely, with no error. And a `baseURL` *without*
`/v1` avoids the rewrite but sends the **real subscription bearer token** to whatever host is named
(`codex.ts:413-423`; upstream's own test `packages/opencode/test/plugin/codex.test.ts:219-254`).
Both are foreclosed by tracon's existing invariants — which is exactly why those invariants should
be asserted by a test, not left implicit.

### Cross-cutting must-dos

1. Pin the flag set, not just the version: `OPENCODE_EXPERIMENTAL_NATIVE_LLM` off,
   `OPENCODE_EXPERIMENTAL_WEBSOCKETS` off, `OPENCODE_CHANNEL` defined at build (§0, §3.3).
2. `OPENCODE_DISABLE_DEFAULT_PLUGINS=1` and `OPENCODE_PURE=1` (§3.6).
3. `OPENCODE_MODELS_PATH` **and** `OPENCODE_DISABLE_MODELS_FETCH=1` together (§7.3).
4. `OPENCODE_DISABLE_PROJECT_CONFIG=1`, `OPENCODE_CONFIG_DIR` read-only, controlled `$HOME` with no
   `.opencode`, authoritative provider config at `/etc/opencode/opencode.json` (§1.5).
5. `OPENCODE_AUTH_CONTENT` for the auth store — validate its JSON first, since a parse failure
   falls through to the file (§2.2).
6. Assert the auth store contains **no `oauth` and no `wellknown` entry** — the first is what makes
   every OAuth plugin inert (§3.5), the second is what prevents remote-config fetch and subprocess
   credential minting (§1.2).
7. Deny `PATCH /config`, `PUT /auth/:providerID`, and the `/provider/**` auth routes (§2.7).
8. Validate `provider.<id>.npm` against the bundled list or a `file://` path (§5.3).
9. Keep AWS env vars out of the runner and `amazon-bedrock` in `disabled_providers` (§5.2 IMDS).
10. Adapter change: base URL into config, not env (§2.5).

### Gate A recommendation

**No STOP condition. Proceed to Gate B.**

Both feasibility gates the plan named resolve favourably, and for one structural reason worth
stating plainly: OpenCode installs its credential-holding, URL-rewriting request machinery **only
when an OAuth record is present in the runner's auth store**. tracon's existing invariant — real
tokens stay in the node's broker and never reach the runner — is precisely the condition under
which that machinery never loads. The boundary tracon already enforces for its own reasons is the
boundary that makes OpenCode gateway-compatible. Anthropic subscription needs no plugin at all
(upstream removed it), and the catalogue story is strictly better than omp's, retiring the
`CHATGPT_ACCOUNT_REFUSES` denylist rather than porting it.

The open items are narrow and empirical, not architectural, and each has a known mitigation that
requires no fork:

- Bun's proxy handling against the pinned binary (§4.2) — documented and corroborated, not observed.
- Self-hosted usage reporting through the gateway (§6.3) — the one place where "unknown becomes
  zero" could quietly violate a budget guarantee.
- Whether the ChatGPT backend accepts a Codex request whose system prompt stays in the message
  array rather than `instructions` (§3.3).
- The ai-sdk packages' actual header bytes (§2.6), unverifiable from this checkout.

None should be closed by reading more source. They should be closed by running the pinned binary
against the gateway — which is what Gate B is for.
