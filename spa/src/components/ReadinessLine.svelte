<script lang="ts">
  // Where a repository stands for each kind of session, before one is spent
  // finding out: an investigation needs only somewhere to run; verifying adds
  // checks; publishing adds a forge credential. Each path lists only what it
  // adds to the one before, so a missing credential never reads as a reason
  // not to investigate.
  import { api } from '../lib/api'
  import { ownGaps, PATHS } from '../lib/readiness'
  import type { RepoReadiness } from '../lib/types'

  let { channel, repo, workItem = null }: { channel: string; repo: string; workItem?: string | null } = $props()

  let view = $state<RepoReadiness | null>(null)
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
</script>

{#if view}
  <details class="ready">
    <summary>
      {#each PATHS as purpose (purpose)}
        <span class:ok={view[purpose].ready} class:no={!view[purpose].ready}
          >{view[purpose].ready ? '✓' : '✗'} {purpose}</span
        >
      {/each}
    </summary>
    <ul>
      {#each PATHS as purpose (purpose)}
        {#each ownGaps(view, purpose) as gap (gap.key)}
          <li><b>{purpose}</b> {gap.message}</li>
        {/each}
        {#each view[purpose].notes as note (note.key)}
          <li class="note"><b>{purpose}</b> {note.message}</li>
        {/each}
      {/each}
      {#if PATHS.every((p) => view?.[p].ready && !view?.[p].notes.length)}
        <li class="note">Ready to investigate, verify and publish.</li>
      {/if}
    </ul>
  </details>
{/if}

<style>
  .ready {
    font: 12px var(--mono);
    color: var(--ink2);
  }
  summary {
    cursor: pointer;
    display: flex;
    flex-wrap: wrap;
    gap: 3px 12px;
  }
  .ok {
    color: var(--ok, var(--ink2));
  }
  .no {
    color: var(--wait);
  }
  ul {
    list-style: none;
    margin: 6px 0 0;
    padding: 0;
    display: grid;
    gap: 3px;
    font: 12.5px var(--sans, inherit);
  }
  li {
    color: var(--ink2);
    overflow-wrap: anywhere;
  }
  li b {
    font: 11.5px var(--mono);
    color: var(--wait);
    margin-right: 6px;
  }
  .note,
  .note b {
    color: var(--dim);
  }
</style>
