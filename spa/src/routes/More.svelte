<script lang="ts">
  // The phone's fifth tab: the rail entries its bottom bar has no room for.
  import { store } from '../lib/store.svelte'
</script>

<div class="h4">More <b>{store.node?.name ?? 'this node'}</b></div>

<nav class="more" aria-label="More destinations">
  <a href="/memories">
    <svg viewBox="0 0 24 24"><path d="M12 3c-4 2-7 5-7 9a7 7 0 0 0 14 0c0-4-3-7-7-9z" /><path d="M12 8v8M9 12h6" /></svg>
    <span><b>Memories</b><small>What sessions learned, kept for the next one</small></span>
  </a>
  <a href="/evidence">
    <svg viewBox="0 0 24 24"><path d="M9 4h6l1 3H8z" /><path d="M6 7h12v13H6z" /><path d="M9 12l2 2 4-4" /></svg>
    <span><b>Evidence</b><small>Captured changes and where they came from</small></span>
  </a>
  <a href="/usage">
    <svg viewBox="0 0 24 24"><path d="M5 19V11M10 19V5M15 19v-6M20 19V8" /></svg>
    <span><b>Usage</b><small>Sessions, tokens and cost by channel</small></span>
  </a>
  <a href="/settings">
    <svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="3" /><path d="M12 3v3M12 18v3M3 12h3M18 12h3M5.6 5.6l2.1 2.1M16.3 16.3l2.1 2.1M18.4 5.6l-2.1 2.1M7.7 16.3l-2.1 2.1" /></svg>
    <span><b>Settings</b><small>Connections, channels, devices, mesh and this node</small></span>
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
  .more svg {
    width: 20px;
    height: 20px;
    flex-shrink: 0;
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
