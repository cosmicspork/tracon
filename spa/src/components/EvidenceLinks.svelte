<script lang="ts">
  import { clock } from '../lib/clock.svelte'
  import { listCandidates, type EvidenceCandidate, type EvidenceCursor } from '../lib/evidence'
  import { formatAge } from '../lib/format'
  let {
    sessionId,
    reviewId,
    workItemId,
    channel,
  }: { sessionId?: string; reviewId?: string; workItemId?: string; channel?: string } = $props()

  let items = $state<EvidenceCandidate[]>([])
  let nextBefore = $state<EvidenceCursor | null>(null)
  let error = $state<string | null>(null)
  let loaded = $state(false)
  let loading = $state(false)
  let requestId = 0

  function sameRequest(requested: { sessionId?: string; reviewId?: string; workItemId?: string; channel?: string }, request: number) {
    return request === requestId
      && sessionId === requested.sessionId
      && reviewId === requested.reviewId
      && workItemId === requested.workItemId
      && channel === requested.channel
  }

  async function load(reset: boolean) {
    const requested = { sessionId, reviewId, workItemId, channel }
    if (!requested.sessionId && !requested.reviewId && !requested.workItemId) {
      requestId += 1
      items = []
      nextBefore = null
      error = null
      loaded = true
      loading = false
      return
    }
    if (!reset && !nextBefore) return
    const request = ++requestId
    loading = true
    error = null
    if (reset) {
      items = []
      nextBefore = null
      loaded = false
    }
    try {
      const page = await listCandidates({
        ...requested,
        before: reset ? undefined : nextBefore ?? undefined,
        limit: 12,
      })
      if (!sameRequest(requested, request)) return
      items = reset ? page.items : [...items, ...page.items]
      nextBefore = page.next_before
    } catch (cause) {
      if (!sameRequest(requested, request)) return
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      if (sameRequest(requested, request)) {
        loaded = true
        loading = false
      }
    }
  }

  function evidencePath(item: EvidenceCandidate) {
    const query = new URLSearchParams({
      candidate: item.candidate.id,
      channel: item.candidate.channel,
    })
    if (item.candidate.owner_node_id) query.set('owner', item.candidate.owner_node_id)
    return `/qa?${query}`
  }

  $effect(() => {
    void sessionId
    void reviewId
    void workItemId
    void channel
    void load(true)
  })
</script>

<section class="evidence-links">
  <div class="h5">Evidence</div>
  {#if !loaded}
    <div class="empty">Loading linked evidence…</div>
  {:else if error && items.length === 0}
    <div class="notice">Evidence could not be loaded · {error}</div>
  {:else if items.length === 0}
    {#if workItemId}
      <div class="empty">No candidate-bound evidence is recorded for this work item. A session must submit its committed change for review before the node can capture it here.</div>
    {:else}
      <div class="empty">No candidate-bound evidence is recorded for this {reviewId ? 'review' : 'session'}.</div>
    {/if}
  {:else}
    <div class="list">
    {#if error}<div class="notice">Evidence could not be loaded · {error}</div>{/if}
      {#each items as item (item.candidate.id)}
        <article>
          <div class="identity">
            <a href={evidencePath(item)}>Evidence · {item.candidate.head_sha.slice(0, 12)}</a>
            <small>{item.candidate.channel} · captured {formatAge(item.candidate.captured_ms, clock.now)}</small>
          </div>
          {#if item.work_items.length || item.reviews.length}
            <div class="relations">
              {#each item.work_items as work (work.id)}
                <a href="/work/{encodeURIComponent(work.id)}">Task · {work.title}</a>
              {/each}
              {#each item.reviews as review (review.id)}
                <a href="/reviews/{encodeURIComponent(review.id)}">Review · {review.title}</a>
              {/each}
              {#if item.more_work_items || item.more_reviews}
                <span class="more">Additional recorded task or review links are omitted from this bounded card.</span>
              {/if}
            </div>
          {/if}
        </article>
      {/each}
    </div>
    {#if nextBefore}
      <button class="btn" type="button" onclick={() => load(false)} disabled={loading}>{loading ? 'Loading…' : 'Load more evidence'}</button>
    {/if}
  {/if}
</section>

<style>
  .evidence-links { display: grid; gap: .55rem; margin-top: 1.25rem; }
  .h5 { color: var(--dim); font: 12px var(--mono); text-transform: uppercase; letter-spacing: .08em; }
  .list { display: grid; gap: .4rem; }
  article { display: grid; gap: .4rem; padding: .7rem .8rem; background: var(--s1); border-left: 3px solid var(--acc); }
  .identity { display: grid; gap: .15rem; min-width: 0; }
  .identity a, .relations a { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  small { color: var(--ink2); font: 11px var(--mono); }

  .relations { display: flex; flex-wrap: wrap; gap: .45rem .7rem; }
  .relations a { color: var(--ink2); font: 12px var(--mono); }
  .notice { color: var(--wait); font: 12px var(--mono); }
  .more { color: var(--dim); font: 11px var(--mono); }
  @media (max-width: 650px) {
    .identity a, .relations a { display: flex; align-items: center; min-height: 44px; }
  }
</style>
