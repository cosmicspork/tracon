<script lang="ts">
  import { onMount } from 'svelte'
  import { admin, type AdminAccess as Access } from '../../lib/admin'
  import Card from './Card.svelte'

  let { onunlock }: { onunlock?: () => void } = $props()

  let access = $state<Access | null>(null)
  let token = $state('')
  let busy = $state(false)
  let error = $state('')

  async function refresh() {
    try {
      access = await admin.access()
      error = ''
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    }
  }

  async function unlock() {
    const supplied = token.trim()
    if (!supplied || busy) return
    busy = true
    error = ''
    try {
      await admin.login(supplied)
      token = ''
      await refresh()
      if (access?.authenticated) onunlock?.()
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      busy = false
    }
  }

  onMount(() => {
    void refresh()
  })
</script>

<Card title="Administrator access" tone="acc">
  {#snippet actions()}
    {#if access?.authenticated}
      <span class="chip">unlocked for this browser</span>
    {:else}
      <span class="chip off">locked</span>
    {/if}
  {/snippet}

  {#if !access?.authenticated && access?.token_configured}
    <form onsubmit={(event) => { event.preventDefault(); void unlock() }}>
      <label>
        <span>Operator token</span>
        <input type="password" autocomplete="current-password" bind:value={token} disabled={busy} />
      </label>
      <button class="btn p" type="submit" disabled={busy || !token.trim()}>{busy ? 'Unlocking…' : 'Unlock'}</button>
    </form>
    <p>Sign in with this node’s operator token to unlock administration in this browser.</p>
  {:else if !access?.authenticated}
    <p>Create an operator token on this node to unlock administration. Remote clients cannot create the first token.</p>
    {#if access?.local}<a class="lnk" href="/settings#maintenance">Create operator access in Maintenance</a>{/if}
  {/if}

  {#if access && !access.local}
    <p class="note">Remote administration: mesh and rollout status are available here. Open the node locally for host recovery or signing.</p>
  {/if}
  {#if error}<p class="error" role="alert">{error}</p>{/if}
</Card>

<style>
  p { margin: 0; color: var(--ink2); font-size: 13px; max-width: 90ch; }
  form { display: flex; flex-wrap: wrap; gap: 8px; align-items: end; }
  label { display: grid; gap: 5px; flex: 1 1 280px; max-width: 420px; }
  label span { font: 500 11px var(--mono); letter-spacing: 0.08em; text-transform: uppercase; color: var(--ink2); }
  input {
    min-height: 36px;
    padding: 8px 10px;
    background: var(--s2);
    color: var(--ink);
    border: 0;
    border-radius: 4px;
    font: 13.5px var(--sans);
  }
  button { min-height: 36px; }
  .note { font: 12.5px var(--mono); color: var(--dim); }
  .error { color: var(--crit); font: 12.5px var(--mono); }
</style>
