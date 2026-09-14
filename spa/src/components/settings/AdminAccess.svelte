<script lang="ts">
  import { onMount } from 'svelte'
  import { admin, type AdminAccess as Access } from '../../lib/admin'

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

<div class="access">
  <div class="heading">
    <span>Administrator access</span>
    {#if access?.authenticated}
      <span class="chip">unlocked for this browser</span>
    {:else}
      <span class="chip off">locked</span>
    {/if}
  </div>

  {#if !access?.authenticated && access?.token_configured}
    <p>Sign in with this node’s operator token to unlock administration in this browser.</p>
    <form onsubmit={(event) => { event.preventDefault(); void unlock() }}>
      <label>
        <span>Operator token</span>
        <input type="password" autocomplete="current-password" bind:value={token} disabled={busy} />
      </label>
      <button class="btn p" type="submit" disabled={busy || !token.trim()}>{busy ? 'Unlocking…' : 'Unlock administrator actions'}</button>
    </form>
  {:else if !access?.authenticated}
    <p>Create an operator token on this node to unlock administration. Remote clients cannot create the first token.</p>
    {#if access?.local}<a class="lnk" href="/settings#maintenance">Create operator access in Maintenance</a>{/if}
  {/if}

  {#if access && !access.local}
    <p class="note">Remote administration: mesh and rollout status are available here. Open the node locally for host recovery or signing.</p>
  {/if}
  {#if error}<p class="error" role="alert">{error}</p>{/if}
</div>

<style>
  .access {
    display: grid;
    gap: 8px;
    background: var(--s1);
    border-left: 3px solid var(--acc);
    border-radius: 4px;
    padding: 12px 14px;
  }
  .heading {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 10px;
    font-weight: 600;
  }
  p { margin: 0; color: var(--ink2); max-width: 68ch; }
  form { display: flex; flex-wrap: wrap; gap: 10px; align-items: end; }
  label { display: grid; gap: 4px; min-width: min(100%, 280px); }
  label span { color: var(--ink2); font: 12px var(--mono); }
  input {
    min-height: 44px;
    padding: 8px 10px;
    background: var(--s2);
    color: var(--ink);
    border: 1px solid var(--rule);
    border-radius: 4px;
  }
  button { min-height: 44px; }
  .note { font: 12.5px var(--mono); color: var(--dim); }
  .error { color: var(--crit); font: 12.5px var(--mono); }
</style>
