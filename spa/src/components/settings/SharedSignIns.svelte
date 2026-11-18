<script lang="ts">
  // Which node renews each subscription sign-in several nodes hold. A
  // refresh revokes every other copy, so one node renews each version; the
  // one set to renew (`[mesh] renew_credentials`), or else the only one on
  // an always-on runtime, goes first. When none does, every holder waits
  // for the hub's claim, and this offers to set one: the same node.toml
  // edit Configuration makes, here or on the peer (which must name this
  // node in `[mesh] admin_peers`).
  import Card from './Card.svelte'
  import { api, ApiError } from '../../lib/api'
  import { providerLabel, sharedSignIns } from '../../lib/providers'
  import { store } from '../../lib/store.svelte'
  import type { ConfigPatchResult } from '../../lib/types'

  const shared = $derived(sharedSignIns(store.nodes))
  let busy = $state('')
  let done = $state<Record<string, string>>({})
  let failed = $state<Record<string, string>>({})

  const nodeName = (id: string) => store.nodes.find((node) => node.id === id)?.name || id.slice(0, 8)
  const isSelf = (id: string) => store.nodes.some((node) => node.id === id && node.is_self)

  async function renewFrom(key: string, nodeId: string) {
    busy = key
    failed = { ...failed, [key]: '' }
    const ops = [{ op: 'set' as const, key: 'mesh.renew_credentials', value: true }]
    try {
      let result: ConfigPatchResult
      if (isSelf(nodeId)) {
        const cfg = await api.config()
        result = await api.patchConfig(cfg.hash, ops)
      } else {
        const cfg = await api.nodeConfig(nodeId)
        result = await api.patchNodeConfig(nodeId, cfg.hash, ops)
      }
      done = {
        ...done,
        [key]: `${nodeName(nodeId)} is set to renew${result.restart_required ? '; restart it for this to take effect' : ''}.`,
      }
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause)
      failed = {
        ...failed,
        [key]:
          cause instanceof ApiError && cause.kind === 'not_admin_peer'
            ? `${nodeName(nodeId)} does not let this node change its settings: set [mesh] renew_credentials = true there, or add this node to its [mesh] admin_peers. ${message}`
            : message,
      }
    } finally {
      busy = ''
    }
  }
</script>

{#if shared.length}
  <Card
    title="Shared sign-ins"
    note="A renewal revokes every other copy of a sign-in, so one node renews each version and hands the new copy to the rest. The node set to renew goes first; the others step in only with the hub's claim."
  >
    <div class="rows">
      {#each shared as signIn (`${signIn.provider}/${signIn.credential}/${signIn.identity ?? ''}`)}
        {@const key = `${signIn.provider}/${signIn.credential}`}
        <div class="row">
          <span class="nm">
            {providerLabel(signIn.provider)}{#if signIn.identity}{' · '}{signIn.identity}{/if}
            <small>held by {signIn.holders.map(nodeName).join(', ')}</small>
          </span>
          {#if signIn.renewer}
            <span class="l">Renewed first by <b>{nodeName(signIn.renewer)}</b>.</span>
          {:else}
            <span class="l warn">No node renews this first: every holder waits for the hub's claim, and none renews while the hub is unreachable.</span>
            <span class="acts">
              {#each signIn.holders as holder (holder)}
                <button class="lnk" disabled={busy !== ''} onclick={() => void renewFrom(key, holder)}>
                  {busy === key ? 'Saving…' : `Renew from ${nodeName(holder)}`}
                </button>
              {/each}
            </span>
          {/if}
          {#if done[key]}<small class="good">{done[key]}</small>{/if}
          {#if failed[key]}<small class="bad">{failed[key]}</small>{/if}
        </div>
      {/each}
    </div>
  </Card>
{/if}

<style>
  .rows {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .row {
    display: flex;
    flex-direction: column;
    gap: 4px;
    background: var(--s2);
    border-radius: 4px;
    padding: 10px 12px;
  }
  .nm small {
    color: var(--ink2);
    margin-left: 6px;
  }
  .l.warn {
    color: var(--wait);
  }
  .acts {
    display: flex;
    flex-wrap: wrap;
    gap: 12px;
  }
  .good {
    color: var(--ok);
  }
  .bad {
    color: var(--crit);
  }
</style>
