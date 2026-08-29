<script lang="ts">
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'
  import { surface } from '../lib/surface.svelte'
  import type { Document, RecallHit } from '../lib/types'

  const KINDS = ['guide', 'ref', 'architecture', 'plan', 'proposal', 'repo', 'note', 'meeting', 'inbox', 'other']

  let channel = $state<string>('')
  let docs = $state<Document[]>([])
  let query = $state('')
  let hits = $state<RecallHit[] | null>(null)
  let error = $state<string | null>(null)
  let creating = $state(false)
  let newSlug = $state('')

  const channels = $derived(store.channels.map((c) => c.name))

  $effect(() => {
    if (!channel && channels.length) channel = channels.includes('personal') ? 'personal' : channels[0]
  })

  $effect(() => {
    void store.docsVersion
    void channel
    api
      .docs(channel || undefined)
      .then((d) => (docs = d.docs))
      .catch((e) => (error = e instanceof Error ? e.message : String(e)))
  })

  // This node embeds, but could not reach its endpoint for this query: the
  // results are narrower than usual, and a search that quietly got worse is
  // exactly what nobody notices.
  let textOnly = $state(false)
  let searchTimer: ReturnType<typeof setTimeout> | undefined
  $effect(() => {
    const q = query.trim()
    clearTimeout(searchTimer)
    if (!q) {
      hits = null
      textOnly = false
      return
    }
    searchTimer = setTimeout(() => {
      api
        .searchDocs(q, channel || undefined)
        .then((d) => {
          hits = d.hits
          textOnly = d.text_only ?? false
        })
        .catch(() => (hits = []))
    }, 150)
  })

  const grouped = $derived.by(() => {
    const m = new Map<string, Document[]>()
    for (const d of docs) m.set(d.kind, [...(m.get(d.kind) ?? []), d])
    return KINDS.filter((k) => m.has(k)).map((k) => [k, m.get(k)!] as const)
  })

  function create() {
    const slug = newSlug.trim().toLowerCase()
    if (!slug || !channel) return
    router.go(`/docs/${channel}/${slug}/edit`)
  }
</script>

<div class="h4">
  Documents
  <b
    >{docs.length} on {channel || '…'}{store.mesh?.hub.state === 'unreachable'
      ? ' · hub down · search is local'
      : ''}{textOnly ? ' · text only · no semantic search' : ''}</b
  >
  {#if !surface.phone}
    <button class="lnk r" onclick={() => (creating = !creating)}>{creating ? 'Cancel' : 'New document'}</button>
  {/if}
</div>

<div class="bar">
  {#if channels.length > 1}
    <select bind:value={channel}>
      {#each channels as c (c)}<option value={c}>{c}</option>{/each}
    </select>
  {/if}
  <input placeholder="Search by content" bind:value={query} />
</div>

{#if creating}
  <div class="new">
    <input placeholder="kind-slug, e.g. guide-deploy" bind:value={newSlug} onkeydown={(e) => e.key === 'Enter' && create()} />
    <button class="btn p" onclick={create} disabled={!newSlug.trim()}>Write it</button>
    <small>Kinds: guide, ref, architecture, plan, proposal, repo, note, meeting, inbox.</small>
  </div>
{/if}

{#if error}
  <div class="empty">{error}</div>
{:else if hits !== null}
  {#if hits.length === 0}
    <div class="empty">Nothing matches.</div>
  {:else}
    <div class="rows">
      {#each hits as h (h.id)}
        <a class="row" href="/docs/{channel}/{h.slug}">
          <span class="bar"></span>
          <span class="t">
            <em>{h.slug?.split('-')[0]}</em>
            {h.title}
            <small>{h.text}</small>
          </span>
          <span class="act">Open</span>
        </a>
      {/each}
    </div>
  {/if}
{:else if docs.length === 0}
  <div class="empty">No documents on {channel} yet. <code>tracon doc import &lt;dir&gt;</code> brings a notebook in.</div>
{:else}
  {#each grouped as [kind, list] (kind)}
    <div class="h5">{kind} <b>{list.length}</b></div>
    <div class="rows">
      {#each list as d (d.id)}
        <a class="row" href="/docs/{d.channel}/{d.slug}">
          <span class="bar"></span>
          <span class="t">
            {d.title}
            <small>{d.slug} · {formatAge(d.updated_ms, clock.now)}</small>
          </span>
          <span class="act">Open</span>
        </a>
      {/each}
    </div>
  {/each}
{/if}

<style>
  .h4 .r {
    margin-left: auto;
    letter-spacing: 0;
    text-transform: none;
  }
  .h5 {
    font: 11.5px var(--mono);
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--dim);
    margin: 14px 0 6px;
  }
  .h5 b {
    color: var(--ink2);
    font-weight: 400;
    margin-left: 6px;
  }
  .bar {
    display: flex;
    gap: 8px;
    margin-bottom: 10px;
  }
  .bar input,
  .bar select,
  .new input {
    font: 13px var(--sans);
    background: var(--s1);
    color: var(--ink);
    border: 0;
    border-radius: 4px;
    padding: 8px 10px;
  }
  .bar input {
    flex: 1;
    min-width: 0;
  }
  .new {
    display: flex;
    gap: 8px;
    align-items: center;
    margin-bottom: 10px;
    flex-wrap: wrap;
  }
  .new input {
    font-family: var(--mono);
    min-width: 260px;
  }
  .new small {
    color: var(--dim);
    font: 11.5px var(--mono);
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
    padding: 10px 14px 10px 0;
    color: inherit;
    text-decoration: none;
    overflow: hidden;
  }
  .row .bar {
    display: block;
    margin: 0;
    align-self: stretch;
    border-radius: 2px 0 0 2px;
    background: var(--s3);
  }
  .t {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .t em {
    font: 11.5px var(--mono);
    font-style: normal;
    color: var(--acc);
    margin-right: 8px;
  }
  .t small {
    display: block;
    font: 11.5px var(--mono);
    color: var(--dim);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .act {
    font: 12.5px var(--mono);
    color: var(--acc);
  }
  .empty code {
    font: 12px var(--mono);
  }
</style>
