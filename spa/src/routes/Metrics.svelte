<script lang="ts">
  import Help from '../components/Help.svelte'
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
  const keyLabel = $derived(view.group === 'model' ? 'Model' : view.group === 'day' ? 'Day' : 'Session')
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
<p class="scope">
  {note || "This node's view."}{#if view.to} Channel outcomes run to now.{/if}
  <Help label="About what is counted"
    >Only calls through this node's gateway count. External agent lanes call their models directly and are left out. Turns the
    gateway couldn't count show as unmetered.</Help
  >
</p>

<LoadGate status={usage} what="usage" onretry={() => usage.retry()} retrying={usage.pending}>
  {#if rows.length === 0}
    <div class="empty">No usage in this window.</div>
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
                <dt title="Last reported by the subscription. The ChatGPT admin console is authoritative.">{r.codex_limits.limit_id === 'codex' ? `${r.codex_limits.provider} plan` : codexLimitLabel(r.codex_limits)}</dt>
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
              <td class="big" data-label="Approvals / accepted">{num(r.approvals_per_accepted_change)}</td>
              <td class="big" data-label="Tokens / accepted">{num(r.tokens_per_accepted_change, formatTokens)}</td>
              <td data-label="Accepted">{r.accepted_changes}</td>
              <td data-label="Rejected">{r.rejected_changes}</td>
              <td data-label="Tokens">{formatTokens(r.tokens)}</td>
              <td data-label="Cost" class:u={r.cost_usd === null}>{r.cost_usd === null ? 'unpriced' : `$${r.cost_usd.toFixed(2)}`}</td>
              <td data-label="Human">{dur(r.human_seconds)}</td>
              <td data-label="Agent">{dur(r.agent_seconds)}</td>
              <td data-label="Sessions">{r.sessions}</td>
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
    <div class="empty">No model calls{view.provider ? ` to ${view.provider}` : ''} in this window.</div>
  {:else}
    <div class="scroll">
      <table>
        <thead>
          <tr><th>Channel</th><th>Provider</th><th>{keyLabel}</th><th>Requests</th><th>Input</th><th>Output</th><th>Cache read</th><th>Sessions</th><th>Cost</th></tr>
        </thead>
        <tbody>
          {#each totals as t, i (i)}
            <tr>
              <td>{t.channel}</td>
              <td data-label="Provider">{t.provider}</td>
              <td data-label={keyLabel} title={t.session_id ?? undefined}>{#if view.group === 'session' && t.session_id}<a class="lnk" href={`/sessions/${t.session_id}`}>{usageKey(t, view)}</a>{:else}{usageKey(t, view)}{/if}</td>
              <td data-label="Requests">{t.requests}</td>
              <td data-label="Input">{formatTokens(t.input_tokens)}</td>
              <td data-label="Output">{formatTokens(t.output_tokens)}</td>
              <td data-label="Cache read">{formatTokens(t.cache_read_tokens)}</td>
              <td data-label="Sessions">{t.sessions}</td>
              <td data-label="Cost" class:u={t.cost_usd === null}>{usageCost(t.cost_usd)}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}
</LoadGate>

<details class="methodology">
  <summary>How Usage is calculated</summary>
  <dl>
    <dt>Time to verified work</dt><dd>Mean time from session start to its first candidate passing every required check.</dd>
    <dt>Setup failures</dt><dd>Sessions that failed before their harness started.</dd>
    <dt>Interventions</dt><dd>Permission answers, review decisions, answered questions, pause and resume.</dd>
    <dt>Request waiting</dt><dd>Each request's wait plus review submit→decision. Overlaps add up.</dd>
    <dt>Tokens per accepted</dt><dd>Tokens of the sessions behind accepted reviews.</dd>
    <dt>Cost</dt><dd>Priced providers only.</dd>
    <dt>Human · agent time</dt><dd>Request→answer and review claim→decision · session start→end.</dd>
    <dt>By provider</dt><dd>Each call the gateway forwarded, by local day. Input excludes cache reads, which are free.</dd>
  </dl>
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
    container-type: inline-size;
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
    vertical-align: bottom;
  }
  td {
    padding: 8px 10px;
    border-bottom: 1px solid var(--rule);
    font: 12.5px var(--mono);
    color: var(--ink2);
    white-space: nowrap;
  }
  /* A long channel name wraps instead of pushing the numbers out of view. */
  td:first-child {
    font: 500 13.5px var(--sans);
    color: var(--ink);
    white-space: normal;
    overflow-wrap: anywhere;
    min-width: 12ch;
  }
  td.big {
    font: 600 16px var(--sans);
    color: var(--ink);
  }
  td.u {
    color: var(--dim);
  }
  td[data-label]::before {
    content: attr(data-label);
    display: none;
    font: 500 10.5px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--dim);
    margin-bottom: 3px;
  }
  /* Too narrow for every column: each row becomes a card of labelled values. */
  @container (max-width: 780px) {
    table,
    tbody,
    td {
      display: block;
    }
    thead {
      display: none;
    }
    tr {
      display: grid;
      grid-template-columns: repeat(auto-fill, minmax(110px, 1fr));
      gap: 10px 16px;
      padding: 12px 0;
      border-bottom: 1px solid var(--rule);
    }
    td {
      padding: 0;
      border: 0;
      white-space: normal;
      overflow-wrap: anywhere;
    }
    td:first-child {
      grid-column: 1 / -1;
      font-weight: 600;
    }
    td[data-label]::before {
      display: block;
    }
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
  .methodology dl {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 6px 16px;
    margin: 0 0 12px;
  }
  .methodology dt {
    color: var(--ink);
  }
  .methodology dd {
    margin: 0;
  }
  @media (max-width: 700px) {
    .methodology dl {
      grid-template-columns: 1fr;
      gap: 2px;
    }
    .methodology dd {
      margin-bottom: 6px;
    }
  }
  .methodology summary {
    color: var(--ink);
    cursor: pointer;
    font: 600 12px var(--mono);
    letter-spacing: 0.04em;
    padding: 11px 0;
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
