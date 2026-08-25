<script lang="ts">
  import Diff from '../components/Diff.svelte'
  import { api } from '../lib/api'
  import { formatAge } from '../lib/format'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'
  import type { Review } from '../lib/types'

  let { id }: { id: string } = $props()

  let review = $state<Review | null>(null)
  let stale = $state<string[]>([])
  let reason = $state('')
  let title = $state('')
  let body = $state('')
  let busy = $state(false)
  let error = $state<string | null>(null)
  let loaded = $state(false)

  $effect(() => {
    void id
    loaded = false
    api
      .review(id)
      .then((d) => {
        review = d.review
        stale = d.stale
        title = d.review.edited_title ?? d.review.title
        body = d.review.edited_body ?? d.review.body
        loaded = true
      })
      .catch((e) => {
        error = e instanceof Error ? e.message : String(e)
        loaded = true
      })
  })

  const target = $derived.by(() => {
    try {
      return JSON.parse(review?.target ?? 'null')
    } catch {
      return null
    }
  })
  const noun = $derived(review?.provider === 'gitlab' ? 'merge request' : 'pull request')
  const files = $derived.by(() => {
    try {
      return JSON.parse(review?.files ?? '[]') as { path: string; blob: string }[]
    } catch {
      return []
    }
  })
  const edited = $derived(
    review !== null && (title !== review.title || body !== review.body),
  )

  async function decide(verdict: 'approve' | 'reject') {
    if (!review) return
    busy = true
    error = null
    try {
      const res = await api.decideReview(id, {
        verdict,
        reason: verdict === 'reject' ? reason : undefined,
        title: verdict === 'approve' ? title : undefined,
        body: verdict === 'approve' ? body : undefined,
      })
      await store.refetch()
      if (res.published) {
        router.go(`/sessions/${review.session_id}`)
      } else {
        router.go('/')
      }
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
    }
  }
</script>

{#if !loaded}
  <div class="empty">Loading review…</div>
{:else if !review}
  <div class="banner crit">not found <b>· {error ?? 'no such review'}</b></div>
{:else}
  <div class="head" class:stale={stale.length > 0}>
    <span class="bar"></span>
    <span class="mono">{formatAge(review.created_ms)}</span>
    <span class="t">
      <em>{stale.length > 0 ? 'Changed since submit' : 'Review'}</em>
      {review.title}
      <small
        >{files.length} files · +{review.added} −{review.removed} · {review.channel}{review.claimed_ms
          ? ' · claimed'
          : ''}</small
      >
    </span>
  </div>

  <dl class="kv">
    <dt>Publishes</dt>
    <dd class="m">
      {noun} → {target?.project} · {target?.branch} into {target?.base}
    </dd>
    <dt>Session</dt>
    <dd class="m"><a href="/sessions/{review.session_id}">{review.session_id.slice(0, 8)}</a></dd>
  </dl>

  {#if stale.length > 0}
    <div class="banner crit">
      changed since submit <b>· {stale.join(', ')} · approve is disabled; ask the agent to resubmit</b>
    </div>
  {/if}

  <div class="h4">Title and body <b>edit before approving if you want to</b></div>
  <input class="edit" bind:value={title} disabled={busy} />
  <textarea class="edit body" bind:value={body} disabled={busy}></textarea>
  {#if edited}
    <div class="note">Edited. Approving publishes what is written here, not what was submitted.</div>
  {/if}

  <div class="h4">Files <b>{files.length}</b></div>
  <div class="files">
    {#each files as f (f.path)}
      <div class:moved={stale.includes(f.path)}>
        <span>{f.path}</span>
        <span class={stale.includes(f.path) ? 'bad' : 'ok'}
          >{stale.includes(f.path) ? 'changed since submit' : 'unchanged'}</span
        >
      </div>
    {/each}
  </div>

  <Diff diff={review.diff} />

  {#if error}
    <div class="banner crit">refused <b>· {error}</b></div>
  {/if}

  <div class="verdict">
    <button class="btn p" disabled={busy || stale.length > 0} onclick={() => decide('approve')}>
      Approve and publish
    </button>
    <input
      bind:value={reason}
      placeholder="Reject reason, one line — goes back to the agent"
      disabled={busy}
    />
    <button class="btn d" disabled={busy || !reason.trim()} onclick={() => decide('reject')}>
      Reject
    </button>
  </div>
{/if}

<style>
  .head {
    display: grid;
    grid-template-columns: 3px 72px minmax(0, 1fr);
    gap: 0 14px;
    align-items: center;
    background: linear-gradient(90deg, var(--wash-wait), var(--s1) 42%);
    border-radius: 4px;
    padding: 10px 14px 10px 0;
    overflow: hidden;
  }
  .head .bar {
    align-self: stretch;
    background: var(--wait);
    border-radius: 2px 0 0 2px;
  }
  .head.stale {
    background: linear-gradient(90deg, var(--wash-crit), var(--s1) 42%);
  }
  .head.stale .bar {
    background: var(--crit);
  }
  .t {
    font-weight: 500;
    min-width: 0;
  }
  .t em {
    font-style: normal;
    font-weight: 400;
    color: var(--wait);
  }
  .head.stale .t em {
    color: var(--crit);
  }
  .t small {
    display: block;
    font: 12px var(--mono);
    color: var(--dim);
    margin-top: 2px;
  }
  .kv {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 5px 16px;
    font-size: 13.5px;
    margin: 0;
  }
  .kv dt {
    font: 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--dim);
    padding-top: 3px;
  }
  .kv dd {
    margin: 0;
  }
  .kv dd.m {
    font: 12.5px var(--mono);
    color: var(--ink2);
  }
  .edit {
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 9px 11px;
    font: 13.5px var(--sans);
  }
  .edit.body {
    min-height: 110px;
    resize: vertical;
    font-family: var(--sans);
  }
  .note {
    font-size: 12.5px;
    color: var(--wait);
  }
  .files {
    background: var(--s1);
    border-radius: 4px;
    font: 12.5px var(--mono);
    overflow: hidden;
  }
  .files div {
    display: flex;
    justify-content: space-between;
    gap: 10px;
    padding: 6px 12px;
    border-top: 1px solid var(--rule);
  }
  .files div:first-child {
    border-top: 0;
  }
  .files .ok {
    color: var(--dim);
  }
  .files .bad {
    color: var(--crit);
  }
  .verdict {
    display: flex;
    gap: 10px;
    flex-wrap: wrap;
    align-items: center;
    border-top: 1px solid var(--rule);
    padding-top: 14px;
  }
  .verdict input {
    flex: 1;
    min-width: 200px;
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13px var(--sans);
  }
</style>
