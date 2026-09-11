<script lang="ts">
  // Where the session runs. The forge is asked first and its answer is the
  // list: picking one there clones it into a checkout the node owns. What
  // this node has worked in before sits under it as a shortcut, and a typed
  // path stays as the escape hatch for a repo no forge knows.
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { isManagedPath, repoLabel, repoMatches } from '../lib/repo'
  import type { ForgeList, ManagedRepo, RecentRepo } from '../lib/types'

  let { value = $bindable(''), channel = '' }: { value?: string; channel?: string } = $props()

  let recents = $state<RecentRepo[]>([])
  let managed = $state<ManagedRepo[]>([])
  let allManaged = $state<ManagedRepo[]>([])
  let version = $state(0)
  $effect(() => {
    void version
    api
      .recentRepos()
      .then((d) => {
        recents = d.repos
        allManaged = d.managed
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
  let loadingMore = $state<string | null>(null)
  let error = $state<string | null>(null)
  let showRecents = $state(false)
  let query = $state('')
  // Search is always available while more provider pages remain: a result may
  // be on the next bounded page even when the loaded rows do not match.
  const forgeCount = $derived((forges ?? []).reduce((n, f) => n + f.repos.length, 0))
  const searchable = $derived(forgeCount > 6 || (forges ?? []).some((f) => !f.complete))
  const shownForges = $derived(
    (forges ?? []).map((f) => ({ ...f, repos: f.repos.filter((r) => repoMatches(query, r.full_name, r.host)) })),
  )
  const shownRecents = $derived(recents.filter((r) => repoMatches(query, repoLabel(r.repo_path), r.repo_path)))
  const shownManaged = $derived(managed.filter((m) => repoMatches(query, m.full_name, m.host)))
  $effect(() => {
    // A channel change re-scopes what the forges answer, so ask again.
    void channel
    forges = null
    if (channel) void browse()
  })

  async function browse() {
    browsing = true
    error = null
    try {
      forges = (await api.forgeRepos(channel)).forges
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
      forges = []
    } finally {
      browsing = false
    }
  }

  async function loadMore(forge: ForgeList) {
    const cursor = forge.next_cursor
    if (!cursor || loadingMore) return
    loadingMore = forge.forge
    try {
      const page = (await api.forgeRepos(channel, forge.forge, cursor)).forges.find(
        (entry) => entry.forge === forge.forge,
      )
      if (!page) throw new Error(`${forge.forge} did not return the requested repository page`)
      forges = (forges ?? []).map((existing) => {
        if (existing.forge !== forge.forge) return existing
        const known = new Set(existing.repos.map((repo) => repo.full_name))
        return {
          ...page,
          repos: [...existing.repos, ...page.repos.filter((repo) => !known.has(repo.full_name))],
        }
      })
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e)
      forges = (forges ?? []).map((existing) =>
        existing.forge === forge.forge ? { ...existing, complete: false, error: message } : existing,
      )
    } finally {
      loadingMore = null
    }
  }
  const known = $derived([...recents, ...managed])
  const chosenManaged = $derived(allManaged.find((m) => m.repo_path === value) ?? null)

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

{#if browsing && forges === null}
  <small>Asking the forges…</small>
{:else if forges !== null && forges.length === 0}
  <small>
    No forge credential is bound to {channel || 'this channel'}.
    <a href="/settings#credentials">Add a GitHub or GitLab token in Settings.</a>
    You can also pick a checkout this node already has below.
  </small>
{:else if forges !== null}
  {#if searchable}
    <input class="search" bind:value={query} placeholder="Search repositories" spellcheck="false" />
  {/if}
  <div class="picker forge">
    {#each shownForges as f (f.forge)}
      {#if f.error}
        <small class="crit">{f.forge}: {f.error}</small>
      {/if}
      {#each f.repos as r (f.forge + r.full_name)}
        <button type="button" disabled={cloning !== null || loadingMore !== null} onclick={() => clone(f.forge, r)}>
          <i></i>
          <span>{r.full_name} <small>{r.host}{r.private ? ' · private' : ''}</small></span>
          <small>{cloning === r.full_name ? 'cloning…' : f.forge}</small>
        </button>
      {/each}
      {#if f.next_cursor}
        <button
          type="button"
          class="more"
          disabled={cloning !== null || loadingMore !== null}
          onclick={() => loadMore(f)}
        >
          {loadingMore === f.forge
            ? `Loading ${f.forge} repositories…`
            : query
              ? `Load more ${f.forge} repositories to keep searching`
              : `Load more ${f.forge} repositories`}
        </button>
      {:else if !f.complete}
        <small class="crit">{f.forge}: repository listing is incomplete</small>
      {/if}
    {/each}
    {#if query && shownForges.every((f) => f.repos.length === 0)}
      <small>
        {(forges ?? []).some((f) => f.next_cursor)
          ? 'No loaded repository matches; load more to continue searching'
          : (forges ?? []).some((f) => !f.complete)
            ? 'No loaded repository matches; the listing is incomplete'
            : 'No repository matches'}
      </small>
    {/if}
  </div>
{/if}

{#if known.length > 0}
  <span
    ><button type="button" class="lnk" onclick={() => (showRecents = !showRecents)}
      >{showRecents ? 'Hide' : `Recently used on this node (${known.length})`}</button
    ></span
  >
  {#if showRecents}
    <div class="picker" role="radiogroup">
      <!-- A path the node chose for its own clone is not worth reading; one
           the operator typed is theirs, and is the only way to tell two apart. -->
      {#each shownRecents as r (r.repo_path)}
        <button type="button" class:on={value === r.repo_path} onclick={() => (value = r.repo_path)}>
          <i></i>
          <span>{repoLabel(r.repo_path)}{#if !isManagedPath(r.repo_path, allManaged)} <small>{r.repo_path}</small>{/if}</span>
          <small>{r.sessions} session{r.sessions === 1 ? '' : 's'} · {formatAge(r.last_used_ms, clock.now)}</small>
        </button>
      {/each}
      {#each shownManaged as m (m.repo_path)}
        <button type="button" class:on={value === m.repo_path} onclick={() => (value = m.repo_path)}>
          <i></i>
          <span>{m.full_name} <small>{m.host}</small></span>
          <small>cloned</small>
        </button>
      {/each}
    </div>
  {/if}
{/if}
{#if error}
  <small class="crit">{error}</small>
{/if}

{#if chosenManaged}
  <div class="chosen">
    <span>{chosenManaged.full_name} <small>{chosenManaged.host} · cloned on this node</small></span>
    <button type="button" class="lnk" onclick={() => (value = '')}>change</button>
  </div>
{:else}
  <input
    bind:value
    placeholder={known.length > 0 ? 'or type a path on the node' : '/Users/you/src/project'}
    spellcheck="false"
  />
{/if}

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
  .picker button.more {
    display: block;
    color: var(--acc);
    text-align: center;
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
  input.search {
    margin-bottom: 4px;
  }
  .chosen {
    display: flex;
    gap: 10px;
    align-items: baseline;
    background: var(--s1);
    border-radius: 4px;
    padding: 8px 10px;
    font: 13.5px var(--sans);
    color: var(--ink);
  }
  .chosen span {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
