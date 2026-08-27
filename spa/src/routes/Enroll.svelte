<script lang="ts">
  // Enroll a new node: three moments. Invite (code, URL, QR, this node's
  // fingerprint), received (the other node answered; confirm its fingerprint),
  // enrolled. Browser only; the phone cannot compare fingerprints usefully.
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { store } from '../lib/store.svelte'
  import type { Invite } from '../lib/types'

  let picked = $state<Record<string, boolean>>({})
  let invite = $state<Invite | null>(null)
  let busy = $state(false)
  let error = $state<string | null>(null)
  let pollTimer: ReturnType<typeof setInterval> | undefined

  const channels = $derived(store.channels.map((c) => c.name))
  const chosen = $derived(channels.filter((c) => picked[c]))
  const remaining = $derived(invite ? Math.max(0, invite.expires_at - Math.floor(clock.now / 1000)) : 0)
  const step = $derived(!invite ? 1 : invite.state === 'admitted' ? 3 : invite.state === 'received' ? 2 : 1)

  async function start() {
    busy = true
    error = null
    try {
      invite = await api.openInvite(chosen)
      pollTimer = setInterval(() => void poll(), 2000)
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
    }
  }

  async function poll() {
    if (!invite || invite.state !== 'waiting') return
    try {
      const next = await api.pollInvite(invite.code)
      invite = next
      if (next.state !== 'waiting') clearInterval(pollTimer)
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
      clearInterval(pollTimer)
    }
  }

  async function admit() {
    if (!invite) return
    busy = true
    error = null
    try {
      invite = await api.admitInvite(invite.code)
      await store.refetch()
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
    }
  }

  async function cancel() {
    clearInterval(pollTimer)
    if (invite) void api.cancelInvite(invite.code).catch(() => {})
    invite = null
    error = null
  }

  $effect(() => () => clearInterval(pollTimer))

  function mmss(s: number): string {
    return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`
  }
</script>

<div class="h4"><a class="lnk" href="/nodes">‹ Nodes</a> Enroll a new node</div>
<div class="steps">
  <span class:cur={step === 1}>1 invite</span>
  <span class:cur={step === 2}>2 confirm</span>
  <span class:cur={step === 3}>3 enrolled</span>
</div>

{#if store.mesh === null || store.mesh.hub.state === 'disabled'}
  <div class="banner crit">no hub configured <b>· run tracon mesh init on this node first</b></div>
{:else if !invite}
  <form class="form" onsubmit={(e) => { e.preventDefault(); void start() }}>
    <div class="runs">
      <span>Channels to hand off</span>
      <div class="pick">
        <label><input type="checkbox" checked disabled /> @mesh <small>always</small></label>
        {#each channels as c (c)}
          <label><input type="checkbox" bind:checked={picked[c]} /> {c}</label>
        {/each}
      </div>
      <small>The new node receives the keys for these channels and this node's policy bundle. Nothing else moves.</small>
    </div>
    {#if error}
      <div class="banner crit">could not open the invitation <b>· {error}</b></div>
    {/if}
    <div><button class="btn p" type="submit" disabled={busy}>Create invitation</button></div>
  </form>
{:else if invite.state === 'waiting'}
  <div class="code">
    {#if invite.qr_svg}
      <div class="qr">{@html invite.qr_svg}</div>
    {/if}
    <div class="kv">
      <span class="k">Code</span><span class="big">{invite.display_code}</span>
      <span class="k">On the new machine</span>
      <span class="v">curl -fsSL https://raw.githubusercontent.com/cosmicspork/tracon/main/install.sh | sh</span>
      <span class="v">tracon enroll {invite.url}</span>
      <span class="k">This node's fingerprint</span><span class="fp">{invite.own_fingerprint}</span>
      <span class="ttl">{remaining > 0 ? `expires in ${mmss(remaining)}` : 'expired'}</span>
    </div>
  </div>
  {#if error}
    <div class="banner crit">waiting failed <b>· {error}</b></div>
  {/if}
  <div><button class="lnk d" onclick={cancel}>Cancel invitation</button></div>
{:else if invite.state === 'received' && invite.received}
  <div class="recv">
    <span class="bar"></span>
    <span class="t">
      <em>Received</em>
      {invite.received.name}
      <small>{invite.received.facts} · asked for {invite.channels.join(', ')}</small>
      <span class="fp">{invite.received_fingerprint}</span>
      <small>Compare with the fingerprint printed in {invite.received.name}'s terminal. Admit only if they match.</small>
    </span>
    <span class="act">
      <button class="lnk d" onclick={cancel} disabled={busy}>They differ — cancel</button>
      <button class="btn p" onclick={admit} disabled={busy}>Fingerprints match, admit</button>
    </span>
  </div>
  {#if error}
    <div class="banner crit">could not admit <b>· {error}</b></div>
  {/if}
{:else if invite.state === 'admitted' && invite.received}
  <div class="done">
    <span class="bar"></span>
    <span class="t">
      <em>Enrolled</em>
      {invite.received.name}
      <small>{invite.channels.join(', ')} handed off · run <code>tracon setup</code> then <code>tracon serve</code> on it</small>
    </span>
  </div>
  <div><a class="lnk" href="/nodes">Back to Nodes</a></div>
{/if}

<style>
  .steps {
    display: flex;
    gap: 14px;
    font: 11.5px var(--mono);
    color: var(--dim);
    letter-spacing: 0.04em;
  }
  .steps .cur {
    color: var(--ink);
  }
  .form {
    display: grid;
    gap: 14px;
    max-width: 640px;
  }
  .runs {
    display: grid;
    gap: 5px;
  }
  .runs > span {
    font: 500 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink2);
  }
  .pick {
    display: flex;
    flex-wrap: wrap;
    gap: 8px 16px;
  }
  .pick label {
    display: inline-flex;
    gap: 7px;
    align-items: center;
    font: 13px var(--sans);
  }
  .pick input {
    accent-color: var(--acc);
    margin: 0;
  }
  .pick small,
  .runs > small {
    font-size: 12.5px;
    color: var(--dim);
  }
  /* The code card: the thing to act on, so the accent bar, not a state. */
  .code {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr);
    gap: 0 18px;
    background: var(--s1);
    border-radius: 0 4px 4px 0;
    border-left: 3px solid var(--acc);
    padding: 14px 16px;
    max-width: 640px;
    overflow: hidden;
  }
  .qr {
    width: 132px;
    height: 132px;
    background: #fff;
    padding: 6px;
    border-radius: 3px;
  }
  .qr :global(svg) {
    width: 100%;
    height: 100%;
    display: block;
  }
  .kv {
    display: grid;
    gap: 8px;
    align-content: start;
    min-width: 0;
  }
  .k {
    font: 500 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--dim);
  }
  .v {
    font: 13px var(--mono);
    color: var(--ink);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .big {
    font: 500 22px var(--mono);
    letter-spacing: 0.18em;
    color: var(--ink);
  }
  .fp {
    font: 13px var(--mono);
    letter-spacing: 0.08em;
    color: var(--ink2);
  }
  .ttl {
    font: 12px var(--mono);
    color: var(--wait);
  }
  .recv,
  .done {
    display: grid;
    grid-template-columns: 3px minmax(0, 1fr) auto;
    gap: 0 14px;
    align-items: center;
    border-radius: 4px;
    padding: 12px 14px 12px 0;
    overflow: hidden;
    max-width: 640px;
  }
  .recv {
    background: linear-gradient(90deg, var(--wash-wait), var(--s1) 42%);
  }
  .recv .bar {
    background: var(--wait);
  }
  .done {
    grid-template-columns: 3px minmax(0, 1fr);
    background: linear-gradient(90deg, var(--wash-ok), var(--s1) 42%);
  }
  .done .bar {
    background: var(--ok);
  }
  .bar {
    align-self: stretch;
    border-radius: 2px 0 0 2px;
  }
  .t {
    font-weight: 500;
    min-width: 0;
  }
  .t em {
    font-style: normal;
    font-weight: 400;
  }
  .recv .t em {
    color: var(--wait);
  }
  .done .t em {
    color: var(--ok);
  }
  .t small {
    display: block;
    font: 12px var(--mono);
    color: var(--dim);
    margin-top: 2px;
    white-space: normal;
  }
  .t .fp {
    font: 500 15px var(--mono);
    letter-spacing: 0.12em;
    color: var(--ink);
    display: block;
    margin-top: 6px;
  }
  .act {
    display: flex;
    gap: 12px;
    align-items: center;
    white-space: nowrap;
  }
  @media (max-width: 700px) {
    .code {
      grid-template-columns: minmax(0, 1fr);
    }
    .recv {
      grid-template-columns: 3px minmax(0, 1fr);
    }
    .act {
      grid-column: 2;
      justify-content: flex-end;
      margin-top: 8px;
    }
  }
</style>
