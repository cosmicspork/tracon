<script lang="ts">
  import { api } from '../lib/api'
  import { renderMessage } from '../lib/markdown'
  import type { OperatorQuestion } from '../lib/types'

  let { question, done }: { question: OperatorQuestion; done?: () => void } = $props()
  let answer = $state('')
  let busy = $state(false)
  let error = $state<string | null>(null)
  const choices = $derived.by(() => {
    try { return JSON.parse(question.choices_json) as string[] } catch { return [] }
  })
  async function submit(value = answer) {
    if (!value.trim() || busy) return
    busy = true; error = null
    try { await api.answerOperatorQuestion(question.id, value); done?.() }
    catch (e) { error = e instanceof Error ? e.message : String(e) }
    finally { busy = false }
  }
  async function cancel() {
    if (busy) return
    busy = true; error = null
    try { await api.cancelOperatorQuestion(question.id); done?.() }
    catch (e) { error = e instanceof Error ? e.message : String(e) }
    finally { busy = false }
  }
</script>

<article class="card">
  <div class="label"><em>Operator question</em>{#if question.session_id}{' · '}<a href="/sessions/{question.session_id}">session</a>{/if}</div>
  <!-- An agent's words, rendered as its messages in the log are: Markdown,
       with nothing fetched and embedded HTML escaped. -->
  <div class="prompt md">{@html renderMessage(question.prompt)}</div>
  {#if choices.length}
    <div class="choices">{#each choices as choice}<button class="btn" onclick={() => submit(choice)} disabled={busy}>{choice}</button>{/each}</div>
  {/if}
  {#if !choices.length}
    <form onsubmit={(e) => { e.preventDefault(); void submit() }}>
      <input bind:value={answer} placeholder="Free-text answer" disabled={busy} />
      <button class="btn p" type="submit" disabled={busy || !answer.trim()}>Answer</button>
      <button type="button" class="lnk d" onclick={cancel} disabled={busy}>Cancel question</button>
    </form>
  {:else}
    <button type="button" class="lnk d" onclick={cancel} disabled={busy}>Cancel question</button>
  {/if}
  {#if error}<div class="error">{error}</div>{/if}
</article>

<style>
  /* Waiting on you, like the rows beside it: the amber bar and wash. */
  .card {
    background: linear-gradient(90deg, var(--wash-wait), var(--s1) 42%);
    border-left: 3px solid var(--wait);
    border-radius: 4px;
    padding: 10px 14px;
  }
  .label { font: 12px var(--mono); color: var(--dim); }
  .label em { font: 500 14px var(--sans); font-style: normal; color: var(--wait); }
  .prompt { margin: .4rem 0 .7rem; overflow-wrap: anywhere; }
  .md > :global(:first-child) { margin-top: 0; }
  .md > :global(:last-child) { margin-bottom: 0; }
  .md :global(p), .md :global(ul), .md :global(ol), .md :global(pre), .md :global(blockquote) { margin: 4px 0; }
  .md :global(ul), .md :global(ol) { padding-left: 2.5ch; }
  .md :global(code) { background: var(--s2); border-radius: 3px; padding: 0 3px; font: 12.5px var(--mono); }
  .md :global(pre) { background: var(--s2); border-radius: 4px; padding: 8px 10px; overflow-x: auto; }
  .md :global(pre code) { background: none; padding: 0; }
  .md :global(blockquote) { border-left: 2px solid var(--rule); padding-left: 1.5ch; color: var(--ink2); }
  form, .choices { display: flex; gap: 8px 12px; flex-wrap: wrap; align-items: center; }
  .choices { margin-bottom: 6px; }
  input {
    flex: 1 1 12rem;
    min-width: 0;
    background: var(--s2);
    color: var(--ink);
    border: 1px solid transparent;
    border-radius: 4px;
    padding: 6px 9px;
  }
  .error { color: var(--crit); margin-top: .4rem; font-size: 12.5px; }
</style>
