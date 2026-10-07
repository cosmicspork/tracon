<script lang="ts">
  // One piece of work, whatever shape it took: what it is for, the one thing
  // to do next, what is in the way, every attempt and how it ended, what the
  // operator decided, where the work lives and what it showed. Everything is
  // the node's recorded state; the next action is derived from it by fixed
  // rules, so it never claims more than the records say.
  //
  // The three verbs: continue (carry on from the last attempt's workspace),
  // change approach (the same, with the operator's new direction handed to the
  // next attempt), abandon (close the item, or put a plain lineage away).
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { attemptState, carries, verbs } from '../lib/continuation'
  import { formatAge, formatTokens } from '../lib/format'
  import { router } from '../lib/router.svelte'
  import type { Continuation } from '../lib/types'

  let {
    itemId = null,
    sessionId = null,
    showIntent = true,
    onchange,
  }: {
    itemId?: string | null
    sessionId?: string | null
    /** An item's page already shows its title and body. */
    showIntent?: boolean
    onchange?: () => void
  } = $props()

  let view = $state<Continuation | null>(null)
  let error = $state<string | null>(null)
  let busy = $state(false)
  let mode = $state<'' | 'approach' | 'abandon'>('')
  let approach = $state('')
  let reason = $state('')

  async function load() {
    try {
      view = itemId ? await api.workContinuation(itemId) : sessionId ? await api.sessionContinuation(sessionId) : null
      error = null
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    }
  }
  $effect(() => {
    void itemId
    void sessionId
    void load()
  })

  const can = $derived(view ? verbs(view) : { continue: false, changeApproach: false, abandon: false })

  async function act(run: () => Promise<void>) {
    if (busy) return
    busy = true
    error = null
    try {
      await run()
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
    }
  }
  function carryOn(withApproach: boolean) {
    const from = view?.actions.continue_from
    if (!from) return
    return act(async () => {
      const next = await api.carryOn({
        session_id: from,
        approach: withApproach ? approach.trim() : undefined,
      })
      router.go(`/sessions/${next.id}`)
    })
  }
  function abandon() {
    if (!view) return
    const target = view.kind === 'item' ? { item_id: view.id } : { session_id: view.id }
    return act(async () => {
      await api.abandonWork({ ...target, reason: reason.trim() })
      mode = ''
      reason = ''
      await load()
      onchange?.()
    })
  }
</script>

{#if view}
  <section class="cont" aria-label="Where this work stands">
    {#if showIntent && view.intent.source !== 'none'}
      <div class="intent">
        <span class="h5">For</span>
        <span>{view.intent.title}</span>
      </div>
    {/if}

    <div class="next {view.next.kind}">
      <span class="h5">Next</span>
      <span class="t">{view.next.text}</span>
      {#if view.next.session_id && ['watch', 'answer', 'resume'].includes(view.next.kind) && view.next.session_id !== sessionId}
        <a class="lnk" href="/sessions/{view.next.session_id}">Open</a>
      {/if}
    </div>

    {#if view.blockers.length}
      <div class="row"><span class="h5">In the way</span><span>{view.blockers.join(' · ')}</span></div>
    {/if}

    {#if can.continue || can.changeApproach || can.abandon}
      <div class="verbs">
        {#if can.continue}
          <button class="btn p" disabled={busy} onclick={() => carryOn(false)}>Continue</button>
        {/if}
        {#if can.changeApproach}
          <button class="btn" class:p={!can.continue} disabled={busy} onclick={() => (mode = mode === 'approach' ? '' : 'approach')}
            >Change approach</button
          >
        {/if}
        {#if can.abandon}
          <button class="lnk d" disabled={busy} onclick={() => (mode = mode === 'abandon' ? '' : 'abandon')}>Abandon</button>
        {/if}
      </div>
      {#if mode === 'approach'}
        <div class="form">
          <textarea
            bind:value={approach}
            rows="3"
            placeholder="How the next attempt should go about it instead"
            aria-label="New approach"
          ></textarea>
          <button class="btn p" disabled={busy || !approach.trim()} onclick={() => carryOn(true)}>Continue this way</button>
        </div>
      {:else if mode === 'abandon'}
        <div class="form">
          <input bind:value={reason} placeholder="Why (optional)" aria-label="Why it is abandoned" />
          <button class="btn" disabled={busy} onclick={abandon}
            >{view.kind === 'item' ? 'Close the item' : 'Stop and put it away'}</button
          >
        </div>
      {/if}
    {/if}

    {#if view.attempts.length}
      <div class="h5">Attempts <b>{view.attempts.length}</b></div>
      <ol class="attempts">
        {#each view.attempts as a (a.id)}
          {@const from = carries(a, view.attempts)}
          <li class:here={a.id === sessionId}>
            <a href="/sessions/{a.id}">{a.phase} {a.id.slice(0, 8)}</a>
            <span class="st">{attemptState(a)}</span>
            <small
              >{a.model}{from ? ` · continues ${from}` : ''} · {formatTokens(a.tokens_used)} tokens · {formatAge(a.created_ms, clock.now)}
              ago</small
            >
          </li>
        {/each}
      </ol>
    {/if}

    {#if view.decisions.answered.length || (view.kind === 'session' && (view.decisions.plan || view.decisions.brief))}
      <div class="h5">Decided <b>{view.decisions.answered.length}</b></div>
      <ul class="decided">
        {#each view.decisions.answered as d (`${d.kind}-${d.session_id}-${d.at_ms}-${d.asked}`)}
          <li><span class="q">{d.asked}</span> <span class="arr">→</span> <span>{d.answer.replaceAll('_', ' ')}</span></li>
        {/each}
      </ul>
    {/if}

    {#if view.workspace}
      <div class="row">
        <span class="h5">Workspace</span>
        <span class="m">{view.workspace.id.slice(0, 12)} · branch {view.workspace.branch}</span>
      </div>
    {/if}

    {#if view.evidence.reviews.length || view.evidence.shown.length}
      <div class="h5">Evidence <b>{view.evidence.reviews.length + view.evidence.shown.length}</b></div>
      <ul class="decided">
        {#each view.evidence.reviews as r (r.id)}
          <li><a href="/reviews/{r.id}">review · {r.title}</a> <span class="st">{r.state}{r.publish_result ? ` · ${r.publish_result}` : ''}</span></li>
        {/each}
        {#each view.evidence.shown as w (w.id)}
          <li>
            {#if w.session_id}<a href="/sessions/{w.session_id}">shown · {w.title}</a>{:else}shown · {w.title}{/if}
            <span class="st">{w.head_sha.slice(0, 7)}{w.stale ? ' · stale' : ''}</span>
          </li>
        {/each}
      </ul>
    {/if}

    {#if error}<div class="banner crit">refused <b>· {error}</b></div>{/if}
  </section>
{:else if error}
  <div class="banner crit">work view unavailable <b>· {error}</b></div>
{/if}

<style>
  .cont {
    display: grid;
    gap: 8px;
    margin: 14px 0;
    max-width: 80ch;
  }
  .h5 {
    font: 11.5px var(--mono);
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--dim);
  }
  .h5 b {
    color: var(--ink2);
    font-weight: 400;
    margin-left: 8px;
    text-transform: none;
    letter-spacing: 0;
  }
  .intent,
  .row,
  .next {
    display: flex;
    gap: 10px;
    align-items: baseline;
    flex-wrap: wrap;
    min-width: 0;
  }
  .next {
    background: var(--s1);
    border-radius: 4px;
    padding: 8px 12px;
  }
  .next .t {
    color: var(--ink);
    flex: 1 1 24ch;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .next.change_approach,
  .next.answer {
    box-shadow: inset 3px 0 0 var(--wait);
  }
  .next.done {
    color: var(--dim);
  }
  .verbs {
    display: flex;
    gap: 12px;
    align-items: center;
    flex-wrap: wrap;
  }
  .form {
    display: grid;
    gap: 8px;
    justify-items: start;
  }
  .form textarea,
  .form input {
    width: 100%;
    box-sizing: border-box;
    background: var(--s2);
    color: var(--ink);
    border: 1px solid var(--rule);
    border-radius: 6px;
    padding: 6px 8px;
    font: 13px var(--sans);
  }
  .attempts,
  .decided {
    margin: 0;
    padding: 0;
    list-style: none;
    display: grid;
    gap: 4px;
  }
  .attempts li {
    display: flex;
    gap: 4px 10px;
    flex-wrap: wrap;
    align-items: baseline;
    font: 12.5px var(--mono);
  }
  .attempts li.here a {
    font-weight: 600;
  }
  .attempts small,
  .st,
  .m {
    color: var(--dim);
    font: 12px var(--mono);
    overflow-wrap: anywhere;
  }
  .decided li {
    font-size: 13px;
    overflow-wrap: anywhere;
  }
  .decided .q {
    color: var(--ink2);
  }
  .arr {
    color: var(--dim);
  }
</style>
