<script lang="ts">
  import HtmlImport from '../components/HtmlImport.svelte'
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'
  import type { Document, RecallHit } from '../lib/types'

  const KINDS = ['guide', 'ref', 'architecture', 'brief', 'plan', 'proposal', 'repo', 'note', 'meeting', 'inbox', 'other']

  let channel = $state<string>('')
  let docs = $state<Document[]>([])
  let query = $state('')
  let hits = $state<RecallHit[] | null>(null)
  let error = $state<string | null>(null)
  let creating = $state(false)
  let importing = $state(false)
  let newSlug = $state('')
  let showArchived = $state(false)

  const channels = $derived(store.channels.map((c) => c.name))

  $effect(() => {
    if (!channel && channels.length) channel = channels.includes('personal') ? 'personal' : channels[0]
  })

  $effect(() => {
    void store.docsVersion
    void channel
    api
      .docs(channel || undefined, undefined, showArchived)
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

  const live = $derived(docs.filter((d) => !d.archived))
  const archived = $derived(docs.filter((d) => d.archived))
  const grouped = $derived.by(() => {
    const m = new Map<string, Document[]>()
    for (const d of live) m.set(d.kind, [...(m.get(d.kind) ?? []), d])
    const groups = KINDS.filter((k) => m.has(k)).map((k) => [k, m.get(k)!] as const)
    return archived.length ? [...groups, ['archived', archived] as const] : groups
  })

  function create() {
    const slug = newSlug.trim().toLowerCase()
    if (!slug || !channel) return
    router.go(`/docs/${channel}/${slug}/edit`)
  }

  function imported(document: Document) {
    importing = false
    router.go(`/docs/${document.channel}/${document.slug}`)
  }
</script>

<div class="h4">
  Documents
  <b
    >{live.length} on {channel || '…'}{store.mesh?.hub.state === 'unreachable'
      ? ' · hub down · search is local'
      : ''}{textOnly ? ' · text only · no semantic search' : ''}</b
  >
  <span class="r actions">
    <button class="btn p" type="button" aria-expanded={creating} onclick={() => { creating = !creating; importing = false }}>{creating ? 'Close editor' : 'New Markdown'}</button>
    <button class="lnk" type="button" aria-expanded={importing} onclick={() => { importing = !importing; creating = false }}>{importing ? 'Cancel import' : 'Import HTML'}</button>
  </span>
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
  <label class="search">
    <span>Search documents</span>
    <input placeholder="Search by content" bind:value={query} />
  </label>
  <label class="toggle"><input type="checkbox" bind:checked={showArchived} /> <span>Show archived</span></label>
</div>

{#if creating}
  <form class="new" onsubmit={(event) => { event.preventDefault(); create() }}>
    <label class="new-slug">
      <span>Document slug</span>
      <input placeholder="kind-slug, e.g. guide-deploy" bind:value={newSlug} />
    </label>
    <button class="btn p" type="submit" disabled={!newSlug.trim()}>Write Markdown</button>
    <small>Kinds: guide, ref, architecture, plan, proposal, repo, note, meeting, inbox.</small>
  </form>
{/if}

{#if importing}
  <HtmlImport {channel} onimported={imported} />
{/if}

{#if error}
  <div class="banner crit">documents <b>· {error}</b></div>
{:else if hits !== null}
  {#if hits.length === 0}
    <div class="empty">Nothing matches this search. <button class="lnk" type="button" onclick={() => (query = '')}>Clear search</button> or use different words.</div>
  {:else}
    <div class="rows">
      {#each hits as h (h.id)}
        <a class="row" href="/docs/{channel}/{h.slug}">
          <span class="bar"></span>
          <span class="t">
            <em>{h.slug?.split('-')[0]}</em>
            {#if h.format === 'html'}<em>HTML</em>{/if}
            {h.title}
            <small>{h.text}</small>
          </span>
          <span class="act">Open</span>
        </a>
      {/each}
    </div>
  {/if}
{:else if docs.length === 0}
  <div class="empty">
    <div>No documents on {channel || 'this channel'} yet.</div>
    <div class="empty-actions">
      <button class="btn p" type="button" onclick={() => { creating = true; importing = false }}>Write Markdown</button>
      <button class="lnk" type="button" onclick={() => { importing = true; creating = false }}>Import HTML</button>
    </div>
    <small><code>tracon doc import &lt;dir&gt;</code> brings a directory of Markdown in.</small>
  </div>
{:else}
  {#each grouped as [kind, list] (kind)}
    <div class="h5">{kind} <b>{list.length}</b></div>
    <div class="rows">
      {#each list as d (d.id)}
        <a class="row" href="/docs/{d.channel}/{d.slug}">
          <span class="bar"></span>
          <span class="t">
            {#if d.format === 'html'}<em>HTML</em>{/if}
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
    align-items: end;
    gap: 8px;
    margin-bottom: 10px;
  }
  .bar .channel,
  .bar .search {
    display: grid;
    gap: 3px;
  }
  .bar .search {
    flex: 1;
    min-width: 0;
  }
  .bar .channel span,
  .bar .search span {
    font: 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink2);
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
  .bar .search input {
    width: 100%;
  }
  .bar .toggle {
    display: flex;
    align-items: center;
    gap: 6px;
    font: 12px var(--mono);
    color: var(--ink2);
    white-space: nowrap;
  }
  .bar .toggle input {
    flex: none;
  }
  .new {
    display: grid;
    grid-template-columns: minmax(260px, 1fr) auto;
    gap: 8px;
    align-items: end;
    max-width: 640px;
    margin-bottom: 10px;
  }
  .new-slug {
    display: grid;
    gap: 3px;
    min-width: 0;
  }
  .new-slug span {
    font: 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink2);
  }
  .new input {
    font-family: var(--mono);
    width: 100%;
    min-width: 0;
  }
  .new small {
    grid-column: 1 / -1;
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
  .empty-actions {
    display: flex;
    justify-content: center;
    gap: 10px;
    margin: 12px 0 6px;
  }
  .empty small {
    color: var(--ink2);
  }
  @media (max-width: 700px) {
    .h4 {
      align-items: center;
      flex-wrap: wrap;
    }
    .h4 .r {
      display: flex;
      flex-wrap: wrap;
      gap: 4px 12px;
      margin-left: 0;
    }
    .bar {
      flex-wrap: wrap;
    }
    .bar .search {
      flex-basis: 100%;
      order: -1;
    }
    .new {
      grid-template-columns: minmax(0, 1fr);
    }
    .new .btn {
      width: 100%;
    }
    .new input {
      font-size: 16px;
    }
    .empty-actions {
      flex-wrap: wrap;
    }
  }
</style>
