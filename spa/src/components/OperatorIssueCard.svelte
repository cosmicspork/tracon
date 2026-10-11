<script lang="ts">
  import { api } from '../lib/api'
  import type { OperatorIssue } from '../lib/types'
  let { issue, done }: { issue: OperatorIssue; done?: () => void } = $props()
  let busy = $state(false)
  let error = $state<string | null>(null)
  const attachments = $derived.by(() => { try { return JSON.parse(issue.attachments_json) as { name: string; content: string }[] } catch { return [] } })
  const outgoingBody = $derived(`${issue.body}\n\n## Inspectable attachments\n\n\`\`\`json\n${JSON.stringify(attachments, null, 2)}\n\`\`\`\n\n<!-- tracon-issue-draft:${issue.id} -->`)
  async function publish() {
    if (busy || issue.state !== 'draft') return
    busy = true; error = null
    try { await api.publishOperatorIssue(issue.id); done?.() }
    catch (e) { error = e instanceof Error ? e.message : String(e) }
    finally { busy = false }
  }
  let discarding = $state(false)
  let discardReason = $state('')
  async function discard() {
    if (busy || (issue.state !== 'draft' && issue.state !== 'uncertain')) return
    busy = true; error = null
    try { await api.discardOperatorIssue(issue.id, discardReason); discarding = false; done?.() }
    catch (e) { error = e instanceof Error ? e.message : String(e) }
    finally { busy = false }
  }
  async function reconcile() {
    if (busy || issue.state !== 'uncertain') return
    busy = true; error = null
    try { await api.reconcileOperatorIssue(issue.id); done?.() }
    catch (e) { error = e instanceof Error ? e.message : String(e) }
    finally { busy = false }
  }
</script>
<article class="card">
  <div class="label"><em>Issue draft</em> · {issue.state}{#if issue.session_id}{' · '}<a href="/sessions/{issue.session_id}">session</a>{/if}</div>
  <h3>{issue.title}</h3>
  <details><summary>Exact outgoing issue body</summary><pre>{outgoingBody}</pre></details>
  <pre>{issue.body}</pre>
  {#if attachments.length}<details><summary>{attachments.length} inspectable attachment{attachments.length === 1 ? '' : 's'}</summary>{#each attachments as attachment}<h4>{attachment.name}</h4><pre>{attachment.content}</pre>{/each}</details>{/if}
  {#if issue.published_url}<a href={issue.published_url} target="_blank" rel="noreferrer">Published issue{#if issue.published_number} #{issue.published_number}{/if}</a>{:else if issue.state === 'draft'}<button class="btn p" onclick={publish} disabled={busy}>Authorize publication to cosmicspork/tracon</button>{:else if issue.state === 'uncertain'}<button class="btn" onclick={reconcile} disabled={busy}>I confirmed the draft marker is absent; retry</button>{/if}
  {#if issue.state === 'discarded'}<div class="label">Discarded{#if issue.discard_reason}: {issue.discard_reason}{/if}</div>{:else if issue.state === 'draft' || issue.state === 'uncertain'}
    {#if discarding}<form class="discard" onsubmit={(e) => { e.preventDefault(); discard() }}><input bind:value={discardReason} placeholder="Reason for the agent (optional)" aria-label="Discard reason" maxlength="2000" /><button class="btn d" type="submit" disabled={busy}>Discard draft</button><button class="lnk" type="button" onclick={() => (discarding = false)} disabled={busy}>Keep</button></form>
    {:else}<button class="lnk d secondary" onclick={() => (discarding = true)} disabled={busy}>Discard…</button>{/if}
  {/if}
  {#if issue.publish_error}<div class="error">Last publication failed: {issue.publish_error}</div>{/if}
  {#if error}<div class="error">{error}</div>{/if}
</article>
<style>
  /* Waiting on you, like the rows beside it: the amber bar and wash. */
  .card {
    background: linear-gradient(90deg, var(--wash-wait), var(--s1) 42%);
    border-left: 3px solid var(--wait);
    border-radius: 4px;
    padding: 10px 14px;
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 4px;
  }
  .card > * { max-width: 100%; }
  .label { font: 12px var(--mono); color: var(--dim); }
  .label em { font: 500 14px var(--sans); font-style: normal; color: var(--wait); }
  h3 { margin: .2rem 0; font: 600 15px var(--sans); overflow-wrap: anywhere; }
  details { align-self: stretch; }
  summary { cursor: pointer; font: 12px var(--mono); color: var(--ink2); }
  h4 { margin: .4rem 0 .2rem; font: 12px var(--mono); color: var(--ink2); }
  pre { align-self: stretch; margin: 0; white-space: pre-wrap; overflow-wrap: anywhere; background: var(--s2); color: var(--ink); padding: 8px; border-radius: 3px; font: 12.5px/1.45 var(--mono); }
  .error { color: var(--crit); margin-top: .4rem; font-size: 12.5px; }
  .discard { align-self: stretch; display: flex; flex-wrap: wrap; align-items: center; gap: 8px 12px; margin-top: .4rem; }
  .discard input { flex: 1 1 12rem; min-width: 0; background: var(--s2); color: var(--ink); border: 1px solid transparent; border-radius: 4px; padding: 6px 9px; }
  .secondary { margin-top: .2rem; }
</style>
