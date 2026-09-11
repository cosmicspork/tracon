<script lang="ts">
  import { api } from '../lib/api'
  import type { OperatorIssue } from '../lib/types'
  let { issue, done }: { issue: OperatorIssue; done?: () => void } = $props()
  let busy = $state(false)
  let error = $state<string | null>(null)
  const attachments = $derived.by(() => { try { return JSON.parse(issue.attachments_json) as { name: string; content: string }[] } catch { return [] } })
  async function publish() {
    if (busy || issue.state !== 'draft') return
    busy = true; error = null
    try { await api.publishOperatorIssue(issue.id); done?.() }
    catch (e) { error = e instanceof Error ? e.message : String(e) }
    finally { busy = false }
  }
</script>
<article class="card">
  <div class="label">Issue draft · {issue.state} <a href="/sessions/{issue.session_id}">session</a></div>
  <h3>{issue.title}</h3>
  <pre>{issue.body}</pre>
  {#if attachments.length}<details><summary>{attachments.length} inspectable attachment{attachments.length === 1 ? '' : 's'}</summary>{#each attachments as attachment}<h4>{attachment.name}</h4><pre>{attachment.content}</pre>{/each}</details>{/if}
  {#if issue.published_url}<a href={issue.published_url} target="_blank" rel="noreferrer">Published issue</a>{:else if issue.state === 'draft'}<button onclick={publish} disabled={busy}>Authorize publication to cosmicspork/tracon</button>{/if}
  {#if issue.publish_error}<div class="error">Last publication failed: {issue.publish_error}</div>{/if}
  {#if error}<div class="error">{error}</div>{/if}
</article>
<style>
  .card { border:1px solid var(--line); padding:.8rem; border-radius:.4rem; } .label { color:var(--dim); font-size:.85rem; }
  h3 { margin:.35rem 0; } pre { white-space:pre-wrap; overflow-wrap:anywhere; background:var(--surface); padding:.55rem; }
  .error { color:var(--red); margin-top:.4rem; }
</style>
