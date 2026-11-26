// The forge text a review's approval sends, as the operator edited it against
// what the agent asked: which fields differ, and what Clear edits puts back.

import type { ReviewOutputs } from './types'

/** The forge fields the review page edits, and the toggles that say which are sent. */
export interface ForgeFields {
  title: string
  body: string
  describe: boolean
  descTitle: string
  descBody: string
  commenting: boolean
  comment: string
}

/**
 * The agent's forge fields: the review's own title and body, and the
 * description and comment its intent asks for. What the page loads before a
 * draft is put over it, and what Clear edits restores.
 */
export function agentForge(review: { title: string; body: string }, forge: ReviewOutputs): ForgeFields {
  return {
    title: review.title,
    body: review.body,
    describe: forge.description !== undefined,
    descTitle: forge.description?.title ?? '',
    descBody: forge.description?.body ?? '',
    commenting: forge.comment !== undefined,
    comment: forge.comment ?? '',
  }
}

/** One field that differs from the agent's text, as it reads before and after. */
export interface ForgeEdit {
  field: 'title' | 'body' | 'description title' | 'description' | 'comment'
  before: string
  after: string
  /** Sent where the agent asked for none, or the reverse, with no text either side. */
  turned?: 'on' | 'off'
}

/**
 * The fields that differ from the agent's, in the order the page shows them.
 * A description or comment counts as what is sent: turned off it reads empty,
 * so turning one off is an edit and text typed under an unticked box is not.
 */
export function forgeEdits(now: ForgeFields, agent: ForgeFields): ForgeEdit[] {
  const sent = (on: boolean, text: string) => (on ? text : '')
  const pairs: [ForgeEdit['field'], string, string][] = [
    ['title', agent.title, now.title],
    ['body', agent.body, now.body],
    ['description title', sent(agent.describe, agent.descTitle), sent(now.describe, now.descTitle)],
    ['description', sent(agent.describe, agent.descBody), sent(now.describe, now.descBody)],
    ['comment', sent(agent.commenting, agent.comment), sent(now.commenting, now.comment)],
  ]
  const edits: ForgeEdit[] = []
  for (const [field, before, after] of pairs) if (before !== after) edits.push({ field, before, after })
  // A description or comment turned on or off with the same text is still a change.
  const toggled = (field: ForgeEdit['field'], was: boolean, is: boolean) =>
    was !== is && !edits.some((e) => e.field.startsWith(field))
  if (toggled('description', agent.describe, now.describe)) {
    edits.push({ field: 'description', before: '', after: '', turned: now.describe ? 'on' : 'off' })
  }
  if (toggled('comment', agent.commenting, now.commenting)) {
    edits.push({ field: 'comment', before: '', after: '', turned: now.commenting ? 'on' : 'off' })
  }
  return edits
}

/** "title", "title and body", "title, body and comment": the fields an edit list names. */
export function fieldList(edits: ForgeEdit[]): string {
  const names = edits.map((e) => e.field)
  if (names.length <= 1) return names.join('')
  return `${names.slice(0, -1).join(', ')} and ${names[names.length - 1]}`
}
