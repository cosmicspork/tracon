<script lang="ts">
  // What a review that pushes straight to its base says before approval: the
  // warning while `warn_direct` is on, the commit it fast-forwards from, and
  // what protects the base on the forge.
  import { directWarning, protectionLine, type BaseProtection, type DirectTarget } from '../lib/direct'

  let {
    target,
    provider,
    from,
    protection,
  }: { target: DirectTarget; provider: string; from?: string; protection?: BaseProtection | null } = $props()

  const warning = $derived(directWarning(target, provider))
  const guard = $derived(protectionLine(protection))
</script>

{#if warning}
  <div class="banner direct" role="alert">
    {warning}{#if from}<b> · fast-forwards {target.base} from {from.slice(0, 8)}</b>{/if}
  </div>
{/if}
{#if guard}
  <div class="banner {guard.tone}">{guard.text}</div>
{/if}

<style>
  .direct {
    font-weight: 500;
  }
</style>
