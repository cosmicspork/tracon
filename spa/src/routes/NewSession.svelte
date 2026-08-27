<script lang="ts">
  import { api } from '../lib/api'
  import { eligibleNodes } from '../lib/nodes'
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

  let nodeId = $state<string | null>(null)
  const channelNames = $derived(store.channels.map((c) => c.name))
  const bindings = $derived(Object.fromEntries(store.channels.map((c) => [c.name, c.nodes])))
  // Bindings decide the set; the operator picks within it. The first ready,
  // reachable node is preselected; this node wins ties.
  const eligible = $derived(eligibleNodes(store.nodes, bindings, channel))
  const node = $derived(
    eligible.find((n) => n.id === nodeId) ?? eligible.find((n) => n.is_self) ?? eligible[0] ?? store.node,
  )
  const blocked = $derived(!node || node.state === 'refused' || node.harness.mismatch === true || !node.reachable)
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
        node_id: node && !node.is_self ? node.id : undefined,
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
      {#each channelNames as c (c)}
        <option value={c}>{c}</option>
      {/each}
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
    <span
      >Runs on {#if store.nodes.length > 1}<em class="opt"
          >{eligible.length} of {store.nodes.length} nodes can run {channel}</em
        >{/if}</span
    >
    {#if store.nodes.length > 1}
      <div class="pick">
        {#each store.nodes as n (n.id)}
          {@const ok = eligible.some((e) => e.id === n.id)}
          <label class:no={!ok}>
            <input type="radio" name="node" value={n.id} disabled={!ok} checked={node?.id === n.id} onchange={() => (nodeId = n.id)} />
            <span class="chip" class:self={n.is_self} class:bad={n.state === 'refused' || n.harness.mismatch} class:off={!n.reachable}>{n.name}</span>
          </label>
        {/each}
      </div>
      <small
        >{store.nodes
          .filter((n) => !eligible.some((e) => e.id === n.id))
          .map((n) =>
            n.state === 'refused'
              ? `${n.name} refused: ${n.failed_check}`
              : !n.reachable
                ? `${n.name} is unreachable`
                : n.harness.mismatch
                  ? `${n.name} has a harness version mismatch`
                  : `${n.name} is not bound to ${channel}`,
          )
          .join(' · ')}</small
      >
    {:else if node?.state === 'refused'}
      <span class="chip bad">{node.name}</span>
      <small class="crit">Refused: {node.failed_check}: {node.failed_detail}</small>
    {:else if node?.harness.mismatch}
      <span class="chip bad">{node.name}</span>
      <small class="crit"
        >Version mismatch: node expects {node.harness.pinned}, host has {node.harness.found}.</small
      >
    {:else if node && !node.reachable}
      <span class="chip off">{node.name}</span>
      <small class="crit">Unreachable. Start when it returns.</small>
    {:else if node}
      <span class="chip" class:self={node.is_self}>{node.name}</span>
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
  .pick {
    display: flex;
    flex-wrap: wrap;
    gap: 8px 16px;
    align-items: center;
  }
  .pick label {
    display: inline-flex;
    gap: 7px;
    align-items: center;
    font: 13px var(--sans);
    color: var(--ink);
    cursor: pointer;
  }
  .pick label.no {
    color: var(--dim);
    cursor: not-allowed;
  }
  .pick input {
    accent-color: var(--acc);
    margin: 0;
  }
</style>
