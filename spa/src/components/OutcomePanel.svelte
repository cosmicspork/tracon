<script lang="ts">
  // What the session came to, as the node recorded it. The agent's prose is
  // shown only under "Claims", each beside what does or does not back it; the
  // verdict lines above are the node's own, so a confident summary cannot
  // read as a verified one.
  import { api } from '../lib/api'
  import { formatTokens } from '../lib/format'
  import { checkLabel, claimStanding, headline } from '../lib/outcome'
  import type { SessionOutcome } from '../lib/types'

  let { id, open = false }: { id: string; open?: boolean } = $props()

  let toggled = $state<boolean | null>(null)
  const expanded = $derived(toggled ?? open)
  let view = $state<SessionOutcome | null>(null)
  let error = $state<string | null>(null)
  let loading = false

  async function load() {
    if (loading) return
    loading = true
    try {
      view = await api.sessionOutcome(id)
      error = null
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      loading = false
    }
  }
  // Read each time it is opened: an outcome moves while reviews are decided
  // and checks finish.
  $effect(() => {
    if (expanded) void load()
  })
</script>

<details id="outcome" class="outcome" open={expanded} ontoggle={(e) => (toggled = e.currentTarget.open)}>
  <summary>Outcome{view ? ` · ${headline(view)}` : ''}</summary>
  {#if error}
    <p class="err">{error}</p>
  {:else if !view}
    <p class="dim">Reading what was recorded…</p>
  {:else}
    <h4>Changed</h4>
    {#if view.changed.reviews.length}
      <ul>
        {#each view.changed.reviews as r (r.id)}
          <li>
            <code>{r.head_sha.slice(0, 12)}</code>
            <span>{r.title} <small>{r.state} · +{r.added} −{r.removed} · {r.files} file{r.files === 1 ? '' : 's'}</small></span>
          </li>
        {/each}
      </ul>
    {:else}
      <p class="dim">
        Nothing submitted for review{view.changed.workspace_changes
          ? `; the workspace changed ${view.changed.workspace_changes} time${view.changed.workspace_changes === 1 ? '' : 's'}`
          : ''}.
      </p>
    {/if}

    <h4>Verified by the node <b>{view.verified.filter((c) => c.current && c.passed).length}</b></h4>
    {#if view.verified.length}
      <ul>
        {#each view.verified as c (c.check_id)}
          <li class:fail={c.failed && c.current} class:old={!c.current}>
            <code>{c.passed ? 'pass' : c.failed ? 'fail' : c.outcome}</code>
            <small>{checkLabel(c)}</small>
          </li>
        {/each}
      </ul>
    {:else}
      <p class="dim">No check has run.</p>
    {/if}

    {#if view.needs_decision.length}
      <h4>Needs your decision <b>{view.needs_decision.length}</b></h4>
      <ul>
        {#each view.needs_decision as p (p.kind + p.id)}
          <li>
            <code>{p.kind}</code>
            <span>{p.title}</span>
          </li>
        {/each}
      </ul>
    {/if}

    {#if view.uncertain.length}
      <h4>Uncertain <b>{view.uncertain.length}</b></h4>
      <ul class="plain">
        {#each view.uncertain as u, i (i)}
          <li class="warn">{u}</li>
        {/each}
      </ul>
    {/if}

    {#if view.claims.length}
      <h4>Claims <b>{view.claims.filter((c) => c.backed).length}/{view.claims.length} backed</b></h4>
      <ul class="plain">
        {#each view.claims as c (c.source + c.id)}
          <li class="claim">
            <span>{c.title}</span>
            <small class:unbacked={!c.backed}>{c.source.replace('_', ' ')} · {claimStanding(c)}</small>
          </li>
        {/each}
      </ul>
    {/if}

    <p class="dim">
      {formatTokens(view.cost.tokens_used)} of {view.cost.budget_tokens > 0
        ? formatTokens(view.cost.budget_tokens)
        : '∞'} tokens{view.cost.cost_usd != null ? ` · $${view.cost.cost_usd.toFixed(2)}` : ''}{view.cost
        .unmetered_turns
        ? ` · ${view.cost.unmetered_turns} unmetered turn${view.cost.unmetered_turns === 1 ? '' : 's'}`
        : ''}
    </p>
  {/if}
</details>

<style>
  .outcome {
    background: var(--s1);
    border-radius: 4px;
    padding: 10px 14px;
    margin-top: 10px;
  }
  summary {
    cursor: pointer;
    font: 12px var(--mono);
    color: var(--dim);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
  h4 {
    margin: 14px 0 4px;
    font: 11.5px var(--mono);
    color: var(--dim);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
  h4 b {
    color: var(--ink);
    font-weight: 500;
  }
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }
  li {
    display: grid;
    grid-template-columns: max-content minmax(0, 1fr);
    gap: 10px;
    align-items: baseline;
    font-size: 13px;
    overflow-wrap: anywhere;
  }
  .plain li {
    grid-template-columns: minmax(0, 1fr);
  }
  .claim {
    gap: 0;
  }
  li code {
    font-size: 12px;
  }
  small {
    color: var(--dim);
    font-size: 11.5px;
  }
  .fail code {
    color: var(--red);
  }
  .old {
    opacity: 0.6;
  }
  .unbacked {
    color: var(--wait);
  }
  .warn {
    color: var(--wait);
    font-size: 12.5px;
  }
  .err {
    color: var(--red);
    font-size: 12.5px;
  }
  .dim {
    color: var(--dim);
    font-size: 12px;
  }
  @media (max-width: 700px) {
    li {
      grid-template-columns: minmax(0, 1fr);
    }
  }
</style>
