<script lang="ts" generics="T extends string">
  // `.segmented`: a choice of a few, as a radiogroup. Arrow keys move the
  // choice (and focus) and wrap; Home and End go to the ends. Only the
  // chosen one is in the tab order.
  import { segmentedKey } from '../segmented'

  let {
    options,
    value,
    label,
    onchange,
  }: {
    options: { value: T; label: string }[]
    value: T
    /** What is being chosen, for the radiogroup's aria-label. */
    label: string
    onchange: (value: T) => void
  } = $props()

  let group: HTMLDivElement | undefined = $state()
  const current = $derived(options.findIndex((o) => o.value === value))

  function keydown(event: KeyboardEvent) {
    const to = segmentedKey(event.key, current, options.length)
    if (to === null) return
    event.preventDefault()
    onchange(options[to].value)
    group?.querySelectorAll<HTMLButtonElement>('button')[to]?.focus()
  }
</script>

<div class="segmented" role="radiogroup" aria-label={label} bind:this={group}>
  {#each options as o, i (o.value)}
    <button
      type="button"
      role="radio"
      aria-checked={o.value === value}
      class:on={o.value === value}
      tabindex={o.value === value || (current < 0 && i === 0) ? 0 : -1}
      onclick={() => onchange(o.value)}
      onkeydown={keydown}>{o.label}</button
    >
  {/each}
</div>
