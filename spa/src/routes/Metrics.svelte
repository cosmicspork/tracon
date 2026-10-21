<script lang="ts">
  import LoadGate from '../components/LoadGate.svelte'
  import { api } from '../lib/api'
  import { codexLimitLabel, formatCodexLimits, formatTokens } from '../lib/format'
  import { Loader } from '../lib/loader.svelte'
  import { router } from '../lib/router.svelte'
  import { localDate, type UsageView, usageSearch, usageView, usageWindow } from '../lib/routes'
  import { store } from '../lib/store.svelte'
  import type { ChannelMetrics, UsageTotal } from '../lib/types'
  import { usageCost, usageCsvHref, usageHeading, usageKey, usageParams, usageProviders, usageSpan } from '../lib/usageReport'

  // The view lives in the address, so a reload or a shared link shows the same numbers.
  const view = $derived(usageView(router.search))
  const usage = new Loader<{ rows: ChannelMetrics[]; note: string }>()
  const breakdown = new Loader<UsageTotal[]>()

  $effect(() => {
    const since = usageWindow(view).since
    usage.load(String(since), () =>
      api.metrics(since).then((d) => ({ rows: d.channels, note: d.note })),
    )
  })
  $effect(() => {
    const params = usageParams(view)
    breakdown.load(usageSearch(view), () => api.usage(params).then((d) => d.totals))
  })
  const rows = $derived(usage.value?.rows ?? [])
  const note = $derived(usage.value?.note ?? '')
  const totals = $derived(breakdown.value ?? [])
  const providers = $derived(usageProviders(store.providers.map((p) => p.name), totals, view.provider))
  const label = $derived(view.days !== null && !view.to ? `last ${view.days} days` : usageSpan(view))

  function show(next: Partial<UsageView>) {
    router.go(`/usage${usageSearch({ ...view, ...next })}`)
  }
  function preset(days: number) {
    show({ days, from: null, to: null })
  }
  /**
   * A custom range keeps the other end where it is, or today when it had
   * none. A From after the To swaps the two, so the range is never empty.
   */
  function range(from: string, to: string) {
    const start = from || localDate(usageWindow(view).since)
    const end = to || localDate(Date.now())
    show(start <= end ? { days: null, from: start, to: end } : { days: null, from: end, to: start })
  }
  function num(x: number | null, f: (n: number) => string = (n) => n.toFixed(1)): string {
    return x === null ? '—' : f(x)
  }
  function dur(s: number): string {
    const m = Math.round(s / 60)
    return m < 60 ? `${m}m` : `${Math.floor(m / 60)}h${String(m % 60).padStart(2, '0')}`
  }
</script>

<div class="h4">
  Usage
  <b>{label} · as seen from {store.node?.name ?? 'this node'}</b>
  <span class="r">
    {#each [7, 30, 90] as d (d)}<button class="lnk" class:on={view.days === d && !view.from && !view.to} type="button" onclick={() => preset(d)}>{d}d</button>{/each}
  </span>
</div>
<div class="range">
  <label>From <input type="date" value={view.from ?? localDate(usageWindow(view).since)} onchange={(e) => range(e.currentTarget.value, view.to ?? '')} /></label>
  <label>To <input type="date" value={view.to ?? localDate(Date.now())} onchange={(e) => range(view.from ?? '', e.currentTarget.value)} /></label>
</div>
<p class="scope">{note || 'Grouped by channel; values are limited to activity this node can observe.'} Only model calls made through this node's gateway are counted: sessions run by an external harness on an external agent lane call their model directly, Tracon never sees those calls, and they are not included. Turns the gateway could not count are shown as unmetered, not as zero.{#if view.to} The channel outcomes below run from the first day to now.{/if}</p>

<LoadGate status={usage} what="usage" onretry={() => usage.retry()} retrying={usage.pending}>
  {#if rows.length === 0}
    <div class="empty">No usage is recorded for this window. <a class="lnk" href="/tasks">Open Tasks</a> to create a clear outcome, then start a session.</div>
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
            {#if r.codex_limits}
              <div class="limits">
                <dt title="What the subscription last reported on an answer through this node's gateway. The ChatGPT workspace admin console remains the authoritative credit ledger.">{r.codex_limits.limit_id === 'codex' ? `${r.codex_limits.provider} plan` : codexLimitLabel(r.codex_limits)}</dt>
                <dd>{#each formatCodexLimits(r.codex_limits) as line, i (i)}<span class:crit={line.startsWith('limit reached')}>{line}</span>{/each}</dd>
              </div>
            {/if}
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
  {/if}
</LoadGate>

<div class="h4 sub">
  {usageHeading(view)}
  <span class="r">
    <label>Provider
      <select value={view.provider ?? ''} onchange={(e) => show({ provider: e.currentTarget.value || null })}>
        <option value="">all</option>
        {#each providers as p (p)}<option value={p}>{p}</option>{/each}
      </select>
    </label>
    <label>Rows
      <select value={view.group} onchange={(e) => show({ group: e.currentTarget.value as UsageView['group'] })}>
        <option value="model">per model</option>
        <option value="day">per day</option>
        <option value="session">per session</option>
      </select>
    </label>
    <a class="lnk" href={usageCsvHref(view)} download>Download CSV</a>
  </span>
</div>
<LoadGate status={breakdown} what="usage by provider" onretry={() => breakdown.retry()} retrying={breakdown.pending}>
  {#if totals.length === 0}
    <div class="empty">The gateway counted no model calls{view.provider ? ` to ${view.provider}` : ''} in this window.</div>
  {:else}
    <div class="scroll">
      <table>
        <thead>
          <tr><th>Channel</th><th>Provider</th><th>{view.group === 'model' ? 'Model' : view.group === 'day' ? 'Day' : 'Session'}</th><th>Requests</th><th>Input</th><th>Output</th><th>Cache read</th><th>Sessions</th><th>Cost</th></tr>
        </thead>
        <tbody>
          {#each totals as t, i (i)}
            <tr>
              <td>{t.channel}</td>
              <td>{t.provider}</td>
              <td title={t.session_id ?? undefined}>{#if view.group === 'session' && t.session_id}<a class="lnk" href={`/sessions/${t.session_id}`}>{usageKey(t, view)}</a>{:else}{usageKey(t, view)}{/if}</td>
              <td>{t.requests}</td>
              <td>{formatTokens(t.input_tokens)}</td>
              <td>{formatTokens(t.output_tokens)}</td>
              <td>{formatTokens(t.cache_read_tokens)}</td>
              <td>{t.sessions}</td>
              <td class:u={t.cost_usd === null}>{usageCost(t.cost_usd)}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}
</LoadGate>

<details class="methodology">
  <summary>How Usage is calculated</summary>
  <p>Time to verified work is the mean session creation→first candidate with all required checks passing; missing verification is not zero. Setup failures are recorded sessions that failed before their harness started. Interventions include permission answers, review decisions, answered questions and operator pause/resume. Request waiting adds each answered request's delay and review submission→decision; overlapping waits are additive, not human working hours. Tokens per accepted change cover the sessions behind accepted reviews. Cost is shown only for priced providers. Human time in the table uses request→answer and review claim→decision; agent time is session start→end. The breakdown by provider counts each model call the gateway forwarded, in local days; input excludes cache reads, which are shown on their own and never charged. Calls a session's harness made around the gateway, as external agents on the external lanes do, are not in these numbers.</p>
</details>

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
  .limits dd {
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 2px;
    text-align: right;
  }
  .limits .crit { color: var(--crit); }
  dd { margin: 0; font-family: var(--mono); }
  .h4 .r {
    margin-left: auto;
    display: flex;
    gap: 10px;
    letter-spacing: 0;
    text-transform: none;
  }
  .h4.sub {
    margin-top: 24px;
  }
  .h4 .r label,
  .range label {
    display: flex;
    align-items: center;
    gap: 6px;
    font: 12px var(--mono);
    color: var(--ink2);
  }
  .range {
    display: flex;
    flex-wrap: wrap;
    gap: 14px;
    margin: -6px 0 14px;
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
  .scope {
    color: var(--ink2);
    font-size: 12.5px;
    max-width: 78ch;
    margin: -10px 0 2px;
  }
  .methodology {
    background: var(--s1);
    border-radius: 6px;
    color: var(--ink2);
    font-size: 12.5px;
    max-width: 78ch;
    padding: 0 14px;
  }
  .methodology summary {
    color: var(--ink);
    cursor: pointer;
    font: 600 12px var(--mono);
    letter-spacing: 0.04em;
    padding: 11px 0;
  }
  .methodology p {
    margin: 0;
    padding: 0 0 14px;
  }
  @media (max-width: 700px) {
    .h4 {
      align-items: center;
      flex-wrap: wrap;
    }
    .h4 .r {
      margin-left: 0;
    }
  }
</style>
