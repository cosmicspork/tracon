<script lang="ts">
  // The one way a provider is added, in the redesigned Connections pane: an
  // inline expansion (never a modal, matching the rest of Settings.svelte),
  // exactly the three options the approved mockup shows. The two subscription
  // options trigger the harness's own sign-in immediately; the third hands
  // off to the API-key/custom form, which is its own component.
  let {
    showCustom = true,
    busy = false,
    onAnthropic,
    onOpenAI,
    onCustom,
    onCancel,
  }: {
    /** False on a peer node: custom-provider creation is loopback-only, so
        only the two subscription sign-ins are offered there. */
    showCustom?: boolean
    busy?: boolean
    onAnthropic: () => void
    onOpenAI: () => void
    onCustom: () => void
    onCancel: () => void
  } = $props()
</script>

<div class="chooser">
  <button class="choice acc" onclick={onAnthropic} disabled={busy}>
    <span class="mark"></span>
    <span class="txt">
      <b>Anthropic subscription</b>
      <span>Sign in with a Claude account. Shared across the mesh once connected.</span>
    </span>
    <span class="go">sign in →</span>
  </button>
  <button class="choice acc" onclick={onOpenAI} disabled={busy}>
    <span class="mark"></span>
    <span class="txt">
      <b>OpenAI subscription</b>
      <span>Sign in with a ChatGPT account, for Codex-backed sessions.</span>
    </span>
    <span class="go">sign in →</span>
  </button>
  {#if showCustom}
    <button class="choice" onclick={onCustom} disabled={busy}>
      <span class="mark"></span>
      <span class="txt">
        <b>API key / custom</b>
        <span>Anthropic, OpenAI, or any OpenAI-compatible endpoint — OpenRouter and the rest.</span>
      </span>
      <span class="go">continue →</span>
    </button>
  {/if}
  <button class="lnk cancel" type="button" onclick={onCancel} disabled={busy}>Cancel</button>
</div>

<style>
  .chooser {
    display: grid;
    gap: 8px;
  }
  .choice {
    display: grid;
    grid-template-columns: auto 1fr auto;
    align-items: center;
    gap: 14px;
    background: var(--s2);
    border-radius: 4px;
    padding: 13px 16px;
    text-align: left;
    border: none;
    color: var(--ink);
    cursor: pointer;
    width: 100%;
    font: inherit;
  }
  .choice:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .choice .mark {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--s3);
  }
  .choice.acc .mark {
    background: var(--acc);
  }
  .choice .txt {
    display: grid;
    gap: 2px;
    min-width: 0;
  }
  .choice .txt b {
    font: 600 13.5px var(--sans);
  }
  .choice .txt span {
    font-size: 12.5px;
    color: var(--ink2);
  }
  .choice .go {
    color: var(--dim);
    font-size: 12px;
    font-family: var(--mono);
    white-space: nowrap;
  }
  .cancel {
    justify-self: start;
  }
  @media (max-width: 700px) {
    .choice {
      grid-template-columns: auto 1fr;
    }
    .choice .go {
      grid-column: 2;
      justify-self: start;
    }
  }
</style>
