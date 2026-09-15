<script lang="ts">
  import { onMount } from 'svelte'
  import Card from './Card.svelte'
  import {
    admin,
    type CompatibilityFact,
    type MeshAdministration as Mesh,
    type MeshInvitation,
    type PolicyCompatibility,
  } from '../../lib/admin'

  let data = $state<Mesh | null>(null)
  let invitation = $state<MeshInvitation | null>(null)
  let invitationChannels = $state<string[]>([])
  let shareChannels = $state<string[]>([])
  let fingerprintsMatch = $state(false)
  let shareConfirmed = $state(false)
  let removing = $state<string | null>(null)
  let busy = $state('')
  let error = $state('')
  let note = $state('')
  let lastAction = $state('')
  const inviteActions = ['invite', 'poll', 'admit', 'cancel']
  const feedbackFor = (card: 'members' | 'invite' | 'share') =>
    card === 'invite' ? inviteActions.includes(lastAction) : card === 'share' ? lastAction === 'share' : !inviteActions.includes(lastAction) && lastAction !== 'share'

  async function load() {
    try {
      data = await admin.mesh()
      error = ''
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    }
  }

  async function act(action: string, work: () => Promise<string | void>) {
    if (busy) return
    busy = action
    lastAction = action
    error = ''
    note = ''
    try {
      note = (await work()) ?? ''
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      busy = ''
    }
  }

  function createInvitation() {
    void act('invite', async () => {
      invitation = await admin.createInvitation(invitationChannels)
      fingerprintsMatch = false
      return `Invitation ${invitation.display_code} is open. It shares only the selected existing channels.`
    })
  }

  function pollInvitation() {
    if (!invitation) return
    void act('poll', async () => {
      invitation = await admin.pollInvitation(invitation!.code)
      return invitation.state === 'received'
        ? 'A joining node answered. Compare both fingerprints before admitting it.'
        : 'The invitation is still waiting for a joining node.'
    })
  }

  function admitInvitation() {
    if (!invitation || !fingerprintsMatch) return
    void act('admit', async () => {
      const result = await admin.admitInvitation(invitation!.code)
      invitation = result.invitation
      await load()
      return result.directory_refreshed
        ? result.effect
        : `${result.effect} The hub directory has not refreshed yet: ${result.directory_refresh_error ?? 'unknown error'}`
    })
  }

  function cancelInvitation() {
    if (!invitation) return
    void act('cancel', async () => {
      const code = invitation!.display_code
      await admin.cancelInvitation(invitation!.code)
      invitation = null
      return `Cancelled invitation ${code}.`
    })
  }

  function removeMember(id: string, name: string) {
    void act(`remove:${id}`, async () => {
      const result = await admin.removeMember(id)
      removing = null
      await load()
      return result.directory_refreshed
        ? `${name} was removed. Membership stops future hub routing; data already shared cannot be retracted.`
        : `${name} was removed, but this node has not refreshed its hub directory: ${result.directory_refresh_error ?? 'unknown error'}`
    })
  }

  function shareWithHub() {
    if (!shareConfirmed || shareChannels.length === 0) return
    void act('share', async () => {
      const result = await admin.shareWithHub(shareChannels)
      shareConfirmed = false
      await load()
      return `${result.effect}: ${result.shared.join(', ')}.`
    })
  }

  const local = $derived(data?.local ?? false)
  const availableChannels = $derived(data?.channels ?? [])
  function comparison<T extends string | number>(
    fact: CompatibilityFact<T>,
    label: string,
    mismatch = 'does not match',
  ) {
    const localValue = fact.local ?? 'unknown'
    const peerValue = fact.peer ?? 'unknown'
    if (fact.state === 'local') return `local ${localValue}`
    if (fact.state === 'compatible') return `${peerValue} · matches local`
    if (fact.state === 'mismatch') {
      return `${peerValue} · local ${localValue}${fact.upgrade_needed ? ' — upgrade needed' : ` — ${mismatch}`}`
    }
    if (fact.peer === null) return `unknown — peer did not advertise ${label}`
    if (fact.local === null) return `unknown — this node has no verified ${label}`
    return `unknown — compatibility could not be determined`
  }

  function receipt(capability: PolicyCompatibility['receipt'], self: boolean) {
    if (capability.state === 'supported') {
      return self ? 'this node supports authenticated policy receipts' : 'peer advertises authenticated policy receipts'
    }
    if (capability.state === 'unsupported') return 'peer explicitly does not support authenticated policy receipts'
    return 'unknown or legacy — peer did not advertise policy receipt support'
  }
  onMount(() => {
    void load()
  })
</script>

{#if !data || !data.capabilities.invite}
  <Card title="Mesh administration">
    {#snippet actions()}
      <button class="lnk" onclick={() => void load()} disabled={busy !== ''}>Refresh members</button>
    {/snippet}
    {#if error}<p class="error" role="alert">{error}</p>{/if}
    {#if !data}
      <p class="dim">Unlock administrator access to read the local membership snapshot.</p>
    {:else}
      <div class="empty">No running hub client is configured on this node. Pair a hub and restart the node before administering members.</div>
    {/if}
  </Card>
{:else}
  <Card title="Members" note={data.inventory_source}>
    {#snippet actions()}
      <button class="lnk" onclick={() => void load()} disabled={busy !== ''}>Refresh members</button>
    {/snippet}
    {#if !local}
      <p class="source">Mesh changes are available remotely. Host service and runtime controls require access from the serving node.</p>
    {/if}
    {#if feedbackFor('members')}
      {#if error}<p class="error" role="alert">{error}</p>{/if}
      {#if note}<p class="note" role="status">{note}</p>{/if}
    {/if}
    {#if data.members.length}
      <div class="members">
        {#each data.members as member (member.id)}
          <article>
            <div class="member-title">
              <strong>{member.self ? `${member.name} · this node` : member.name || 'unnamed node'}</strong>
              <span class:offline={!member.reachable}>{member.reachable ? 'reachable' : 'not currently reachable'}</span>
            </div>
            <p><code>{member.id}</code></p>
            <dl>
              <div><dt>Channels</dt><dd>{member.channels.filter((channel) => channel !== '@mesh').join(', ') || 'none reported'}</dd></div>
              <div><dt>Runtime</dt><dd>{member.compatibility.runtime}{member.compatibility.pinned ? ` · expects ${member.compatibility.pinned}` : ''}{member.compatibility.found ? ` · reports ${member.compatibility.found}` : ''}</dd></div>
              <div><dt>Models</dt><dd>{member.compatibility.models.state === 'unknown' ? 'unknown — node has not reported models' : member.compatibility.models.state === 'none_offered' ? 'none offered' : `${member.compatibility.models.offered} offered`}</dd></div>
              <div><dt>App</dt><dd class:mismatch={member.compatibility.application.state === 'mismatch'}>{comparison(member.compatibility.application, 'its application version')}</dd></div>
              <div><dt>Wire</dt><dd class:mismatch={member.compatibility.wire.state === 'mismatch'}>{comparison(member.compatibility.wire, 'its wire contract')}</dd></div>
              <div><dt>Policy signer</dt><dd class:mismatch={member.compatibility.policy.identity.state === 'mismatch'}>{comparison(member.compatibility.policy.identity, 'a policy signing identity', 'trust identity differs')}</dd></div>
              <div><dt>Policy bundle</dt><dd class:mismatch={member.compatibility.policy.bundle.state === 'mismatch'}>{comparison(member.compatibility.policy.bundle, 'a policy bundle hash', 'policy rollout needed')}</dd></div>
              <div><dt>Policy receipt</dt><dd class:unknown={member.compatibility.policy.receipt.state === 'unknown_or_legacy'}>{receipt(member.compatibility.policy.receipt, member.self)}</dd></div>
            </dl>
            {#if !member.self && member.channels.includes('@mesh')}
              {#if removing === member.id}
                <div class="remove-confirm">
                  <p>Remove <b>{member.name || member.id}</b> from the hub? Future hub routing and local grants stop after refresh. Data already shared cannot be retracted.</p>
                  <button class="btn d" onclick={() => removeMember(member.id, member.name || member.id)} disabled={busy !== ''}>{busy === `remove:${member.id}` ? 'Removing…' : 'Confirm removal'}</button>
                  <button class="lnk" onclick={() => (removing = null)} disabled={busy !== ''}>Keep member</button>
                </div>
              {:else}
                <button class="lnk d" onclick={() => (removing = member.id)} disabled={busy !== ''}>Remove member…</button>
              {/if}
            {/if}
          </article>
        {/each}
      </div>
    {:else}
      <div class="empty">No node records are available from this node yet.</div>
    {/if}
  </Card>

  <Card title="Add a member" note="Choose the existing channels this invitation may hand to the new member. The mesh coordination channel is always included.">
        {#if availableChannels.length}
      <div class="choices">
        {#each availableChannels as channel (channel.name)}
          <label><input type="checkbox" bind:group={invitationChannels} value={channel.name} disabled={busy !== ''} /> {channel.name} <small>{channel.nodes.length} known member{channel.nodes.length === 1 ? '' : 's'}</small></label>
        {/each}
      </div>
    {:else}
      <p class="dim">This node holds no ordinary channel keys to share yet.</p>
    {/if}
    <button class="btn p" onclick={createInvitation} disabled={busy !== '' || invitationChannels.length === 0}>{busy === 'invite' ? 'Opening invitation…' : 'Create invitation for selected channels'}</button>

    {#if invitation}
      <div class="invite">
        <span><b>{invitation.state === 'waiting' ? 'Waiting for a node to join' : invitation.state === 'received' ? 'Fingerprint check required' : 'Member admitted'}</b> · code <code>{invitation.display_code}</code></span>
        <span>Shares <code>{invitation.channels.filter((channel) => channel !== '@mesh').join(', ') || 'no ordinary channel'}</code>.</span>
        <label class="url"><span>Invitation URL</span><input readonly value={invitation.url} /></label>
        {#if invitation.qr_svg}
          <div class="qr" aria-label="QR code for the invitation URL">{@html invitation.qr_svg}</div>
        {/if}
        <div class="actions">
          {#if invitation.state === 'waiting'}
            <button class="btn" onclick={pollInvitation} disabled={busy !== ''}>{busy === 'poll' ? 'Checking…' : 'Check for joining node'}</button>
            <button class="lnk d" onclick={cancelInvitation} disabled={busy !== ''}>Cancel invitation</button>
          {:else if invitation.state === 'received'}
            <span>Joining node: <code>{invitation.received?.name ?? 'unnamed'} · {invitation.received_fingerprint ?? 'fingerprint unavailable'}</code></span>
            <span>This node: <code>{invitation.own_fingerprint ?? 'fingerprint unavailable'}</code></span>
            <label class="confirm"><input type="checkbox" bind:checked={fingerprintsMatch} disabled={busy !== ''} /> I compared these fingerprints out of band and they match.</label>
            <button class="btn p" onclick={admitInvitation} disabled={busy !== '' || !fingerprintsMatch}>{busy === 'admit' ? 'Handing off selected keys…' : 'Confirm and admit this member'}</button>
          {/if}
        </div>
      </div>
    {/if}
    {#if feedbackFor('invite')}
      {#if error}<p class="error" role="alert">{error}</p>{/if}
      {#if note}<p class="note" role="status">{note}</p>{/if}
    {/if}
  </Card>

  <Card title="Share a channel with the hub" note="The hub replica receives the selected channel keys so it can aggregate summaries. It does not become a member that can run work.">
    <div class="choices">
      {#each availableChannels as channel (channel.name)}
        <label><input type="checkbox" bind:group={shareChannels} value={channel.name} disabled={busy !== ''} /> {channel.name}</label>
      {/each}
    </div>
    <label class="confirm"><input type="checkbox" bind:checked={shareConfirmed} disabled={busy !== '' || shareChannels.length === 0} /> I understand the selected channel keys are being handed to this hub replica.</label>
    <button class="btn p" onclick={shareWithHub} disabled={busy !== '' || !shareConfirmed || shareChannels.length === 0}>{busy === 'share' ? 'Sharing selected keys…' : 'Share selected channels with hub'}</button>
    <p class="dim">{data.capabilities.edit_member_channels.reason}</p>
    {#if feedbackFor('share')}
      {#if error}<p class="error" role="alert">{error}</p>{/if}
      {#if note}<p class="note" role="status">{note}</p>{/if}
    {/if}
  </Card>
{/if}

<style>
  .source, .dim, .note, .error, small { font: 12px var(--mono); }
  .source, .dim { color: var(--dim); margin: 0; }
  .note { color: var(--ok); margin: 0; }
  .error { color: var(--crit); margin: 0; }
  .choices { display: flex; flex-wrap: wrap; gap: 8px 14px; }
  .choices label, .confirm { display: inline-flex; gap: 7px; align-items: center; color: var(--ink2); }
  .choices small { color: var(--dim); }
  .invite, article { display: grid; gap: 7px; min-width: 0; background: var(--s2); border-radius: 4px; padding: 11px 12px; overflow-wrap: anywhere; }
  .invite span, article p, dd { margin: 0; color: var(--ink2); }
  .url { display: grid; gap: 4px; max-width: 100%; }
  .url span { font: 12px var(--mono); color: var(--dim); }
  .url input { min-height: 40px; width: 100%; background: var(--s3); border: 0; border-radius: 4px; color: var(--ink2); padding: 7px 9px; font: 12px var(--mono); }
  .qr { width: 132px; height: 132px; padding: 6px; background: #fff; border-radius: 3px; }
  .qr :global(svg) { display: block; width: 132px; height: 132px; }
  .actions, .remove-confirm { display: flex; flex-wrap: wrap; align-items: center; gap: 9px; }
  .members { display: grid; grid-template-columns: repeat(auto-fit, minmax(270px, 1fr)); gap: 8px; }
  .member-title { display: flex; justify-content: space-between; flex-wrap: wrap; gap: 8px; }
  .member-title span { color: var(--ok); font: 12px var(--mono); }
  .member-title span.offline { color: var(--dim); }
  dl { margin: 0; display: grid; gap: 3px; }
  dl div { display: grid; grid-template-columns: 100px minmax(0, 1fr); gap: 8px; }
  dd { overflow-wrap: anywhere; }
  dd.mismatch { color: var(--wait); }
  dd.unknown { color: var(--dim); }
  dt { color: var(--dim); }
  .remove-confirm { margin-top: 4px; border-top: 1px solid var(--rule); padding-top: 9px; }
  .remove-confirm p { color: var(--crit); }
  .choices + .btn, .confirm + .btn { justify-self: start; }
  .btn.d { background: var(--crit); color: var(--bg); }
  .empty { padding: 14px; border-radius: 4px; color: var(--dim); }
  @media (max-width: 700px) {
    .choices { flex-direction: column; align-items: flex-start; }
    .members { grid-template-columns: minmax(0, 1fr); }
  }
</style>
