<script lang="ts">
  // The browser end of a device approval: another window (the desktop app,
  // or a browser with no passkey) asked to be signed in or to confirm an
  // admin action, and sent its operator here. They compare the code with the
  // one that window shows, and approve with a passkey or deny.
  import { api, ApiError } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import {
    approvalHeadline,
    cannotApprove,
    codeComplete,
    settled,
    type DeviceApprovalView,
  } from '../lib/device'
  import { formatAge, formatExpiry } from '../lib/format'
  import { passkeysSupported } from '../lib/passkey'
  import { StepUpCancelled } from '../lib/stepup.svelte'

  let { id, ondone }: { id: string; ondone: () => void } = $props()

  let view = $state<DeviceApprovalView | null>(null)
  let error = $state<string | null>(null)
  let busy = $state(false)
  let code = $state('')

  async function load() {
    try {
      view = await api.deviceApproval(id)
      error = null
    } catch (err) {
      error = err instanceof ApiError ? err.message : 'the node did not answer'
    }
  }

  $effect(() => {
    void load()
  })

  const blocked = $derived(view ? cannotApprove(view, passkeysSupported()) : null)
  const done = $derived(view ? settled(view.status) : null)

  async function decide(approve: boolean) {
    if (busy || !view) return
    busy = true
    error = null
    try {
      if (approve) await api.deviceApprove(id, code)
      else await api.deviceDeny(id)
    } catch (err) {
      if (!(err instanceof StepUpCancelled)) error = err instanceof Error ? err.message : String(err)
    } finally {
      busy = false
      await load()
    }
  }
</script>

<main>
  <section class="sheet" data-testid="device-approval">
    {#if !view}
      <h1>Approve a request</h1>
      <p>{error ?? 'Loading…'}</p>
      <div class="acts"><button class="lnk" type="button" onclick={ondone}>Close</button></div>
    {:else}
      <h1 data-testid="device-approval-headline">{approvalHeadline(view)}</h1>
      {#if view.kind === 'step-up' && view.action}
        <code class="action">{view.action}</code>
      {/if}
      {#if !done}
        <p class="warn">
          Type the code the window you are {view.kind === 'sign-in' ? 'signing in' : 'using'} shows. If
          you did not just ask, or no window of yours shows a code, deny it: someone sent you this link
          to get your access.
        </p>
        <label class="code">
          <span>Code from that window</span>
          <input
            bind:value={code}
            placeholder="BCDF-GHJK"
            maxlength="12"
            autocomplete="off"
            autocapitalize="characters"
            autocorrect="off"
            spellcheck="false"
            disabled={busy}
            data-testid="device-approval-code"
          />
        </label>
        {#if view.code_tries_left < 3}
          <p class="dim">{view.code_tries_left} {view.code_tries_left === 1 ? 'try' : 'tries'} left before it is denied.</p>
        {/if}
      {/if}
      <dl>
        <dt>Asked</dt>
        <dd>{formatAge(view.created_ms, clock.now)} ago</dd>
        {#if view.requester_session}
          <dt>Session</dt>
          <dd>
            {view.requester_session}
            {#if view.requester_delegated_from}<small>(signed in by approval from {view.requester_delegated_from})</small>{/if}
          </dd>
        {/if}
        {#if view.requester_agent}
          <dt>Claims to be</dt>
          <dd class="agent">“{view.requester_agent}”</dd>
        {/if}
        {#if view.requester_addr}
          <dt>From</dt>
          <dd>{view.requester_addr} <small>(as reported)</small></dd>
        {/if}
        {#if !done}
          <dt>Status</dt>
          <dd>waiting, {formatExpiry(view.expires_ms, clock.now)}</dd>
        {/if}
      </dl>
      {#if view.same_session && !done}
        <p class="dim">This is the window that asked.</p>
      {/if}
      {#if done}
        <p class="done" data-testid="device-approval-done">{done}</p>
        <div class="acts"><button class="btn p" type="button" onclick={ondone}>Done</button></div>
      {:else}
        {#if blocked}<p class="err">{blocked}</p>{/if}
        {#if error}<p class="err" data-testid="device-approval-problem">{error}</p>{/if}
        <div class="acts">
          <button
            class="btn p"
            type="button"
            onclick={() => void decide(true)}
            disabled={busy || !!blocked || !codeComplete(code)}
            data-testid="device-approve"
          >
            {busy ? 'Working…' : view.local ? 'Approve' : 'Approve with passkey'}
          </button>
          <button class="btn d" type="button" onclick={() => void decide(false)} disabled={busy} data-testid="device-deny">
            Deny
          </button>
        </div>
      {/if}
    {/if}
  </section>
</main>

<style>
  main {
    min-height: 100dvh;
    display: grid;
    place-items: center;
    padding: 24px 16px;
  }
  .sheet {
    display: grid;
    gap: 12px;
    width: min(440px, 100%);
    background: var(--s1);
    border-radius: 6px;
    padding: 24px 20px;
  }
  h1 {
    margin: 0;
    font: 600 17px var(--sans);
    color: var(--ink);
    overflow-wrap: anywhere;
  }
  p {
    margin: 0;
    color: var(--ink2);
    font-size: 13.5px;
    line-height: 1.5;
  }
  .warn {
    color: var(--ink);
  }
  .dim {
    color: var(--dim);
  }
  .done {
    color: var(--ink);
    font-weight: 500;
  }
  .code {
    display: grid;
    gap: 2px;
    justify-items: center;
    background: var(--s2);
    border-radius: 6px;
    padding: 12px;
  }
  .code span {
    color: var(--dim);
    font: 12px var(--sans);
  }
  .code input {
    width: 100%;
    box-sizing: border-box;
    text-align: center;
    background: var(--s1);
    border: none;
    border-radius: 4px;
    padding: 8px;
    font: 600 24px var(--mono);
    letter-spacing: 0.12em;
    color: var(--ink);
    text-transform: uppercase;
  }
  .action {
    font: 12.5px var(--mono);
    color: var(--ink);
    background: var(--s2);
    border-radius: 4px;
    padding: 6px 8px;
    overflow-wrap: anywhere;
  }
  dl {
    display: grid;
    grid-template-columns: max-content minmax(0, 1fr);
    gap: 4px 12px;
    margin: 0;
    font-size: 13px;
  }
  dt {
    color: var(--dim);
  }
  dd {
    margin: 0;
    color: var(--ink2);
    overflow-wrap: anywhere;
  }
  .agent {
    font: 12px var(--mono);
  }
  small {
    color: var(--dim);
  }
  .err {
    color: var(--crit);
    font: 12.5px var(--mono);
    overflow-wrap: anywhere;
  }
  .acts {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px 16px;
  }
</style>
