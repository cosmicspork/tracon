<script lang="ts">
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { store } from '../lib/store.svelte'
  import type { Memory } from '../lib/types'

  let channel = $state<string>('')
  let memories = $state<Memory[]>([])
  let error = $state<string | null>(null)
  let loaded = $state(false)
  let editingId = $state<string | null>(null)
  let draft = $state('')
  let busy = $state(false)

  const channels = $derived(store.channels.map((c) => c.name))

  $effect(() => {
    if (!channel && channels.length) channel = channels.includes('personal') ? 'personal' : channels[0]
  })

  // No single `state` filter covers "live": a promoted memory is as live as
  // an active one (the corpus recalls and injects both the same way), it
  // just carries the state promotion left it in rather than being rewritten
  // to "active". Fetch every state for the channel and keep only the two
  // that are actually live, leaving the promotion queue (candidate/proposed)
  // and rejected items out of this browse view.
  const LIVE_STATES = new Set(['active', 'promoted'])

  function load() {
    if (!channel) return
    loaded = false
    api
      .memories(channel)
      .then((d) => {
        memories = d.memories.filter((m) => LIVE_STATES.has(m.state))
        loaded = true
      })
      .catch((e) => {
        error = e instanceof Error ? e.message : String(e)
        loaded = true
      })
  }

  $effect(() => {
    void channel
    load()
  })

  function edit(m: Memory) {
    editingId = m.id
    draft = m.body
    error = null
  }

  function cancel() {
    editingId = null
    draft = ''
  }

  async function save(id: string) {
    const body = draft.trim()
    if (!body) return
    busy = true
    error = null
    try {
      await api.editMemory(id, body)
      editingId = null
      load()
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
    }
  }

  async function retire(id: string) {
    busy = true
    error = null
    try {
      await api.retireMemory(id)
      if (editingId === id) editingId = null
      load()
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
    }
  }
</script>

<div class="h4">
  Memories
  <b>{memories.length} active on {channel || '…'}</b>
</div>

<div class="bar">
  {#if channels.length > 1}
    <label class="channel">
      <span>Channel</span>
      <select bind:value={channel}>
        {#each channels as c (c)}<option value={c}>{c}</option>{/each}
      </select>
    </label>
  {/if}
</div>

{#if error}
  <div class="banner crit">memories <b>· {error}</b></div>
{/if}

{#if !loaded}
  <div class="empty">Loading…</div>
{:else if memories.length === 0}
  <div class="empty">No active memories on {channel || 'this channel'}.</div>
{:else}
  <div class="rows">
    {#each memories as m (m.id)}
      <div class="row" class:editing={editingId === m.id}>
        <span class="bar-mark"></span>
        {#if editingId === m.id}
          <span class="t">
            <textarea bind:value={draft} spellcheck="false"></textarea>
            <div class="edit-act">
              <button class="btn p" disabled={busy || !draft.trim()} onclick={() => save(m.id)}>Save</button>
              <button class="lnk" disabled={busy} onclick={cancel}>Cancel</button>
            </div>
          </span>
        {:else}
          <span class="t">
            <em>{m.kind}</em>
            {m.body}
            <small
              >{m.scope}{m.scope_ref ? ` · ${m.scope_ref.slice(0, 8)}` : ''} · {Math.round(m.confidence * 100)}% ·
              {formatAge(m.updated_ms, clock.now)}</small
            >
          </span>
          <span class="act">
            <button class="lnk" disabled={busy} onclick={() => edit(m)}>Edit</button>
            <button class="lnk d" disabled={busy} onclick={() => retire(m.id)}>Retire</button>
          </span>
        {/if}
      </div>
    {/each}
  </div>
{/if}

<style>
  .bar {
    display: flex;
    align-items: end;
    gap: 8px;
    margin-bottom: 10px;
  }
  .bar .channel {
    display: grid;
    gap: 3px;
  }
  .bar .channel span {
    font: 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink2);
  }
  .bar select {
    font: 13px var(--sans);
    background: var(--s1);
    color: var(--ink);
    border: 0;
    border-radius: 4px;
    padding: 8px 10px;
  }
  .rows {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .row {
    display: grid;
    grid-template-columns: 3px minmax(0, 1fr) auto;
    gap: 0 14px;
    align-items: center;
    background: var(--s1);
    border-radius: 4px;
    padding: 11px 14px 11px 0;
  }
  .row.editing {
    grid-template-columns: 3px minmax(0, 1fr);
    align-items: start;
  }
  .bar-mark {
    align-self: stretch;
    border-radius: 2px 0 0 2px;
    background: var(--s3);
  }
  .t {
    min-width: 0;
    white-space: pre-wrap;
  }
  .t em {
    font: 11.5px var(--mono);
    font-style: normal;
    color: var(--wait);
    text-transform: uppercase;
    letter-spacing: 0.04em;
    margin-right: 8px;
  }
  .t small {
    display: block;
    font: 11.5px var(--mono);
    color: var(--dim);
    margin-top: 3px;
    white-space: normal;
  }
  .t textarea {
    display: block;
    width: 100%;
    min-height: 4.5em;
    font: 13px var(--sans);
    color: var(--ink);
    background: var(--s2);
    border: 0;
    border-radius: 3px;
    padding: 8px 10px;
    resize: vertical;
    box-sizing: border-box;
  }
  .edit-act {
    display: flex;
    gap: 12px;
    margin-top: 8px;
  }
  .act {
    display: flex;
    gap: 10px;
    align-items: center;
    white-space: nowrap;
  }
  .empty {
    color: var(--ink2);
  }
  @media (max-width: 700px) {
    .row {
      grid-template-columns: 3px minmax(0, 1fr);
      gap: 6px 12px;
    }
    .act {
      grid-column: 2;
    }
  }
</style>
