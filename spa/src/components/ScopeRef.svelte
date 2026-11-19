<script lang="ts">
  // A memory's scope reference as the operator reads it. A `project` scope's
  // reference is sha256(channel ‖ remote), so it reads as the remote.
  import { projectLabel } from '../lib/labels.svelte'
  import { shortHash } from '../lib/names'
  import Name from './Name.svelte'

  let { channel, scope, ref }: { channel: string; scope: string; ref: string } = $props()

  const label = $derived(scope === 'project' ? (projectLabel(channel, ref) ?? 'unlisted project') : shortHash(ref))
</script>

<Name id={ref} {label} />
