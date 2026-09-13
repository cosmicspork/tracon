<script lang="ts">
  // Enrollment has one live home: administrator-gated Mesh administration in
  // Settings. This route explains the trust and data boundary without leaving
  // a second live invitation flow behind.
  import { store } from '../lib/store.svelte'

  const meshReady = $derived(store.mesh !== null && store.mesh.hub.state !== 'disabled')
  const local = $derived(store.node?.loopback ?? false)
</script>

<div class="h4"><a class="lnk" href="/nodes">‹ Nodes</a> Enroll a node</div>
<p class="lede">
  Add a peer to the mesh without granting it live access to this machine. A node always keeps its own host configuration, provider logins, browser subscriptions, and running sessions.
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
      This route intentionally does not create or keep a live invitation. The canonical Mesh administration panel requires an explicit administrator session on the serving node before it can issue, cancel, or admit a one-time invitation.
    </p>
    <a class="btn p" href="/settings#mesh">Open Mesh administration</a>
    {#if !local}
      <p class="note">You reached the serving node remotely. Open Mesh administration on that node to create or admit an invitation.</p>
    {/if}
  </section>

  <section class="enrollment">
    <div class="h5">Choose exactly what the new node can read</div>
    <p>
      <code>@mesh</code> is the required system coordination channel: it lets members identify and coordinate with one another. Select a work channel only when this peer should receive that channel’s shared work and data key.
    </p>
    <p class="note">
      An invitation never transfers a provider token, operator token, browser subscription, host credential, checkout, or running session. A peer needs its own local setup and provider connection before it can run work.
    </p>
  </section>

  <section class="enrollment">
    <div class="h5">Verify trust before admitting the peer</div>
    <p>
      Compare the joining node’s fingerprint with the fingerprint it prints through a separate trusted path. Admit only an exact match. Cancelling a mismatch leaves no persistent invitation or live connection.
    </p>
    <p class="note">
      Once admitted, the node is a connected mesh peer with only <code>@mesh</code> and the selected channel keys. It manages its own compatibility, providers, and readiness; view that operational state on <a href="/nodes">Nodes</a>.
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
