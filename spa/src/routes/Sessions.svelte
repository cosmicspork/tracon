<script lang="ts">
  // Every session this node knows about, including the ones put away. The home
  // shows what landed lately; this is where the rest of it lives.
  import SessionRow from '../components/SessionRow.svelte'
  import { api } from '../lib/api'
  import { isTerminalState } from '../lib/queue'
  import { store } from '../lib/store.svelte'
  import type { Session } from '../lib/types'

  let showArchived = $state(false)
  // `store.sessions` is the whole table, kept current by the stream — the same
  // source the home's short list is derived from.
  const all = $derived([...store.sessions.values()].sort((a, b) => b.created_ms - a.created_ms))
  const running = $derived(all.filter((s) => !isTerminalState(s.state) && !s.archived_ms))
  const ended = $derived(all.filter((s) => isTerminalState(s.state) && !s.archived_ms))
  const archived = $derived(all.filter((s) => s.archived_ms))

  function toggle(s: Session) {
    const call = s.archived_ms ? api.unarchiveSession(s.id) : api.archiveSession(s.id)
    void call.then(() => store.refetch()).catch(() => store.refetch())
  }
</script>

<div class="h4">Running <b>{running.length}</b></div>
{#if running.length === 0}
  <div class="empty">No sessions running. <a href="/">Start one.</a></div>
{:else}
  <div class="rows">
    {#each running as s (s.id)}
      <SessionRow session={s} />
    {/each}
  </div>
{/if}

<div class="h4">Ended <b>{ended.length}</b></div>
{#if ended.length === 0}
  <div class="empty">Nothing has ended yet.</div>
{:else}
  <div class="rows">
    {#each ended as s (s.id)}
      <SessionRow session={s} onarchive={toggle} />
    {/each}
  </div>
{/if}

{#if archived.length}
  <div class="h4">
    Archived <b>{archived.length}</b>
    <button class="lnk r" onclick={() => (showArchived = !showArchived)}
      >{showArchived ? 'hide' : 'show'}</button
    >
  </div>
  {#if showArchived}
    <div class="rows">
      {#each archived as s (s.id)}
        <SessionRow session={s} onarchive={toggle} />
      {/each}
    </div>
  {/if}
{/if}

<style>
  .rows {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .h4 .r {
    margin-left: auto;
    font-size: 12.5px;
    letter-spacing: 0;
    text-transform: none;
  }
</style>
