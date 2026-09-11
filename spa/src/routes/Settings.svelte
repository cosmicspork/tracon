<script lang="ts">
  // Standing a node up, without a shell on it.
  //
  // Sections in the order an install meets them: prove the boundary, point the
  // node at a harness and its limits, give it credentials, say what each
  // channel runs, decide who may reach it, and pair a hub. What rewrites
  // node.toml or the trust root is done at the node itself — shown here with
  // the reason, never hidden.
  import { onMount } from 'svelte'
  import CredentialSettings from '../components/CredentialSettings.svelte'
  import ModelPicker from '../components/ModelPicker.svelte'
  import { api } from '../lib/api'
  import {
    check as checkDesktopUpdate,
    desktopUpdateAction,
    desktopUpdatesAvailable,
    install as installDesktopUpdate,
    status as desktopUpdateStatus,
  } from '../lib/desktop-update'
  import {
    installCli as installDesktopCli,
    restartNode as restartDesktopNode,
    setupStatus as desktopSetupStatus,
    type SetupStatus,
  } from '../lib/desktop-setup'
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { remedy } from '../lib/refusal'
  import { modelPatch, phaseDefaults } from '../lib/bindings'
  import { recentModelValues } from '../lib/models'
  import { changedSubset, hashToken, loginUrl, mintToken } from '../lib/settings'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'
  import type { UpdateStatus } from '../lib/desktop-update'
  import type { AuthorityGrant, BoundaryCheck, EnrollStatus, NodeConfig } from '../lib/types'

  const local = $derived(store.node?.loopback ?? false)
  const origin = typeof location === 'undefined' ? '' : location.origin
  const externalChannels = $derived(
    store.channels.filter((c) => !c.archived).map((c) => c.name),
  )
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


  // --- channels ---------------------------------------------------------
  let channelName = $state('')
  let channelNote = $state('')
  function addChannel() {
    return act('channel', async () => {
      const res = await api.createChannel(channelName.trim())
      channelNote = res.created ? `created ${res.name}` : `${res.name} was already here`
      if (res.note) channelNote += ` · ${res.note}`
      await store.refetch()
      channelName = ''
    })
  }


  // --- authority ---------------------------------------------------------
  let authority = $state<{ policy: { version: number; rules: unknown[] }; grants: AuthorityGrant[] } | null>(null)
  let grant = $state({
    action: 'merge' as AuthorityGrant['action'],
    verdict: 'allow' as AuthorityGrant['verdict'],
    target: '',
    channel: '',
    session_id: null as string | null,
    revision: null as string | null,
    expires_ms: null as number | null,
    reason: '',
  })
  async function loadAuthority() {
    authority = await api.authorityGrants()
  }
  void loadAuthority()
  function saveGrant() {
    return act('authority', async () => {
      await api.createAuthorityGrant({
        ...grant,
        channel: grant.channel.trim() || (store.node?.default_channel ?? 'personal'),
        target: grant.target.trim(),
        revision: grant.revision?.trim() || null,
        session_id: grant.session_id?.trim() || null,
        reason: grant.reason.trim(),
      })
      grant = { action: 'merge', verdict: 'allow', target: '', channel: '', session_id: null, revision: null, expires_ms: null, reason: '' }
      await loadAuthority()
    })
  }
  function revokeGrant(id: string) {
    return act('authority', async () => {
      await api.revokeAuthorityGrant(id)
      await loadAuthority()
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
  const recentModels = $derived(recentModelValues(store.sessions.values()))
  let savedChannel = $state('')
  const open_channels = $derived(store.channels.filter((c) => !c.archived))
  const archived_channels = $derived(store.channels.filter((c) => c.archived))
  function archiveChannel(name: string, away: boolean) {
    return act('channel-archive', async () => {
      await (away ? api.archiveChannel(name) : api.unarchiveChannel(name))
      await store.refetch()
    })
  }
  let deleting = $state('')
  function deleteChannel(name: string) {
    return act('channel-delete', async () => {
      await api.deleteChannel(name)
      deleting = ''
      await store.refetch()
    })
  }
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
  const paired = $derived(store.mesh !== null && store.mesh.hub.state !== 'disabled')
  let unpairing = $state(false)
  let unpaired = $state(false)
  const hubState = $derived(
    unpaired
      ? 'unpaired · restart to disconnect'
      : store.mesh?.hub.state === 'connected'
        ? 'connected'
        : store.mesh?.hub.state === 'unreachable'
          ? 'unreachable'
          : 'not paired',
  )
  const hubHost = $derived.by(() => {
    const url = store.mesh?.hub_url
    if (!url) return ''
    try {
      return new URL(url).host
    } catch {
      return url
    }
  })
  function unpair() {
    return act('unpair', async () => {
      const res = await api.meshUnpair()
      unpairing = false
      unpaired = true
      if (res.restart_required) restartOwed = true
    })
  }
  let hubUrl = $state('')
  let meshInit = $state<Awaited<ReturnType<typeof api.meshInit>> | null>(null)
  let admitCopied = $state(false)
  let admitCopyError = $state('')
  function initMesh() {
    return act('mesh', async () => {
      meshInit = await api.meshInit(hubUrl.trim())
      admitCopied = false
      admitCopyError = ''
      restartOwed = true
    })
  }
  async function copyAdmit() {
    if (!meshInit) return
    admitCopyError = ''
    try {
      await navigator.clipboard.writeText(meshInit.admit_with)
      admitCopied = true
    } catch (e) {
      admitCopyError = e instanceof Error ? e.message : String(e)
    }
  }

  let invitation = $state('')
  let enroll = $state<EnrollStatus | null>(null)
  let enrollPollError = $state('')
  let poll: ReturnType<typeof setInterval> | undefined
  function stopEnrollPoll() {
    if (poll) clearInterval(poll)
    poll = undefined
  }
  async function readEnroll(): Promise<boolean> {
    try {
      const next = await api.enrollStatus()
      enroll = next
      enrollPollError = ''
      if (next.done) {
        stopEnrollPoll()
        if (next.restart_required) restartOwed = true
        await store.refetch()
      }
      return next.done
    } catch (e) {
      stopEnrollPoll()
      enrollPollError = e instanceof Error ? e.message : String(e)
      return true
    }
  }
  function startEnroll() {
    return act('enroll', async () => {
      stopEnrollPoll()
      enroll = null
      enrollPollError = ''
      await api.startEnroll(invitation.trim())
      invitation = ''
      const done = await readEnroll()
      if (!done) poll = setInterval(() => void readEnroll(), 2000)
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
  let desktopSetup = $state<SetupStatus | null>(null)
  let desktopSetupError = $state('')
  let desktopBusy = $state(false)

  onMount(() => {
    if (desktopUpdatesAvailable()) {
      desktopSetupStatus()
        .then((s) => (desktopSetup = s))
        .catch(() => {})
    }
  })

  async function runDesktop(f: () => Promise<SetupStatus>) {
    desktopBusy = true
    desktopSetupError = ''
    try {
      desktopSetup = await f()
    } catch (e) {
      desktopSetupError = e instanceof Error ? e.message : String(e)
    } finally {
      desktopBusy = false
    }
  }

  // Hash navigation is keyed by the router revision rather than mount alone:
  // clicking the same in-app destination must focus it again after async
  // sections above it have changed height.
  $effect(() => {
    const revision = router.revision
    const hash = router.hash
    void revision
    if (hash !== '#mesh' && hash !== '#boundary') return
    let last = -1
    let tries = 0
    let timer: ReturnType<typeof setTimeout>
    const settle = () => {
      const el = document.getElementById(hash.slice(1))
      if (!el) return
      const top = el.getBoundingClientRect().top + window.scrollY
      if (top !== last) {
        last = top
        el.scrollIntoView({ block: 'start' })
      }
      if (++tries < 12) timer = setTimeout(settle, 100)
    }
    timer = setTimeout(settle, 0)
    return () => clearTimeout(timer)
  })
  onMount(() => stopEnrollPoll)

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
    reached from elsewhere <b>· configuration and hub pairing are changed at the node itself</b>
  </div>
{/if}

<!-- 1. The boundary, first: nothing runs until it passes. -->
<section id="boundary">
  <div class="h5">Boundary <b>{store.node?.state ?? '…'}</b></div>
  {#if refused && store.node}
    <p class="why">
      <b>{store.node.failed_check}: {store.node.failed_detail}</b>
      <i>{remedy(store.node.failed_check)}</i>
    </p>
  {/if}
  <div class="acts">
    <button class="btn" onclick={recheck} disabled={busy !== ''} title="Run the boundary checks again">
      {busy === 'check' ? 'Checking…' : 'Re-check'}
    </button>
    <button
      class="btn p"
      onclick={() => setup(false)}
      disabled={busy !== ''}
      title="Create the network, gateway and images the boundary needs. Builds images, so it takes minutes"
    >
      {busy === 'setup' ? 'Running setup…' : 'Run setup'}
    </button>
    <button
      class="btn"
      onclick={() => setup(true)}
      disabled={busy !== ''}
      title="Rebuild the gateway and harness images from scratch"
    >
      Rebuild images
    </button>
  </div>
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
        <span>Default channel</span>
        <select bind:value={form.session.default_channel} disabled={!local}>
          <option value="">none</option>
          {#each open_channels as c (c.name)}<option value={c.name}>{c.name}</option>{/each}
        </select>
        <small>What the composer starts on, until a browser picks another and keeps it.</small>
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
  {:else if configError}
    <p class="why"><b>node.toml could not be read</b><i>{configError}</i></p>
  {:else}
    <div class="empty">Reading node.toml…</div>
  {/if}
</section>

<!-- 3. What it may use, and on whose behalf. -->
<section id="credentials">
  <div class="h5">Credentials <b>provider and forge access held by this node</b></div>
  <CredentialSettings />
</section>
<section id="authority">
  <div class="h5">Authority <b>signed policy is inspectable; local grants are narrow and revocable</b></div>
  {#if authority}
    <p class="why">Policy bundle version {authority.policy.version} has {authority.policy.rules.length} signed rules. This interface cannot change signing keys, trust roots, or policy text.</p>
    <div class="grid">
      <label><span>Action</span><select bind:value={grant.action} disabled={!local}><option value="merge">merge</option><option value="publish">publish</option><option value="ticket_transition">ticket transition</option><option value="deploy">deploy</option></select></label>
      <label><span>Decision</span><select bind:value={grant.verdict} disabled={!local}><option value="allow">allow</option><option value="ask">ask</option><option value="deny">deny</option></select></label>
      <label><span>Canonical target</span><input bind:value={grant.target} disabled={!local} placeholder="github:owner/repo:pr:42" /></label>
      <label><span>Channel</span><input bind:value={grant.channel} disabled={!local} placeholder={store.node?.default_channel ?? 'personal'} /></label>
      <label><span>Immutable revision (optional)</span><input bind:value={grant.revision} disabled={!local} placeholder="commit SHA" /></label>
      <label><span>Expires at (optional)</span><input type="datetime-local" value={grant.expires_ms ? new Date(grant.expires_ms).toISOString().slice(0, 16) : ''} onchange={(e) => grant.expires_ms = e.currentTarget.value ? Date.parse(e.currentTarget.value) : null} disabled={!local} /></label>
      <label><span>Reason</span><input bind:value={grant.reason} disabled={!local} placeholder="why this precise action is permitted" /></label>
    </div>
    <div class="acts"><button class="btn p" onclick={saveGrant} disabled={!local || busy !== ''}>Add scoped grant</button></div>
    {#if authority.grants.length}
      <div class="credentials">
        {#each authority.grants as item (item.id)}
          <div class:dim={item.revoked_ms !== null} class="credential">
            <b>{item.verdict} {item.action}</b>
            <span>{item.target} · {item.channel}{item.revision ? ` · ${item.revision}` : ''}{item.expires_ms ? ` · expires ${new Date(item.expires_ms).toLocaleString()}` : ''}</span>
            <small>{item.reason}</small>
            {#if item.revoked_ms === null}<button class="btn" onclick={() => revokeGrant(item.id)} disabled={!local || busy !== ''}>Revoke</button>{:else}<small>revoked</small>{/if}
          </div>
        {/each}
      </div>
    {:else}<div class="empty">No local grants. Unmatched consequential actions ask.</div>{/if}
  {:else}<div class="empty">Reading signed policy and local grants…</div>{/if}
</section>

<!-- 4. The channels, what each runs, and which are still in use. -->
<section>
  <div class="h5">
    Channels <b>create channels, choose Plan and Execute defaults, and archive or restore them</b>
  </div>
  <label class="new-channel">
    <span>New channel</span>
    <input bind:value={channelName} placeholder="work" spellcheck="false" />
    <small>{channelNote || 'A channel is a key. Work on it is unreadable to a node that was never handed one.'}</small>
  </label>
  <div class="acts">
    <button class="btn" onclick={addChannel} disabled={!channelName.trim() || busy !== ''}>Create channel</button>
  </div>
  {#if store.channels.length === 0}
    <div class="empty">No channels yet.</div>
  {:else}
    {#if models.length === 0}
      <div class="empty">No node offers a model yet. Channel lifecycle is still available; <a href="/nodes">connect a provider</a> to choose defaults.</div>
    {/if}
    <div class="phases">
      {#each open_channels as c (c.name)}
        {@const plan = phaseDefaults(c.bindings, 'plan')}
        {@const execute = phaseDefaults(c.bindings, 'execute')}
        <div class="ch">
          <span class="nm">{c.name}</span>
          {#each [['plan', plan], ['execute', execute]] as const as [ph, b] (ph)}
            <label>
              <span>{ph === 'plan' ? 'Plan' : 'Execute'}</span>
              <ModelPicker
                value={b.model ?? ''}
                {models}
                recent={recentModels}
                none="none · the session names one"
                disabled={busy !== ''}
                onchange={(v) => bindModel(c.name, ph, v)}
              />
            </label>
          {/each}
          <div class="end">
            {#if savedChannel === c.name}<small>saved · handed to every node</small>{/if}
            <button class="lnk" disabled={busy !== ''} onclick={() => archiveChannel(c.name, true)}>archive</button>
          </div>
        </div>
      {/each}
    </div>
    {#if archived_channels.length}
      <div class="h5 sub">
        Archived <b>{archived_channels.length} · their work is kept; no new session starts on them</b>
      </div>
      <div class="phases">
        {#each archived_channels as c (c.name)}
          <div class="ch off">
            <span class="nm">{c.name}</span>
            {#if deleting === c.name}
              <span class="note crit">
                Deletes the channel and its key from this node. Its sessions and work stay in history.
              </span>
              <div class="end">
                <button class="lnk d" disabled={busy !== ''} onclick={() => deleteChannel(c.name)}>
                  {busy === 'channel-delete' ? 'Deleting…' : 'delete'}
                </button>
                <button class="lnk" disabled={busy !== ''} onclick={() => (deleting = '')}>keep</button>
              </div>
            {:else}
              <span class="note">archived · {c.nodes.length} node{c.nodes.length === 1 ? '' : 's'} still hold its key</span>
              <div class="end">
                <button class="lnk" disabled={busy !== ''} onclick={() => archiveChannel(c.name, false)}>bring back</button>
                <button class="lnk d" disabled={busy !== ''} onclick={() => (deleting = c.name)}>delete</button>
              </div>
            {/if}
          </div>
        {/each}
      </div>
    {/if}
  {/if}
</section>

<!-- 5. Who may reach it. -->
<section>
  <div class="h5">Access</div>
  <div class="field access-field">
    <span>Reach this node from another device</span>
    <input bind:value={publicUrl} placeholder="https://node.tailnet.ts.net" spellcheck="false" />
    <small>
      Scan the code from a phone, or paste the token into a browser. On iOS add the page to the Home
      Screen so push can reach it. Issuing rotates the token and logs every client out, including this one.
    </small>
  </div>
  <div class="acts">
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

  <div class="h5 sub">
    Your own harness <b>a terminal you run, using this node's tools</b>
  </div>
  <p class="lede">
    A terminal you run can use this node's tools without ever holding a credential. It runs
    as you, outside the boundary; reviews still go through sessions the node starts.
  </p>
  {#if !cfg}
    <small>Loading…</small>
  {:else if !cfg.external.enabled}
    <small>Off. Set <code>[external] enabled = true</code> in node.toml and restart.</small>
  {:else if externalChannels.length === 0}
    <small>Create a channel first.</small>
  {:else}
    <div class="mcp">
      {#each externalChannels as name (name)}
        <code>claude mcp add --transport http tracon-{name} {origin}/mcp/external/{name}</code>
      {/each}
    </div>
    {#if !local}
      <small>From another machine, add <code>--header "Authorization: Bearer &lt;operator token&gt;"</code>.</small>
    {/if}
  {/if}
</section>

<!-- 6. The hub: how this node and your others reach each other. -->
<section id="mesh">
  <div class="h5">
    Hub <b>{hubState}</b>
  </div>
  {#if paired && store.mesh}
    {@const m = store.mesh}
    {@const down = m.hub.state === 'unreachable'}
    <div class="hub" class:off={down || unpaired}>
      <span class="bar"></span>
      <span class="nm">
        {hubHost}
        <small>{m.hub_url}</small>
      </span>
      <span class="st">
        <span class="l" class:off={down}>
          <span class="chip" class:off={down}>{down ? 'unreachable' : 'connected'}</span>
          {#if down && m.hub.state === 'unreachable'}
            · since {formatAge(m.hub.since_ms, clock.now)} ago
          {:else if m.last_ok_ms}
            · heard {formatAge(m.last_ok_ms, clock.now)} ago
          {/if}
        </span>
        <span>
          {m.queued} queued · {m.delivered_since_reconnect} delivered since reconnect{m.undecryptable
            ? ` · ${m.undecryptable} unreadable`
            : ''}
        </span>
        {#if m.last_error}<span class="l bad">{m.last_error}</span>{/if}
        {#if m.last_refusal}<span class="l bad">refused: {m.last_refusal}</span>{/if}
      </span>
      <div class="end">
        {#if !local}
          <small>changed at the node</small>
        {:else if unpaired}
          <small>restart to disconnect</small>
        {:else if unpairing}
          <span class="note crit">Forgets the hub on restart. Channels and keys stay; joining again is one invitation.</span>
          <button class="lnk d" disabled={busy !== ''} onclick={unpair}>{busy === 'unpair' ? 'Unpairing…' : 'unpair'}</button>
          <button class="lnk" disabled={busy !== ''} onclick={() => (unpairing = false)}>keep</button>
        {:else}
          <button class="lnk d" disabled={busy !== ''} onclick={() => (unpairing = true)}>unpair</button>
        {/if}
      </div>
    </div>
  {:else}
    <small>A hub is a small always-on relay your nodes dial out to. Without one this node is reachable only where it is.</small>
  {/if}
  {#if local && !paired}
    <div class="pairing">
      <label>
        <span>Join an existing hub</span>
        <input bind:value={invitation} placeholder="full invitation URL" spellcheck="false" />
        <small>Paste the URL from <code>tracon mesh invite</code>, or from the Nodes screen of a node already on the mesh.</small>
      </label>
      <div class="acts">
        <button class="btn" onclick={startEnroll} disabled={!invitation.trim() || busy !== ''}>
          {busy === 'enroll' ? 'Joining…' : 'Join hub'}
        </button>
      </div>
      {#if enroll}
        <ul class="log">
          {#each enroll.lines as line, i (i)}<li>{line}</li>{/each}
          {#if enroll.error}<li class="bad">{enroll.error}</li>{/if}
        </ul>
        {#if enroll.done && enroll.channels.length}
          <small>Channels received: {enroll.channels.join(', ')}.</small>
        {/if}
        {#if enroll.done && enroll.restart_required}
          <small class="good">Joined. Restart this node to connect it to the hub.</small>
        {/if}
      {/if}
      {#if enrollPollError}<small class="bad">{enrollPollError}</small>{/if}

      <details class="first-node">
        <summary>Set up the first node</summary>
        <div class="first-node-body">
          <label>
            <span>Hub URL</span>
            <input bind:value={hubUrl} placeholder="https://hub.example.com" spellcheck="false" />
            <small>Creates the mesh channel here and saves the hub this node will trust.</small>
          </label>
          <div class="acts">
            <button class="btn" onclick={initMesh} disabled={!hubUrl.trim() || busy !== ''}>
              {busy === 'mesh' ? 'Preparing…' : 'Prepare first node'}
            </button>
          </div>
          {#if meshInit}
            <div class="mesh-result">
              <b>Node prepared</b>
              <small>Hub URL: {meshInit.hub_url}</small>
              <label>
                <span>Hub admission value</span>
                <input value={meshInit.admit_with} readonly />
              </label>
              <div class="copy-value">
                <button class="lnk" onclick={copyAdmit}>{admitCopied ? 'Copied' : 'Copy'}</button>
                {#if admitCopyError}<small class="bad">{admitCopyError}</small>{/if}
              </div>
              <ol>
                <li>Configure and start the hub with that exact value.</li>
                <li>Restart tracon on this node so it dials the hub.</li>
              </ol>
            </div>
          {/if}
        </div>
      </details>
    </div>
  {:else if !local && !paired}
    <small>Which hub a node belongs to is decided at the node itself.</small>
  {/if}
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
    {#if desktopSetup}
      <small>
        Node v{desktopSetup.node_version ?? '?'} ·
        {desktopSetup.owner === 'service'
          ? 'runs under the service'
          : desktopSetup.owner === 'migrated'
            ? 'runs inside the app; restart the app to move it under the service'
            : 'started outside the app'}
        · CLI {desktopSetup.cli_version ? `v${desktopSetup.cli_version}` : 'not installed'}
        {#if desktopSetupError}· {desktopSetupError}{/if}
      </small>
      <div class="acts">
        {#if desktopSetup.sidecar_version && desktopSetup.cli_version !== desktopSetup.sidecar_version}
          <button class="btn" disabled={desktopBusy} onclick={() => runDesktop(installDesktopCli)}>
            Install CLI v{desktopSetup.sidecar_version}
          </button>
        {/if}
        {#if desktopSetup.owner === 'service'}
          <button class="lnk d" disabled={desktopBusy} onclick={() => runDesktop(restartDesktopNode)}>
            Restart the node (ends running sessions)
          </button>
        {/if}
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
  select {
    background: var(--s2);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13.5px var(--sans);
    min-width: 0;
  }
  input:disabled,
  select:disabled {
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
  .mcp {
    display: grid;
    gap: 6px;
  }
  .mcp code {
    font: 12px var(--mono);
    color: var(--ink);
    background: var(--s2);
    padding: 6px 8px;
    border-radius: 4px;
    word-break: break-all;
  }
  .lede {
    margin: 0 0 4px;
    color: var(--ink2);
    font-size: 13.5px;
    max-width: 64ch;
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
  .ch .end {
    align-self: center;
    display: flex;
    gap: 12px;
    align-items: baseline;
    justify-content: flex-end;
  }
  .ch .end small {
    color: var(--ok);
    font: 11.5px var(--mono);
  }
  .ch .end .lnk {
    font-size: 12.5px;
  }
  .ch.off {
    grid-template-columns: minmax(90px, 140px) minmax(0, 1fr) auto;
    color: var(--dim);
  }
  .ch .note {
    align-self: center;
    font: 11.5px var(--mono);
    color: var(--dim);
  }
  .ch .note.crit {
    color: var(--crit);
  }
  /* The hub, drawn like a node card: a bar for state, a name, a status column. */
  .hub {
    display: grid;
    grid-template-columns: 3px minmax(120px, 170px) minmax(0, 1fr) auto;
    gap: 0 14px;
    background: var(--s2);
    border-radius: 4px;
    padding: 11px 14px 11px 0;
    overflow: hidden;
  }
  .hub .bar {
    align-self: stretch;
    border-radius: 2px 0 0 2px;
    background: var(--ok);
  }
  .hub.off .bar {
    background: var(--dim);
  }
  .hub .nm {
    font-weight: 600;
    min-width: 0;
  }
  .hub .nm small {
    display: block;
    font: 11.5px var(--mono);
    color: var(--dim);
    font-weight: 400;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .hub .st {
    display: flex;
    flex-direction: column;
    gap: 3px;
    font: 12.5px var(--mono);
    color: var(--ink2);
    min-width: 0;
  }
  .hub .st span {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .hub .st .l.off {
    color: var(--dim);
  }
  .hub .st .l.bad {
    color: var(--crit);
    white-space: normal;
  }
  .hub .end {
    align-self: center;
    display: flex;
    flex-direction: column;
    gap: 4px;
    align-items: flex-end;
    text-align: right;
    max-width: 260px;
  }
  .hub .end .lnk {
    font-size: 12.5px;
  }
  .hub .end .note {
    font: 11.5px var(--mono);
  }
  .hub .end .note.crit {
    color: var(--crit);
  }
  @media (max-width: 700px) {
    .hub {
      grid-template-columns: 3px minmax(0, 1fr);
      gap: 4px 12px;
    }
    .hub .st,
    .hub .end {
      grid-column: 2;
      align-items: flex-start;
      text-align: left;
    }
  }
  .h5.sub {
    margin-top: 14px;
  }
  .pairing,
  .first-node-body,
  .mesh-result {
    display: grid;
    gap: 8px;
  }
  .first-node {
    border-top: 1px solid var(--s3);
    padding-top: 10px;
    margin-top: 4px;
  }
  .first-node summary {
    cursor: pointer;
    color: var(--ink2);
    font: 500 12.5px var(--mono);
  }
  .first-node-body {
    margin-top: 10px;
  }
  .mesh-result {
    background: var(--s2);
    border-radius: 4px;
    padding: 10px;
    color: var(--ink2);
    font-size: 12.5px;
  }
  .mesh-result b {
    color: var(--ok);
    font-weight: 500;
  }
  .mesh-result ol {
    margin: 0;
    padding-left: 20px;
  }
  .copy-value {
    display: flex;
    gap: 10px;
    align-items: baseline;
  }
  small.good {
    color: var(--ok);
  }
  small.bad {
    color: var(--crit);
  }
  @media (max-width: 700px) {
    .ch {
      grid-template-columns: minmax(0, 1fr);
      gap: 8px;
    }
  }
</style>
