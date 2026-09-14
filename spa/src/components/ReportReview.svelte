<script lang="ts">
  import { decideReport, type NarrativeReport } from '../lib/reports'
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'

  let { report }: { report: NarrativeReport } = $props()

  let reason = $state('')
  let busy = $state(false)
  let error = $state<string | null>(null)
  const waiting = $derived(report.state === 'new' || report.state === 'claimed')

  async function decide(verdict: 'acknowledge' | 'request_changes') {
    if (busy || !waiting) return
    busy = true
    error = null
    try {
      await decideReport(report.id, {
        verdict,
        reason: verdict === 'request_changes' ? reason : reason.trim() || undefined,
        head_sha: report.head_sha,
      })
      await store.refetch()
      router.go('/')
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
    }
  }
</script>

<header class="head">
  <div>
    <span class="overline">Narrative report</span>
    <h1>{report.title}</h1>
    <p>Operator feedback, without code changes or publication.</p>
  </div>
  <span class="state">{report.state === 'acknowledged' ? 'Acknowledged' : report.state === 'revising' ? 'Changes requested' : waiting ? 'Awaiting operator' : 'Receipt unavailable'}</span>
</header>

<dl class="meta">
  <div><dt>Channel</dt><dd>{report.channel}</dd></div>
  <div><dt>Source</dt><dd><a href="/sessions/{report.session_id}">Session record</a></dd></div>
  <div><dt>Submitted</dt><dd>{formatAge(report.created_ms, clock.now)} ago</dd></div>
  <div><dt>Version</dt><dd>{report.head_sha.slice(0, 12)}</dd></div>
</dl>

<article class="body">{report.body}</article>

{#if report.state === 'acknowledged'}
  <div class="banner ok">acknowledged <b>· this records receipt only; no code was published</b>{#if report.verdict_reason} · {report.verdict_reason}{/if}</div>
{:else if report.state === 'revising'}
  <div class="banner dim">changes requested <b>· the submitter must revise this narrative and call submit_report with this report id</b>{#if report.verdict_reason} · {report.verdict_reason}{/if}</div>
{:else if waiting}
  <section class="decision">
    <h2>Operator decision</h2>
    <p>Acknowledge receipt, or ask the submitter to revise.</p>
    <label for="report-note">Note (required for changes)</label>
    <input id="report-note" bind:value={reason} disabled={busy} placeholder="What should the submitter know?" />
    {#if error}<div class="banner crit">refused <b>· {error}</b></div>{/if}
    <div class="actions">
      <button class="btn p" disabled={busy} onclick={() => decide('acknowledge')}>Acknowledge report</button>
      <button class="btn" disabled={busy || !reason.trim()} onclick={() => decide('request_changes')}>Request changes</button>
    </div>
  </section>
{:else}
  <div class="banner dim">Receipt status unavailable <b>· open this report on its owning node, or update both nodes to synchronize acknowledgements.</b></div>
{/if}

<style>
  .head { display: flex; justify-content: space-between; gap: 16px; align-items: start; margin-bottom: 20px; }
  .overline, .state { font: 12px var(--mono); color: var(--ink2); text-transform: uppercase; letter-spacing: .08em; }
  h1 { margin: 4px 0; font: 600 28px var(--sans); }
  p { margin: 0; color: var(--ink2); }
  .state { border: 1px solid var(--rule); padding: 5px 8px; white-space: nowrap; }
  .meta { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 8px 16px; margin: 0 0 20px; font: 12px var(--mono); }
  .meta div { min-width: 0; }
  dt { color: var(--dim); } dd { margin: 2px 0 0; overflow-wrap: anywhere; }
  .body { white-space: pre-wrap; overflow-wrap: anywhere; border-block: 1px solid var(--rule); padding: 18px 0; line-height: 1.55; }
  .decision { margin-top: 24px; }
  label { display: block; margin-top: 12px; font: 13px var(--sans); color: var(--ink2); }
  h2 { margin: 0 0 4px; font: 600 17px var(--sans); }
  input { box-sizing: border-box; width: 100%; margin: 12px 0 8px; padding: 9px 10px; color: var(--ink); background: var(--bg); border: 1px solid var(--rule); font: 13px var(--sans); }
  .actions { display: flex; flex-wrap: wrap; gap: 8px; }
  .banner { margin-top: 20px; }
  @media (max-width: 640px) { .head { display: block; } .state { display: inline-block; margin-top: 8px; } .meta { grid-template-columns: repeat(2, minmax(0, 1fr)); } }
</style>
