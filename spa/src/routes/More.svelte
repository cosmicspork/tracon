<script lang="ts">
  // The phone's last tab: every visible navigation entry its bottom bar has
  // no room for, in the operator's order (lib/nav.ts), then Settings.
  import NavIcon from '../components/NavIcon.svelte'
  import { BUILT_IN_INFO, drawNav, phoneNav } from '../lib/nav'
  import { navigation } from '../lib/navigation.svelte'
  import { store } from '../lib/store.svelte'

  const rest = $derived(phoneNav(drawNav(navigation.list, navigation.plugins)).more)
</script>

<div class="h4">More <b>{store.node?.name ?? 'this node'}</b></div>

<nav class="more" aria-label="More destinations">
  {#each rest as e (e.key)}
    <a href={e.href}>
      <NavIcon entry={e} size={20} />
      <span><b>{e.label}</b><small>{e.hint}</small></span>
    </a>
  {/each}
  <a href="/settings">
    <NavIcon entry={{ builtin: 'settings', icon: null }} size={20} />
    <span><b>Settings</b><small>{BUILT_IN_INFO.settings.hint}</small></span>
  </a>
</nav>

<style>
  .more {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .more a {
    display: flex;
    align-items: center;
    gap: 14px;
    min-height: 56px;
    padding: 10px 14px;
    background: var(--s1);
    border-radius: 4px;
    color: var(--ink);
    text-decoration: none;
  }
  .more a:hover {
    background: var(--s2);
  }
  .more :global(svg),
  .more :global(img) {
    width: 20px;
    height: 20px;
    flex-shrink: 0;
  }
  .more :global(svg) {
    stroke: var(--ink2);
    fill: none;
    stroke-width: 1.7;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
  .more span {
    display: grid;
    gap: 2px;
    min-width: 0;
  }
  .more b {
    font: 600 14.5px var(--sans);
  }
  .more small {
    color: var(--ink2);
    font-size: 12.5px;
  }
</style>
