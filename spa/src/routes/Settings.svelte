<script lang="ts">
  // Canonical configuration home. Connections can target one selected node;
  // shared channels, devices, mesh, policy, and maintenance retain distinct
  // scopes so a local host change is never mistaken for a peer or device change.
  import { onMount, tick } from 'svelte'
  import LaunchManifest from '../components/LaunchManifest.svelte'
  import { api } from '../lib/api'
  import { admin } from '../lib/admin'
  import ChannelMeters from '../components/ChannelMeters.svelte'
  import ChannelNotifications from '../components/ChannelNotifications.svelte'
  import CredentialSettings from '../components/CredentialSettings.svelte'
  import Credentials from '../components/Credentials.svelte'
  import Notifications from '../components/Notifications.svelte'
  import ProviderCard from '../components/ProviderCard.svelte'
  import HubRollups from '../components/HubRollups.svelte'
  import TransferInbox from '../components/TransferInbox.svelte'
  import AdminAccess from '../components/settings/AdminAccess.svelte'
  import Maintenance from '../components/settings/Maintenance.svelte'
  import MeshAdministration from '../components/settings/MeshAdministration.svelte'
  import PolicyManagement from '../components/settings/PolicyManagement.svelte'
  import ModelPicker from '../components/ModelPicker.svelte'
  import {
    check as checkDesktopUpdate,
    desktopUpdateAction,
    desktopUpdatesAvailable,
    install as installDesktopUpdate,
    status as desktopUpdateStatus,
  } from '../lib/desktop-update'
  import {
    installCli as installDesktopCli,
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
  import type { AuthorityGrant, BoundaryCheck, EnrollStatus, NodeConfig, PolicyRule } from '../lib/types'

  const local = $derived(store.node?.loopback ?? false)
  const origin = typeof location === 'undefined' ? '' : location.origin
  const params = $derived(new URLSearchParams(router.search))
  const sections = ['connections', 'channels', 'devices', 'mesh', 'policies', 'maintenance']
  const activeSection = $derived(sections.includes(router.hash.slice(1)) ? router.hash.slice(1) : 'connections')
  let adminRevision = $state(0)
  const requestedNode = $derived(params.get('node'))
  const selectedNode = $derived(
    store.nodes.find((node) => node.id === requestedNode) ??
      store.node ??
      null,
  )
  const selectedNodeId = $derived(selectedNode?.id ?? '')
  const selectedProviders = $derived(
    selectedNode?.is_self ? store.providers : (selectedNode?.providers ?? []),
  )
  const selectedNodeName = $derived(selectedNode?.name ?? 'the serving node')
  const selectedNodeIsServing = $derived(selectedNode?.id === store.node?.id)
  const externalChannels = $derived(
    store.channels.filter((c) => !c.archived).map((c) => c.name),
  )
  const refused = $derived(store.node?.state === 'refused')

  function selectNode(id: string) {
    router.go(`/settings?node=${encodeURIComponent(id)}#connections`)
  }

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


  let authority = $state<{ policy: { version: number; rules: PolicyRule[]; trusted: boolean }; grants: AuthorityGrant[] } | null>(null)
  let grant = $state({
    action: 'merge' as AuthorityGrant['action'],
    verdict: 'ask' as AuthorityGrant['verdict'],
    target: '',
    channel: '',
    session_id: null as string | null,
    revision: null as string | null,
    expires_ms: null as number | null,
    reason: '',
  })
  let grantResource = $state<'repository' | 'deployment' | 'ticket' | 'advanced'>('repository')
  let grantConfirmation = $state<Omit<AuthorityGrant, 'id' | 'revoked_ms' | 'created_ms'> | null>(null)
  let revoking = $state('')
  const grantTargetHint = $derived(
    grantResource === 'repository'
      ? 'github:owner/repo:pr:42'
      : grantResource === 'deployment'
        ? 'gitlab:group/project:environment:qa — or qa:<target>:command:<binary> for a command target'
        : grantResource === 'ticket'
          ? 'github:owner/repo:issue:42'
          : 'canonical, no-space identifier',
  )
  async function loadAuthority() {
    authority = await api.authorityGrants()
  }
  void loadAuthority()
  function preparedGrant(): Omit<AuthorityGrant, 'id' | 'revoked_ms' | 'created_ms'> {
    return {
      ...grant,
      channel: grant.channel.trim() || (store.node?.default_channel ?? 'personal'),
      target: grant.target.trim(),
      revision: grant.revision?.trim() || null,
      session_id: grant.session_id?.trim() || null,
      reason: grant.reason.trim(),
    }
  }
  function reviewGrant() {
    const next = preparedGrant()
    if (!next.target || /\s/.test(next.target) || !next.reason) {
      error = 'Choose one canonical target and explain why this narrow local grant is needed.'
      return
    }
    grantConfirmation = next
  }
  function saveGrant() {
    const confirmedGrant = grantConfirmation
    if (!confirmedGrant) return
    return act('authority', async () => {
      await api.createAuthorityGrant(confirmedGrant)
      grant = { action: 'merge', verdict: 'ask', target: '', channel: '', session_id: null, revision: null, expires_ms: null, reason: '' }
      grantConfirmation = null
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
  let rotatingToken = $state(false)
  function confirmIssueToken() {
    rotatingToken = false
    return issueToken()
  }

  let issued = $state<{ token: string; svg: string; applied: boolean } | null>(null)
  let publicUrl = $state('')
  function issueToken() {
    return act('token', async () => {
      // Minted here: the node is told the hash and never the token itself.
      const token = mintToken()
      const base = publicUrl.trim() || location.origin
      const { svg } = await api.qr(loginUrl(base, token))
      // Retain the replacement before revoking any session. QR generation or
      // a lost response must not discard the only copy of a newly active token.
      issued = { token, svg, applied: false }
      await api.setToken(await hashToken(token))
      issued.applied = true
      try {
        await admin.login(token)
      } finally {
        adminRevision += 1
      }
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

  // Keep section navigation visible and focus the selected panel after it mounts.
  $effect(() => {
    void router.revision
    const section = activeSection
    void tick().then(() => {
      const panel = document.getElementById(`settings-${section}`)
      if (panel) {
        panel.focus({ preventScroll: true })
        window.scrollTo({ top: 0 })
      }
    })
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

<nav class="section-nav" aria-label="Settings sections">
  <a href="#connections" class:on={activeSection === 'connections'} aria-current={activeSection === 'connections' ? 'page' : undefined}>Connections</a>
  <a href="#channels" class:on={activeSection === 'channels'} aria-current={activeSection === 'channels' ? 'page' : undefined}>Channels</a>
  <a href="#devices" class:on={activeSection === 'devices'} aria-current={activeSection === 'devices' ? 'page' : undefined}>Devices &amp; notifications</a>
  <a href="#mesh" class:on={activeSection === 'mesh'} aria-current={activeSection === 'mesh' ? 'page' : undefined}>Mesh</a>
  <a href="#policies" class:on={activeSection === 'policies'} aria-current={activeSection === 'policies' ? 'page' : undefined}>Permissions &amp; policies</a>
  <a href="#maintenance" class:on={activeSection === 'maintenance'} aria-current={activeSection === 'maintenance' ? 'page' : undefined}>Maintenance</a>
</nav>

{#if error}<div class="banner crit">{error}</div>{/if}
{#if !local}
  <div class="banner dim">
    You reached the serving node remotely. Settings that rewrite its host configuration remain clearly marked; selecting a peer below only changes which connection you manage.
  </div>
{/if}

{#key adminRevision}
{#if ['mesh', 'policies', 'maintenance'].includes(activeSection)}
  <AdminAccess onunlock={() => adminRevision += 1} />
{/if}

{#if activeSection === 'connections'}

<section id="settings-connections" tabindex="-1">
  <div class="h5">Connections <b>choose a node, then manage only its provider connection</b></div>
  <label class="node-target">
    <span>Managing node</span>
    <select value={selectedNodeId} onchange={(event) => selectNode(event.currentTarget.value)}>
      {#each store.nodes as node (node.id)}
        <option value={node.id}>{node.name || node.id.slice(0, 8)}{node.is_self ? ' · serving node' : node.reachable ? ' · peer' : ' · peer unavailable'}</option>
      {/each}
    </select>
    <small>
      {#if selectedNodeIsServing}
        Provider and credential changes below are made on the node serving this page.
      {:else}
        You are managing {selectedNodeName}. Provider commands are sealed to that peer; credentials remain on it and no peer host configuration is exposed here.
      {/if}
    </small>
  </label>
  {#if !selectedNode}
    <div class="empty">Waiting for a node to select…</div>
  {:else if !selectedNodeIsServing && !selectedNode.reachable}
    <div class="empty">{selectedNodeName} is unavailable. Its last advertised provider state is shown when it returns; no connection command is sent while it is offline.</div>
  {:else if selectedProviders.length}
    <div class="providers">
      {#each selectedProviders as provider (provider.name)}
        <ProviderCard p={provider} nodeId={selectedNode.id} />
      {/each}
    </div>
  {:else if !selectedNodeIsServing && selectedNode.providers === undefined}
    <div class="empty">{selectedNodeName} has not advertised provider capability. It may be an older peer; update it or manage providers on that node directly.</div>
  {:else}
    <div class="empty">No provider connections are configured for {selectedNodeName}.</div>
  {/if}

  <div class="h5 sub">Credentials on the serving node <b>forge tokens and sealed copies, never values</b></div>
  <p class="lede">
    These credentials live on {store.node?.name ?? 'the serving node'}, not on the selected peer. Sharing a copy sends it sealed to one named peer and never retrieves a value from that peer.
  </p>
  <CredentialSettings />
  <Credentials />
</section>
{/if}

{#if activeSection === 'channels'}
<section id="settings-channels" tabindex="-1">
  <div class="h5">Channels <b>shared keys, model defaults, and lifecycle</b></div>
  <label class="new-channel">
    <span>New channel</span>
    <input bind:value={channelName} placeholder="work" spellcheck="false" />
    <small>{channelNote || 'A channel is a shared key. A node that was never handed it cannot read its work.'}</small>
  </label>
  <div class="acts">
    <button class="btn" onclick={addChannel} disabled={!channelName.trim() || busy !== ''}>Create shared channel</button>
  </div>
  {#if store.channels.length === 0}
    <div class="empty">No channels yet.</div>
  {:else}
    {#if models.length === 0}
      <div class="empty">No node offers a model yet. Channel lifecycle still works; <a href="#connections">connect a provider</a> before choosing model defaults.</div>
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
            {#if savedChannel === c.name}<small>saved · handed to member nodes</small>{/if}
            <button class="lnk" disabled={busy !== ''} onclick={() => archiveChannel(c.name, true)}>Archive shared channel</button>
          </div>
        </div>
      {/each}
    </div>
    <ChannelNotifications />
    {#if archived_channels.length}
      <div class="h5 sub">
        Archived channels <b>{archived_channels.length} · work remains in history; no new session starts on them</b>
      </div>
      <div class="phases">
        {#each archived_channels as c (c.name)}
          <div class="ch off">
            <span class="nm">{c.name}</span>
            {#if deleting === c.name}
              <span class="note crit">Delete this channel and its key from the serving node? Sessions and work remain in history; member nodes may still hold their copies until propagation.</span>
              <div class="end">
                <button class="lnk d" disabled={busy !== ''} onclick={() => deleteChannel(c.name)}>
                  {busy === 'channel-delete' ? 'Deleting…' : 'Confirm delete'}
                </button>
                <button class="lnk" disabled={busy !== ''} onclick={() => (deleting = '')}>Keep channel</button>
              </div>
            {:else}
              <span class="note">archived · {c.nodes.length} node{c.nodes.length === 1 ? '' : 's'} still hold its key</span>
              <div class="end">
                <button class="lnk" disabled={busy !== ''} onclick={() => archiveChannel(c.name, false)}>Restore</button>
                <button class="lnk d" disabled={busy !== ''} onclick={() => (deleting = c.name)}>Delete</button>
              </div>
            {/if}
          </div>
        {/each}
      </div>
    {/if}
  {/if}
  <ChannelMeters />
</section>
<LaunchManifest />
{/if}

{#if activeSection === 'devices'}
<section id="settings-devices" tabindex="-1">
  <div class="h5">Devices &amp; notifications <b>this browser’s subscription and devices registered on the serving node</b></div>
  <Notifications />
</section>
{/if}

{#if activeSection === 'mesh'}
<section id="settings-mesh" tabindex="-1">
  <div class="h5">Mesh <b>{hubState}</b></div>
  <p class="lede">Manage the mesh connected to {store.node?.name ?? 'this serving node'}.</p>
  {#if paired && store.mesh}
    {@const m = store.mesh}
    {@const down = m.hub.state === 'unreachable'}
    <div class="hub" class:off={down || unpaired}>
      <span class="bar"></span>
      <span class="nm">{hubHost}<small>{m.hub_url}</small></span>
      <span class="st">
        <span class="l" class:off={down}>
          <span class="chip" class:off={down}>{down ? 'unreachable' : 'connected'}</span>
          {#if down && m.hub.state === 'unreachable'}
            · since {formatAge(m.hub.since_ms, clock.now)} ago
          {:else if m.last_ok_ms}
            · heard {formatAge(m.last_ok_ms, clock.now)} ago
          {/if}
        </span>
        <span>{m.queued} queued · {m.delivered_since_reconnect} delivered since reconnect{m.undecryptable ? ` · ${m.undecryptable} unreadable` : ''}{m.held ? ` · ${m.held} held for a key` : ''}</span>
        {#if m.last_error}<span class="l bad">{m.last_error}</span>{/if}
        {#if m.last_refusal}<span class="l bad">refused: {m.last_refusal}</span>{/if}
      </span>
      <div class="end">
        {#if !local}
          <small>Unpair from the serving node.</small>
        {:else if unpaired}
          <small>Restart the serving node to disconnect.</small>
        {:else if unpairing}
          <span class="note crit">Forget this hub on restart? Channels and keys stay; joining again requires a new invitation.</span>
          <button class="lnk d" disabled={busy !== ''} onclick={unpair}>{busy === 'unpair' ? 'Unpairing…' : 'Confirm unpair'}</button>
          <button class="lnk" disabled={busy !== ''} onclick={() => (unpairing = false)}>Keep mesh</button>
        {:else}
          <button class="lnk d" disabled={busy !== ''} onclick={() => (unpairing = true)}>Unpair serving node</button>
        {/if}
      </div>
    </div>
  {:else}
    <small>A hub is a small relay nodes dial out to. Without one, this node is reachable only where it is.</small>
  {/if}
  {#if local && !paired}
    <div class="pairing">
      <label>
        <span>Join an existing hub</span>
        <input bind:value={invitation} placeholder="full invitation URL" spellcheck="false" />
        <small>Paste the URL from <code>tracon mesh invite</code>, or receive it from an operator who already manages that mesh.</small>
      </label>
      <div class="acts">
        <button class="btn" onclick={startEnroll} disabled={!invitation.trim() || busy !== ''}>
          {busy === 'enroll' ? 'Joining…' : 'Join hub on serving node'}
        </button>
      </div>
      {#if enroll}
        <ul class="log">
          {#each enroll.lines as line, i (i)}<li>{line}</li>{/each}
          {#if enroll.error}<li class="bad">{enroll.error}</li>{/if}
        </ul>
        {#if enroll.done && enroll.channels.length}<small>Channels received: {enroll.channels.join(', ')}.</small>{/if}
        {#if enroll.done && enroll.restart_required}<small class="good">Joined. Restart the serving node to connect it to the hub.</small>{/if}
      {/if}
      {#if enrollPollError}<small class="bad">{enrollPollError}</small>{/if}
      <details class="first-node">
        <summary>Set up the first node</summary>
        <div class="first-node-body">
          <label>
            <span>Hub URL</span>
            <input bind:value={hubUrl} placeholder="https://hub.example.com" spellcheck="false" />
            <small>Creates the mesh channel here and saves the hub the serving node will trust.</small>
          </label>
          <div class="acts">
            <button class="btn" onclick={initMesh} disabled={!hubUrl.trim() || busy !== ''}>{busy === 'mesh' ? 'Preparing…' : 'Prepare first node'}</button>
          </div>
          {#if meshInit}
            <div class="mesh-result">
              <b>Serving node prepared</b>
              <small>Hub URL: {meshInit.hub_url}</small>
              <label><span>Hub admission value</span><input value={meshInit.admit_with} readonly /></label>
              <div class="copy-value">
                <button class="lnk" onclick={copyAdmit}>{admitCopied ? 'Copied' : 'Copy'}</button>
                {#if admitCopyError}<small class="bad">{admitCopyError}</small>{/if}
              </div>
              <ol><li>Configure and start the hub with that exact value.</li><li>Restart tracon on this node so it dials the hub.</li></ol>
            </div>
          {/if}
        </div>
      </details>
    </div>
  {:else if !local && !paired}
    <small>Which hub a node belongs to is decided on the serving node itself.</small>
  {/if}
  <MeshAdministration />
  <HubRollups />
</section>
{/if}

{#if activeSection === 'policies'}
<section id="settings-policies" tabindex="-1">
  <div class="h5">Permissions &amp; policies <b>signed policy and narrow, local authority grants</b></div>
  <PolicyManagement onapplied={loadAuthority} />
  <div class="h5 sub">Local authority grant <b>applies only on {store.node?.name ?? 'the serving node'}; unmatched consequential actions ask</b></div>
  {#if authority}
    {#if authority.policy.trusted}
      <p class="lede">Signed policy bundle version {authority.policy.version} is trusted. A local grant cannot change signing keys, trust roots, or a signed denial.</p>
    {:else}
      <div class="banner crit">No verified policy is active. Install a signed policy before granting consequential actions.</div>
    {/if}
    {#if grantConfirmation}
      <div class="grant-confirmation">
        <b>Confirm this exact local grant</b>
        <span>{grantConfirmation.verdict} {grantConfirmation.action} · {grantConfirmation.target} · channel {grantConfirmation.channel}{grantConfirmation.session_id ? ` · session ${grantConfirmation.session_id}` : ''}{grantConfirmation.revision ? ` · immutable revision ${grantConfirmation.revision}` : ''}{grantConfirmation.expires_ms ? ` · expires ${new Date(grantConfirmation.expires_ms).toLocaleString()}` : ''}</span>
        <small>{grantConfirmation.reason}</small>
        <div class="acts">
          <button class="btn p" onclick={saveGrant} disabled={!local || busy !== ''}>{busy === 'authority' ? 'Saving…' : 'Confirm local grant'}</button>
          <button class="lnk" onclick={() => (grantConfirmation = null)} disabled={busy !== ''}>Edit grant</button>
        </div>
      </div>
    {:else}
      <div class="grid">
        <label><span>Action</span><select bind:value={grant.action} disabled={!local}><option value="merge">merge</option><option value="publish">publish</option><option value="ticket_transition">ticket transition</option><option value="deploy">deploy</option><option value="terminal">terminal</option></select></label>
        <label><span>Decision</span><select bind:value={grant.verdict} disabled={!local}><option value="ask">ask · safe default</option><option value="allow">allow</option><option value="deny">deny</option></select><small>Ask preserves an explicit operator decision.</small></label>
        <label><span>Resource type</span><select bind:value={grantResource} disabled={!local}><option value="repository">repository pull or merge request</option><option value="deployment">QA deployment</option><option value="ticket">ticket transition</option><option value="advanced">advanced canonical target</option></select></label>
        <label><span>Exact target</span><input bind:value={grant.target} disabled={!local} placeholder={grantTargetHint} spellcheck="false" /><small>One canonical identifier, no spaces. This is the only target the grant can match.</small></label>
        <label><span>Channel scope</span><input bind:value={grant.channel} disabled={!local} placeholder={store.node?.default_channel ?? 'personal'} /><small>Empty uses the serving node default channel.</small></label>
        <label><span>Reason</span><input bind:value={grant.reason} disabled={!local} placeholder="why this exact scoped action is needed" /></label>
      </div>
      <details class="advanced-grant">
        <summary>Advanced scope and expiry</summary>
        <div class="grid">
          <label><span>Session (optional)</span><input bind:value={grant.session_id} disabled={!local} placeholder="limit to one session id" /></label>
          <label><span>Immutable revision (optional)</span><input bind:value={grant.revision} disabled={!local} placeholder="commit SHA" /><small>Required for allow grants to merge, publish, or deploy.</small></label>
          <label><span>Expires at (optional)</span><input type="datetime-local" value={grant.expires_ms ? new Date(grant.expires_ms).toISOString().slice(0, 16) : ''} onchange={(event) => grant.expires_ms = event.currentTarget.value ? Date.parse(event.currentTarget.value) : null} disabled={!local} /></label>
        </div>
      </details>
      {#if grant.action === 'terminal'}
        <p class="why">A terminal grant opens an interactive shell inside one session's workspace. Target is <code>terminal:&lt;session id&gt;:&lt;workspace path&gt;</code> and the session field is required, because the grant is of a terminal in that directory. It is not a per-command ledger: everything typed at the prompt afterwards runs without a further decision and raises no tool call. The node records that a terminal was opened, with what shell and where, and how much traffic crossed it — not what was run in it.</p>
      {/if}
      <div class="acts"><button class="btn p" onclick={reviewGrant} disabled={!local || busy !== ''}>Review scoped grant</button></div>
    {/if}
    {#if authority.grants.length}
      <div class="credentials">
        {#each authority.grants as item (item.id)}
          <div class:dim={item.revoked_ms !== null} class="credential">
            <b>{item.verdict} {item.action}</b>
            <span>{item.target} · {item.channel}{item.session_id ? ` · session ${item.session_id}` : ''}{item.revision ? ` · revision ${item.revision}` : ''}{item.expires_ms ? ` · expires ${new Date(item.expires_ms).toLocaleString()}` : ''}</span>
            <small>{item.reason}</small>
            {#if item.revoked_ms !== null}
              <small>revoked</small>
            {:else if revoking === item.id}
              <div class="acts"><small>Revoke this local grant immediately? Future actions will be evaluated again.</small><button class="btn d" onclick={() => revokeGrant(item.id)} disabled={!local || busy !== ''}>Confirm revoke</button><button class="lnk" onclick={() => (revoking = '')} disabled={busy !== ''}>Keep grant</button></div>
            {:else}
              <button class="btn" onclick={() => (revoking = item.id)} disabled={!local || busy !== ''}>Revoke</button>
            {/if}
          </div>
        {/each}
      </div>
    {:else}<div class="empty">No local grants. Unmatched consequential actions ask.</div>{/if}
  {:else}<div class="empty">Reading signed policy and local grants…</div>{/if}
</section>
{/if}

{#if activeSection === 'maintenance'}
<section id="settings-maintenance" tabindex="-1">
  <div class="h5">Maintenance <b>serving-node readiness, host settings, recovery, and desktop tooling</b></div>
  <div class="h5 sub">Operator access <b>for the serving node</b></div>
  <div class="field access-field">
    <span>Reach this node from another device</span>
    <input bind:value={publicUrl} placeholder="https://node.tailnet.ts.net" spellcheck="false" />
    <small>A replacement token signs other clients out. This browser signs in with the new token; save it before leaving this page.</small>
  </div>
  {#if rotatingToken}
    <div class="grant-confirmation"><b>Create or replace operator access?</b><span>Other signed-in browsers and desktop clients lose access. The replacement is shown only here; save it immediately.</span><div class="acts"><button class="btn d" onclick={confirmIssueToken} disabled={busy !== ''}>{busy === 'token' ? 'Creating…' : 'Confirm new token'}</button><button class="lnk" onclick={() => (rotatingToken = false)} disabled={busy !== ''}>Cancel</button></div></div>
  {:else}
    <div class="acts"><button class="btn" onclick={() => (rotatingToken = true)} disabled={busy !== ''}>Create or rotate operator token</button></div>
  {/if}
  {#if issued}
    <div class="issued"><p>{issued.applied ? 'Token created. Save it before leaving this page.' : 'Replacement prepared; activation is not confirmed. Keep this token until access is verified.'}</p><div class="qr">{@html issued.svg}</div><code>{issued.token}</code></div>
  {/if}

  <div class="h5 sub">Boundary <b>{store.node?.state ?? '…'} · runs on the serving node</b></div>
  {#if refused && store.node}
    <p class="why"><b>{store.node.failed_check}: {store.node.failed_detail}</b><i>{remedy(store.node.failed_check)}</i></p>
  {/if}
  <div class="acts">
    <button class="btn p" onclick={() => setup(false)} disabled={!local || busy !== ''} title="Create the network, gateway and images this node needs">{busy === 'setup' ? 'Running setup…' : 'Prepare isolated runtime'}</button>
    <button class="btn" onclick={() => setup(true)} disabled={!local || busy !== ''} title="Rebuild the serving node gateway and harness images from scratch">Rebuild runtime images</button>
  </div>
  {#if checks}<ul class="checks">{#each checks as c (c.id)}<li><span class="chip" class:bad={!c.ok}>{c.ok ? 'ok' : 'fail'}</span> {c.id} · <small>{c.detail}</small></li>{/each}</ul>{/if}

  <div class="h5 sub">Harness and limits <b>host configuration for the serving node only</b></div>
  {#if form && cfg}
    <div class="grid">
      <label><span>Harness</span><select bind:value={form.harness.id} disabled={!local}><option value="omp">omp</option><option value="claude">claude</option></select><small>Running: {cfg.running.harness_id} {cfg.running.harness_version}</small></label>
      <label><span>Harness version</span><input bind:value={form.harness.version} disabled={!local} spellcheck="false" /></label>
      <label><span>Session budget (tokens)</span><input type="number" bind:value={form.session.budget_tokens} disabled={!local} /></label>
      <label><span>Default channel</span><select bind:value={form.session.default_channel} disabled={!local}><option value="">none</option>{#each open_channels as c (c.name)}<option value={c.name}>{c.name}</option>{/each}</select><small>What the composer starts on until a browser selects another.</small></label>
      <label><span>Podman binary</span><input bind:value={form.boundary.podman} disabled={!local} placeholder="found on PATH" spellcheck="false" /><small>Empty resolves from PATH, then usual install locations.</small></label>
      <label><span>Node name</span><input bind:value={form.node_name} disabled={!local} spellcheck="false" /></label>
    </div>
    <div class="acts"><button class="btn p" onclick={saveConfig} disabled={!local || !dirty || busy !== ''}>{busy === 'config' ? 'Saving…' : 'Save serving-node configuration'}</button>{#if changed.length}<small>wrote {changed.join(', ')}</small>{/if}</div>
  {:else if configError}
    <p class="why"><b>node.toml could not be read</b><i>{configError}</i></p>
  {:else}<div class="empty">Reading serving-node configuration…</div>{/if}

  <div class="h5 sub">Your own harness <b>terminal access without credential transfer</b></div>
  <p class="lede">Connect a terminal harness to these node tools. It runs outside Tracon’s isolation boundary. Use <code>submit_report</code> for an operator report and notification without Git or publication; <code>report_status</code> reads feedback.</p>
  {#if !cfg}
    <small>Loading…</small>
  {:else if !cfg.external.enabled}
    <small>Off. Set <code>[external] enabled = true</code> in serving-node <code>node.toml</code> and restart.</small>
  {:else if externalChannels.length === 0}
    <small>Create a shared channel first.</small>
  {:else}
    <div class="mcp">{#each externalChannels as name (name)}<code>claude mcp add --transport http tracon-{name} {origin}/mcp/external/{name}</code>{/each}</div>
    {#if !local}<small>From another machine, add <code>--header "Authorization: Bearer &lt;operator token&gt;"</code>.</small>{/if}
  {/if}

  {#if desktopUpdate}
    <div class="h5 sub">Desktop app <b>on this device</b></div>
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
    {#if desktopAction}<div class="acts"><button class="btn p" onclick={runDesktopUpdate} disabled={desktopAction.disabled}>{desktopAction.label}</button></div>{/if}
    {#if desktopSetup}
      <small>
        Node v{desktopSetup.node_version ?? '?'} ·
        {desktopSetup.owner === 'service' ? 'runs under the service' : desktopSetup.owner === 'migrated' ? 'runs inside the app; restart the app to move it under the service' : 'started outside the app'}
        · CLI {desktopSetup.cli_version ? `v${desktopSetup.cli_version}` : 'not installed'}{#if desktopSetupError} · {desktopSetupError}{/if}
      </small>
      <div class="acts">
        {#if desktopSetup.sidecar_version && desktopSetup.cli_version !== desktopSetup.sidecar_version}<button class="btn" disabled={desktopBusy} onclick={() => runDesktop(installDesktopCli)}>Install CLI v{desktopSetup.sidecar_version}</button>{/if}
      </div>
    {/if}
  {/if}
  <div class="h5 sub">Session transfer <b>start an isolated continuation; no credentials move</b></div>
  <TransferInbox />
  <Maintenance />
</section>
{/if}
{/key}

<style>
  section {
    display: grid;
    gap: 8px;
    background: var(--s1);
    border-radius: 4px;
    padding: 14px 16px;
    max-width: 960px;
    scroll-margin-top: 12px;
  }
  section[tabindex="-1"]:focus { outline: none; }
  .section-nav {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    max-width: 960px;
    position: sticky;
    top: 0;
    z-index: 3;
    background: var(--bg);
    padding: 2px 0 6px;
    scrollbar-width: thin;
  }
  .section-nav a {
    flex: 0 0 auto;
    min-height: 44px;
    display: inline-flex;
    align-items: center;
    padding: 0 8px;
    color: var(--ink2);
    font: 500 13px var(--sans);
    text-decoration: none;
  }
  .section-nav a:hover,
  .section-nav a.on,
  .section-nav a:focus-visible {
    color: var(--ink);
    background: var(--s2);
    border-radius: 4px;
  }
  .providers {
    display: grid;
    gap: 5px;
  }
  .grant-confirmation {
    display: grid;
    gap: 6px;
    border-left: 3px solid var(--wait);
    background: var(--s2);
    padding: 10px 12px;
    color: var(--ink2);
    font: 12.5px var(--mono);
  }
  .advanced-grant {
    display: grid;
    gap: 10px;
    padding: 10px 0 0;
    border-top: 1px solid var(--rule);
  }
  .advanced-grant summary {
    cursor: pointer;
    color: var(--ink2);
    font: 12px var(--mono);
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
    max-width: 100%;
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
