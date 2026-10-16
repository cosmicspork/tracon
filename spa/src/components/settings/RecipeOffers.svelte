<script lang="ts">
  import { tick } from 'svelte'
  import { api } from '../../lib/api'
  import { formatAge, formatDuration } from '../../lib/format'
  import { OFFERS_ANCHOR, inboxOrder, reviewIsStale, shown } from '../../lib/recipes'
  import { router } from '../../lib/router.svelte'
  import { store } from '../../lib/store.svelte'
  import type { RecipeOffer, RecipeOfferView } from '../../lib/types'

  let { onaccepted }: { onaccepted?: () => void } = $props()

  const local = $derived(store.node?.loopback ?? false)

  let offers = $state<RecipeOffer[]>([])
  let open = $state<string | null>(null)
  let view = $state<RecipeOfferView | null>(null)
  let path = $state('')
  let replaceImage = $state(false)
  let keepCredentials = $state(false)
  let busy = $state('')
  let error = $state('')
  let note = $state('')

  const ordered = $derived(inboxOrder(offers))
  const pending = $derived(offers.filter((offer) => offer.state === 'pending').length)
  const pinned = $derived(Boolean(view?.before?.image && view?.offer.recipe.dockerfile))
  const lends = $derived(view?.credentials.state === 'dropped' || view?.credentials.state === 'kept')
  // An accept writes only what was reviewed: a change to the form waits for
  // another comparison.
  const stale = $derived(reviewIsStale(view, path, replaceImage, keepCredentials))

  async function load() {
    try {
      offers = (await api.recipeOffers()).offers
      error = ''
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    }
  }
  // Read again on each arrival, so a tapped notification or Home's card that
  // lands here while it is already open shows the offer it named; a link that
  // names this block scrolls to it once it has something to show.
  $effect(() => {
    void router.revision
    void load().then(async () => {
      if (new URLSearchParams(router.search).get('focus') !== OFFERS_ANCHOR) return
      await tick()
      document.getElementById(OFFERS_ANCHOR)?.scrollIntoView({ block: 'start' })
    })
  })
  // Home counts what is undecided; a decision here should leave it at once.
  const decided = async () => {
    await load()
    void store.refreshInterventions()
  }

  async function compare(offer: RecipeOffer) {
    busy = `view:${offer.id}`
    error = ''
    try {
      view = await api.recipeOffer(offer.id, path.trim(), replaceImage, keepCredentials)
      path = view.target.path
    } catch (cause) {
      view = null
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
    open = offer.id
    view = null
    path = ''
    replaceImage = false
    keepCredentials = false
    void compare(offer)
  }

  async function accept(offer: RecipeOffer) {
    if (busy || !view || stale) return
    busy = `accept:${offer.id}`
    error = ''
    note = ''
    try {
      const done = await api.acceptRecipeOffer(offer.id, {
        path: view.target.path,
        replace_image: view.replace_image,
        keep_credentials: view.keep_credentials,
        viewed_hash: view.viewed_hash,
      })
      note = `Accepted onto ${done.path} in node.toml.${done.building ? ' This node is building its own image from it now.' : ''}`
      open = null
      view = null
      await decided()
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
      await decided()
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      busy = ''
    }
  }
</script>

{#if offers.length}
  <section class="offers" id={OFFERS_ANCHOR} aria-label="Recipes offered by other nodes">
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
        {#if open === offer.id}
          <div class="review">
            <label>
              <span>Apply to</span>
              <input bind:value={path} spellcheck="false" placeholder={offer.repo} disabled={busy !== ''} />
              <small>
                {#if view}{view.target.exists ? 'The entry already answering for this repository.' : 'A new entry.'}{/if}
                Name another entry's path, or an absolute checkout, to put it there.
              </small>
            </label>
            {#if pinned}
              <label class="toggle">
                <input
                  type="checkbox"
                  checked={replaceImage}
                  onchange={(event) => {
                    replaceImage = event.currentTarget.checked
                    void compare(offer)
                  }}
                  disabled={busy !== ''}
                />
                Build the offered Dockerfile in place of the image this entry pins
              </label>
            {/if}
            {#if lends && view}
              <label class="toggle">
                <input
                  type="checkbox"
                  checked={keepCredentials}
                  onchange={(event) => {
                    keepCredentials = event.currentTarget.checked
                    void compare(offer)
                  }}
                  disabled={busy !== ''}
                />
                Keep lending {view.credentials.names.join(', ')} to the preparation this offer changes
              </label>
              <small>The offer changes what that preparation runs or reaches. Kept, this repository is held: nothing is built or warmed for it until you start its build here, and it says what it lends when you do.</small>
            {/if}
            <div class="acts">
              <button class="lnk" type="button" onclick={() => void compare(offer)} disabled={busy !== '' || !stale}>Compare</button>
            </div>
            {#if view && !stale}
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
              {#if view.kept.length}<p class="dim">Kept as this node has them: {view.kept.join(', ')}.</p>{/if}
              {#if !local}
                <p class="dim">Accepting writes node.toml, so from off the node it asks for your passkey.</p>
              {/if}
            {:else if view}
              <p class="dim">Compare again to see what this would write.</p>
            {/if}
            <div class="acts">
              <button class="btn p" type="button" onclick={() => void accept(offer)} disabled={busy !== '' || !view || stale}>
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
