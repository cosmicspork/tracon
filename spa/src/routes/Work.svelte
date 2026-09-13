<script lang="ts">
  import WorkRow from '../components/WorkRow.svelte'
  import { api } from '../lib/api'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'
  import type { WorkView } from '../lib/types'
  import { workState } from '../lib/work'

  let channel = $state<string>('')
  let items = $state<WorkView[]>([])
  let error = $state<string | null>(null)
  let adding = $state(false)
  let title = $state('')
  let body = $state('')
  let priority = $state('0')
  let busy = $state(false)
  let showClosed = $state(false)

  const channels = $derived(store.channels.filter((c) => !c.archived).map((c) => c.name))
  $effect(() => {
    if (!channel && channels.length) channel = channels.includes('personal') ? 'personal' : channels[0]
  })
  $effect(() => {
    void store.workVersion
    void store.sessions
    if (!channel) return
    api
      .work(channel)
      .then((d) => (items = d.items))
      .catch((e) => (error = e instanceof Error ? e.message : String(e)))
  })

  const titles = $derived(new Map(items.map((i) => [i.id, i.title])))
  const ready = $derived(items.filter((i) => workState(i) === 'ready'))
  const blocked = $derived(items.filter((i) => workState(i) === 'blocked'))
  const inSession = $derived(items.filter((i) => workState(i) === 'insession'))
  const closed = $derived(items.filter((i) => workState(i) === 'closed'))

  async function add(e: SubmitEvent) {
    e.preventDefault()
    if (!title.trim()) return
    busy = true
    error = null
    try {
      const item = await api.addWork({ channel, title: title.trim(), body: body.trim(), priority: Number(priority) || 0 })
      title = ''
      body = ''
      adding = false
      router.go(`/work/${item.id}`)
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
    }
  }
</script>

<div class="h4">
  Work
  <b>{items.length - closed.length} open on {channel || '…'}{closed.length ? ` · ${closed.length} closed` : ''}</b>
  <button class="btn p r" type="button" aria-expanded={adding} onclick={() => (adding = !adding)}>{adding ? 'Close form' : 'New work item'}</button>
</div>

{#if channels.length > 1}
  <div class="bar">
    <label class="channel">
      <span>Channel</span>
      <select bind:value={channel}>
        {#each channels as c (c)}<option value={c}>{c}</option>{/each}
      </select>
    </label>
  </div>
{/if}

{#if adding}
  <form class="new" onsubmit={add}>
    <label class="field">
      <span>Work title</span>
      <input placeholder="What needs doing" bind:value={title} />
    </label>
    <label class="field">
      <span>Requirements</span>
      <textarea placeholder="What done looks like and what it must not touch" bind:value={body}></textarea>
    </label>
    <div class="row2">
      <label class="priority"><span>Priority</span><input class="pri" bind:value={priority} inputmode="numeric" pattern="-?[0-9]*" /><small>Higher numbers run first.</small></label>
      <button class="btn p" type="submit" disabled={busy || !title.trim()}>Add work item</button>
      <small>Dependencies are set on the item afterwards.</small>
    </div>
  </form>
{/if}

{#if error}
  <div class="banner crit">Could not load work <b>· {error}</b></div>
{/if}

<div class="h5">Ready <b>{ready.length}</b></div>
{#if ready.length === 0}
  {#if items.length === closed.length}
    <div class="empty">No open work on {channel || 'this channel'} yet. <button class="lnk" type="button" onclick={() => (adding = true)}>Add a work item</button> to give a session a clear outcome.</div>
  {:else if blocked.length}
    <div class="empty">Every open item is blocked. Review each blocker below or <button class="lnk" type="button" onclick={() => (adding = true)}>add unblocked work</button>.</div>
  {:else}
    <div class="empty">An open session is already working. Follow it below or <button class="lnk" type="button" onclick={() => (adding = true)}>add another work item</button>.</div>
  {/if}
{:else}
  <div class="rows">
    {#each ready as v (v.id)}<WorkRow item={v} {titles} />{/each}
  </div>
{/if}

{#if blocked.length}
  <div class="h5">Blocked <b>{blocked.length}</b></div>
  <div class="rows">
    {#each blocked as v (v.id)}<WorkRow item={v} {titles} />{/each}
  </div>
{/if}

{#if inSession.length}
  <div class="h5">In session <b>{inSession.length}</b></div>
  <div class="rows">
    {#each inSession as v (v.id)}<WorkRow item={v} {titles} />{/each}
  </div>
{/if}

{#if closed.length}
  <div class="h5">
    Closed <b>{closed.length}</b>
    <button class="lnk" onclick={() => (showClosed = !showClosed)}>{showClosed ? 'Hide' : 'Show'}</button>
  </div>
  {#if showClosed}
    <div class="rows">
      {#each closed as v (v.id)}<WorkRow item={v} {titles} />{/each}
    </div>
  {/if}
{/if}

<style>
  .h4 .r {
    margin-left: auto;
    letter-spacing: 0;
    text-transform: none;
  }
  .h5 {
    font: 600 14px var(--sans);
    color: var(--ink2);
    margin: 14px 0 6px;
    display: flex;
    gap: 10px;
    align-items: baseline;
  }
  .h5 b {
    color: var(--ink2);
    font-weight: 400;
  }
  .h5 .lnk {
    letter-spacing: 0;
    text-transform: none;
    font-size: 12px;
  }
  .bar select,
  .new input,
  .new textarea {
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13.5px var(--sans);
  }
  .bar .channel {
    display: grid;
    gap: 3px;
  }
  .bar .channel span,
  .field > span,
  .priority > span {
    color: var(--ink2);
    font: 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
  }
  .new {
    display: grid;
    gap: 8px;
    max-width: 640px;
  }
  .field {
    display: grid;
    gap: 3px;
  }
  .new textarea {
    min-height: 90px;
    resize: vertical;
  }
  .row2 {
    display: flex;
    gap: 12px;
    align-items: center;
  }
  .priority {
    display: flex;
    gap: 8px;
    align-items: center;
  }
  .pri {
    width: 56px;
  }
  .row2 small {
    color: var(--dim);
    font-size: 12px;
  }
  .rows {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  /* Adding an item is directing work, so the phone gets it too: full-width
     fields, 16px inputs so iOS does not zoom the form. */
  @media (max-width: 700px) {
    .h4 {
      align-items: center;
      flex-wrap: wrap;
    }
    .h4 .r {
      margin-left: 0;
    }
    .new input,
    .new textarea {
      font-size: 16px;
    }
    .row2 {
      flex-wrap: wrap;
    }
    .row2 .btn {
      flex: 1;
    }
  }
</style>
