<script lang="ts">
  import { formatTokens } from '../lib/format'
  import { store } from '../lib/store.svelte'

  const channels = $derived(store.channels.filter((c) => !c.name.startsWith('@')))
  function width(c: (typeof channels)[number]): number {
    if (!c.ceiling.ceiling) return 0
    return Math.min(100, Math.round((c.ceiling.usage_today / c.ceiling.ceiling) * 100))
  }
</script>

<div class="h4">Today per channel <b>gateway tokens · resets at local midnight</b><a class="lnk r" href="/metrics">Metrics</a></div>
<div class="meters">
  {#each channels as c (c.name)}
    <div class="meter {c.ceiling.state}">
      <span>{c.name}</span>
      <div class="track"><div class="fill" style="width:{width(c)}%"></div></div>
      <span class="v"
        >{formatTokens(c.ceiling.usage_today)}
        {#if c.ceiling.ceiling}<em>of {formatTokens(c.ceiling.ceiling)}{c.ceiling.state === 'near' ? ' · near' : c.ceiling.state === 'at' ? ' · at ceiling' : ''}</em
          >{:else}<em>· no ceiling</em>{/if}</span
      >
    </div>
  {/each}
  {#if channels.length === 0}
    <div class="empty">No channels yet.</div>
  {/if}
</div>

<style>
  .h4 .r {
    margin-left: auto;
    letter-spacing: 0;
    text-transform: none;
  }
  .meters {
    display: flex;
    flex-direction: column;
    gap: 8px;
    background: var(--s1);
    border-radius: 4px;
    padding: 12px 14px;
  }
  .meter {
    display: grid;
    grid-template-columns: 96px minmax(0, 1fr) auto;
    gap: 0 12px;
    align-items: center;
    font: 12.5px var(--mono);
    color: var(--ink2);
  }
  .meter > span:first-child {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .track {
    height: 6px;
    border-radius: 3px;
    background: var(--s3);
    overflow: hidden;
  }
  .fill {
    height: 100%;
    background: var(--acc);
  }
  .meter.near .fill {
    background: var(--wait);
  }
  .meter.near .v {
    color: var(--wait);
  }
  .meter.at .fill {
    background: var(--crit);
  }
  .meter.at .v {
    color: var(--crit);
  }
  .v {
    white-space: nowrap;
  }
  .v em {
    font-style: normal;
    color: var(--dim);
  }
  @media (max-width: 700px) {
    .meter {
      grid-template-columns: 72px minmax(0, 1fr) auto;
    }
  }
</style>
