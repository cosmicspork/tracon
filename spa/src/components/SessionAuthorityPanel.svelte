<script lang="ts">
  // What this session may do, in task terms: where it runs, what it can reach,
  // what runs unattended, what it must still ask about, and what is refused
  // outright. The node assembles it by running its own policy and grants, so
  // this renders an answer rather than deriving a second one — a panel that
  // computed its own verdicts would eventually disagree with the gate, and the
  // operator would have no way to tell which of the two was lying.
  import { api } from '../lib/api'
  import { formatDuration, formatTokens } from '../lib/format'
  import type { ActionStanding, SessionAuthority } from '../lib/types'

  let { id, open = false }: { id: string; open?: boolean } = $props()

  // The prop opens the panel; it does not hold it open. Null means the
  // operator has not touched it, so the anchor decides; once they have, their
  // choice does, even while the address still carries the anchor.
  let toggled = $state<boolean | null>(null)
  const expanded = $derived(toggled ?? open)
  let view = $state<SessionAuthority | null>(null)
  let error = $state<string | null>(null)
  let loaded = $state(false)

  async function load() {
    if (loaded) return
    loaded = true
    try {
      view = await api.sessionAuthority(id)
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
      loaded = false
    }
  }
  // Fetched when the panel is opened, not on every session render: it is a
  // question the operator asks, and asking it of every screen would cost a
  // policy read and a broker read per session row.
  $effect(() => {
    if (expanded) void load()
  })

  const byVerdict = (verdict: ActionStanding['verdict']) =>
    (view?.actions ?? []).filter((a) => a.verdict === verdict)
  const allowed = $derived(byVerdict('allow'))
  const asked = $derived(byVerdict('ask'))
  const refused = $derived(byVerdict('deny'))

  function scopeNote(action: ActionStanding): string | null {
    if (!action.scoped.length) return null
    return action.scoped
      .map((s) => Object.entries(s.args).map(([key, globs]) => `${key} ${globs.join(', ')}`).join('; '))
      .join(' · ')
  }

  function grantNote(action: ActionStanding): string | null {
    const grants = action.grants ?? []
    if (!grants.length) return null
    return grants.map((g) => `${g.verdict} ${g.target}`).join(' · ')
  }
</script>

<details id="authority" class="authority" open={expanded} ontoggle={(e) => (toggled = e.currentTarget.open)}>
  <summary>What this session may do</summary>
  {#if error}
    <p class="err">{error}</p>
  {:else if !view}
    <p class="dim">Reading the node's policy and grants…</p>
  {:else}
    <dl>
      <dt>Runs on</dt>
      <dd>
        {view.node.name ?? view.node.id.slice(0, 8)}{view.node.is_self ? ' (this node)' : ''} ·
        isolation {view.node.isolation ?? 'unknown'}{view.node.failed_check
          ? ` · ${view.node.failed_check}${view.node.failed_detail ? `: ${view.node.failed_detail}` : ''}`
          : ''}
      </dd>
      <dt>Harness</dt>
      <dd>
        {view.harness.id}
        {view.harness.found ?? view.harness.expected}{view.harness.found &&
        view.harness.found !== view.harness.expected
          ? ` · node expects ${view.harness.expected}`
          : ''}{view.harness.protocol ? ` · protocol ${view.harness.protocol}` : ''}
      </dd>
      <dt>Image</dt>
      <dd><code>{view.image.execution}</code> · {view.image.runtime}</dd>
      <dt>Can reach</dt>
      <dd>
        <code>{view.access.workspace}</code> on <code>{view.access.branch}</code>
        {#if view.access.egress.length}
          · egress: {view.access.egress.join(', ')}
        {:else}
          · no egress hosts allowed
        {/if}
        {#if view.access.credentials.length}
          · credentials: {view.access.credentials.join(', ')}
        {:else}
          · no credential bound to {view.channel}
        {/if}
      </dd>
      <dt>Limits</dt>
      <dd>
        {formatTokens(view.limits.tokens_used)} of
        {view.limits.budget_tokens > 0 ? formatTokens(view.limits.budget_tokens) : '∞'} tokens ·
        unanswered requests are denied after {formatDuration(view.limits.permission_timeout_secs * 1000)}
        {#if view.limits.ceiling.ceiling}
          · {view.channel} daily ceiling {formatTokens(view.limits.ceiling.usage_today)}/{formatTokens(
            view.limits.ceiling.ceiling,
          )}
        {/if}
      </dd>
    </dl>

    {#if !view.policy.trusted}
      <p class="warn">
        The signed bundle is unavailable or invalid, so no rule allows anything and every request is
        asked. That is the fail-closed path, not a policy that happens to be strict.
      </p>
    {/if}

    <h4>Runs without asking <b>{allowed.length + view.unattended_commands.length}</b></h4>
    <ul>
      {#each view.unattended_commands as rule (rule.rule_id)}
        <li>
          <code>{rule.commands.join(' · ')}</code>
          <small>{rule.reason} <em>{rule.rule_id}</em></small>
        </li>
      {/each}
      {#each allowed as action (action.name)}
        <li>
          <code>{action.name}</code>
          <small>{action.reason ?? 'allowed'} <em>{action.rule_id ?? ''}</em></small>
        </li>
      {/each}
    </ul>

    <h4>Still asks you <b>{asked.length}</b></h4>
    <ul>
      {#each asked as action (action.name)}
        <li>
          <code>{action.name}</code>
          <small>
            {#if grantNote(action)}
              a grant covers {grantNote(action)}; anything else is asked
            {:else if scopeNote(action)}
              unattended only for {scopeNote(action)}; anything else is asked
            {:else}
              no rule covers it, so it is yours to decide
            {/if}
          </small>
        </li>
      {/each}
    </ul>

    {#if refused.length}
      <h4>Refused outright <b>{refused.length}</b></h4>
      <ul>
        {#each refused as action (action.name)}
          <li>
            <code>{action.name}</code>
            <small>{action.reason ?? 'denied'} <em>{action.rule_id ?? ''}</em></small>
          </li>
        {/each}
      </ul>
    {/if}

    <p class="dim">
      Policy v{view.policy.version} · {view.policy.rules} rules · {view.grants.length} live grant{view
        .grants.length === 1
        ? ''
        : 's'} · <a href="/settings#policies">change what is asked</a>
    </p>
  {/if}
</details>

<style>
  .authority {
    background: var(--s1);
    border-radius: 4px;
    padding: 10px 14px;
    margin-top: 10px;
  }
  summary {
    cursor: pointer;
    font: 12px var(--mono);
    color: var(--dim);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
  dl {
    display: grid;
    grid-template-columns: max-content minmax(0, 1fr);
    gap: 4px 14px;
    margin: 10px 0 0;
  }
  dt {
    font: 11.5px var(--mono);
    color: var(--dim);
    text-align: right;
  }
  dd {
    margin: 0;
    min-width: 0;
    font-size: 13px;
    overflow-wrap: anywhere;
  }
  h4 {
    margin: 14px 0 4px;
    font: 11.5px var(--mono);
    color: var(--dim);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
  h4 b {
    color: var(--ink);
    font-weight: 500;
  }
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }
  li {
    display: grid;
    grid-template-columns: max-content minmax(0, 1fr);
    gap: 10px;
    align-items: baseline;
  }
  li code {
    font-size: 12px;
  }
  li small {
    color: var(--dim);
    font-size: 11.5px;
    overflow-wrap: anywhere;
  }
  li small em {
    font-style: normal;
    opacity: 0.6;
  }
  .warn {
    color: var(--wait);
    font-size: 12.5px;
  }
  .err {
    color: var(--red);
    font-size: 12.5px;
  }
  .dim {
    color: var(--dim);
    font-size: 12px;
  }
  @media (max-width: 700px) {
    dl,
    li {
      grid-template-columns: minmax(0, 1fr);
    }
    dt {
      text-align: left;
    }
  }
</style>
