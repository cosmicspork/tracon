<script lang="ts">
  // Standing a node up, without a shell on it.
  //
  // Four sections in the order an install meets them: prove the boundary,
  // point the node at a harness and its limits, give it credentials, and
  // decide who may reach it. What rewrites node.toml or the trust root is
  // done at the node itself — shown here with the reason, never hidden.
  import { onMount } from 'svelte'
  import { api } from '../lib/api'
  import {
    check as checkDesktopUpdate,
    desktopUpdateAction,
    install as installDesktopUpdate,
    status as desktopUpdateStatus,
  } from '../lib/desktop-update'
  import { remedy } from '../lib/refusal'
  import { modelPatch, phaseDefaults } from '../lib/bindings'
  import { changedSubset, hashToken, loginUrl, mintToken } from '../lib/settings'
  import { store } from '../lib/store.svelte'
  import type { UpdateStatus } from '../lib/desktop-update'
  import type { BoundaryCheck, EnrollStatus, NodeConfig } from '../lib/types'

  const local = $derived(store.node?.loopback ?? false)
  const refused = $derived(store.node?.state === 'refused')

  let busy = $state('')
  let error = $state('')
  let checks = $state<BoundaryCheck[] | null>(null)

  async function act(what: string, f: () => Promise<unknown>) {
    busy = what
    error = ''
    try {
      await f()
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = ''
    }
  }

  // --- boundary ---------------------------------------------------------
  function recheck() {
    return act('check', async () => {
      checks = (await api.checkBoundary()).checks.checks
    })
  }
  function setup(rebuild: boolean) {
    return act('setup', async () => {
      checks = (await api.runSetup(rebuild)).checks.checks
    })
  }

  // --- configuration ----------------------------------------------------
  let cfg = $state<NodeConfig | null>(null)
  let form = $state<NodeConfig | null>(null)
  let changed = $state<string[]>([])
  let restartOwed = $state(false)

  let configError = $state('')
  // A one-shot read at mount, not an effect: nothing reactive decides when
  // node.toml should be re-read, and saving reloads it explicitly.
  async function loadConfig() {
    try {
      const c = await api.config()
      cfg = c
      form = structuredClone(c)
      configError = ''
    } catch (e) {
      configError = e instanceof Error ? e.message : String(e)
    }
  }
  void loadConfig()

  const dirty = $derived(
    cfg && form
      ? Object.keys(changedSubset(cfg as never, form as never)).length > 0
      : false,
  )

  function saveConfig() {
    if (!cfg || !form) return
    return act('config', async () => {
      const patch = changedSubset(cfg as never, form as never)
      const res = await api.putConfig(patch)
      changed = res.changed
      restartOwed = restartOwed || res.restart_required
      await loadConfig()
    })
  }

  // --- credentials ------------------------------------------------------
  let toml = $state('')
  let imported = $state<string[]>([])
  function importCreds() {
    return act('creds', async () => {
      imported = (await api.importCredentials(toml)).imported
      toml = ''
    })
  }

  // --- channels ---------------------------------------------------------
  let channelName = $state('')
  let channelNote = $state('')
  function addChannel() {
    return act('channel', async () => {
      const res = await api.createChannel(channelName.trim())
      channelNote = res.created ? `created ${res.name}` : `${res.name} was already here`
      if (res.note) channelNote += ` · ${res.note}`
      channelName = ''
    })
  }

  // --- phase models -----------------------------------------------------
  // A channel decides which model plans and which one builds, so the operator
  // names them once instead of at every start. The node reads the same keys.
  const models = $derived.by(() => {
    const seen = new Map<string, string>()
    for (const n of store.nodes) for (const m of n.models) seen.set(m.value, m.name)
    return [...seen].map(([value, name]) => ({ value, name }))
  })
  let savedChannel = $state('')
  function bindModel(channel: string, phase: 'plan' | 'execute', model: string) {
    return act('binding', async () => {
      // A standalone node lists channels it has no row for; the create is
      // idempotent and gives the bindings somewhere to live.
      await api.createChannel(channel)
      await api.putChannelBindings(channel, modelPatch(phase, model))
      await store.refetch()
      savedChannel = channel
    })
  }

  // --- access -----------------------------------------------------------
  let issued = $state<{ token: string; svg: string } | null>(null)
  let publicUrl = $state('')
  function issueToken() {
    return act('token', async () => {
      // Minted here: the node is told the hash and never the token itself.
      const token = mintToken()
      await api.setToken(await hashToken(token))
      const base = publicUrl.trim() || location.origin
      const { svg } = await api.qr(loginUrl(base, token))
      issued = { token, svg }
    })
  }

  // --- mesh -------------------------------------------------------------
  let hubUrl = $state('')
  let meshNote = $state('')
  function initMesh() {
    return act('mesh', async () => {
      const res = await api.meshInit(hubUrl.trim())
      meshNote = `${res.hub_url} · admit this node on the hub with ${res.admit_with}`
      restartOwed = true
    })
  }

  let invitation = $state('')
  let enroll = $state<EnrollStatus | null>(null)
  let poll: ReturnType<typeof setInterval> | undefined
  function startEnroll() {
    return act('enroll', async () => {
      await api.startEnroll(invitation.trim())
      invitation = ''
      poll = setInterval(async () => {
        try {
          enroll = await api.enrollStatus()
          if (enroll.done) {
            clearInterval(poll)
            if (enroll.restart_required) restartOwed = true
          }
        } catch {
          clearInterval(poll)
        }
      }, 2000)
    })
  }

  // The node interface normally lives at a loopback origin. Its remote Tauri
  // capability is intentionally unavailable to an ordinary browser or a node
  // reached elsewhere, so a null result means there is no desktop UI to show.
  let desktopUpdate = $state<UpdateStatus | null>(null)
  let desktopUpdateError = $state('')
  const desktopAction = $derived(
    desktopUpdate ? desktopUpdateAction(desktopUpdate) : null,
  )

  onMount(() => {
    let disposed = false
    let timer: ReturnType<typeof setInterval> | undefined
    const readUpdate = async () => {
      try {
        const next = await desktopUpdateStatus()
        if (disposed || !next) return
        desktopUpdate = next
        desktopUpdateError = ''
        if (!['checking', 'downloading'].includes(next.state) && timer) {
          clearInterval(timer)
          timer = undefined
        } else if (['checking', 'downloading'].includes(next.state) && !timer) {
          timer = setInterval(() => void readUpdate(), 500)
        }
      } catch (e) {
        if (!disposed) desktopUpdateError = e instanceof Error ? e.message : String(e)
      }
    }
    void readUpdate()
    return () => {
      disposed = true
      if (timer) clearInterval(timer)
    }
  })

  async function runDesktopUpdate() {
    if (!desktopUpdate || !desktopAction?.command) return
    const prior = desktopUpdate
    desktopUpdate =
      desktopAction.command === 'check'
        ? { ...prior, state: 'checking', available_version: undefined, message: undefined }
        : { ...prior, state: 'downloading', message: undefined }
    try {
      desktopUpdate =
        desktopAction.command === 'check'
          ? await checkDesktopUpdate()
          : await installDesktopUpdate()
    } catch (e) {
      desktopUpdate = {
        ...prior,
        state: 'failed',
        available_version: undefined,
        message: e instanceof Error ? e.message : String(e),
      }
    }
  }
</script>

<div class="h4">
  Settings
  <b>{store.node?.name ?? 'this node'}</b>
  {#if restartOwed}
    <span class="chip warn r" title="The running node read its configuration at startup. Quit and reopen the app, or restart the service.">restart owed</span>
  {/if}
</div>

{#if error}<div class="banner crit">{error}</div>{/if}
{#if !local}
  <div class="banner dim">
    reached from elsewhere <b>· running this node is a full seat from here; changing what it <em>is</em> — its configuration, its hub — is done at the node itself</b>
  </div>
{/if}

<!-- 1. The boundary, first: nothing runs until it passes. -->
<section>
  <div class="h5">Boundary <b>{store.node?.state ?? '…'}</b></div>
  {#if refused && store.node}
    <p class="why">
      <b>{store.node.failed_check}: {store.node.failed_detail}</b>
      <i>{remedy(store.node.failed_check)}</i>
    </p>
  {/if}
  <div class="acts">
    <button class="btn" onclick={recheck} disabled={busy !== ''}>
      {busy === 'check' ? 'Checking…' : 'Re-check'}
    </button>
    <button class="btn p" onclick={() => setup(false)} disabled={busy !== ''}>
      {busy === 'setup' ? 'Running setup…' : 'Run setup'}
    </button>
    <button class="btn" onclick={() => setup(true)} disabled={busy !== ''}>Rebuild images</button>
  </div>
  <small>Setup creates the network, gateway and images this node's boundary needs. It builds images, so it takes minutes.</small>
  {#if checks}
    <ul class="checks">
      {#each checks as c (c.id)}
        <li><span class="chip" class:bad={!c.ok}>{c.ok ? 'ok' : 'fail'}</span> {c.id} · <small>{c.detail}</small></li>
      {/each}
    </ul>
  {/if}
</section>

<!-- 2. What it runs, and the limits it runs under. -->
<section>
  <div class="h5">Harness and limits</div>
  {#if form && cfg}
    <div class="grid">
      <label>
        <span>Harness</span>
        <select bind:value={form.harness.id} disabled={!local}>
          <option value="omp">omp</option>
          <option value="claude">claude</option>
        </select>
        <small>Running: {cfg.running.harness_id} {cfg.running.harness_version}</small>
      </label>
      <label>
        <span>Harness version</span>
        <input bind:value={form.harness.version} disabled={!local} spellcheck="false" />
      </label>
      <label>
        <span>Session budget (tokens)</span>
        <input type="number" bind:value={form.session.budget_tokens} disabled={!local} />
      </label>
      <label>
        <span>Podman binary</span>
        <input bind:value={form.boundary.podman} disabled={!local} placeholder="found on PATH" spellcheck="false" />
        <small>Empty resolves from PATH, then the usual install locations.</small>
      </label>
      <label>
        <span>Node name</span>
        <input bind:value={form.node_name} disabled={!local} spellcheck="false" />
      </label>
    </div>
    <div class="acts">
      <button class="btn p" onclick={saveConfig} disabled={!local || !dirty || busy !== ''}>
        {busy === 'config' ? 'Saving…' : 'Save'}
      </button>
      {#if changed.length}<small>wrote {changed.join(', ')}</small>{/if}
    </div>
    <small>Written to <code>{cfg.readonly.config_path}</code>, which is re-serialised: comments in that file are not preserved.</small>
  {:else if configError}
    <p class="why"><b>node.toml could not be read</b><i>{configError}</i></p>
  {:else}
    <div class="empty">Reading node.toml…</div>
  {/if}
</section>

<!-- 3. What it may use, and on whose behalf. -->
<section>
  <div class="h5">Credentials <b>connect a model provider on <a href="/nodes">Nodes</a></b></div>
  <label>
    <span>Import credentials</span>
    <textarea
      bind:value={toml}
      rows="6"
      spellcheck="false"
      placeholder={'[credentials.gh]\nchannels = ["personal"]\n[credentials.gh.env]\nGH_TOKEN = "…"'}
    ></textarea>
    <small>Sealed on arrival under this node's identity. Bind it to a peer with <code>nodes = [...]</code> and share it from Nodes.</small>
  </label>
  <div class="acts">
    <button class="btn p" onclick={importCreds} disabled={!toml.trim() || busy !== ''}>
      {busy === 'creds' ? 'Sealing…' : 'Import'}
    </button>
    {#if imported.length}<small>sealed {imported.join(', ')}</small>{/if}
  </div>
</section>

<!-- 4. What each channel runs, per phase. -->
<section>
  <div class="h5">
    Models by phase <b>a channel decides once; a session may still name its own</b>
  </div>
  {#if store.channels.length === 0}
    <div class="empty">No channels yet. Create one below.</div>
  {:else if models.length === 0}
    <div class="empty">No node offers a model yet. <a href="/nodes">Connect a provider.</a></div>
  {:else}
    <div class="phases">
      {#each store.channels as c (c.name)}
        {@const plan = phaseDefaults(c.bindings, 'plan')}
        {@const execute = phaseDefaults(c.bindings, 'execute')}
        <div class="ch">
          <span class="nm">{c.name}</span>
          <label>
            <span>Plan</span>
            <select
              value={plan.model ?? ''}
              disabled={busy !== ''}
              onchange={(e) => bindModel(c.name, 'plan', e.currentTarget.value)}
            >
              <option value="">none · the session names one</option>
              {#each models as m (m.value)}<option value={m.value}>{m.name}</option>{/each}
            </select>
          </label>
          <label>
            <span>Execute</span>
            <select
              value={execute.model ?? ''}
              disabled={busy !== ''}
              onchange={(e) => bindModel(c.name, 'execute', e.currentTarget.value)}
            >
              <option value="">none · the session names one</option>
              {#each models as m (m.value)}<option value={m.value}>{m.name}</option>{/each}
            </select>
          </label>
          <small>{savedChannel === c.name ? 'saved · handed to every node on this channel' : ''}</small>
        </div>
      {/each}
    </div>
  {/if}
</section>

<!-- 5. Who may reach it, and which mesh it belongs to. -->
<section>
  <div class="h5">Channels and access</div>
  <div class="grid">
    <label>
      <span>New channel</span>
      <input bind:value={channelName} placeholder="work" spellcheck="false" />
      <small>{channelNote || 'A channel is a key. Work on it is unreadable to a node that was never handed one.'}</small>
    </label>
    <div class="field">
      <span>Reach this node from a phone</span>
      <input bind:value={publicUrl} placeholder="https://node.tailnet.ts.net" spellcheck="false" />
      <small>Issuing rotates the token and logs every client out, including this one.</small>
    </div>
  </div>
  <div class="acts">
    <button class="btn" onclick={addChannel} disabled={!channelName.trim() || busy !== ''}>Create channel</button>
    <button class="btn" onclick={issueToken} disabled={busy !== ''}>
      {busy === 'token' ? 'Issuing…' : 'Issue an operator token'}
    </button>
  </div>
  {#if issued}
    <div class="issued">
      <p>Shown once. Scan it, or copy the token.</p>
      <div class="qr">{@html issued.svg}</div>
      <code>{issued.token}</code>
    </div>
  {/if}

  {#if local}
    <div class="grid">
      <label>
        <span>Hub (the first node)</span>
        <input bind:value={hubUrl} placeholder="https://hub.example.com" spellcheck="false" />
        <small>{meshNote || 'Mints the mesh channel here and points this node at a hub.'}</small>
      </label>
      <label>
        <span>Join a mesh</span>
        <input bind:value={invitation} placeholder="an invitation URL" spellcheck="false" />
        <small>From <code>tracon mesh invite</code>, or the Nodes screen of an enrolled node.</small>
      </label>
    </div>
    <div class="acts">
      <button class="btn" onclick={initMesh} disabled={!hubUrl.trim() || busy !== ''}>Point at hub</button>
      <button class="btn" onclick={startEnroll} disabled={!invitation.trim() || busy !== ''}>Enrol this node</button>
    </div>
    {#if enroll}
      <ul class="log">
        {#each enroll.lines as line, i (i)}<li>{line}</li>{/each}
        {#if enroll.error}<li class="bad">{enroll.error}</li>{/if}
      </ul>
    {/if}
  {/if}
  <small>Running the node under the platform's supervisor stays a shell job: <code>tracon service install</code> has to outlive the node it manages.</small>
</section>

{#if desktopUpdate}
  <section>
    <div class="h5">Desktop app</div>
    <small>
      Running v{desktopUpdate.current_version} ·
      {#if desktopUpdate.state === 'current'}
        Up to date
      {:else if desktopUpdate.state === 'available'}
        v{desktopUpdate.available_version} is ready to install
      {:else if desktopUpdate.state === 'failed'}
        {desktopUpdate.message || desktopUpdateError}
      {:else if desktopUpdate.state === 'unsupported'}
        {desktopUpdate.message}
      {:else if desktopUpdate.state === 'checking'}
        Checking the latest release…
      {:else if desktopUpdate.state === 'downloading'}
        Downloading and verifying the update…
      {:else}
        Check GitHub for a newer release.
      {/if}
    </small>
    {#if desktopAction}
      <div class="acts">
        <button
          class="btn p"
          onclick={runDesktopUpdate}
          disabled={desktopAction.disabled}
        >
          {desktopAction.label}
        </button>
      </div>
    {/if}
  </section>
{/if}

<style>
  section {
    display: grid;
    gap: 8px;
    background: var(--s1);
    border-radius: 4px;
    padding: 14px 16px;
    max-width: 720px;
  }
  .h5 {
    font: 500 13px var(--sans);
    color: var(--ink);
  }
  .h5 b {
    font-weight: 400;
    color: var(--dim);
    margin-left: 6px;
  }
  .grid {
    display: grid;
    gap: 12px;
    grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
  }
  label,
  .field {
    display: grid;
    gap: 5px;
    min-width: 0;
  }
  label > span,
  .field > span {
    font: 500 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink2);
  }
  input,
  select,
  textarea {
    background: var(--s2);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13.5px var(--sans);
    min-width: 0;
  }
  textarea {
    font: 12.5px var(--mono);
    resize: vertical;
  }
  input:disabled,
  select:disabled,
  textarea:disabled {
    opacity: 0.5;
  }
  small {
    font-size: 12.5px;
    color: var(--dim);
  }
  .acts {
    display: flex;
    gap: 8px;
    align-items: center;
    flex-wrap: wrap;
  }
  .why {
    margin: 0;
    font: 12.5px var(--mono);
    color: var(--crit);
  }
  .why b {
    font-weight: 400;
    display: block;
  }
  .why i {
    font-style: normal;
    color: var(--ink2);
    display: block;
    margin-top: 3px;
  }
  .checks,
  .log {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 4px;
    font: 12.5px var(--mono);
  }
  .log li {
    color: var(--ink2);
  }
  .log li.bad {
    color: var(--crit);
  }
  .issued {
    display: grid;
    gap: 8px;
    justify-items: start;
  }
  .issued p {
    margin: 0;
    color: var(--wait);
    font: 12.5px var(--mono);
  }
  .qr :global(svg) {
    width: 180px;
    height: 180px;
    background: #fff;
    padding: 8px;
    border-radius: 4px;
  }
  .issued code {
    font: 12px var(--mono);
    color: var(--ink);
    background: var(--s2);
    padding: 6px 8px;
    border-radius: 4px;
    word-break: break-all;
  }
  .chip.r {
    margin-left: auto;
  }
  .phases {
    display: grid;
    gap: 6px;
  }
  .ch {
    display: grid;
    grid-template-columns: minmax(90px, 140px) minmax(0, 1fr) minmax(0, 1fr) auto;
    gap: 12px;
    align-items: end;
    background: var(--s1);
    border-radius: 4px;
    padding: 10px 12px;
  }
  .ch .nm {
    font: 500 13.5px var(--sans);
    color: var(--ink);
    align-self: center;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .ch label {
    display: grid;
    gap: 4px;
    min-width: 0;
  }
  .ch small {
    align-self: center;
    color: var(--ok);
    font: 11.5px var(--mono);
  }
  @media (max-width: 700px) {
    .ch {
      grid-template-columns: minmax(0, 1fr);
      gap: 8px;
    }
  }
</style>
