<script lang="ts">
  import { api } from '../lib/api'
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
  <div class="label">Operator question <a href="/sessions/{question.session_id}">session</a></div>
  <p>{question.prompt}</p>
  {#if choices.length}
    <div class="choices">{#each choices as choice}<button onclick={() => submit(choice)} disabled={busy}>{choice}</button>{/each}</div>
  {/if}
  <form onsubmit={(e) => { e.preventDefault(); void submit() }}>
    <input bind:value={answer} placeholder="Free-text answer" disabled={busy} />
    <button type="submit" disabled={busy || !answer.trim()}>Answer</button>
    <button type="button" class="quiet" onclick={cancel} disabled={busy}>Cancel question</button>
  </form>
  {#if error}<div class="error">{error}</div>{/if}
</article>

<style>
  .card { border: 1px solid var(--line); padding: .8rem; border-radius: .4rem; }
  .label { color: var(--dim); font-size: .85rem; } p { margin: .4rem 0 .7rem; white-space: pre-wrap; }
  form,.choices { display:flex; gap:.45rem; flex-wrap:wrap; } input { flex:1; min-width:12rem; }
  .quiet { background:transparent; } .error { color:var(--red); margin-top:.4rem; }
</style>
