<script lang="ts">
  import Diff from '../components/Diff.svelte'
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'
  import { surface } from '../lib/surface.svelte'
  import { reviewChecks, reviewVerdict, type Review } from '../lib/types'

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
    // Release on navigating away. The node's sweeper covers a client that
    // vanishes without getting here.
    return () => {
      if (!decided) void api.releaseReview(id).catch(() => {})
    }
  })

  let decided = $state(false)

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
  const verdict = $derived(review ? reviewVerdict(review) : null)
  const checks = $derived(review ? reviewChecks(review) : [])
  const session = $derived(review ? store.sessions.get(review.session_id) : undefined)
  const reviewer = $derived(review?.review_session_id ? store.sessions.get(review.review_session_id) : undefined)
  const edited = $derived(
    review !== null && (title !== review.title || body !== review.body),
  )

  async function decide(verdict: 'approve' | 'reject' | 'revise') {
    if (!review) return
    busy = true
    error = null
    try {
      decided = true
      const res = await api.decideReview(id, {
        verdict,
        reason: verdict === 'approve' ? undefined : reason,
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
    <span class="mono">{formatAge(review.created_ms, clock.now)}</span>
    <span class="t">
      <em>{stale.length > 0 ? 'Changed since submit' : 'Review'}</em>
      {title || review.title}
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

  {#if checks.length}
    <div class="checks">
      {#each checks as c (c.command)}<span class="chip ok">✓ {c.command} · {Math.round(c.ms / 1000)}s</span>{/each}
    </div>
  {/if}

  <dl class="prov">
    <div><dt>Model</dt><dd>{session ? `${session.model} · ${session.phase}` : '—'}</dd></div>
    <div><dt>Item</dt><dd>{#if session?.work_item_id}<a href="/work/{session.work_item_id}">{session.work_item_id.slice(0, 8)}</a>{:else}none{/if}</dd></div>
    <div><dt>Policy</dt><dd>{session?.policy_version != null ? `working-agreements v${session.policy_version}` : '—'}</dd></div>
    <div><dt>Reviewed by</dt><dd>{reviewer ? `${reviewer.model} · fresh session` : review.review_session_id ? 'fresh session' : 'no review model bound'}</dd></div>
    <div><dt>Commit</dt><dd>{review.head_sha.slice(0, 8)}</dd></div>
  </dl>

  {#if verdict}
    <div class="verdict" class:rc={verdict.verdict === 'request_changes'}>
      <span class="vbar"></span>
      <div class="in">
        <div class="who"><b>{verdict.verdict === 'approve' ? 'Approves' : 'Request changes'}</b> · {verdict.model} · read only the requirements and the diff{reviewer ? ` · ${Math.round(reviewer.tokens_used / 1000)}k tokens` : ''}</div>
        <div class="sum">{verdict.summary}</div>
        {#if verdict.findings?.length}
          <ul class="findings">
            {#each verdict.findings as f, i (i)}
              <li><span class="sev {f.severity ?? 'should'}">{f.severity ?? 'should'}</span><span class="path">{f.path ?? ''}{f.line ? `:${f.line}` : ''}</span><span>{f.note}</span></li>
            {/each}
          </ul>
        {/if}
      </div>
    </div>
  {:else if review.review_session_id}
    <div class="banner dim">a fresh session is reading this review <b>· its verdict lands here; yours decides</b></div>
  {/if}

  {#if review.state === 'revising'}
    <div class="banner ok">
      changes requested <b>· waiting on the agent to resubmit · {review.verdict_reason}</b>
    </div>
  {/if}

  {#if stale.length > 0}
    <div class="banner crit">
      changed since submit <b>· {stale.join(', ')} · approve is disabled; ask the agent to resubmit</b>
    </div>
  {/if}

  <div class="h4">
    Title and body <b>{surface.phone ? 'edited on the desktop' : 'edit before approving if you want to'}</b>
  </div>
  <input class="edit" bind:value={title} disabled={busy || surface.phone} />
  <textarea class="edit body" bind:value={body} disabled={busy || surface.phone}></textarea>
  {#if edited}
    <div class="note">Edited. Approving publishes what is written here, not what was submitted.</div>
  {/if}

  <div class="h4">Files <b>{files.length}</b></div>
  {#if !surface.phone}
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
  {/if}

  <!-- On the phone the file list is the diff: it decides most reviews, and each
       file opens to its hunks when it does not. -->
  <Diff diff={review.diff} perFile={surface.phone} />

  {#if error}
    <div class="banner crit">refused <b>· {error}</b></div>
  {/if}

  <div class="verdict">
    <button class="btn p" disabled={busy || stale.length > 0} onclick={() => decide('approve')}>
      Approve and publish
    </button>
    <input
      bind:value={reason}
      placeholder="What to change, or why you are rejecting — goes back to the agent"
      disabled={busy}
    />
    <button class="btn" disabled={busy || !reason.trim()} onclick={() => decide('revise')}>
      Request changes
    </button>
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
  .checks {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
  }
  .chip.ok {
    background: var(--wash-ok);
    color: var(--ok);
  }
  .prov {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
    gap: 10px 16px;
    background: var(--s1);
    border-radius: 4px;
    padding: 10px 14px;
    margin: 0;
  }
  .prov div {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .prov dt {
    font: 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--dim);
  }
  .prov dd {
    margin: 0;
    font: 12.5px var(--mono);
    color: var(--ink2);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .verdict {
    display: grid;
    grid-template-columns: 3px minmax(0, 1fr);
    gap: 0 14px;
    background: var(--s1);
    border-radius: 4px;
    overflow: hidden;
  }
  .vbar {
    background: var(--acc);
  }
  .verdict.rc .vbar {
    background: var(--wait);
  }
  .verdict .in {
    padding: 10px 14px 12px 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .who {
    font: 12px var(--mono);
    color: var(--dim);
  }
  .who b {
    font-weight: 500;
    color: var(--acc);
  }
  .verdict.rc .who b {
    color: var(--wait);
  }
  .sum {
    font-size: 13.5px;
    max-width: 70ch;
  }
  .findings {
    display: flex;
    flex-direction: column;
    gap: 4px;
    margin: 2px 0 0;
    padding: 0;
    list-style: none;
  }
  .findings li {
    display: grid;
    grid-template-columns: 64px 170px minmax(0, 1fr);
    gap: 10px;
    font-size: 12.5px;
  }
  .sev {
    font: 11px var(--mono);
    color: var(--wait);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
  .sev.nit {
    color: var(--dim);
  }
  .sev.blocking {
    color: var(--crit);
  }
  .path {
    font: 12px var(--mono);
    color: var(--ink2);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  @media (max-width: 700px) {
    .findings li {
      grid-template-columns: 64px 1fr;
    }
    .path {
      grid-column: 2;
    }
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
