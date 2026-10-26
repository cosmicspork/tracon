<script lang="ts">
  // One check run, opened from its line on the review: what ran, in which
  // image, with which inputs, and everything it printed.
  import { failuresOf, inputRows, outcomeLabel, runDuration, runTone } from '../lib/checkRuns'
  import type { CandidateCheckRun } from '../lib/types'

  let { run, onclose }: { run: CandidateCheckRun | null; onclose: () => void } = $props()

  let dialog = $state<HTMLDialogElement | null>(null)

  $effect(() => {
    if (!dialog) return
    if (run && !dialog.open) dialog.showModal()
    else if (!run && dialog.open) dialog.close()
  })

  const rows = $derived(run ? inputRows(run) : null)
  const failures = $derived(run ? failuresOf(run) : '')
</script>

<dialog
  bind:this={dialog}
  class="check"
  aria-labelledby="check-detail-title"
  onclose={onclose}
  onclick={(e) => {
    // A click on the backdrop lands on the dialog itself.
    if (e.target === dialog) dialog?.close()
  }}
>
  {#if run}
    <div class="in">
      <header>
        <span class="dot {runTone(run)}" aria-hidden="true"></span>
        <h2 id="check-detail-title"><code>{run.command ?? 'command not recorded'}</code></h2>
        <button class="lnk" type="button" onclick={() => dialog?.close()}>Close</button>
      </header>
      <p class="said">
        {outcomeLabel(run)} · {runDuration(run)}{run.exit_code != null ? ` · exit ${run.exit_code}` : ''}{run.reused_from_id
          ? ` · reused from ${run.reused_from_id.slice(0, 8)}`
          : ''}
      </p>
      {#if failures}
        <pre class="failures" aria-label="what failed">{failures}</pre>
      {/if}
      <h3>Log</h3>
      <pre class="log">{run.log || '(no retained output)'}</pre>
      <h3>Execution image</h3>
      <p><code>{run.execution_image ?? 'image identity not recorded'}</code></p>
      <h3>Inputs</h3>
      {#if rows}
        <table>
          <tbody>
            {#each rows as row (row.label)}
              <tr>
                <th scope="row">{row.label}</th>
                <td>
                  {#if row.values.length === 0}
                    <span class="none">none</span>
                  {:else}
                    <div class="vals">{#each row.values as v, i (i)}<code>{v}</code>{/each}</div>
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      {:else}
        <p class="none">Input identity was not recorded.</p>
      {/if}
    </div>
  {/if}
</dialog>

<style>
  dialog.check {
    width: min(860px, calc(100vw - 32px));
    max-height: calc(100dvh - 48px);
    padding: 0;
    border: 1px solid var(--rule);
    border-radius: 6px;
    background: var(--bg);
    color: var(--ink);
  }
  dialog.check::backdrop {
    background: rgb(0 0 0 / 0.5);
  }
  .in {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 14px 16px 16px;
    font-size: 12.5px;
  }
  header {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  h2 {
    flex: 1;
    min-width: 0;
    margin: 0;
    font-size: 13.5px;
    font-weight: 500;
    overflow-wrap: anywhere;
  }
  h3 {
    margin: 10px 0 0;
    font: 11px var(--mono);
    color: var(--dim);
    letter-spacing: 0.08em;
    text-transform: uppercase;
  }
  p {
    margin: 0;
  }
  .said {
    font: 12px var(--mono);
    color: var(--dim);
  }
  code {
    font: 12px var(--mono);
    color: var(--ink2);
    overflow-wrap: anywhere;
  }
  pre {
    margin: 0;
    font: 11px/1.45 var(--mono);
  }
  pre.log {
    max-height: 340px;
    overflow: auto;
    white-space: pre;
    background: var(--s1);
    border-radius: 4px;
    padding: 8px 10px;
  }
  /* Wrapped: a failure line cut off at the edge says nothing. */
  pre.failures {
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    padding-left: 8px;
    border-left: 2px solid var(--crit);
    color: var(--ink2);
  }
  table {
    border-collapse: collapse;
    width: 100%;
  }
  th,
  td {
    text-align: left;
    vertical-align: top;
    padding: 5px 0;
    border-top: 1px solid var(--rule);
  }
  tr:first-child th,
  tr:first-child td {
    border-top: 0;
  }
  th {
    width: 150px;
    padding-right: 12px;
    font: 11.5px var(--mono);
    font-weight: 400;
    color: var(--dim);
  }
  .vals {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .none {
    color: var(--dim);
  }
  .dot {
    flex: none;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--dim);
  }
  .dot.ok {
    background: var(--ok);
  }
  .dot.bad {
    background: var(--crit);
  }
  .dot.running {
    background: var(--wait);
  }
  @media (max-width: 700px) {
    th {
      width: 110px;
    }
  }
</style>
