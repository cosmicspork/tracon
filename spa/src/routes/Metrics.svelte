<script lang="ts">
  import { api } from '../lib/api'
  import { formatTokens } from '../lib/format'
  import { store } from '../lib/store.svelte'
  import type { ChannelMetrics } from '../lib/types'

  let days = $state(30)
  let rows = $state<ChannelMetrics[]>([])
  let note = $state('')
  let error = $state<string | null>(null)

  $effect(() => {
    api
      .metrics(Date.now() - days * 86_400_000)
      .then((d) => {
        rows = d.channels
        note = d.note
      })
      .catch((e) => (error = e instanceof Error ? e.message : String(e)))
  })

  function num(x: number | null, f: (n: number) => string = (n) => n.toFixed(1)): string {
    return x === null ? '—' : f(x)
  }
  function dur(s: number): string {
    const m = Math.round(s / 60)
    return m < 60 ? `${m}m` : `${Math.floor(m / 60)}h${String(m % 60).padStart(2, '0')}`
  }
</script>

<div class="h4">
  Metrics
  <b>last {days} days · as seen from {store.node?.name ?? 'this node'}</b>
  <span class="r">
    {#each [7, 30, 90] as d (d)}<button class="lnk" class:on={days === d} onclick={() => (days = d)}>{d}d</button>{/each}
  </span>
</div>

{#if error}
  <div class="banner crit">metrics <b>· {error}</b></div>
{:else}
  <div class="scroll">
    <table>
      <thead>
        <tr><th>Channel</th><th>Approvals / accepted</th><th>Tokens / accepted</th><th>Accepted</th><th>Rejected</th><th>Tokens</th><th>Cost</th><th>Human</th><th>Agent</th><th>Sessions</th></tr>
      </thead>
      <tbody>
        {#each rows as r (r.channel)}
          <tr>
            <td>{r.channel}</td>
            <td class="big">{num(r.approvals_per_accepted_change)}</td>
            <td class="big">{num(r.tokens_per_accepted_change, formatTokens)}</td>
            <td>{r.accepted_changes}</td>
            <td>{r.rejected_changes}</td>
            <td>{formatTokens(r.tokens)}</td>
            <td class:u={r.cost_usd === null}>{r.cost_usd === null ? 'unpriced' : `$${r.cost_usd.toFixed(2)}`}</td>
            <td>{dur(r.human_seconds)}</td>
            <td>{dur(r.agent_seconds)}</td>
            <td>{r.sessions}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  </div>
  <p class="note">{note}. Approvals count permission answers and review verdicts. Tokens per accepted change are the gateway tokens of the sessions behind each accepted review. Cost appears only for providers with a price; a subscription stays unpriced. Human time is request→answer and claim→decision; agent time is session start→end.</p>
{/if}

<style>
  .h4 .r {
    margin-left: auto;
    display: flex;
    gap: 10px;
    letter-spacing: 0;
    text-transform: none;
  }
  .h4 .r .lnk.on {
    border-bottom-color: currentColor;
  }
  .scroll {
    overflow-x: auto;
  }
  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 13px;
  }
  th {
    text-align: left;
    font: 500 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--dim);
    padding: 6px 10px;
    border-bottom: 1px solid var(--rule);
    white-space: nowrap;
  }
  td {
    padding: 8px 10px;
    border-bottom: 1px solid var(--rule);
    font: 12.5px var(--mono);
    color: var(--ink2);
    white-space: nowrap;
  }
  td:first-child {
    font: 500 13.5px var(--sans);
    color: var(--ink);
  }
  td.big {
    font: 600 16px var(--sans);
    color: var(--ink);
  }
  td.u {
    color: var(--dim);
  }
  .note {
    font-size: 12.5px;
    color: var(--ink2);
    max-width: 78ch;
    margin: 0;
  }
</style>
