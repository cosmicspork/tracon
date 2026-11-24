<script lang="ts">
  // A navigation entry's icon: a built-in's own glyph, or a plugin's icon
  // (PluginIcon). The surrounding nav styles size and stroke the SVGs.
  import PluginIcon from './PluginIcon.svelte'
  import type { DrawnEntry } from '../lib/nav'

  let { entry, size = 17 }: { entry: Pick<DrawnEntry, 'builtin' | 'icon'> | { builtin: 'settings'; icon: null }; size?: number } = $props()
</script>

{#if entry.builtin === 'home'}
  <svg viewBox="0 0 24 24"><path d="M4 6h16M4 12h16M4 18h10" /></svg>
{:else if entry.builtin === 'tasks'}
  <svg viewBox="0 0 24 24"><path d="M5 7h3M5 12h3M5 17h3" /><path d="M11 7h8M11 12h8M11 17h5" /></svg>
{:else if entry.builtin === 'sessions'}
  <svg viewBox="0 0 24 24"><rect x="4" y="5" width="16" height="14" rx="2" /><path d="M8 10l3 2-3 2M13 14h3" /></svg>
{:else if entry.builtin === 'docs'}
  <svg viewBox="0 0 24 24"><path d="M6 3h9l4 4v14H6z" /><path d="M9 11h7M9 15h7M9 7h3" /></svg>
{:else if entry.builtin === 'memories'}
  <svg viewBox="0 0 24 24"><path d="M12 3c-4 2-7 5-7 9a7 7 0 0 0 14 0c0-4-3-7-7-9z" /><path d="M12 8v8M9 12h6" /></svg>
{:else if entry.builtin === 'plugins'}
  <svg viewBox="0 0 24 24"><path d="M9 4v4M15 4v4" /><path d="M6 8h12v4a6 6 0 0 1-12 0z" /><path d="M12 18v3" /></svg>
{:else if entry.builtin === 'usage'}
  <svg viewBox="0 0 24 24"><path d="M5 19V11M10 19V5M15 19v-6M20 19V8" /></svg>
{:else if entry.builtin === 'settings'}
  <svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="3" /><path d="M12 3v3M12 18v3M3 12h3M18 12h3M5.6 5.6l2.1 2.1M16.3 16.3l2.1 2.1M18.4 5.6l-2.1 2.1M7.7 16.3l-2.1 2.1" /></svg>
{:else}
  <PluginIcon src={entry.icon} {size} />
{/if}
