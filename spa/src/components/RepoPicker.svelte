<script lang="ts">
  // Where the session runs: repositories this node already knows (recent, and
  // managed clones), a browse-and-clone over the channel's forges, and the
  // typed path kept as the escape hatch for a repo the node has not seen.
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { repoLabel } from '../lib/repo'
  import type { ForgeList, ManagedRepo, RecentRepo } from '../lib/types'

  let { value = $bindable(''), channel = '' }: { value?: string; channel?: string } = $props()

  let recents = $state<RecentRepo[]>([])
  let managed = $state<ManagedRepo[]>([])
  let version = $state(0)
  $effect(() => {
    void version
    api
      .recentRepos()
      .then((d) => {
        recents = d.repos
        // A managed clone that has run a session is already in the recents.
        managed = d.managed.filter((m) => !d.repos.some((r) => r.repo_path === m.repo_path))
      })
      .catch(() => {
        recents = []
        managed = []
      })
  })

  let browsing = $state(false)
  let forges = $state<ForgeList[] | null>(null)
  let cloning = $state<string | null>(null)
  let error = $state<string | null>(null)
  $effect(() => {
    // A channel change re-scopes what the forges answer.
    void channel
    browsing = false
    forges = null
  })

  async function browse() {
    browsing = true
    error = null
    try {
      forges = (await api.forgeRepos(channel)).forges
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
      browsing = false
    }
  }

  async function clone(forge: string, r: { host: string; owner: string; name: string; full_name: string }) {
    if (cloning) return
    cloning = r.full_name
    error = null
    try {
      const d = await api.cloneRepo({ channel, forge, host: r.host, owner: r.owner, name: r.name })
      value = d.repo_path
      browsing = false
      version += 1
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      cloning = null
    }
  }

</script>

{#if recents.length > 0 || managed.length > 0}
  <div class="picker" role="radiogroup">
    {#each recents as r (r.repo_path)}
      <button type="button" class:on={value === r.repo_path} onclick={() => (value = r.repo_path)}>
        <i></i>
        <span>{repoLabel(r.repo_path)} <small>{r.repo_path}</small></span>
        <small>{r.sessions} session{r.sessions === 1 ? '' : 's'} · {formatAge(r.last_used_ms, clock.now)}</small>
      </button>
    {/each}
    {#each managed as m (m.repo_path)}
      <button type="button" class:on={value === m.repo_path} onclick={() => (value = m.repo_path)}>
        <i></i>
        <span>{repoLabel(m.repo_path, m.full_name)} <small>{m.repo_path}</small></span>
        <small>cloned · {m.host}</small>
      </button>
    {/each}
  </div>
{/if}

{#if !browsing}
  <span><button type="button" class="lnk" onclick={browse}>Clone from a forge…</button></span>
{:else if forges === null}
  <small>Asking the forges…</small>
{:else if forges.length === 0}
  <small>
    No forge credential is bound to {channel || 'this channel'}: import a gh or glab
    credential and bind it, or type a path below.
  </small>
{:else}
  <div class="picker forge">
    {#each forges as f (f.forge)}
      {#if f.error}
        <small class="crit">{f.forge}: {f.error}</small>
      {:else}
        {#each f.repos as r (f.forge + r.full_name)}
          <button type="button" disabled={cloning !== null} onclick={() => clone(f.forge, r)}>
            <i></i>
            <span>{r.full_name} <small>{r.host}{r.private ? ' · private' : ''}</small></span>
            <small>{cloning === r.full_name ? 'cloning…' : f.forge}</small>
          </button>
        {/each}
      {/if}
    {/each}
  </div>
{/if}
{#if error}
  <small class="crit">{error}</small>
{/if}

<input
  bind:value
  placeholder={recents.length > 0 || managed.length > 0 ? 'or type a path on the node' : '/Users/you/src/project'}
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
  .picker button:disabled {
    color: var(--dim);
    cursor: default;
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
  .forge i {
    border-radius: 3px;
  }
  .picker small,
  small {
    font: 11px var(--mono);
    color: var(--dim);
  }
  small.crit {
    color: var(--crit);
  }
  .lnk {
    background: none;
    border: 0;
    padding: 0;
    font: 12.5px var(--sans);
    color: var(--acc);
    cursor: pointer;
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
