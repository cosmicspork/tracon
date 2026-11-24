<script lang="ts">
  // The review history: every decided review this node holds or mirrors,
  // newest first, so one can be found again without its address. Most of all
  // a review from a harness the operator runs, which has no session to lead
  // back to it. A page at a time; archiving a session hides none of them.
  import SignInBanner from '../components/SignInBanner.svelte'
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { humanizeError } from '../lib/errors'
  import { formatAge } from '../lib/format'
  import { chipLabel } from '../lib/nodes'
  import { appendPage, historyQuery, repoLabel, sourceLabel, verdictLabel, type HistoryFilter } from '../lib/reviewHistory'
  import { store } from '../lib/store.svelte'
  import type { DecidedReview } from '../lib/types'
  import { publishedChange, requestName } from '../lib/verdicts'

  let external = $state(false)
  let channel = $state('')
  let rows = $state<DecidedReview[]>([])
  let next = $state<string | null>(null)
  let loading = $state(true)
  let error = $state<string | null>(null)
  /** Which filter the rows shown are for, so a late answer for an old one is dropped. */
  let asked = 0

  const channels = $derived([...new Set(store.channels.map((c) => c.name))].sort())

  async function load(filter: HistoryFilter, before: string | null) {
    const mine = before ? asked : ++asked
    loading = true
    error = null
    try {
      const page = await api.decidedReviews(historyQuery(filter, before))
      if (mine !== asked) return
      rows = before ? appendPage(rows, page.reviews) : page.reviews
      next = page.next
    } catch (e) {
      if (mine !== asked) return
      error = humanizeError(e instanceof Error ? e.message : String(e)) ?? 'the node did not say why'
    } finally {
      if (mine === asked) loading = false
    }
  }

  $effect(() => {
    const filter = { external, channel }
    rows = []
    next = null
    void load(filter, null)
  })
</script>

<SignInBanner />

<div class="h4">
  Decided reviews {#if rows.length}<b>{rows.length}{next ? '+' : ''}</b>{/if}
  <span class="r">
    <label class="opt-check"><input type="checkbox" bind:checked={external} /> external only</label>
    <label class="chan">
      <span>channel</span>
      <select bind:value={channel} aria-label="Channel">
        <option value="">all</option>
        {#each channels as c (c)}<option value={c}>{c}</option>{/each}
      </select>
    </label>
  </span>
</div>

{#if error}
  <div class="banner crit" role="alert">
    Could not load reviews <b>· {error}</b>
    <button class="lnk" type="button" onclick={() => void load({ external, channel }, rows.length ? next : null)}>Retry</button>
  </div>
{/if}

{#if rows.length === 0 && loading}
  <div class="empty">Loading reviews…</div>
{:else if rows.length === 0 && !error}
  <div class="empty">
    {external || channel ? 'No decided reviews match.' : 'No review has been decided yet. Open ones wait on the home.'}
  </div>
{:else}
  <div class="rows">
    {#each rows as r (r.id)}
      {@const change = publishedChange(r.publish_result)}
      <div class="line">
        <a class="row {r.state}" href="/reviews/{r.id}">
          <span class="bar"></span>
          <span class="chip" class:self={r.held_here} title={r.held_here ? 'held by this node' : 'mirrored from the node that holds it'}
            >{chipLabel(store.nodes, r.node_id)}</span
          >
          <span class="mono age" title="decided {new Date(r.decided_ms).toLocaleString()}">{formatAge(r.decided_ms, clock.now)}</span>
          <span class="t">
            <em>{verdictLabel(r.state)}</em>
            {r.title}
            <small>{[repoLabel(r), r.branch, sourceLabel(r), r.session_id ? r.channel : null].filter(Boolean).join(' · ')}</small>
          </span>
        </a>
        {#if change}
          <a class="pr mono" href={change.url} target="_blank" rel="noopener noreferrer">{requestName(r.publish_result) ?? 'published'}</a>
        {:else}
          <span class="pr gap" aria-hidden="true"></span>
        {/if}
      </div>
    {/each}
  </div>
  {#if next}
    <button class="btn more" type="button" disabled={loading} onclick={() => void load({ external, channel }, next)}
      >{loading ? 'Loading…' : 'Load more'}</button
    >
  {/if}
{/if}

<style>
  .h4 .r {
    margin-left: auto;
    display: flex;
    flex-wrap: wrap;
    gap: 6px 16px;
    align-items: center;
    font: 12.5px var(--sans);
    letter-spacing: 0;
    text-transform: none;
  }
  .opt-check,
  .chan {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    color: var(--ink2);
  }
  .rows {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .line {
    display: flex;
    align-items: stretch;
    gap: 4px;
    min-width: 0;
  }
  .row {
    flex: 1;
    min-width: 0;
    display: grid;
    grid-template-columns: 3px 88px 48px minmax(0, 1fr);
    gap: 12px;
    align-items: center;
    padding: 8px 12px 8px 0;
    background: var(--s1);
    border-radius: 4px;
    color: inherit;
    text-decoration: none;
  }
  .row:hover {
    background: var(--s2);
  }
  .bar {
    align-self: stretch;
    border-radius: 4px 0 0 4px;
    background: var(--dim);
  }
  .row.approved .bar {
    background: var(--ok);
  }
  .row.rejected .bar {
    background: var(--crit);
  }
  .chip {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .age {
    font-size: 12px;
    color: var(--dim);
  }
  .t {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .t em {
    font-style: normal;
    font-weight: 500;
    margin-right: 6px;
  }
  .t small {
    display: block;
    font-size: 12px;
    color: var(--dim);
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .pr {
    flex: none;
    width: 84px;
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--s1);
    border-radius: 4px;
    font-size: 12px;
  }
  .pr.gap {
    background: none;
  }
  .more {
    margin-top: 10px;
    align-self: flex-start;
  }
  @media (max-width: 700px) {
    .row {
      grid-template-columns: 3px 64px minmax(0, 1fr);
    }
    .pr {
      width: 64px;
    }
    .age {
      display: none;
    }
  }
</style>
