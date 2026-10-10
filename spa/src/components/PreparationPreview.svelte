<script lang="ts">
  // What preparing this checkout would do, and what in it the node will not
  // do, read before launch. A devcontainer hook or an install script is
  // explained here, with where its work belongs, rather than discovered as a
  // failure (or a silent skip) once a session is running. The launcher
  // fetches it, since a blocking incompatibility also holds Start.
  import { incompatibilityCount, preparationLine } from '../lib/preparation'
  import type { PreparationPreview } from '../lib/types'

  let { view }: { view: PreparationPreview | null } = $props()

  const counts = $derived(view ? incompatibilityCount(view) : null)
</script>

{#if view}
  <div class="prep">
    <small>Preparation: {preparationLine(view)}</small>
    {#if counts}
      <details>
        <summary class:crit={!view.ready}>{counts}</summary>
        <ul>
          {#each view.incompatible as i (i.source + i.item)}
            <li class:crit={i.blocking}>
              <code>{i.source} · {i.item}</code>
              {i.reason}{#if i.instead}. Instead: {i.instead}{/if}.
            </li>
          {/each}
        </ul>
      </details>
    {/if}
  </div>
{/if}

<style>
  .prep {
    display: grid;
    gap: 3px;
  }
  small,
  summary {
    font-size: 12.5px;
    color: var(--dim);
  }
  summary {
    cursor: pointer;
  }
  .crit {
    color: var(--crit);
  }
  ul {
    margin: 4px 0 0;
    padding-left: 16px;
    display: grid;
    gap: 4px;
    font-size: 12.5px;
    color: var(--ink2);
  }
  li.crit {
    color: var(--ink2);
  }
  li.crit code {
    color: var(--crit);
  }
  code {
    font-size: 11.5px;
    margin-right: 4px;
  }
</style>
