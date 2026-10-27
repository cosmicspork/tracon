<script lang="ts">
  // Enrollment has one live home: administrator-gated Mesh administration in
  // Settings. This route explains the trust and data boundary without leaving
  // a second live invitation flow behind.
  import { store } from '../lib/store.svelte'

  const meshReady = $derived(store.mesh !== null && store.mesh.hub.state !== 'disabled')
  const local = $derived(store.node?.loopback ?? false)
</script>

<div class="h4"><a class="lnk" href="/settings#mesh">‹ Mesh</a> Enroll a node</div>
<p class="lede">
  Add a peer to the mesh. It keeps its own configuration, logins and sessions.
</p>

<div class="steps" aria-label="Enrollment steps">
  <span>1 prepare scoped invitation</span>
  <span>2 verify fingerprints</span>
  <span>3 admit connected peer</span>
</div>

{#if !meshReady}
  <div class="banner crit">
    No mesh hub is configured. Set up the serving node’s hub in <a href="/settings#mesh">Settings › Mesh</a> before enrolling another node.
  </div>
{:else}
  <section class="enrollment">
    <div class="h5">Enrollment is managed in Settings › Mesh</div>
    <p>
      Invitations are issued from Mesh administration on the serving node.
    </p>
    <a class="btn p" href="/settings#mesh">Open Mesh administration</a>
    {#if !local}
      <p class="note">Remote: open Mesh administration on the node itself.</p>
    {/if}
  </section>

  <section class="enrollment">
    <div class="h5">Choose exactly what the new node can read</div>
    <p>
      <code>@mesh</code> is always included. Add a work channel only to share its work and key.
    </p>
    <p class="note">
      No tokens, credentials or sessions are transferred. The peer needs its own provider to run work.
    </p>
  </section>

  <section class="enrollment">
    <div class="h5">Verify trust before admitting the peer</div>
    <p>
      Compare fingerprints over a trusted path. Admit only an exact match.
    </p>
    <p class="note">
      Once admitted, the node is a connected mesh peer with only <code>@mesh</code> and the selected channel keys. See its state in <a href="/settings#mesh">Settings › Mesh</a>.
    </p>
  </section>
{/if}

<style>
  .lede {
    max-width: 66ch;
    margin: 0 0 10px;
    color: var(--ink2);
    font-size: 13.5px;
  }
  .steps {
    display: flex;
    flex-wrap: wrap;
    gap: 6px 14px;
    margin-bottom: 12px;
    color: var(--dim);
    font: 11.5px var(--mono);
  }
  .enrollment {
    display: grid;
    gap: 9px;
    max-width: 680px;
    padding: 14px 16px;
    background: var(--s1);
    border-radius: 4px;
  }
  .h5 {
    color: var(--ink);
    font: 500 13px var(--sans);
  }
  p {
    max-width: 66ch;
    margin: 0;
    color: var(--ink2);
    font-size: 13.5px;
  }
  .note {
    color: var(--dim);
    font: 12px var(--mono);
  }
  code {
    color: var(--ink);
  }
  .btn {
    justify-self: start;
  }
</style>
