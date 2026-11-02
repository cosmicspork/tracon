<script lang="ts">
  // Every call the session made to the node's tools, and who let it through:
  // the policy, a grant, the node's own rules, or the operator (and from
  // where). The node records each decision as it makes it; this panel reads
  // that record back and words it, so it can only say what the gate did.
  import { api } from '../lib/api'
  import { auditSummary, decidedBy, decisionLabel, outcomeNote } from '../lib/audit'
  import type { AuditCall } from '../lib/types'

  let { id, open = false }: { id: string; open?: boolean } = $props()

  let toggled = $state<boolean | null>(null)
  const expanded = $derived(toggled ?? open)
  let calls = $state<AuditCall[] | null>(null)
  let error = $state<string | null>(null)
  let loading = $state(false)

  async function load() {
    loading = true
    error = null
    try {
      calls = (await api.sessionAudit(id)).calls
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      loading = false
    }
  }
  // Read when opened, not with every render of the session: it is a question
  // the operator asks, and the log already shows each call as it happens.
  $effect(() => {
    if (expanded && calls === null && !loading) void load()
  })

  const time = (ms: number) => new Date(ms).toLocaleTimeString()
  const shown = (v: unknown) => JSON.stringify(v, null, 2)
  const newestFirst = $derived([...(calls ?? [])].reverse())
</script>

<details id="audit" class="audit" open={expanded} ontoggle={(e) => (toggled = e.currentTarget.open)}>
  <summary>Calls to the node</summary>
  {#if error}
    <p class="err">{error}</p>
  {:else if calls === null}
    <p class="dim">Reading the session's calls…</p>
  {:else}
    <p class="dim head">
      {auditSummary(calls)} ·
      <button class="lnk" onclick={load} disabled={loading}>{loading ? 'reading…' : 'refresh'}</button>
    </p>
    <ol>
      {#each newestFirst as call, i (`${call.at_ms}-${i}`)}
        {@const [label, tone] = decisionLabel(call)}
        {@const note = outcomeNote(call)}
        <li>
          <details>
            <summary class="row">
              <span class="mono at" title={new Date(call.at_ms).toLocaleString()}>{time(call.at_ms)}</span>
              <code>{call.tool}</code>
              <span class="tone {tone}">{label}</span>
              <span class="who">{decidedBy(call)}{call.decided_ms ? ` · ${time(call.decided_ms)}` : ''}</span>
            </summary>
            {#if note}<p class="note">{note}</p>{/if}
            <pre>{shown(call.arguments)}</pre>
            {#if call.edited_arguments !== undefined}
              <p class="dim">ran as the operator edited it:</p>
              <pre>{shown(call.edited_arguments)}</pre>
            {/if}
            {#if call.approval_id}
              <p class="dim">
                approval <a href="/approvals/{call.approval_id}">{call.approval_id.slice(0, 8)}</a>{call.approval_state
                  ? ` · ${call.approval_state}`
                  : ''}
              </p>
            {/if}
          </details>
          {#if note}<small class="note-line">{note}</small>{/if}
        </li>
      {:else}
        <li class="dim">Nothing yet.</li>
      {/each}
    </ol>
    <p class="dim">Arguments are shown with credentials and secret-looking lines removed, and long text cut short.</p>
  {/if}
</details>

<style>
  .audit {
    background: var(--s1);
    border-radius: 4px;
    padding: 10px 14px;
    margin-top: 10px;
  }
  .audit > summary {
    cursor: pointer;
    font: 12px var(--mono);
    color: var(--dim);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
  .head {
    margin: 10px 0 6px;
  }
  ol {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  li {
    min-width: 0;
  }
  .row {
    cursor: pointer;
    display: grid;
    grid-template-columns: max-content max-content max-content minmax(0, 1fr);
    gap: 10px;
    align-items: baseline;
    font-size: 12.5px;
  }
  .at {
    color: var(--dim);
    font-size: 11.5px;
  }
  .row code {
    font-size: 12px;
  }
  .tone {
    font: 11.5px var(--mono);
  }
  .tone.ok {
    color: var(--ok);
  }
  .tone.crit {
    color: var(--crit);
  }
  .tone.wait {
    color: var(--wait);
  }
  .tone.mark {
    color: var(--dim);
  }
  .who {
    color: var(--dim);
    font-size: 11.5px;
    overflow-wrap: anywhere;
  }
  li:has(> details[open]) > .note-line {
    display: none;
  }
  .note-line,
  .note {
    display: block;
    color: var(--dim);
    font-size: 11.5px;
    overflow-wrap: anywhere;
    margin: 0 0 2px;
  }
  pre {
    margin: 4px 0 6px;
    padding: 6px 8px;
    background: var(--s2);
    border-radius: 3px;
    font: 11.5px var(--mono);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    max-height: 320px;
    overflow: auto;
  }
  .err {
    color: var(--crit);
    font-size: 12.5px;
  }
  .dim {
    color: var(--dim);
    font-size: 12px;
  }
  .lnk {
    background: none;
    border: 0;
    padding: 0;
    color: var(--acc);
    font: inherit;
    cursor: pointer;
  }
  @media (max-width: 700px) {
    .row {
      grid-template-columns: max-content minmax(0, 1fr);
    }
  }
</style>
