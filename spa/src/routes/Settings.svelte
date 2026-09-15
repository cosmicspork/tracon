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
  import { connectableProviders } from '../lib/providers'
  import HubRollups from '../components/HubRollups.svelte'
  import TransferInbox from '../components/TransferInbox.svelte'
  import AdminAccess from '../components/settings/AdminAccess.svelte'
  import Maintenance from '../components/settings/Maintenance.svelte'
  import MeshAdministration from '../components/settings/MeshAdministration.svelte'
  import PolicyManagement from '../components/settings/PolicyManagement.svelte'
  import ModelPicker from '../components/ModelPicker.svelte'
  import Card from '../components/settings/Card.svelte'
  import { preferences as desktopPreferences, setPreferences as setDesktopPreferences, type DesktopPrefs } from '../lib/desktop-prefs'
  import { setTheme, storedTheme, themes, type Theme } from '../lib/theme'
  import {
    check as checkDesktopUpdate,
    desktopUpdateAction,
    desktopUpdatesAvailable,
    install as installDesktopUpdate,
    status as desktopUpdateStatus,
  } from '../lib/desktop-update'
  import {
    canRestartNode,
    installCli as installDesktopCli,
    nodeOwnerSummary,
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
  import type { AuthorityGrant, BoundaryCheck, EnrollStatus, NodeConfig, PolicyRule } from '../lib/types'

  const local = $derived(store.node?.loopback ?? false)
  const origin = typeof location === 'undefined' ? '' : location.origin
  const params = $derived(new URLSearchParams(router.search))
  const sections = [
    ['general', 'General'],
    ['connections', 'Connections'],
    ['channels', 'Channels'],
    ['devices', 'Devices & notifications'],
    ['mesh', 'Mesh'],
    ['policies', 'Permissions & policies'],
    ['maintenance', 'Maintenance'],
  ] as const
  const activeSection = $derived(
    sections.find(([id]) => id === router.hash.slice(1))?.[0] ?? 'general',
  )
  let adminRevision = $state(0)
  const requestedNode = $derived(params.get('node'))
  const selectedNode = $derived(
    store.nodes.find((node) => node.id === requestedNode) ??
      store.node ??
      null,
  )
  const selectedNodeId = $derived(selectedNode?.id ?? '')
  const selectedProviders = $derived(
    connectableProviders(selectedNode?.is_self ? store.providers : (selectedNode?.providers ?? [])),
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
  let desktopPrefs = $state<DesktopPrefs | null>(null)
  let desktopPrefsError = $state('')
  let theme = $state<Theme>(storedTheme())

  onMount(() => {
    if (desktopUpdatesAvailable()) {
      desktopSetupStatus()
        .then((s) => (desktopSetup = s))
        .catch(() => {})
      desktopPreferences()
        .then((p) => (desktopPrefs = p))
        .catch((e) => (desktopPrefsError = e instanceof Error ? e.message : String(e)))
    }
  })

  function chooseTheme(next: Theme) {
    theme = next
    setTheme(next)
  }

  async function setDesktopPref(update: Partial<DesktopPrefs>) {
    desktopPrefsError = ''
    try {
      desktopPrefs = await setDesktopPreferences(update)
    } catch (e) {
      desktopPrefsError = e instanceof Error ? e.message : String(e)
    }
  }

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
  {#each sections as [id, label] (id)}
    <a href="#{id}" class:on={activeSection === id} aria-current={activeSection === id ? 'page' : undefined}>{label}</a>
  {/each}
</nav>

{#if error}<div class="banner crit">{error}</div>{/if}
{#if !local && activeSection !== 'general'}
  <div class="banner dim">
    You reached the serving node remotely. Settings that rewrite its host configuration remain clearly marked; selecting a peer below only changes which connection you manage.
  </div>
{/if}

{#key adminRevision}
<div class="panel" id="settings-{activeSection}" tabindex="-1">
{#if ['mesh', 'policies', 'maintenance'].includes(activeSection)}
  <AdminAccess onunlock={() => adminRevision += 1} />
{/if}

{#if activeSection === 'general'}
  <Card title="Appearance" note="Applies to this browser only.">
    <div class="segmented" role="radiogroup" aria-label="Colour scheme">
      {#each themes as t (t)}
        <button type="button" role="radio" aria-checked={theme === t} class:on={theme === t} onclick={() => chooseTheme(t)}>
          {t === 'auto' ? 'Match system' : t === 'light' ? 'Light' : 'Dark'}
        </button>
      {/each}
    </div>
  </Card>

  {#if desktopPrefs}
    <Card title="Desktop app" note="How the app behaves on this machine. Stored beside node.toml, not in it.">
      <div class="toggles">
        <label class="toggle">
          <input type="checkbox" checked={desktopPrefs.open_window_at_launch} onchange={(e) => setDesktopPref({ open_window_at_launch: e.currentTarget.checked })} />
          <span><b>Open the window at launch</b><small>Off starts tracon in the menu bar only, which suits a login item.</small></span>
        </label>
        {#if desktopSetup?.platform !== 'linux'}
          <label class="toggle">
            <input type="checkbox" checked={desktopPrefs.cmd_q_quits} onchange={(e) => setDesktopPref({ cmd_q_quits: e.currentTarget.checked })} />
            <span><b>⌘Q quits</b><small>Off closes to the menu bar; Quit in the menu bar icon still ends the app.</small></span>
          </label>
          <label class="toggle">
            <input type="checkbox" checked={desktopPrefs.hide_dock_when_closed} onchange={(e) => setDesktopPref({ hide_dock_when_closed: e.currentTarget.checked })} />
            <span><b>Hide the dock icon while closed</b><small>The menu bar icon stays either way.</small></span>
          </label>
        {/if}
      </div>
      <p class="hint">Show or hide the window from anywhere with <kbd>Ctrl</kbd> <kbd>Alt</kbd> <kbd>T</kbd>.</p>
      {#if desktopPrefsError}<small class="bad">{desktopPrefsError}</small>{/if}
    </Card>
  {/if}

  {#if desktopUpdate || desktopSetup}
    <Card title="Version &amp; updates" note="The desktop app, the node it runs, and the tracon command line on this machine.">
      <dl class="facts">
        {#if desktopUpdate}
          <div>
            <dt>App</dt>
            <dd>
              v{desktopUpdate.current_version} ·
              {#if desktopUpdate.state === 'current'}
                up to date
              {:else if desktopUpdate.state === 'available'}
                v{desktopUpdate.available_version} is ready to install
              {:else if desktopUpdate.state === 'failed'}
                <span class="bad">{desktopUpdate.message || desktopUpdateError}</span>
              {:else if desktopUpdate.state === 'unsupported'}
                {desktopUpdate.message}
              {:else if desktopUpdate.state === 'checking'}
                checking the latest release…
              {:else if desktopUpdate.state === 'downloading'}
                downloading and verifying the update…
              {:else}
                not checked yet
              {/if}
            </dd>
          </div>
        {/if}
        {#if desktopSetup}
          <div><dt>Node</dt><dd>{desktopSetup.node_version ? `v${desktopSetup.node_version} · ` : ''}{nodeOwnerSummary(desktopSetup)}</dd></div>
          <div><dt>CLI</dt><dd>{desktopSetup.cli_version ? `v${desktopSetup.cli_version}` : 'not installed'}</dd></div>
        {/if}
      </dl>
      {#if desktopSetup?.service_failing}
        <p class="why"><b>The node keeps exiting under the service</b><i>{desktopSetup.service_error ?? 'It left no reason in its log; `tracon service status` shows what the supervisor saw.'}</i></p>
      {/if}
      {#if desktopSetupError}<small class="bad">{desktopSetupError}</small>{/if}
      <div class="acts">
        {#if desktopAction}<button class="btn p" onclick={runDesktopUpdate} disabled={desktopAction.disabled}>{desktopAction.label}</button>{/if}
        {#if desktopSetup && canRestartNode(desktopSetup)}<button class="btn" class:p={desktopSetup.service_failing} disabled={desktopBusy} onclick={() => runDesktop(restartDesktopNode)}>{desktopBusy ? 'Working…' : 'Restart node'}</button>{/if}
        {#if desktopSetup?.sidecar_version && desktopSetup.cli_version !== desktopSetup.sidecar_version}<button class="btn" disabled={desktopBusy} onclick={() => runDesktop(installDesktopCli)}>Install CLI v{desktopSetup.sidecar_version}</button>{/if}
      </div>
    </Card>
  {/if}
{/if}

{#if activeSection === 'connections'}
  <Card
    title="Model providers"
    note={selectedNodeIsServing
      ? `Sign-ins held by ${selectedNodeName}, the node serving this page.`
      : `Managing ${selectedNodeName}. Commands are sealed to that peer; its credentials stay on it.`}
  >
    {#snippet actions()}
      {#if store.nodes.length > 1}
        <label class="inline-select">
          <span>Node</span>
          <select value={selectedNodeId} onchange={(event) => selectNode(event.currentTarget.value)}>
            {#each store.nodes as node (node.id)}
              <option value={node.id}>{node.name || node.id.slice(0, 8)}{node.is_self ? ' · serving node' : node.reachable ? ' · peer' : ' · peer unavailable'}</option>
            {/each}
          </select>
        </label>
      {/if}
    {/snippet}
    {#if !selectedNode}
      <div class="empty">Waiting for a node to select…</div>
    {:else if !selectedNodeIsServing && !selectedNode.reachable}
      <div class="empty">{selectedNodeName} is unavailable. Its last advertised provider state is shown when it returns; no connection command is sent while it is offline.</div>
    {:else if selectedProviders.length}
      <div class="rows">
        {#each selectedProviders as provider (provider.name)}
          <ProviderCard p={provider} nodeId={selectedNode.id} />
        {/each}
      </div>
    {:else if !selectedNodeIsServing && selectedNode.providers === undefined}
      <div class="empty">{selectedNodeName} has not advertised provider capability. It may be an older peer; update it or manage providers on that node directly.</div>
    {:else}
      <div class="empty">No provider connections are configured for {selectedNodeName}.</div>
    {/if}
  </Card>

  <Card title="Forge tokens" note={`GitHub and GitLab tokens on ${store.node?.name ?? 'the serving node'}, and the channels each may serve. Values are never shown.`}>
    <CredentialSettings />
  </Card>

  <Credentials />
{/if}

{#if activeSection === 'channels'}
  <Card title="Channels" note="A channel is a shared key: a node that was never handed it cannot read its work. Each one names the models that plan and execute by default.">
    <form class="inline-form" onsubmit={(event) => { event.preventDefault(); void addChannel() }}>
      <input bind:value={channelName} placeholder="New channel name" aria-label="New channel name" spellcheck="false" />
      <button class="btn" type="submit" disabled={!channelName.trim() || busy !== ''}>Create channel</button>
      {#if channelNote}<small>{channelNote}</small>{/if}
    </form>
    {#if store.channels.length === 0}
      <div class="empty">No channels yet.</div>
    {:else}
      {#if models.length === 0}
        <div class="empty">No node offers a model yet. Channel lifecycle still works; <a href="#connections">connect a provider</a> before choosing model defaults.</div>
      {/if}
      <div class="channel-table">
        <div class="ch head" aria-hidden="true"><span>Channel</span><span>Plan model</span><span>Execute model</span><span></span></div>
        {#each open_channels as c (c.name)}
          {@const plan = phaseDefaults(c.bindings, 'plan')}
          {@const execute = phaseDefaults(c.bindings, 'execute')}
          <div class="ch">
            <span class="nm">{c.name}</span>
            {#each [['plan', plan], ['execute', execute]] as const as [ph, b] (ph)}
              <label>
                <span class="phase">{ph === 'plan' ? 'Plan' : 'Execute'}</span>
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
              {#if savedChannel === c.name}<small>saved · handed to members</small>{/if}
              <button class="lnk" disabled={busy !== ''} onclick={() => archiveChannel(c.name, true)}>Archive</button>
            </div>
          </div>
        {/each}
      </div>
    {/if}
  </Card>

  {#if archived_channels.length}
    <Card title="Archived channels" note="Their work stays in history; no new session starts on them.">
      <div class="channel-table">
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
              <span class="note">{c.nodes.length} node{c.nodes.length === 1 ? '' : 's'} still hold its key</span>
              <div class="end">
                <button class="lnk" disabled={busy !== ''} onclick={() => archiveChannel(c.name, false)}>Restore</button>
                <button class="lnk d" disabled={busy !== ''} onclick={() => (deleting = c.name)}>Delete</button>
              </div>
            {/if}
          </div>
        {/each}
      </div>
    </Card>
  {/if}

  {#if store.channels.length}<ChannelNotifications />{/if}
  <ChannelMeters />
  <LaunchManifest />
{/if}

{#if activeSection === 'devices'}
  <Notifications />
{/if}

{#if activeSection === 'mesh'}
  <Card title="Hub" note={`The relay ${store.node?.name ?? 'this node'} dials out to. Without one, this node is reachable only where it is.`}>
    {#snippet actions()}
      <span class="chip" class:off={hubState !== 'connected'} class:bad={hubState === 'unreachable'}>{hubState}</span>
    {/snippet}
    {#if paired && store.mesh}
      {@const m = store.mesh}
      {@const down = m.hub.state === 'unreachable'}
      <div class="hub" class:off={down || unpaired}>
        <span class="bar"></span>
        <span class="nm">{hubHost}<small>{m.hub_url}</small></span>
        <span class="st">
          <span class="l" class:off={down}>
            {#if down && m.hub.state === 'unreachable'}
              unreachable since {formatAge(m.hub.since_ms, clock.now)} ago
            {:else if m.last_ok_ms}
              heard {formatAge(m.last_ok_ms, clock.now)} ago
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
            <button class="lnk d" disabled={busy !== ''} onclick={() => (unpairing = true)}>Unpair</button>
          {/if}
        </div>
      </div>
    {/if}
    {#if local && !paired}
      <div class="pairing">
        <label class="field">
          <span>Join an existing hub</span>
          <input bind:value={invitation} placeholder="full invitation URL" spellcheck="false" />
          <small>Paste the URL from <code>tracon mesh invite</code>, or receive it from an operator who already manages that mesh.</small>
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
          {#if enroll.done && enroll.channels.length}<small>Channels received: {enroll.channels.join(', ')}.</small>{/if}
          {#if enroll.done && enroll.restart_required}<small class="good">Joined. Restart the serving node to connect it to the hub.</small>{/if}
        {/if}
        {#if enrollPollError}<small class="bad">{enrollPollError}</small>{/if}
        <details class="first-node">
          <summary>Set up the first node</summary>
          <div class="first-node-body">
            <label class="field">
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
                <label class="field"><span>Hub admission value</span><input value={meshInit.admit_with} readonly /></label>
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
  </Card>
  <MeshAdministration />
  <HubRollups />
{/if}

{#if activeSection === 'policies'}
  <PolicyManagement onapplied={loadAuthority} />

  <Card title="Local grants" note={`Narrow exceptions that apply only on ${store.node?.name ?? 'the serving node'}. Consequential actions no grant matches still ask.`}>
    {#if !authority}
      <div class="empty">Reading signed policy and local grants…</div>
    {:else}
      {#if !authority.policy.trusted}
        <div class="banner crit">No verified policy is active. Install a signed policy before granting consequential actions.</div>
      {/if}
      {#if authority.grants.length}
        <div class="rows">
          {#each authority.grants as item (item.id)}
            <div class:dim={item.revoked_ms !== null} class="grant">
              <b>{item.verdict} {item.action}</b>
              <span>{item.target} · {item.channel}{item.session_id ? ` · session ${item.session_id}` : ''}{item.revision ? ` · revision ${item.revision}` : ''}{item.expires_ms ? ` · expires ${new Date(item.expires_ms).toLocaleString()}` : ''}</span>
              <small>{item.reason}</small>
              {#if item.revoked_ms !== null}
                <small>revoked</small>
              {:else if revoking === item.id}
                <div class="acts"><small>Revoke this local grant immediately? Future actions will be evaluated again.</small><button class="btn d" onclick={() => revokeGrant(item.id)} disabled={!local || busy !== ''}>Confirm revoke</button><button class="lnk" onclick={() => (revoking = '')} disabled={busy !== ''}>Keep grant</button></div>
              {:else}
                <div class="acts"><button class="lnk d" onclick={() => (revoking = item.id)} disabled={!local || busy !== ''}>Revoke</button></div>
              {/if}
            </div>
          {/each}
        </div>
      {:else}
        <div class="empty">No local grants.</div>
      {/if}
    {/if}
  </Card>

  {#if authority}
    <Card title="New local grant" note={authority.policy.trusted ? `Signed policy v${authority.policy.version} is trusted. A local grant cannot change signing keys, trust roots, or a signed denial.` : undefined}>
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
          <label><span>Decision</span><select bind:value={grant.verdict} disabled={!local}><option value="ask">ask · safe default</option><option value="allow">allow</option><option value="deny">deny</option></select></label>
          <label><span>Resource type</span><select bind:value={grantResource} disabled={!local}><option value="repository">pull or merge request</option><option value="deployment">QA deployment</option><option value="ticket">ticket transition</option><option value="advanced">advanced canonical target</option></select></label>
          <label><span>Channel scope</span><input bind:value={grant.channel} disabled={!local} placeholder={store.node?.default_channel ?? 'personal'} /></label>
          <label class="wide"><span>Exact target</span><input bind:value={grant.target} disabled={!local} placeholder={grantTargetHint} spellcheck="false" /><small>One canonical identifier, no spaces. This is the only target the grant can match.</small></label>
          <label class="wide"><span>Reason</span><input bind:value={grant.reason} disabled={!local} placeholder="why this exact scoped action is needed" /></label>
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
        <div class="acts"><button class="btn p" onclick={reviewGrant} disabled={!local || busy !== ''}>Review grant</button><small>An empty channel uses the serving node default. Ask keeps an explicit operator decision.</small></div>
      {/if}
    </Card>
  {/if}
{/if}

{#if activeSection === 'maintenance'}
  <Card title="Node configuration" note="Host settings in the serving node's node.toml. Most take effect after a restart.">
    {#if form && cfg}
      <div class="grid">
        <label><span>Node name</span><input bind:value={form.node_name} disabled={!local} spellcheck="false" /></label>
        <label><span>Harness</span><select bind:value={form.harness.id} disabled={!local}><option value="opencode">opencode</option><option value="claude">claude</option></select><small>Running: {cfg.running.harness_id} {cfg.running.harness_version}</small></label>
        <label><span>Harness version</span><input bind:value={form.harness.version} disabled={!local} spellcheck="false" /></label>
        <label><span>Session budget (tokens)</span><input type="number" bind:value={form.session.budget_tokens} disabled={!local} /></label>
        <label><span>Default channel</span><select bind:value={form.session.default_channel} disabled={!local}><option value="">none</option>{#each open_channels as c (c.name)}<option value={c.name}>{c.name}</option>{/each}</select><small>Where the composer starts until a browser picks another.</small></label>
        <label><span>Podman binary</span><input bind:value={form.boundary.podman} disabled={!local} placeholder="found on PATH" spellcheck="false" /><small>Empty resolves from PATH, then usual install locations.</small></label>
      </div>
      <div class="acts"><button class="btn p" onclick={saveConfig} disabled={!local || !dirty || busy !== ''}>{busy === 'config' ? 'Saving…' : 'Save configuration'}</button>{#if changed.length}<small>wrote {changed.join(', ')}</small>{/if}</div>
    {:else if configError}
      <p class="why"><b>node.toml could not be read</b><i>{configError}</i></p>
    {:else}<div class="empty">Reading serving-node configuration…</div>{/if}
  </Card>

  <Card title="Operator access" note="A token for reaching this node from another device. Creating a new one signs every other client out; this browser signs in with it.">
    <label class="field">
      <span>Address other devices use</span>
      <input bind:value={publicUrl} placeholder="https://node.tailnet.ts.net" spellcheck="false" />
    </label>
    {#if rotatingToken}
      <div class="grant-confirmation"><b>Create or replace operator access?</b><span>Other signed-in browsers and desktop clients lose access. The replacement is shown only here; save it immediately.</span><div class="acts"><button class="btn d" onclick={confirmIssueToken} disabled={busy !== ''}>{busy === 'token' ? 'Creating…' : 'Confirm new token'}</button><button class="lnk" onclick={() => (rotatingToken = false)} disabled={busy !== ''}>Cancel</button></div></div>
    {:else}
      <div class="acts"><button class="btn" onclick={() => (rotatingToken = true)} disabled={busy !== ''}>Create or rotate token</button></div>
    {/if}
    {#if issued}
      <div class="issued"><p>{issued.applied ? 'Token created. Save it before leaving this page.' : 'Replacement prepared; activation is not confirmed. Keep this token until access is verified.'}</p><div class="qr">{@html issued.svg}</div><code>{issued.token}</code></div>
    {/if}
  </Card>

  <Maintenance>
    {#snippet boundary()}
      <div class="acts">
        <span class="chip" class:bad={refused}>{store.node?.state ?? '…'}</span>
        <button class="btn p" onclick={() => setup(false)} disabled={!local || busy !== ''} title="Create the network, gateway and images this node needs">{busy === 'setup' ? 'Running setup…' : 'Prepare isolated runtime'}</button>
        <button class="btn" onclick={() => setup(true)} disabled={!local || busy !== ''} title="Rebuild the serving node gateway and harness images from scratch">Rebuild runtime images</button>
      </div>
      {#if refused && store.node}
        <p class="why"><b>{store.node.failed_check}: {store.node.failed_detail}</b><i>{remedy(store.node.failed_check)}</i></p>
      {/if}
      {#if checks}<ul class="checks">{#each checks as c (c.id)}<li><span class="chip" class:bad={!c.ok}>{c.ok ? 'ok' : 'fail'}</span> {c.id} · <small>{c.detail}</small></li>{/each}</ul>{/if}
    {/snippet}
  </Maintenance>

  <Card title="Your own harness" note="Connect a terminal harness to this node's tools. It runs outside tracon's isolation boundary and never receives credentials.">
    {#if !cfg}
      <small>Loading…</small>
    {:else if !cfg.external.enabled}
      <small>Off. Set <code>[external] enabled = true</code> in serving-node <code>node.toml</code> and restart.</small>
    {:else if externalChannels.length === 0}
      <small>Create a shared channel first.</small>
    {:else}
      <div class="mcp">{#each externalChannels as name (name)}<code>claude mcp add --transport http tracon-{name} {origin}/mcp/external/{name}</code>{/each}</div>
      <small>Use <code>submit_report</code> for an operator report and notification without Git or publication; <code>report_status</code> reads feedback.{#if !local} From another machine, add <code>--header "Authorization: Bearer &lt;operator token&gt;"</code>.{/if}</small>
    {/if}
  </Card>

  <Card title="Session transfer" note="Start an isolated continuation of a session exported from another node. No credentials move.">
    <TransferInbox />
  </Card>
{/if}
</div>
{/key}

<style>
  .panel {
    display: grid;
    gap: 12px;
    align-content: start;
    min-width: 0;
  }
  .panel:focus {
    outline: none;
  }
  .section-nav {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
    position: sticky;
    top: 0;
    z-index: 3;
    background: var(--bg);
    padding: 2px 0 8px;
    border-bottom: 1px solid var(--rule);
  }
  .section-nav a {
    flex: 0 0 auto;
    min-height: 36px;
    display: inline-flex;
    align-items: center;
    padding: 0 12px;
    border-radius: 4px;
    color: var(--ink2);
    font: 500 13px var(--sans);
    text-decoration: none;
  }
  .section-nav a:hover,
  .section-nav a.on,
  .section-nav a:focus-visible {
    color: var(--ink);
    background: var(--s2);
  }
  .rows {
    display: grid;
    gap: 6px;
  }
  .grid {
    display: grid;
    gap: 14px 16px;
    grid-template-columns: repeat(auto-fill, minmax(240px, 1fr));
    align-items: start;
  }
  .grid .wide {
    grid-column: 1 / -1;
  }
  label,
  .field {
    display: grid;
    gap: 5px;
    align-content: start;
    min-width: 0;
  }
  .field {
    max-width: 560px;
  }
  label > span,
  .field > span {
    font: 500 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink2);
  }
  input:not([type='checkbox']),
  select {
    background: var(--s2);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13.5px var(--sans);
    min-width: 0;
    min-height: 36px;
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
    gap: 8px 12px;
    align-items: center;
    flex-wrap: wrap;
  }
  .inline-form {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px 12px;
  }
  .inline-form input {
    flex: 0 1 280px;
  }
  .inline-select {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .inline-select select {
    min-height: 30px;
    padding: 4px 8px;
    font-size: 13px;
  }

  /* General */
  .segmented {
    display: inline-flex;
    justify-self: start;
    background: var(--s2);
    border-radius: 6px;
    padding: 3px;
    gap: 2px;
  }
  .segmented button {
    border: 0;
    background: none;
    color: var(--ink2);
    font: 500 13px var(--sans);
    padding: 6px 14px;
    border-radius: 4px;
    cursor: pointer;
  }
  .segmented button.on {
    background: var(--s1);
    color: var(--ink);
    box-shadow: 0 1px 2px rgba(0, 0, 0, 0.15);
  }
  .toggles {
    display: grid;
    gap: 2px;
  }
  .toggle {
    display: flex;
    align-items: flex-start;
    gap: 12px;
    padding: 8px 0;
    cursor: pointer;
  }
  .toggle + .toggle {
    border-top: 1px solid var(--rule);
  }
  .toggle input {
    margin: 3px 0 0;
    width: 16px;
    height: 16px;
    flex: none;
  }
  .toggle > span {
    display: grid;
    gap: 2px;
    font: 13.5px var(--sans);
    letter-spacing: 0;
    text-transform: none;
    color: var(--ink);
  }
  .toggle b {
    font-weight: 500;
  }
  .hint {
    margin: 0;
    font-size: 12.5px;
    color: var(--dim);
  }
  kbd {
    font: 11.5px var(--mono);
    background: var(--s2);
    border-radius: 3px;
    padding: 1px 5px;
    color: var(--ink2);
  }
  .facts {
    margin: 0;
    display: grid;
    gap: 6px;
  }
  .facts div {
    display: grid;
    grid-template-columns: 80px minmax(0, 1fr);
    gap: 12px;
  }
  .facts dt {
    font: 12px var(--mono);
    color: var(--dim);
  }
  .facts dd {
    margin: 0;
    color: var(--ink2);
    overflow-wrap: anywhere;
  }

  /* Channels */
  .channel-table {
    display: grid;
  }
  .ch {
    display: grid;
    grid-template-columns: minmax(100px, 180px) minmax(0, 1fr) minmax(0, 1fr) minmax(110px, auto);
    gap: 12px 16px;
    align-items: center;
    padding: 10px 0;
    border-top: 1px solid var(--rule);
  }
  .ch.head {
    padding: 0 0 6px;
    border-top: 0;
    font: 500 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink2);
  }
  .ch .phase {
    display: none;
  }
  .ch :global(.mp input) {
    background: var(--s2);
  }
  .ch .nm {
    font: 500 13.5px var(--sans);
    color: var(--ink);
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .ch .end {
    display: flex;
    gap: 12px;
    align-items: baseline;
    justify-content: flex-end;
    flex-wrap: wrap;
  }
  .ch .end small {
    color: var(--ok);
    font: 11.5px var(--mono);
  }
  .ch.off {
    grid-template-columns: minmax(100px, 180px) minmax(0, 1fr) auto;
    color: var(--dim);
  }
  .ch .note {
    font: 12px var(--mono);
    color: var(--dim);
  }
  .ch .note.crit {
    color: var(--crit);
  }

  /* Policies */
  .grant-confirmation {
    display: grid;
    gap: 6px;
    border-left: 3px solid var(--wait);
    border-radius: 4px;
    background: var(--s2);
    padding: 10px 12px;
    color: var(--ink2);
    font: 12.5px var(--mono);
  }
  .advanced-grant {
    display: grid;
    gap: 10px;
  }
  .advanced-grant summary {
    cursor: pointer;
    color: var(--acc);
    font: 500 13px var(--sans);
  }
  .advanced-grant[open] summary {
    margin-bottom: 10px;
  }
  .grant {
    display: grid;
    gap: 3px;
    background: var(--s2);
    border-radius: 4px;
    padding: 10px 12px;
    font: 12.5px var(--mono);
    color: var(--ink2);
  }
  .grant b {
    color: var(--ink);
    font: 500 13.5px var(--sans);
  }
  .grant.dim {
    opacity: 0.6;
  }

  /* Maintenance */
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

  /* Mesh */
  .hub {
    display: grid;
    grid-template-columns: 3px minmax(120px, 200px) minmax(0, 1fr) auto;
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
  .hub .end .note {
    font: 11.5px var(--mono);
  }
  .hub .end .note.crit {
    color: var(--crit);
  }
  .pairing,
  .first-node-body,
  .mesh-result {
    display: grid;
    gap: 8px;
  }
  .first-node {
    border-top: 1px solid var(--rule);
    padding-top: 10px;
    margin-top: 4px;
  }
  .first-node summary {
    cursor: pointer;
    color: var(--acc);
    font: 500 13px var(--sans);
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
  .bad,
  small.bad {
    color: var(--crit);
  }

  @media (max-width: 700px) {
    .section-nav {
      flex-wrap: nowrap;
      overflow-x: auto;
      scrollbar-width: none;
    }
    .section-nav::-webkit-scrollbar {
      display: none;
    }
    .section-nav a {
      min-height: 44px;
    }
    .grid .wide {
      grid-column: auto;
    }
    .ch,
    .ch.off {
      grid-template-columns: minmax(0, 1fr);
      gap: 8px;
    }
    .ch.head {
      display: none;
    }
    .ch .phase {
      display: block;
    }
    .ch .end {
      justify-content: flex-start;
    }
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
</style>
