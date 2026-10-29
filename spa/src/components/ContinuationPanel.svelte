<script lang="ts">
  // One piece of work, whatever shape it took, at the top of its page: one
  // bar of everything linked to it (task, plan, sessions, review, request,
  // branch), the one thing to do next, what is in the way, and what the
  // operator decided. Everything is the node's recorded state; the next action
  // is derived from it by fixed rules, so it never claims more than the
  // records say.
  //
  // The three verbs: continue (carry on from the last attempt's workspace),
  // change approach (the same, with the operator's new direction handed to the
  // next attempt), abandon (close the item, or put a plain lineage away). Once
  // the followed request is merged, closing the task is the next action, and
  // it is offered here first.
  import { api } from '../lib/api'
  import { autogrow } from '../lib/autogrow'
  import { closeOffer, verbs, workLinks } from '../lib/continuation'
  import { router } from '../lib/router.svelte'
  import type { Continuation } from '../lib/types'

  let {
    itemId = null,
    sessionId = null,
    actions = true,
    stamp = '',
    onchange,
  }: {
    itemId?: string | null
    sessionId?: string | null
    /** Next and its verbs: a live session's page has its own controls, all
        but closing the task once its request is merged. */
    actions?: boolean
    /** Read the view again when this changes: the session's state, say. */
    stamp?: string
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
    void stamp
    void load()
  })
  const links = $derived(view ? workLinks(view, { itemId, sessionId }) : [])

  const can = $derived(view ? verbs(view) : { continue: false, changeApproach: false, abandon: false })
  const closing = $derived(view?.kind === 'item' && view.next.kind === 'close' ? closeOffer(view) : null)

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
  function closeTask() {
    if (!view) return
    const id = view.id
    return act(async () => {
      await api.closeTask(id)
      await load()
      onchange?.()
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
    {#if links.length}
      <nav class="links" aria-label="Linked to this work">
        {#each links as g, i (i)}
          <span class="grp">
            {#if g.label}<span class="h5">{g.label}</span>{/if}
            {#each g.links as l, j (j)}
              <span class="ln">
                {#if !l.href}<span class="m" title={l.title}>{l.text}</span>
                {:else if l.here}<b title={l.title}>{l.text}</b>
                {:else if l.external}<a href={l.href} title={l.title} target="_blank" rel="noopener">{l.text}</a>
                {:else}<a href={l.href} title={l.title}>{l.text}</a>{/if}
                {#if l.note}<span class="st">{l.note}</span>{/if}
              </span>
            {/each}
          </span>
        {/each}
      </nav>
    {/if}

    {#if actions || closing}
      <div class="next {view.next.kind}">
        <span class="h5">Next</span>
        <span class="t">{view.next.text}</span>
        {#if view.next.session_id && ['watch', 'answer', 'resume'].includes(view.next.kind) && view.next.session_id !== sessionId}
          <a class="lnk" href="/sessions/{view.next.session_id}">Open</a>
        {/if}
      </div>
    {/if}

    {#if view.blockers.length}
      <div class="row"><span class="h5">In the way</span><span>{view.blockers.join(' · ')}</span></div>
    {/if}

    {#if closing || (actions && (can.continue || can.changeApproach || can.abandon))}
      <div class="verbs">
        {#if closing}
          <button class="btn p" disabled={busy} onclick={closeTask}>Close task</button>
        {/if}
        {#if actions && can.continue}
          <button class="btn" class:p={!closing} disabled={busy} onclick={() => carryOn(false)}>Continue</button>
        {/if}
        {#if actions && can.changeApproach}
          <button class="btn" class:p={!can.continue} disabled={busy} onclick={() => (mode = mode === 'approach' ? '' : 'approach')}
            >Change approach</button
          >
        {/if}
        {#if actions && can.abandon}
          <button class="lnk d" disabled={busy} onclick={() => (mode = mode === 'abandon' ? '' : 'abandon')}>Abandon</button>
        {/if}
      </div>
      {#if mode === 'approach'}
        <div class="form">
          <textarea
            class="grow"
            bind:value={approach}
            use:autogrow={approach}
            rows="3"
            placeholder="How the next session should go about it instead"
            aria-label="New approach"
          ></textarea>
          <button class="btn p" disabled={busy || !approach.trim()} onclick={() => carryOn(true)}>Continue this way</button>
        </div>
      {:else if mode === 'abandon'}
        <div class="form">
          <input bind:value={reason} placeholder="Why (optional)" aria-label="Why it is abandoned" />
          <button class="btn" disabled={busy} onclick={abandon}
            >{view.kind === 'item' ? 'Close the task' : 'Stop and put it away'}</button
          >
        </div>
      {/if}
    {/if}

    {#if view.decisions.answered.length}
      <!-- Folded: at the top of the page, a long list would push the work down. -->
      <details class="decided-fold">
        <summary class="h5">Decided <b>{view.decisions.answered.length}</b></summary>
        <ul class="decided">
          {#each view.decisions.answered as d (`${d.kind}-${d.session_id}-${d.at_ms}-${d.asked}`)}
            <li><span class="q">{d.asked}</span> <span class="arr">→</span> <span>{d.answer.replaceAll('_', ' ')}</span></li>
          {/each}
        </ul>
      </details>
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
  .links {
    display: flex;
    flex-wrap: wrap;
    gap: 6px 18px;
    align-items: baseline;
    font: 12.5px var(--mono);
    min-width: 0;
  }
  .grp,
  .ln {
    display: inline-flex;
    flex-wrap: wrap;
    gap: 4px 8px;
    align-items: baseline;
    min-width: 0;
  }
  .ln a,
  .ln b,
  .ln .m {
    overflow-wrap: anywhere;
  }
  .ln b {
    font-weight: 600;
    color: var(--ink);
  }
  .decided-fold summary {
    cursor: pointer;
  }
  .decided-fold .decided {
    margin-top: 6px;
  }
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
  .decided {
    margin: 0;
    padding: 0;
    list-style: none;
    display: grid;
    gap: 4px;
  }
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
