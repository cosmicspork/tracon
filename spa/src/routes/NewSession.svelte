<script lang="ts">
  import { api } from '../lib/api'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'

  let channel = $state('personal')
  let repo = $state('')
  let branch = $state('')
  let workItem = $state('')
  // No default, deliberately: a session without an explicit model is a
  // validation failure, and the form makes that unreachable instead.
  let model = $state('')
  let budget = $state('2000000')
  let busy = $state(false)
  let error = $state<string | null>(null)

  const node = $derived(store.node)
  const blocked = $derived(node?.state === 'refused' || node?.harness.mismatch === true)
  const ready = $derived(!blocked && repo.trim() !== '' && model !== '' && !busy)

  async function start(e: SubmitEvent) {
    e.preventDefault()
    busy = true
    error = null
    try {
      const s = await api.createSession({
        channel,
        repo_path: repo.trim(),
        branch: branch.trim() || undefined,
        work_item_id: workItem.trim() || undefined,
        model,
        budget_tokens: Number(budget) || undefined,
      })
      await store.refetch()
      router.go(`/sessions/${s.id}`)
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
    }
  }
</script>

<div class="h4">New session</div>

<form class="form" onsubmit={start}>
  <label>
    <span>Channel <em class="req">required</em></span>
    <select bind:value={channel}>
      <option value="personal">personal</option>
      <option value="work">work</option>
    </select>
  </label>
  <label>
    <span>Repository <em class="req">required</em></span>
    <input bind:value={repo} placeholder="/Users/you/src/project" spellcheck="false" />
  </label>
  <label>
    <span>Branch</span>
    <input bind:value={branch} placeholder="feat/…  (a name is generated if empty)" spellcheck="false" />
    <small>The worktree is created from origin's default branch, outside the repo.</small>
  </label>
  <label>
    <span>Work item <em class="opt">optional in Phase 1</em></span>
    <input bind:value={workItem} placeholder="NUDEV-25" spellcheck="false" />
  </label>
  <label>
    <span>Model <em class="req">required · no default</em></span>
    <select bind:value={model}>
      <option value="" disabled selected>Choose a model</option>
      {#each node?.models ?? [] as m (m.value)}
        <option value={m.value}>{m.name}</option>
      {/each}
    </select>
    {#if node && node.models.length === 0}
      <small class="crit">The node offered no models; check the harness credentials.</small>
    {/if}
  </label>
  <label>
    <span>Budget <em class="opt">tokens</em></span>
    <input bind:value={budget} inputmode="numeric" pattern="[0-9]*" />
    <small>The session is killed at this number, checked at each turn's end.</small>
  </label>
  <div class="runs">
    <span>Runs on</span>
    {#if node?.state === 'refused'}
      <span class="chip bad">{node.name}</span>
      <small class="crit">Refused: {node.failed_check}: {node.failed_detail}</small>
    {:else if node?.harness.mismatch}
      <span class="chip bad">{node.name}</span>
      <small class="crit"
        >Version mismatch: node expects {node.harness.pinned}, host has {node.harness.found}.</small
      >
    {:else if node}
      <span class="chip">{node.name}</span>
      <small>{node.harness.id} {node.harness.found ?? node.harness.pinned} · boundary check passed</small>
    {/if}
  </div>
  {#if error}
    <div class="banner crit">could not start <b>· {error}</b></div>
  {/if}
  <div>
    <button class="btn p" type="submit" disabled={!ready}>Start session</button>
  </div>
</form>

<style>
  .form {
    display: grid;
    gap: 14px;
    max-width: 520px;
  }
  label {
    display: grid;
    gap: 5px;
  }
  label > span,
  .runs > span {
    font: 500 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink2);
    display: flex;
    gap: 10px;
    align-items: baseline;
  }
  em.req {
    color: var(--crit);
    font: 11px var(--mono);
    font-style: normal;
    letter-spacing: 0;
    text-transform: none;
  }
  em.opt {
    color: var(--dim);
    font: 11px var(--mono);
    font-style: normal;
    letter-spacing: 0;
    text-transform: none;
  }
  select,
  input {
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13.5px var(--sans);
  }
  small {
    font-size: 12.5px;
    color: var(--dim);
  }
  small.crit {
    color: var(--crit);
  }
  .runs {
    display: grid;
    gap: 5px;
  }
</style>
