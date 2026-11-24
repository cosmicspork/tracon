<script lang="ts">
  // `.toggle`: a labelled switch, a checkbox with a title and an optional
  // hint, as Settings › General's. Space toggles it, as any checkbox. Put
  // several in a `.toggles` block and they are ruled apart.
  let {
    checked,
    title,
    hint,
    disabled = false,
    onchange,
  }: {
    checked: boolean
    title: string
    hint?: string
    disabled?: boolean
    onchange: (checked: boolean) => void
  } = $props()
</script>

<label class="toggle">
  <input type="checkbox" {checked} {disabled} onchange={(e) => onchange(e.currentTarget.checked)} />
  <span><b>{title}</b>{#if hint}<small>{hint}</small>{/if}</span>
</label>
