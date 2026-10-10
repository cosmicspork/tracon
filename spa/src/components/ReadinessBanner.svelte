<script lang="ts">
  // Where a repository stands for each kind of session, before one is spent
  // finding out: an investigation needs only somewhere to run; verifying adds
  // checks; publishing adds a forge credential. A ready repository says
  // nothing. One that is not gets a banner naming each gap once, under the
  // first path it stops, so a missing credential never reads as a reason not
  // to investigate.
  import { api } from '../lib/api'
  import { allGaps, headline, remedy, unready } from '../lib/readiness'
  import { repoLabel } from '../lib/repo'
  import type { RepoReadiness } from '../lib/types'

  let {
    channel,
    repo,
    workItem = null,
    view = $bindable(null),
  }: {
    channel: string
    repo: string
    workItem?: string | null
    /** The node's answer, for a launcher that must not start what it refuses. */
    view?: RepoReadiness | null
  } = $props()
  let timer: ReturnType<typeof setTimeout> | undefined

  $effect(() => {
    const key = [channel, repo.trim(), workItem]
    clearTimeout(timer)
    view = null
    if (!key[0] || !key[1]) return
    // Typed paths change a keystroke at a time; ask once they settle.
    timer = setTimeout(async () => {
      try {
        const answer = await api.readiness(channel, repo.trim(), workItem)
        if (answer.channel === channel && answer.repo === repo.trim()) view = answer
      } catch {
        view = null
      }
    }, 300)
    return () => clearTimeout(timer)
  })

  const blocked = $derived(view ? unready(view) : [])
</script>

{#if view && blocked.length}
  <div class="banner ready" class:crit={blocked[0] === 'investigate'}>
    {headline(view)} <b>· {repoLabel(view.repo)} on {view.channel}</b>
    {#each allGaps(view) as { purpose, gap } (purpose + gap.key)}
      {@const fix = remedy(gap.key)}
      <i><span class="path">{purpose}</span> {gap.message}{#if fix}. <a href={fix.href}>{fix.label}</a>{/if}</i>
    {/each}
  </div>
{/if}

<style>
  .ready i {
    overflow-wrap: anywhere;
  }
  .path {
    color: var(--wait);
    margin-right: 4px;
  }
  .crit .path {
    color: var(--crit);
  }
</style>
