<script lang="ts">
  // A subscription sign-in that can no longer be renewed, on this node or a
  // peer, while a channel's sessions still use it: said where the operator
  // looks first, with the way to the card that signs in again.
  import { providerCardHref, providerLabel, signInsNeeded } from '../lib/providers'
  import { store } from '../lib/store.svelte'

  const needed = $derived(signInsNeeded(store.nodes, store.providers))
</script>

{#each needed as entry (entry.provider)}
  <div class="banner crit" role="status">
    {providerLabel(entry.provider)} needs a new sign-in on {entry.nodes.map((node) => node.name).join(', ')}
    · <a href={providerCardHref(entry.nodes[0].id)}>Sign in again</a>
  </div>
{/each}
