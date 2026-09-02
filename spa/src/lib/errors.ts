// A session that failed says why in the operator's language. What the node
// stores is the harness transport's own words — `rpc: rpc error -32603:
// Internal error (Unknown tool …)` names a JSON-RPC code and a wrapper, which
// is a diagnosis for whoever wrote the adapter and noise for everyone else.
// The raw text is never lost: it stays in the session's log.

const LITERALS: Record<string, string> = {
  'node restarted while the session was live': 'the node restarted while this session was live',
  'lost on owner': 'the node that owned this session lost it',
}

const PATTERNS: [RegExp, string][] = [
  [/^Internal error \(Unknown tool[^)]*\)$/i, 'the harness called a tool this node does not offer'],
  [/^Internal error\b/i, 'the harness hit an internal error'],
  [/^Method not found\b/i, 'the harness asked for something this node does not implement'],
]

/** What a failed session's stored error says to the person reading it. */
export function humanizeError(raw: string | null | undefined): string | null {
  if (!raw) return null
  const text = raw.trim()
  if (text === '') return null
  if (LITERALS[text]) return LITERALS[text]
  // `rpc: rpc error <code>: <message>` — two nested transport wrappers whose
  // only readable part is the message they carry.
  const inner = text.replace(/^rpc:\s*rpc error\s+-?\d+:\s*/i, '').trim()
  for (const [re, said] of PATTERNS) {
    if (re.test(inner)) return said
  }
  if (LITERALS[inner]) return LITERALS[inner]
  return inner === '' ? text : inner
}
