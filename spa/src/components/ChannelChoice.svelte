<script lang="ts">
  let {
    legend,
    choices,
    selected,
    onchange,
    disabled = false,
  }: {
    legend: string
    choices: { name: string; archived?: number | null }[]
    selected: string[]
    onchange: (next: string[]) => void
    disabled?: boolean
  } = $props()

  function toggle(channel: string, checked: boolean) {
    const next = selected.filter((name) => name !== channel)
    if (checked) next.push(channel)
    onchange(next)
  }
</script>

<fieldset>
  <legend>{legend}</legend>
  {#each choices as channel (channel.name)}
    <label class="channel-choice">
      <input
        type="checkbox"
        checked={selected.includes(channel.name)}
        {disabled}
        onchange={(event) => toggle(channel.name, event.currentTarget.checked)}
      />
      {channel.name}{channel.archived ? ' · archived' : ''}
    </label>
  {/each}
</fieldset>

<style>
  fieldset {
    display: flex;
    flex-wrap: wrap;
    gap: 6px 16px;
    border: 0;
    padding: 0;
    margin: 2px 0;
  }
  legend {
    width: 100%;
    margin-bottom: 4px;
    color: var(--dim);
  }
  .channel-choice {
    display: flex;
    flex-direction: row;
    align-items: center;
    gap: 6px;
  }
</style>
