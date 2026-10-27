<script lang="ts">
  // The passkeys that confirm admin actions made from off the serving node.
  // Listing is ordinary; adding (after the pairing window) and removing are
  // admin actions, so away from the node each asks for a passkey first.
  import { onMount } from 'svelte'
  import { api, type PasskeyInfo, type PasskeyList } from '../../lib/api'
  import { clock } from '../../lib/clock.svelte'
  import { formatAge } from '../../lib/format'
  import { passkeysSupported } from '../../lib/passkey'
  import { deviceLabel, Enroller, StepUpCancelled } from '../../lib/stepup.svelte'
  import Card from './Card.svelte'

  let list = $state<PasskeyList | null>(null)
  let loadError = $state('')
  let label = $state(deviceLabel(navigator.userAgent))
  let removing = $state<PasskeyInfo | null>(null)
  let removeError = $state('')
  let removeBusy = $state(false)
  let notice = $state('')
  const enroller = new Enroller()
  const supported = passkeysSupported()

  async function refresh() {
    try {
      list = await api.passkeys()
      loadError = ''
    } catch (e) {
      loadError = e instanceof Error ? e.message : String(e)
    }
  }

  /** Passkeys belong to the node's public origin; a page anywhere else (the
      desktop app, a port-forward) cannot make or use one. */
  const here = $derived(!!list?.relying_party && list.relying_party.origin === location.origin)

  async function add() {
    notice = ''
    if (await enroller.run(label.trim() || deviceLabel(navigator.userAgent))) {
      notice = 'Passkey added.'
      await refresh()
    }
  }

  async function remove() {
    if (!removing) return
    removeBusy = true
    removeError = ''
    try {
      await api.deletePasskey(removing.id)
      removing = null
      await refresh()
    } catch (e) {
      removeError = e instanceof StepUpCancelled ? '' : e instanceof Error ? e.message : String(e)
    } finally {
      removeBusy = false
    }
  }

  function transports(p: PasskeyInfo): string {
    try {
      const t = p.transports ? (JSON.parse(p.transports) as string[]) : []
      return t.join(', ')
    } catch {
      return ''
    }
  }

  onMount(() => {
    void refresh()
  })
</script>

<Card
  title="Passkeys"
  note="Confirm credential, token, grant and mesh changes made away from the node."
>
  {#if loadError}
    <span class="note crit">{loadError}</span>
  {:else if list}
    {#if list.unavailable}
      <span class="note crit" data-testid="passkeys-unavailable">{list.unavailable}</span>
    {/if}
    {#if list.passkeys.length}
      <div class="keys" data-testid="passkey-list">
        {#each list.passkeys as p (p.id)}
          <div class="key">
            <span class="name">{p.label}{#if p.backup_state}<span class="chip">synced</span>{/if}</span>
            <span class="dim">
              added {formatAge(p.created_ms, clock.now)} ago · {#if p.last_used_ms}last used {formatAge(p.last_used_ms, clock.now)} ago{:else}not used yet{/if}{#if transports(p)}{' · '}{transports(p)}{/if}
            </span>
            <button class="lnk d" type="button" onclick={() => { removing = p; removeError = '' }} disabled={removeBusy}>Remove</button>
          </div>
        {/each}
      </div>
    {:else if !list.unavailable}
      <div class="empty">
        No passkey is enrolled.{#if list.enrollment.open && list.enrollment.closes_ms}
          This browser was just paired, so it can add the first one without confirming for another {Math.max(1, Math.ceil((list.enrollment.closes_ms - clock.now) / 60_000))} min.
        {:else if !list.local}
          Pair this browser again (<code>tracon auth issue --url</code> on the node) to add the first one.
        {/if}
      </div>
    {/if}

    {#if removing}
      <div class="confirm">
        <b>Remove “{removing.label}”?</b>
        <span>
          {#if list.passkeys.length === 1}It is the only one: until another is added, admin actions work only at the node, or from a browser paired again within its first 15 minutes.{:else}It stops confirming anything on this node.{/if}
          The passkey itself stays in your password manager until you delete it there.
        </span>
        {#if removeError}<span class="note crit">{removeError}</span>{/if}
        <div class="acts">
          <button class="btn d" type="button" onclick={remove} disabled={removeBusy}>{removeBusy ? 'Removing…' : 'Remove'}</button>
          <button class="lnk" type="button" onclick={() => (removing = null)} disabled={removeBusy}>Cancel</button>
        </div>
      </div>
    {/if}

    {#if list.relying_party}
      {#if !supported}
        <span class="dim">This browser cannot make passkeys.</span>
      {:else if !here}
        <span class="dim">These passkeys belong to <code>{list.relying_party.origin}</code>; add one from a browser open there.</span>
      {:else}
        <div class="add">
          <label class="field">
            <span>Name</span>
            <input bind:value={label} maxlength="64" spellcheck="false" />
          </label>
          <button class="btn" type="button" onclick={add} disabled={enroller.busy} data-testid="add-passkey">
            {enroller.busy ? 'Follow the prompts…' : enroller.held ? 'Try again' : 'Add a passkey'}
          </button>
        </div>
      {/if}
    {/if}
    {#if enroller.problem}<span class="note crit" data-testid="passkey-problem">{enroller.problem}</span>{/if}
    {#if notice}<span class="note" data-testid="passkey-notice">{notice}</span>{/if}
  {/if}
</Card>

<style>
  .keys {
    display: grid;
    gap: 6px;
  }
  .key {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    gap: 4px 16px;
    align-items: center;
    background: var(--s2);
    border-radius: 4px;
    padding: 9px 12px;
    font: 12.5px var(--mono);
    color: var(--ink2);
  }
  .name {
    color: var(--ink);
    display: inline-flex;
    gap: 8px;
    align-items: baseline;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .key .dim {
    grid-column: 1;
  }
  .key button {
    grid-column: 2;
    grid-row: 1 / span 2;
  }
  .chip {
    font: 11px var(--mono);
    color: var(--ok);
  }
  .dim,
  .empty {
    color: var(--dim);
    font-size: 13px;
    line-height: 1.5;
  }
  .confirm {
    display: grid;
    gap: 8px;
    background: var(--wash-crit);
    border-radius: 4px;
    padding: 10px 12px;
    font-size: 13px;
    color: var(--ink2);
  }
  .confirm b {
    color: var(--ink);
  }
  .acts,
  .add {
    display: flex;
    flex-wrap: wrap;
    align-items: end;
    gap: 8px 16px;
  }
  .field {
    display: grid;
    gap: 4px;
    font-size: 12.5px;
    color: var(--dim);
  }
  .field input {
    background: var(--s2);
    border: none;
    border-radius: 4px;
    padding: 8px 10px;
    color: var(--ink);
    width: min(240px, 100%);
  }
  .note {
    font: 12.5px var(--mono);
    color: var(--ok);
    overflow-wrap: anywhere;
  }
  .note.crit {
    color: var(--crit);
  }
  code {
    font: 12px var(--mono);
    overflow-wrap: anywhere;
  }
</style>
