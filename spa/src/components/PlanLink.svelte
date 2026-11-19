<script lang="ts">
  // A link to a plan doc by its title. A plan's slug (`plan-<12 hex>`) is
  // derived from its task's id and is not something to read; the title is.
  // While the title loads, or when the doc cannot be read, the link reads
  // "plan doc".
  import { docTitle } from '../lib/labels.svelte'

  let { channel, slug }: { channel: string; slug: string } = $props()

  const title = $derived(docTitle(channel, slug))
</script>

<a href="/docs/{channel}/{slug}" title={slug}>{title ?? 'plan doc'}</a>
