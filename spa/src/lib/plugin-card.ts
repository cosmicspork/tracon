// A card a plugin raised through the node (`approval/request`): its own
// account of what it will do, in formats the node renders. Allowing it runs
// nothing on the node; the plugin acts on the decision.

export const PLUGIN_CARD_TOOL = 'plugin_card'

export type PluginBlock =
  | { format: 'markdown'; title?: string; text: string }
  | { format: 'diff'; title?: string; text: string }
  | { format: 'source'; title?: string; text: string; language?: string }
  /** A text before and after; the page diffs it by word. */
  | { format: 'word_diff'; title?: string; before: string; after: string }
  | { format: 'table'; title?: string; rows: { key: string; value: string }[] }

export interface PluginCard {
  plugin: string
  version: string | null
  title: string
  summary: string | null
  content: PluginBlock[]
  /** The agent whose call the card was raised for, as the node checked it. */
  requestedFor: { session: string | null; harness: string | null } | null
}

function str(v: unknown): string | null {
  return typeof v === 'string' ? v : null
}

function block(v: unknown): PluginBlock | null {
  if (!v || typeof v !== 'object') return null
  const b = v as Record<string, unknown>
  const title = str(b.title) ?? undefined
  switch (b.format) {
    case 'markdown':
    case 'diff':
      return typeof b.text === 'string' ? { format: b.format, title, text: b.text } : null
    case 'source':
      return typeof b.text === 'string'
        ? { format: 'source', title, text: b.text, language: str(b.language) ?? undefined }
        : null
    case 'word_diff':
      return typeof b.before === 'string' && typeof b.after === 'string'
        ? { format: 'word_diff', title, before: b.before, after: b.after }
        : null
    case 'table':
      if (!Array.isArray(b.rows)) return null
      return {
        format: 'table',
        title,
        rows: b.rows
          .filter((r): r is Record<string, unknown> => !!r && typeof r === 'object')
          .map((r) => ({ key: String(r.key ?? ''), value: String(r.value ?? '') })),
      }
    default:
      return null
  }
}

/** The card a plugin raised, or null for any other tool. A block the page
 *  cannot read is shown as its source rather than dropped. */
export function pluginCard(tool: string, args: unknown): PluginCard | null {
  if (tool !== PLUGIN_CARD_TOOL || !args || typeof args !== 'object') return null
  const a = args as Record<string, unknown>
  const content = Array.isArray(a.content)
    ? a.content.map(
        (b) => block(b) ?? ({ format: 'source', text: JSON.stringify(b, null, 2), language: 'json' } as PluginBlock),
      )
    : []
  const r = a.requested_for as Record<string, unknown> | null | undefined
  return {
    plugin: str(a.plugin) ?? 'plugin',
    version: str(a.version),
    title: str(a.title) ?? '',
    summary: str(a.summary),
    content,
    requestedFor: r && typeof r === 'object' ? { session: str(r.session), harness: str(r.harness) } : null,
  }
}

/** Who the page says asked: the plugin, and the agent it asked for. */
export function askedBy(card: PluginCard): string {
  const who = `plugin ${card.plugin}${card.version ? ` ${card.version}` : ''}`
  if (!card.requestedFor) return who
  const agent = card.requestedFor.harness ?? 'an agent'
  return `${who}, for ${agent}${card.requestedFor.session ? '' : ' (external)'}`
}
