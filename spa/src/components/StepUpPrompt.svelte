<script lang="ts">
  // The confirmation every admin action from off the node asks for. The
  // button is the user gesture the passkey sheet needs, which is why this is
  // a prompt and not something run silently behind the request.
  import { isTauri } from '@tauri-apps/api/core'

  import { confirming } from '../lib/passkey'
  import { stepUp } from '../lib/stepup.svelte'
  import DeviceCode from './DeviceCode.svelte'

  const desktop = typeof window !== 'undefined' && isTauri()

  const pending = $derived(stepUp.pending)
  const what = $derived(pending ? confirming(pending.stepUp) : null)

  /** Confirming in another browser, by choice or because this one cannot. */
  const viaDevice = $derived(stepUp.elsewhere || !!stepUp.device)

  function onkey(e: KeyboardEvent) {
    if (e.key === 'Escape' && pending && !stepUp.busy) stepUp.cancel()
  }
</script>

<svelte:window onkeydown={onkey} />

{#if pending}
  <div class="scrim">
    <div class="sheet" role="dialog" aria-modal="true" aria-labelledby="step-up-title" data-testid="step-up">
      <h2 id="step-up-title">{viaDevice ? 'Confirm in a browser with your passkey' : 'Confirm with your passkey'}</h2>
      <p>
        Confirm with your passkey.
      </p>
      {#if what?.headline}<p class="what" data-testid="step-up-summary">{what.headline}</p>{/if}
      {#if what?.detail}<code class="action">{what.detail}</code>{/if}
      {#if stepUp.device}
        <DeviceCode start={stepUp.device} {desktop} />
      {:else if viaDevice && !stepUp.problem}
        <p>Asking the node…</p>
      {/if}
      {#if stepUp.problem}<p class="err" data-testid="step-up-problem">{stepUp.problem}</p>{/if}
      <div class="acts">
        {#if stepUp.elsewhere}
          {#if stepUp.problem}
            <button class="btn p" type="button" onclick={() => void stepUp.confirmElsewhere()} data-testid="step-up-elsewhere">
              Ask again
            </button>
          {/if}
        {:else if !stepUp.device}
          <button
            class="btn p"
            type="button"
            onclick={() => void stepUp.confirm()}
            disabled={stepUp.busy}
            data-testid="step-up-confirm"
          >
            {stepUp.busy ? 'Waiting for the passkey…' : stepUp.problem ? 'Try again' : 'Confirm'}
          </button>
          <button
            class="lnk"
            type="button"
            onclick={() => void stepUp.confirmElsewhere()}
            disabled={stepUp.busy}
            data-testid="step-up-elsewhere"
          >
            Use another device
          </button>
        {/if}
        <button class="lnk" type="button" onclick={() => stepUp.cancel()} disabled={stepUp.busy}>Cancel</button>
      </div>
    </div>
  </div>
{/if}

<style>
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 100;
    display: grid;
    place-items: center;
    padding: 16px;
    background: rgba(0, 0, 0, 0.55);
  }
  .sheet {
    display: grid;
    gap: 12px;
    width: min(420px, 100%);
    background: var(--s1);
    border-radius: 6px;
    padding: 22px 20px;
  }
  h2 {
    margin: 0;
    font: 600 16px var(--sans);
    color: var(--ink);
  }
  p {
    margin: 0;
    color: var(--ink2);
    font-size: 13.5px;
    line-height: 1.5;
  }
  .what {
    color: var(--ink);
    font: 500 14px var(--sans);
    overflow-wrap: anywhere;
  }
  .action {
    font: 12.5px var(--mono);
    color: var(--ink);
    background: var(--s2);
    border-radius: 4px;
    padding: 6px 8px;
    overflow-wrap: anywhere;
  }
  .err {
    color: var(--crit);
    font: 12.5px var(--mono);
    overflow-wrap: anywhere;
  }
  .acts {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px 16px;
  }
</style>
