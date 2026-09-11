<script lang="ts">
  import { api } from '../lib/api'
  import { formatTokens } from '../lib/format'
  import { store } from '../lib/store.svelte'
  import type { ChannelMetrics } from '../lib/types'

  let days = $state(30)
  let rows = $state<ChannelMetrics[]>([])
  let note = $state('')
  let error = $state<string | null>(null)

  let generation = 0
  $effect(() => {
    const request = ++generation
    error = null
    api
      .metrics(Date.now() - days * 86_400_000)
      .then((d) => {
        if (request !== generation) return
        rows = d.channels
        note = d.note
      })
      .catch((e) => {
        if (request === generation) error = e instanceof Error ? e.message : String(e)
      })
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
  <div class="workflow">
    {#each rows as r (r.channel)}
      <section>
        <h2>{r.channel}</h2>
        <dl>
          <div><dt>Time to verified work</dt><dd>{num(r.seconds_to_first_verified_candidate, dur)}</dd></div>
          <div><dt>Verified sessions</dt><dd>{r.verified_sessions}</dd></div>
          <div><dt>Session setup failures</dt><dd>{r.setup_failures}</dd></div>
          <div><dt>Human interventions</dt><dd>{r.interventions}</dd></div>
          <div><dt>Request waiting</dt><dd>{dur(r.human_wait_seconds)}</dd></div>
        </dl>
      </section>
    {/each}
  </div>
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
  <p class="note">{note}. Time to verified work is the mean session creation→first candidate with all required checks passing; missing verification is not zero. Setup failures are recorded sessions that failed before their harness started. Interventions include permission answers, review decisions, answered questions and operator pause/resume. Request waiting adds each answered request's delay and review submission→decision; overlapping waits are additive, not human working hours. Tokens per accepted change cover the sessions behind accepted reviews. Cost is shown only for priced providers. Human time in the table uses request→answer and review claim→decision; agent time is session start→end.</p>
{/if}

<style>
  .workflow {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(min(100%, 260px), 1fr));
    gap: 12px;
    margin-bottom: 20px;
  }
  .workflow section {
    background: var(--s1);
    padding: 14px;
    border-radius: 6px;
  }
  h2 {
    font: 600 15px var(--sans);
    margin: 0 0 12px;
  }
  dl {
    margin: 0;
    font: 13px var(--sans);
  }
  dl div {
    display: flex;
    justify-content: space-between;
    gap: 12px;
    padding: 5px 0;
  }
  dt { color: var(--ink2); }
  dd { margin: 0; font-family: var(--mono); }
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
