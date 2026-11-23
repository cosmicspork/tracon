// What an approval publishes to the forge as text: the change's title and
// description and the comment, exactly as they will be sent. The node
// composes it with the function the approval itself uses
// (`POST /api/reviews/{id}/outgoing`); `localOutgoing` stands in only where
// the node cannot answer for it — a review another node holds — and follows
// the node's rules (`Outputs::approved`, `compose_generated_details`).
import type { ReviewOutputs } from './types'
import { composeGeneratedDetails } from './verdicts'

/** Why the node refuses an approval that sends a description without an opener. */
export const OPENER_MISSING = 'write an opener above the generated details'

/** The approval as the screen would send it now. */
export interface OutgoingAsked {
  title?: string
  body?: string
  outputs?: ReviewOutputs
  opener?: string
  revision_id?: string
}

/** What the forge would get. */
export interface Outgoing {
  /** The title and body a new change opens with or an open one is given; null when none is sent. */
  description: { title: string; body: string } | null
  comment: string | null
  /** Why the approval would be refused as written; `description` is then null. */
  refusal: string | null
}

export interface LocalInputs {
  title: string
  body: string
  /** What approval sends besides the commits: the edited outputs, or for a mirrored review the revision's own. */
  forge: ReviewOutputs
  /** The open change it updates, if it updates one. */
  change: boolean
  /** A repository's first push, or a push straight to the base: nothing is opened. */
  opensNone: boolean
  /** `[review] generated_details` on the node that publishes. */
  generatedDetails: boolean
  opener: string
}

/** The outgoing text composed here, by the node's rules. */
export function localOutgoing(i: LocalInputs): Outgoing {
  if (i.opensNone) return { description: null, comment: null, refusal: null }
  const comment = i.forge.comment ?? null
  let description = i.forge.description ?? (i.change ? null : { title: i.title, body: i.body })
  if (description && i.generatedDetails) {
    const body = composeGeneratedDetails(i.opener, description.body)
    if (body === null) return { description: null, comment, refusal: OPENER_MISSING }
    description = { title: description.title, body }
  }
  return { description, comment, refusal: null }
}

/** Whether two answers say the same, so a reply that changes nothing is not shown as news. */
export function sameOutgoing(a: Outgoing | null, b: Outgoing | null): boolean {
  return JSON.stringify(a) === JSON.stringify(b)
}
