<script lang="ts">
  // What the item's acceptance criteria are bound to, where the item is read.
  //
  // One thing is shown that a prettier panel would collapse: a criterion whose
  // checks all pass is not shown as done. It says "checks pass · unjudged",
  // because passing checks an agent proposed is not the customer agreeing with
  // the standard — and the operator's verdict is the only thing here that can
  // say `met`. The gaps are shown beside the criteria for the same reason: a
  // view that lists only what is covered reads as though that were everything.
  import { api } from '../lib/api'
  import { PROVENANCE, refHref, refLabel } from '../lib/brief'
  import { COVERAGE, LINK_KINDS, STANDARD, VERDICTS, attention, linkSays, whatIsLeft } from '../lib/criteria'
  import { surface } from '../lib/surface.svelte'
  import type { Criteria, LinkKind, Provenance, Verdict, WorkView } from '../lib/types'

  let {
    item,
    criteria,
    revisionId,
    onchange,
  }: {
    item: WorkView
    criteria: Criteria | null
    /** Set on a review screen, so a verdict binds to what is on it. */
    revisionId?: string
    onchange: () => void
  } = $props()

  let busy = $state(false)
  let error = $state<string | null>(null)
  /** The criterion whose link form or verdict form is open, by key. */
  let linking = $state<string | null>(null)
  let judging = $state<string | null>(null)
  let kind = $state<LinkKind>('check')
  let linkProvenance = $state<Provenance>('decided')
  let value = $state('')
  let verdict = $state<Verdict>('met')
  let note = $state('')

  async function act(f: () => Promise<unknown>) {
    busy = true
    error = null
    try {
      await f()
      onchange()
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
    }
  }

  function addLink(key: string) {
    const v = value.trim()
    if (!v || !criteria) return
    return act(async () => {
      await api.addCriterionLink(item.id, key, {
        kind,
        value: v,
        provenance: linkProvenance,
        if_hash: criteria.hash,
      })
      value = ''
      linking = null
    })
  }

  function removeLink(key: string, index: number) {
    if (!criteria) return
    return act(() => api.removeCriterionLink(item.id, key, index, criteria.hash))
  }

  function judge(key: string) {
    return act(async () => {
      await api.judgeCriterion(item.id, {
        criterion: key,
        verdict,
        note: note.trim() || undefined,
        candidate_id: criteria?.candidate?.id ?? null,
        revision_id: revisionId,
      })
      note = ''
      judging = null
    })
  }
</script>

<section class="criteria">
  <div class="h5">
    Criteria
    {#if criteria}<b>{criteria.summary}</b>{/if}
  </div>

  {#if !criteria}
    <div class="empty">
      No brief, so no criteria are stated. What would make this good is worked out from the item and
      its plan, and nothing here claims the customer agreed to a standard.
    </div>
  {:else}
    <div class="meta">
      {#if criteria.candidate}
        against candidate <b>{criteria.candidate.head_sha.slice(0, 8)}</b>
      {:else}
        nothing has been captured for this item yet, so no check has a result to report
      {/if}
      · <a href="/docs/{criteria.channel}/{criteria.brief_slug}">{criteria.brief_slug}</a>
    </div>

    {#if criteria.criteria.length === 0}
      <div class="absent">{criteria.absent ?? 'no success criteria are stated'}</div>
    {:else}
      <div class="left">{whatIsLeft(criteria)}</div>
      <ul class="rows">
        {#each criteria.criteria as c, i (i)}
          <li class={attention(c.coverage)}>
            <div class="line">
              <span class="mark {c.provenance}" title={PROVENANCE[c.provenance].title}>{PROVENANCE[c.provenance].label}</span>
              <span class="text">{c.text}</span>
            </div>
            <div class="states">
              <span class="mark std {c.standard}" title={STANDARD[c.standard].title}>standard: {STANDARD[c.standard].label}</span>
              <span class="mark cov {attention(c.coverage)}" title={COVERAGE[c.coverage].title}>{COVERAGE[c.coverage].label}</span>
              {#if c.duplicate}
                <span class="mark warn">
                  Duplicate criterion; <a href="/docs/{criteria.channel}/{criteria.brief_slug}">edit the brief to distinguish these lines.</a>
                </span>
              {/if}
            </div>
            {#each c.refs as r, j (j)}
              {@const href = refHref(criteria.channel, r)}
              {#if href}
                <a class="ref" class:unknown={r.known === false} {href}>{r.kind}: {refLabel(r)}</a>
              {:else}
                <span class="ref">{r.kind}: {refLabel(r)}</span>
              {/if}
            {/each}

            {#if c.links.length === 0}
              <div class="absent">Nothing points at this. No check, scenario or observation is named.</div>
            {:else}
              <ul class="links">
                {#each c.links as l (l.index)}
                  <li>
                    <span class="mark {l.provenance}" title={PROVENANCE[l.provenance].title}>{PROVENANCE[l.provenance].label}</span>
                    <code>{l.value}</code>
                    <span class="says">{linkSays(l)}</span>
                    {#if !surface.phone}
                      <button class="lnk d" onclick={() => removeLink(c.key, l.index)} disabled={busy || c.duplicate}>Remove</button>
                    {/if}
                  </li>
                {/each}
              </ul>
            {/if}

            {#if c.judgement}
              <div class="verdict">
                You judged this <b>{c.judgement.verdict.replace('_', ' ')}</b>
                {#if c.judgement.note}· {c.judgement.note}{/if}
              </div>
            {:else if c.earlier_judgement}
              <div class="verdict earlier">
                Judged <b>{c.earlier_judgement.verdict.replace('_', ' ')}</b>
                {c.earlier_judgement.candidate_id ? 'for another attempt' : 'with no attempt named'} — it does not
                settle this one.
              </div>
            {/if}

            {#if !surface.phone}
              {#if c.duplicate}
                <div class="actions">
                  <button class="lnk" disabled>Say what settles it</button>
                  <button class="lnk" disabled>Judge it</button>
                </div>
              {:else if linking === c.key}
                <div class="form">
                  <div class="row">
                    <select bind:value={kind} aria-label="what would settle it">
                      {#each LINK_KINDS as k (k)}<option value={k}>{k}</option>{/each}
                    </select>
                    <select bind:value={linkProvenance} aria-label="who is behind this link">
                      <option value="decided">decided · you say this settles it</option>
                      <option value="observed">observed · the customer needed it</option>
                      <option value="inferred">inferred · a proposal, not a standard</option>
                    </select>
                  </div>
                  <input
                    placeholder={kind === 'check'
                      ? 'the configured command, exactly'
                      : kind === 'scenario'
                        ? "the scenario's name"
                        : 'what someone was seen to do'}
                    bind:value
                    onkeydown={(e) => e.key === 'Enter' && addLink(c.key)}
                  />
                  <div class="actions">
                    <button class="btn p" onclick={() => addLink(c.key)} disabled={busy || !value.trim()}>Add it</button>
                    <button class="lnk" onclick={() => (linking = null)} disabled={busy}>Cancel</button>
                  </div>
                </div>
              {:else if judging === c.key}
                <div class="form">
                  <div class="row">
                    <select bind:value={verdict} aria-label="your verdict">
                      {#each VERDICTS as v (v)}<option value={v}>{v.replace('_', ' ')}</option>{/each}
                    </select>
                  </div>
                  <input placeholder="what you looked at, in one sentence" bind:value={note} />
                  {#if !criteria.candidate}
                    <div class="notice">
                      Nothing has been captured for this item, so this verdict is about the criterion itself and not
                      about any attempt at it.
                    </div>
                  {/if}
                  <div class="actions">
                    <button class="btn p" onclick={() => judge(c.key)} disabled={busy}>Record the verdict</button>
                    <button class="lnk" onclick={() => (judging = null)} disabled={busy}>Cancel</button>
                  </div>
                </div>
              {:else}
                <div class="actions">
                  <button class="lnk" onclick={() => ((linking = c.key), (judging = null))} disabled={busy}>
                    Say what settles it
                  </button>
                  <button class="lnk" onclick={() => ((judging = c.key), (linking = null))} disabled={busy}>
                    Judge it
                  </button>
                </div>
              {/if}
            {/if}
          </li>
        {/each}
      </ul>
    {/if}

    {#if criteria.gaps.uncovered.length > 0}
      <div class="section">
        <div class="head">Nothing agreed points at these</div>
        <ul class="plain">
          {#each criteria.criteria.filter((c) => c.coverage === 'nothing_points_at_it' || c.coverage === 'only_proposed') as c, i (i)}
            <li>{c.text} <span class="says">{COVERAGE[c.coverage].label}</span></li>
          {/each}
        </ul>
      </div>
    {/if}

    {#if criteria.gaps.assumptions.length > 0}
      <div class="section">
        <div class="head">What the brief assumes</div>
        <ul class="plain">
          {#each criteria.gaps.assumptions as a, i (i)}
            <li>
              <span class="mark {a.provenance}" title={PROVENANCE[a.provenance].title}>{PROVENANCE[a.provenance].label}</span>
              {a.text}
              <span class="says">{a.heading}</span>
            </li>
          {/each}
        </ul>
      </div>
    {/if}

    {#if criteria.gaps.questions.length > 0}
      <div class="section">
        <div class="head">Still open</div>
        <ul class="plain">
          {#each criteria.gaps.questions as q, i (i)}<li>{q}</li>{/each}
        </ul>
      </div>
    {/if}

    {#if criteria.gaps.orphaned_judgements.length > 0}
      <div class="section">
        <div class="head">Verdicts on criteria that were reworded</div>
        <ul class="plain">
          {#each criteria.gaps.orphaned_judgements as j (j.id)}
            <li>
              <b>{j.verdict.replace('_', ' ')}</b> · {j.criterion_text}
              <span class="says">this line is no longer in the brief</span>
            </li>
          {/each}
        </ul>
      </div>
    {/if}
  {/if}

  {#if error}<div class="banner crit">refused <b>· {error}</b></div>{/if}
</section>

<style>
  .criteria {
    display: grid;
    gap: 8px;
    margin-top: 14px;
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
  .meta,
  .left {
    font: 12px var(--mono);
    color: var(--dim);
  }
  .left {
    color: var(--ink2);
  }
  .empty {
    max-width: 70ch;
    color: var(--ink2);
    font-size: 13.5px;
  }
  .notice {
    color: var(--wait);
    font: 12px var(--mono);
    max-width: 70ch;
  }
  .absent {
    color: var(--dim);
    font: 12.5px var(--mono);
  }
  ul {
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .rows {
    display: grid;
    gap: 8px;
    max-width: 80ch;
  }
  .rows > li {
    background: var(--s1);
    border-radius: 4px;
    border-left: 2px solid var(--dim);
    padding: 8px 12px;
    display: grid;
    gap: 5px;
    font-size: 13.5px;
    line-height: 1.45;
  }
  .rows > li.gap {
    border-left-color: var(--wait);
  }
  .rows > li.fail {
    border-left-color: var(--crit);
  }
  .rows > li.settled {
    border-left-color: var(--ok);
  }
  .states {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
  }
  .links {
    display: grid;
    gap: 4px;
    padding-left: 12px;
  }
  .links code {
    font: 12px var(--mono);
    color: var(--ink2);
  }
  .says {
    font: 11.5px var(--mono);
    color: var(--dim);
    margin-left: 7px;
  }
  .verdict {
    font: 12px var(--mono);
    color: var(--ok);
  }
  .verdict.earlier {
    color: var(--wait);
  }
  .mark {
    font: 11px var(--mono);
    letter-spacing: 0.04em;
    padding: 1px 5px;
    border-radius: 3px;
    background: var(--wash-dim);
    color: var(--dim);
    margin-right: 6px;
  }
  .mark.observed,
  .mark.agreed {
    background: var(--wash-ok);
    color: var(--ok);
  }
  .mark.inferred,
  .mark.proposed,
  .mark.warn,
  .mark.cov.wait,
  .mark.cov.gap {
    background: var(--wash-wait);
    color: var(--wait);
  }
  .mark.decided {
    background: var(--wash-run);
    color: var(--acc);
  }
  .mark.cov.fail {
    background: var(--wash-crit);
    color: var(--crit);
  }
  .mark.cov.settled {
    background: var(--wash-ok);
    color: var(--ok);
  }
  .ref {
    font: 11.5px var(--mono);
    color: var(--ink2);
    margin-right: 7px;
    white-space: nowrap;
  }
  .ref.unknown {
    color: var(--dim);
    text-decoration: line-through dotted;
  }
  .section {
    background: var(--s1);
    border-radius: 4px;
    padding: 8px 12px;
    max-width: 80ch;
  }
  .section .head {
    font: 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--dim);
    margin-bottom: 4px;
  }
  .plain {
    display: grid;
    gap: 4px;
    font-size: 13.5px;
  }
  .form {
    display: grid;
    gap: 7px;
    max-width: 640px;
  }
  .form .row {
    display: flex;
    gap: 8px;
  }
  .form input,
  .form select {
    background: var(--s2);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13px var(--sans);
  }
  .form .row select {
    flex: 1;
    min-width: 0;
  }
  .actions {
    display: flex;
    gap: 12px;
    align-items: center;
    flex-wrap: wrap;
  }
</style>
