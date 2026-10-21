<script lang="ts">
  import { api } from '../lib/api'
  import { codexLimitLabel, formatCodexLimits, formatTokens } from '../lib/format'
  import { store } from '../lib/store.svelte'
  import type { CodexLimits } from '../lib/types'
  import Card from './settings/Card.svelte'

  // What a subscription plan said about its windows today, read once when
  // the card opens: the plan reports on each answer, not on a clock.
  let limits = $state<CodexLimits[]>([])
  $effect(() => {
    const today = new Date()
    today.setHours(0, 0, 0, 0)
    api.codexLimits(today.getTime()).then(
      (l) => (limits = l),
      () => (limits = []),
    )
  })

  // An archived channel with nothing spent today is history, not a meter.
  const channels = $derived(
    store.channels.filter((c) => !c.name.startsWith('@') && (!c.archived || c.ceiling.usage_today > 0)),
  )
  function width(c: (typeof channels)[number]): number {
    if (!c.ceiling.ceiling) return 0
    return Math.min(100, Math.round((c.ceiling.usage_today / c.ceiling.ceiling) * 100))
  }
</script>

<Card title="Usage today" note="Gateway tokens per channel. Resets at local midnight.">
{#snippet actions()}<a class="lnk" href="/usage">Full usage</a>{/snippet}
<div class="meters">
  {#each channels as c (c.name)}
    <div class="meter {c.ceiling.state}">
      <span>{c.name}</span>
      <div class="track"><div class="fill" style="width:{width(c)}%"></div></div>
      <span class="v"
        >{formatTokens(c.ceiling.usage_today)}
        {#if c.ceiling.ceiling}<em>of {formatTokens(c.ceiling.ceiling)}{c.ceiling.state === 'near' ? ' · near' : c.ceiling.state === 'at' ? ' · at ceiling' : ''}</em
          >{:else}<em>· no ceiling</em>{/if}
        <!-- A meter that stopped measuring for part of the day says so, rather
             than letting those turns read as a quiet afternoon. -->
        {#if c.ceiling.unmetered_turns > 0}<em
            class="unsure"
            title="Turns today whose model calls this node could not count: the provider returned no usage. Not counted above — unknown, not free."
            >· {c.ceiling.unmetered_turns} unmetered</em
          >{/if}</span
      >
      {#each limits.filter((l) => l.channel === c.name) as l (l.provider)}
        <div class="plan" title="What the subscription last reported through this node's gateway. The ChatGPT workspace admin console remains the authoritative credit ledger.">
          {codexLimitLabel(l)}: {#each formatCodexLimits(l) as line, i (i)}<span class:crit={line.startsWith('limit reached')}>{i ? ' · ' : ''}{line}</span>{/each}
        </div>
      {/each}
    </div>
  {/each}
  {#if channels.length === 0}
    <div class="empty">No channels yet.</div>
  {/if}
</div>
</Card>

<style>
  .meters {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .meter {
    display: grid;
    grid-template-columns: 96px minmax(0, 1fr) auto;
    gap: 0 12px;
    align-items: center;
    font: 12.5px var(--mono);
    color: var(--ink2);
  }
  .meter > span:first-child {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .plan {
    grid-column: 2 / -1;
    font-size: 11.5px;
    color: var(--dim);
  }
  .plan .crit {
    color: var(--crit);
  }
  .track {
    height: 6px;
    border-radius: 3px;
    background: var(--s3);
    overflow: hidden;
  }
  .fill {
    height: 100%;
    background: var(--acc);
  }
  .meter.near .fill {
    background: var(--wait);
  }
  .meter.near .v {
    color: var(--wait);
  }
  .meter.at .fill {
    background: var(--crit);
  }
  .meter.at .v {
    color: var(--crit);
  }
  .v {
    white-space: nowrap;
  }
  .v em {
    font-style: normal;
    color: var(--dim);
  }
  .v em.unsure {
    color: var(--wait);
  }
  @media (max-width: 700px) {
    .meter {
      grid-template-columns: 72px minmax(0, 1fr) auto;
    }
  }
</style>
