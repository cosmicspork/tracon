<script lang="ts">
  // Where the session runs: the repositories this node already knows, offered
  // as a pick, with the typed path kept as the escape hatch for a repo the
  // node has not seen yet.
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import type { RecentRepo } from '../lib/types'

  let { value = $bindable('') }: { value?: string } = $props()

  let recents = $state<RecentRepo[]>([])
  $effect(() => {
    api
      .recentRepos()
      .then((d) => (recents = d.repos))
      .catch(() => (recents = []))
  })

  function basename(p: string): string {
    const parts = p.split('/').filter(Boolean)
    return parts[parts.length - 1] ?? p
  }
</script>

{#if recents.length > 0}
  <div class="picker" role="radiogroup">
    {#each recents as r (r.repo_path)}
      <button type="button" class:on={value === r.repo_path} onclick={() => (value = r.repo_path)}>
        <i></i>
        <span>{basename(r.repo_path)} <small>{r.repo_path}</small></span>
        <small>{r.sessions} session{r.sessions === 1 ? '' : 's'} · {formatAge(r.last_used_ms, clock.now)}</small>
      </button>
    {/each}
  </div>
{/if}
<input
  bind:value
  placeholder={recents.length > 0 ? 'or type a path on the node' : '/Users/you/src/project'}
  spellcheck="false"
/>

<style>
  .picker {
    display: flex;
    flex-direction: column;
    gap: 3px;
    background: var(--s1);
    border-radius: 4px;
    padding: 4px;
    max-height: 220px;
    overflow: auto;
  }
  .picker button {
    display: grid;
    grid-template-columns: 16px minmax(0, 1fr) auto;
    gap: 10px;
    align-items: center;
    padding: 6px 8px;
    border-radius: 3px;
    font: 13px var(--sans);
    color: var(--ink);
    background: none;
    border: 0;
    text-align: left;
    cursor: pointer;
  }
  .picker button.on {
    background: var(--s3);
  }
  .picker button > span {
    min-width: 0;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .picker i {
    width: 12px;
    height: 12px;
    border-radius: 50%;
    border: 1.5px solid var(--dim);
    display: inline-block;
  }
  .picker button.on i {
    border-color: var(--acc);
    background: var(--acc);
    box-shadow: inset 0 0 0 2px var(--s3);
  }
  .picker small {
    font: 11px var(--mono);
    color: var(--dim);
  }
  input {
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13.5px var(--sans);
    width: 100%;
    box-sizing: border-box;
  }
</style>
