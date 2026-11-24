<script lang="ts">
  // A plugin page's icon: the file the node serves from this origin
  // (`/api/plugins/{name}/icon`), always as an <img> and never as inline SVG
  // markup, so nothing in it runs here. Without one, or when it fails to
  // load, the generic plugin glyph.
  let { src, size = 17, label = '' }: { src: string | null; size?: number; label?: string } = $props()

  let failed = $state(false)
  $effect(() => {
    void src
    failed = false
  })
</script>

{#if src && !failed}
  <img class="icon" {src} alt={label} width={size} height={size} onerror={() => (failed = true)} />
{:else}
  <svg class="icon glyph" viewBox="0 0 24 24" width={size} height={size} aria-hidden={label ? undefined : 'true'} role={label ? 'img' : undefined} aria-label={label || undefined}
    ><path d="M9 4v4M15 4v4" /><path d="M6 8h12v4a6 6 0 0 1-12 0z" /><path d="M12 18v3" /></svg
  >
{/if}

<style>
  .icon {
    flex-shrink: 0;
    object-fit: contain;
  }
  .glyph {
    stroke: currentColor;
    fill: none;
    stroke-width: 1.7;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
</style>
