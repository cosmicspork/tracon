<script lang="ts">
  // Settings › General › Navigation: the order of the rail and the phone
  // bar, what is shown, and which plugin pages are in it (lib/nav.ts). Every
  // change applies at once and is kept on this device for this node.
  // Reordering is by buttons, so it works by keyboard and on a phone.
  import Card from './Card.svelte'
  import NavIcon from '../NavIcon.svelte'
  import PluginIcon from '../PluginIcon.svelte'
  import { addable, BUILT_IN_INFO, entryKey, isDefaultNav, pluginIconUrl, type NavEntry } from '../../lib/nav'
  import { navigation } from '../../lib/navigation.svelte'

  /** What Add offers. Plugin pages today; built so other kinds can join. */
  const offers = $derived(
    addable(navigation.list, navigation.plugins ?? []).map((p) => ({
      key: `plugin:${p.name}`,
      label: p.active?.ui_title || p.name,
      detail: `plugin · ${p.name}`,
      icon: pluginIconUrl(p),
      add: () => navigation.addPlugin(p.name),
    })),
  )
  let picking = $state(false)

  $effect(() => {
    void navigation.refreshPlugins()
  })

  function label(e: NavEntry): string {
    if (e.kind === 'builtin') return BUILT_IN_INFO[e.id].label
    const p = navigation.plugins?.find((x) => x.name === e.name)
    return p?.active?.ui_title || e.name
  }
  function missing(e: NavEntry): string | null {
    if (e.kind !== 'plugin' || navigation.plugins === null) return null
    const p = navigation.plugins.find((x) => x.name === e.name)
    if (!p || p.state === 'removed') return 'not installed; shown again once it is'
    if (!p.active?.ui) return 'has no page in its active build'
    if (!(p.offered_on?.length ?? 0)) return 'not offered on any channel'
    return null
  }
  function icon(e: NavEntry) {
    if (e.kind === 'builtin') return { builtin: e.id, icon: null }
    const p = navigation.plugins?.find((x) => x.name === e.name)
    return { builtin: null, icon: p ? pluginIconUrl(p) : null }
  }
</script>

<Card title="Navigation" note="The rail and the phone's bar, on this device. The first four shown fill the phone's bar; the rest are under More.">
  <ol class="entries" aria-label="Navigation entries">
    {#each navigation.list as e, i (entryKey(e))}
      {@const name = label(e)}
      {@const why = missing(e)}
      <li class:hidden={e.hidden}>
        <span class="ic"><NavIcon entry={icon(e)} /></span>
        <span class="name"><b>{name}</b>{#if why}<small class="note">{why}</small>{/if}</span>
        <span class="acts">
          <button class="lnk" type="button" disabled={i === 0} onclick={() => navigation.move(i, -1)} aria-label="Move {name} up">Move up</button>
          <button class="lnk" type="button" disabled={i === navigation.list.length - 1} onclick={() => navigation.move(i, 1)} aria-label="Move {name} down">Move down</button>
          {#if e.kind === 'builtin' && e.id === 'home'}
            <span class="fixed">always shown</span>
          {:else}
            <label class="shown">
              <input type="checkbox" checked={!e.hidden} onchange={(ev) => navigation.hide(i, !ev.currentTarget.checked)} />
              <span>Shown</span>
            </label>
          {/if}
          {#if e.kind === 'plugin'}
            <button class="lnk" type="button" onclick={() => navigation.removePlugin(e.name)} aria-label="Remove {name} from navigation">Remove</button>
          {/if}
        </span>
      </li>
    {/each}
    <li class="settings">
      <span class="ic"><NavIcon entry={{ builtin: 'settings', icon: null }} /></span>
      <span class="name"><b>Settings</b><small class="note">fixed: always last</small></span>
    </li>
  </ol>

  <div class="foot">
    {#if picking}
      <div class="picker" role="group" aria-label="Add to navigation">
        {#each offers as o (o.key)}
          <button
            class="btn"
            type="button"
            onclick={() => {
              o.add()
              picking = false
            }}
          >
            <PluginIcon src={o.icon} size={15} />
            {o.label}
            <small>{o.detail}</small>
          </button>
        {:else}
          <span class="note">Nothing to add: every plugin with a page is in the list.</span>
        {/each}
        <button class="lnk" type="button" onclick={() => (picking = false)}>Cancel</button>
      </div>
    {:else}
      <button class="btn" type="button" onclick={() => (picking = true)}>Add</button>
    {/if}
    <button class="lnk" type="button" disabled={isDefaultNav(navigation.list)} onclick={() => navigation.reset()}>Reset to default</button>
  </div>
</Card>

<style>
  .entries {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 2px;
  }
  li {
    display: grid;
    grid-template-columns: 20px minmax(0, 1fr) auto;
    gap: 12px;
    align-items: center;
    padding: 6px 10px;
    background: var(--s1);
    border-radius: 4px;
  }
  li.hidden .name b,
  li.hidden .ic {
    color: var(--dim);
  }
  .ic {
    display: flex;
    color: var(--ink2);
  }
  .ic :global(svg),
  .ic :global(img) {
    width: 17px;
    height: 17px;
  }
  .ic :global(svg) {
    stroke: currentColor;
    fill: none;
    stroke-width: 1.7;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
  .name {
    display: grid;
    gap: 1px;
    min-width: 0;
  }
  .name b {
    font-weight: 500;
  }
  .name small {
    font-size: 12px;
    color: var(--dim);
  }
  .acts {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: flex-end;
    gap: 4px 14px;
  }
  .shown {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 13px;
    color: var(--ink2);
  }
  .fixed {
    font: 12px var(--mono);
    color: var(--dim);
  }
  .foot,
  .picker {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px 14px;
    margin-top: 10px;
  }
  .picker {
    margin-top: 0;
  }
  .picker .btn {
    display: inline-flex;
    align-items: center;
    gap: 8px;
  }
  .picker small {
    font: 11.5px var(--mono);
    color: var(--ink2);
  }
  @media (max-width: 700px) {
    li {
      grid-template-columns: 20px minmax(0, 1fr);
    }
    .acts {
      grid-column: 1 / -1;
      justify-content: flex-start;
    }
  }
</style>
