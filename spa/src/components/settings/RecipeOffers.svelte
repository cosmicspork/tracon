<script lang="ts">
  import { api } from '../../lib/api'
  import { formatAge, formatDuration } from '../../lib/format'
  import { inboxOrder, shown } from '../../lib/recipes'
  import { store } from '../../lib/store.svelte'
  import type { RecipeOffer, RecipeOfferView } from '../../lib/types'

  let { onaccepted }: { onaccepted?: () => void } = $props()

  const local = $derived(store.node?.loopback ?? false)

  let offers = $state<RecipeOffer[]>([])
  let open = $state<string | null>(null)
  let view = $state<RecipeOfferView | null>(null)
  let path = $state('')
  let replaceImage = $state(false)
  let busy = $state('')
  let error = $state('')
  let note = $state('')

  const ordered = $derived(inboxOrder(offers))
  const pending = $derived(offers.filter((offer) => offer.state === 'pending').length)
  const pinned = $derived(Boolean(view?.before?.image && view?.offer.recipe.dockerfile))

  async function load() {
    try {
      offers = (await api.recipeOffers()).offers
      error = ''
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    }
  }
  void load()

  async function show(offer: RecipeOffer, chosen = '') {
    busy = `view:${offer.id}`
    error = ''
    try {
      view = await api.recipeOffer(offer.id, chosen, replaceImage)
      open = offer.id
      path = view.target.path
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      busy = ''
    }
  }

  function toggle(offer: RecipeOffer) {
    if (open === offer.id) {
      open = null
      view = null
      return
    }
    replaceImage = false
    void show(offer)
  }

  async function accept(offer: RecipeOffer) {
    if (busy) return
    busy = `accept:${offer.id}`
    error = ''
    note = ''
    try {
      const done = await api.acceptRecipeOffer(offer.id, { path, replace_image: replaceImage })
      note = `Accepted onto ${done.path} in node.toml.${done.building ? ' This node is building its own image from it now.' : ''}`
      open = null
      view = null
      await load()
      onaccepted?.()
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      busy = ''
    }
  }

  async function decline(offer: RecipeOffer) {
    if (busy) return
    busy = `decline:${offer.id}`
    error = ''
    note = ''
    try {
      await api.declineRecipeOffer(offer.id)
      open = null
      view = null
      await load()
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      busy = ''
    }
  }
</script>

{#if offers.length}
  <section class="offers" aria-label="Recipes offered by other nodes">
    <h3>Offered by other nodes{#if pending}{' · '}<span class="count">{pending} waiting</span>{/if}</h3>
    <p class="dim">
      A channel co-member offered how it builds, prepares and checks a repository. Nothing changes here until you
      accept one; this node then builds and pins its own image. An offer never carries an image, a digest, a credential
      or a path on the sender's machine.
    </p>
    {#each ordered as offer (offer.id)}
      <article class="offer" class:decided={offer.state !== 'pending'}>
        <header>
          <code>{offer.repo}</code>
          <small>
            from {offer.sender_name} on {offer.channel} · {formatAge(offer.received_ms)} ago
            {#if offer.state === 'pending'}· lapses in {formatDuration(Math.max(0, offer.expires_ms - Date.now()))}{:else}· {offer.state}{#if offer.decided_path} onto {offer.decided_path}{/if}{/if}
          </small>
        </header>
        {#if offer.note}<p class="said">“{offer.note}”</p>{/if}
        {#if offer.state === 'pending'}
          <div class="acts">
            <button class="lnk" type="button" onclick={() => toggle(offer)} disabled={busy !== ''}>
              {open === offer.id ? 'Hide changes' : busy === `view:${offer.id}` ? 'Reading…' : 'Review changes'}
            </button>
            <button class="lnk d" type="button" onclick={() => void decline(offer)} disabled={busy !== ''}>
              {busy === `decline:${offer.id}` ? 'Declining…' : 'Decline'}
            </button>
          </div>
        {/if}
        {#if open === offer.id && view}
          <div class="review">
            <label>
              <span>Apply to</span>
              <input bind:value={path} spellcheck="false" placeholder={offer.repo} disabled={busy !== ''} />
              <small>
                {view.target.exists ? 'The entry already answering for this repository.' : 'A new entry.'} Name another
                entry's path, or an absolute checkout, to put it there.
              </small>
            </label>
            {#if pinned}
              <label class="toggle">
                <input type="checkbox" bind:checked={replaceImage} disabled={busy !== ''} />
                Build the offered Dockerfile in place of the image this entry pins
              </label>
            {/if}
            <div class="acts">
              <button class="lnk" type="button" onclick={() => void show(offer, path)} disabled={busy !== ''}>Compare again</button>
            </div>
            {#if view.changes.length}
              <table>
                <thead><tr><th>Field</th><th>Here now</th><th>Offered</th></tr></thead>
                <tbody>
                  {#each view.changes as change (change.field)}
                    <tr>
                      <th scope="row">{change.field}</th>
                      <td><pre>{shown(change.before)}</pre></td>
                      <td><pre>{shown(change.after)}</pre></td>
                    </tr>
                  {/each}
                </tbody>
              </table>
            {:else}
              <p class="dim">The entry already reads this way: accepting changes nothing.</p>
            {/if}
            {#each view.notes as line (line)}<p class="caution">{line}</p>{/each}
            <p class="dim">Kept as this node has them: {view.kept.join(', ')}.</p>
            {#if !local}
              <p class="dim">Accepting writes node.toml, so from off the node it asks for your passkey.</p>
            {/if}
            <div class="acts">
              <button class="btn p" type="button" onclick={() => void accept(offer)} disabled={busy !== ''}>
                {busy === `accept:${offer.id}` ? 'Accepting…' : 'Accept'}
              </button>
            </div>
          </div>
        {/if}
      </article>
    {/each}
  </section>
{/if}
{#if error}<p class="error" role="alert">{error}</p>{/if}
{#if note}<p class="note" role="status">{note}</p>{/if}

<style>
  .offers { display: grid; gap: 10px; }
  h3 { margin: 0; font: 500 11px var(--mono); letter-spacing: 0.08em; text-transform: uppercase; color: var(--ink2); }
  .count { color: var(--wait); }
  .offer { background: var(--s2); border-radius: 4px; padding: 10px 12px; display: grid; gap: 8px; }
  .offer.decided { opacity: 0.65; }
  header { display: flex; flex-wrap: wrap; align-items: baseline; gap: 4px 12px; }
  header code { font: 13px var(--mono); overflow-wrap: anywhere; }
  .said { margin: 0; font-size: 13px; color: var(--ink2); }
  .review { display: grid; gap: 10px; }
  label { display: grid; gap: 5px; min-width: 0; }
  label > span:first-child { font: 500 11px var(--mono); letter-spacing: 0.08em; text-transform: uppercase; color: var(--ink2); }
  label.toggle { display: flex; align-items: center; gap: 6px; }
  input:not([type='checkbox']) {
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13px var(--mono);
    min-width: 0;
    min-height: 36px;
  }
  table { width: 100%; border-collapse: collapse; font: 12px var(--mono); table-layout: fixed; }
  th, td { text-align: left; vertical-align: top; padding: 4px 6px; border-top: 1px solid var(--s1); }
  thead th { color: var(--ink2); font-weight: 500; }
  pre { margin: 0; white-space: pre-wrap; overflow-wrap: anywhere; font: inherit; }
  small { font-size: 12.5px; color: var(--dim); }
  .acts { display: flex; flex-wrap: wrap; align-items: center; gap: 8px 14px; }
  .caution { margin: 0; color: var(--wait); font-size: 12.5px; }
  .dim, .note, .error { margin: 0; font-size: 12.5px; }
  .dim { color: var(--dim); }
  .note { color: var(--ok); }
  .error { color: var(--crit); }
</style>
