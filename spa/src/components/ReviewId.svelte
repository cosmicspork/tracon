<script lang="ts">
  // A review as its header names it: the generated name (renamable, as
  // everywhere) and a short id chip, the form agents write in notes. Clicking
  // either opens the full id with a Copy button: a click, never a hover, so a
  // phone reads it the same way.
  import { tick } from 'svelte'

  import Name from './Name.svelte'
  import { shortId } from '../lib/reviewStatus'
  import type { NameKind } from '../lib/names'

  let { kind, id }: { kind: NameKind; id: string } = $props()

  const panelId = $props.id()
  let open = $state(false)
  let copied = $state(false)
  let root = $state<HTMLElement>()
  let chip = $state<HTMLButtonElement>()
  let panel = $state<HTMLElement>()
  /** Where the panel sits, under the chip: fixed, since a header may clip what overflows it. */
  let at = $state({ top: 0, left: 0 })
  let timer: ReturnType<typeof setTimeout> | undefined

  async function toggle() {
    open = !open
    copied = false
    if (!open || !root) return
    const anchor = root.getBoundingClientRect()
    at = { top: anchor.bottom + 6, left: anchor.left }
    await tick()
    // Keep the panel on screen when the chip sits near the right edge.
    const r = panel?.getBoundingClientRect()
    if (r && r.right > window.innerWidth - 8) at = { ...at, left: Math.max(8, at.left - (r.right - (window.innerWidth - 8))) }
  }

  /** A click on the name itself opens it too; its own copy and rename buttons keep theirs. */
  function onName(e: MouseEvent) {
    if ((e.target as Element | null)?.closest('button, input, a')) return
    void toggle()
  }

  async function copy() {
    try {
      await navigator.clipboard.writeText(id)
      copied = true
      clearTimeout(timer)
      timer = setTimeout(() => (copied = false), 1200)
    } catch {
      // The id is shown in the panel to select by hand.
    }
  }

  function outside(e: PointerEvent) {
    if (open && root && !root.contains(e.target as Node)) open = false
  }

  function key(e: KeyboardEvent) {
    if (open && e.key === 'Escape') {
      open = false
      chip?.focus()
    }
  }
</script>

<svelte:window onpointerdown={outside} onkeydown={key} onscroll={() => (open = false)} />

<span class="ident" bind:this={root}>
  <!-- The chip beside it is the keyboard's way in. -->
  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <span class="nm" onclick={onName}><Name {kind} {id} renamable /></span>
  <button
    bind:this={chip}
    type="button"
    class="idchip"
    aria-label="Show the full id"
    aria-expanded={open}
    aria-controls={panelId}
    onclick={toggle}>{shortId(id)}</button
  >
  {#if open}
    <span class="panel" id={panelId} role="dialog" aria-label="Review id" bind:this={panel} style:top="{at.top}px" style:left="{at.left}px">
      <code>{id}</code>
      <button type="button" class="btn" onclick={copy}>{copied ? 'Copied' : 'Copy'}</button>
    </span>
  {/if}
</span>

<style>
  .ident {
    position: relative;
    display: inline-flex;
    align-items: baseline;
    gap: 6px;
    flex-wrap: wrap;
  }
  .nm {
    cursor: pointer;
  }
  .idchip {
    font: 11px var(--mono);
    padding: 0 6px;
    border: 1px solid var(--rule);
    border-radius: 3px;
    background: var(--s2);
    color: var(--ink2);
    cursor: pointer;
  }
  .idchip:hover,
  .idchip[aria-expanded='true'] {
    color: var(--acc);
    border-color: var(--acc);
  }
  .panel {
    position: fixed;
    z-index: 20;
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
    width: max-content;
    max-width: min(420px, calc(100vw - 16px));
    padding: 8px 10px;
    background: var(--s3);
    border: 1px solid var(--rule);
    border-radius: 6px;
    box-shadow: 0 6px 18px rgba(0, 0, 0, 0.25);
    font-weight: 400;
  }
  .panel code {
    font: 12px var(--mono);
    color: var(--ink);
    overflow-wrap: anywhere;
    user-select: all;
  }
</style>
