<script lang="ts">
  // What a window waiting on a device approval shows: the code to compare,
  // and the ways to reach a browser that can approve it. The link is a plain
  // `_blank` anchor, so the tap that follows it is the user gesture, and the
  // desktop app hands it to the system browser.
  import type { DeviceStart } from '../lib/device'

  let { start, desktop }: { start: DeviceStart; desktop: boolean } = $props()
</script>

<div class="device" data-testid="device-code">
  <p>
    {#if desktop}
      Open the link in a browser signed in to this node, or scan the code with your phone, and
      approve with your passkey there.
    {:else}
      Open the link on a device signed in to this node with a passkey, such as your phone (scan the
      code), and approve there.
    {/if}
  </p>
  <div class="code" aria-label="Code to compare">
    <span>Code</span>
    <b data-testid="device-user-code">{start.user_code}</b>
  </div>
  <p class="hint">The browser that approves shows a code too. Approve only if the two match.</p>
  <a
    class="btn p"
    href={start.approve_url}
    target="_blank"
    rel="noopener noreferrer"
    data-testid="device-open"
  >
    {desktop ? 'Open in browser' : 'Open the link'}
  </a>
  {#if start.approve_qr}
    <!-- Made by the node from the approval address alone. -->
    <div class="qr">{@html start.approve_qr}</div>
  {/if}
  <code class="url">{start.approve_url}</code>
  <p class="wait">Waiting for approval…</p>
</div>

<style>
  .device {
    display: grid;
    gap: 10px;
  }
  p {
    margin: 0;
    color: var(--ink2);
    font-size: 13.5px;
    line-height: 1.5;
  }
  .code {
    display: grid;
    gap: 2px;
    justify-items: center;
    background: var(--s2);
    border-radius: 6px;
    padding: 10px;
  }
  .code span {
    color: var(--dim);
    font: 12px var(--sans);
  }
  .code b {
    font: 600 24px var(--mono);
    letter-spacing: 0.12em;
    color: var(--ink);
  }
  .hint,
  .wait {
    font-size: 12.5px;
    color: var(--dim);
  }
  a.btn {
    justify-self: start;
    text-decoration: none;
  }
  .qr {
    justify-self: center;
    width: min(180px, 60vw);
    background: #fff;
    border-radius: 4px;
    padding: 6px;
  }
  .qr :global(svg) {
    display: block;
    width: 100%;
    height: auto;
  }
  .url {
    font: 11.5px var(--mono);
    color: var(--ink2);
    overflow-wrap: anywhere;
  }
</style>
