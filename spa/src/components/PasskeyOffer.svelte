<script lang="ts">
  // Right after pairing: the one moment this browser may add the node's first
  // passkey without already having one. Offered once; Settings > Access has
  // the same control for later.
  import { api, type PasskeyList } from '../lib/api'
  import { passkeysSupported } from '../lib/passkey'
  import { deviceLabel, Enroller } from '../lib/stepup.svelte'
  import { store } from '../lib/store.svelte'

  let list = $state<PasskeyList | null>(null)
  let done = $state(false)
  const enroller = new Enroller()

  $effect(() => {
    if (!store.justPaired || !passkeysSupported()) return
    void api
      .passkeys()
      .then((l) => (list = l))
      .catch(() => (store.justPaired = false))
  })

  const offered = $derived(
    store.justPaired &&
      !!list?.enrollment.open &&
      !!list.relying_party &&
      list.relying_party.origin === location.origin,
  )

  async function add() {
    if (await enroller.run(deviceLabel(navigator.userAgent))) done = true
  }

  function close() {
    store.justPaired = false
    done = false
  }
</script>

{#if offered || done}
  <div class="scrim">
    <div class="sheet" role="dialog" aria-modal="true" aria-labelledby="passkey-offer-title" data-testid="passkey-offer">
      {#if done}
        <h2 id="passkey-offer-title">Passkey added</h2>
        <p>Admin actions you make from this device will ask for it. Everyday use needs nothing more.</p>
        <div class="acts"><button class="btn p" type="button" onclick={close}>Done</button></div>
      {:else}
        <h2 id="passkey-offer-title">Add a passkey for this device?</h2>
        <p>
          This browser is signed in. Changing credentials, tokens, grants or the mesh from here needs a
          passkey to confirm each change. Right after pairing you can add the first one without
          confirming; later, adding one needs an existing passkey or the node itself.
        </p>
        {#if enroller.problem}<p class="err" data-testid="passkey-problem">{enroller.problem}</p>{/if}
        <div class="acts">
          <button class="btn p" type="button" onclick={add} disabled={enroller.busy} data-testid="offer-add-passkey">
            {enroller.busy ? 'Follow the prompts…' : enroller.held ? 'Try again' : 'Add a passkey'}
          </button>
          <button class="lnk" type="button" onclick={close} disabled={enroller.busy}>Not now</button>
        </div>
      {/if}
    </div>
  </div>
{/if}

<style>
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 90;
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
