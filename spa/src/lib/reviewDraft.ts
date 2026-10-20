// What the operator has written on a review and not sent, kept on the node so
// it survives a reload, a reconnect and a change of device. Each save is made
// on top of the version this screen last saw; when another device saved first
// the node refuses it, and this says so rather than writing over the other.

import { api, ReviewDraftConflict } from './api'
import type { ReviewDraft, ReviewDraftFields } from './types'

/**
 * `clean`: nothing unsaved. `saving`. `saved`: the node holds what is on the
 * screen. `conflict`: another device saved first; nothing more is saved until
 * the operator chooses. `unsaved`: the node could not be reached; the next
 * edit tries again.
 */
export type DraftState = 'clean' | 'saving' | 'saved' | 'conflict' | 'unsaved'

/** A held draft as it applies to the revision on screen. */
export interface DraftForRevision {
  /** What to put back on the screen. */
  fields: ReviewDraftFields
  /** The draft was written against an earlier revision. */
  fromEarlier: boolean
  /**
   * The draft's squash message, written against an earlier revision and
   * different from what this one proposes. It is not applied: the operator
   * chooses between it and the proposal, as between two devices' drafts.
   */
  staleMessage: string | null
}

/**
 * Put back what the operator left on a review, except a squash message
 * written against an earlier revision: this revision's proposal, which may be
 * the agent's new `message`, is not silently replaced by it (#592).
 */
export function draftForRevision(held: ReviewDraft, revisionId: string | undefined, proposed: string): DraftForRevision {
  const fromEarlier = !!held.revision_id && held.revision_id !== revisionId
  const { message, ...rest } = held.draft
  if (!fromEarlier || message === undefined || message.trim() === proposed.trim()) {
    return { fields: held.draft, fromEarlier, staleMessage: null }
  }
  return { fields: rest, fromEarlier, staleMessage: message }
}

/**
 * The squash message as a draft keeps it: only when the operator changed it
 * from the proposal. An untouched one is left out, so a later revision's new
 * proposal is not met by a "drafted against an earlier revision" message
 * nobody wrote.
 */
export function draftedMessage(message: string, proposed: string): string | undefined {
  return message.trim() !== proposed ? message : undefined
}

type Save = typeof api.saveReviewDraft

export class ReviewDraftSync {
  private version = 0
  private sent = ''
  private timer: ReturnType<typeof setTimeout> | null = null
  private stopped = false
  /** What the other device saved, while in `conflict`. */
  conflict: ReviewDraft | null = null
  state: DraftState = 'clean'

  constructor(
    private reviewId: string,
    private revisionId: string | undefined,
    private onState: (state: DraftState) => void,
    private save: Save = api.saveReviewDraft,
    private delayMs = 600,
  ) {}

  /** The node's draft, as loaded, and the screen after it was applied. */
  loaded(draft: ReviewDraft | null, fields: ReviewDraftFields) {
    this.version = draft?.version ?? 0
    this.sent = JSON.stringify(fields)
    this.set(draft ? 'saved' : 'clean')
  }

  /** The screen changed. Saved after a pause, unless it is what was sent. */
  edit(fields: ReviewDraftFields) {
    if (this.stopped || this.state === 'conflict') return
    const key = JSON.stringify(fields)
    if (key === this.sent) return
    if (this.timer) clearTimeout(this.timer)
    this.timer = setTimeout(() => void this.flush(fields), this.delayMs)
  }

  async flush(fields: ReviewDraftFields) {
    if (this.timer) clearTimeout(this.timer)
    this.timer = null
    if (this.stopped) return
    const key = JSON.stringify(fields)
    this.set('saving')
    try {
      const saved = await this.save(this.reviewId, {
        base_version: this.version,
        revision_id: this.revisionId,
        draft: fields,
      })
      this.version = saved.version
      this.sent = key
      this.set('saved')
    } catch (e) {
      if (e instanceof ReviewDraftConflict) {
        this.conflict = e.current
        this.set('conflict')
      } else {
        this.set('unsaved')
      }
    }
  }

  /** Keep what this screen has, over what the other device saved. */
  async keepMine(fields: ReviewDraftFields) {
    this.version = this.conflict?.version ?? 0
    this.conflict = null
    this.set('saving')
    await this.flush(fields)
  }

  /** Take what the other device saved; the caller puts it on the screen. */
  takeTheirs(): ReviewDraftFields {
    const theirs = this.conflict
    this.version = theirs?.version ?? 0
    this.sent = JSON.stringify(theirs?.draft ?? {})
    this.conflict = null
    this.set(theirs ? 'saved' : 'clean')
    return theirs?.draft ?? {}
  }

  /** A verdict is going out: nothing pending may recreate the draft after it. */
  stop() {
    this.stopped = true
    if (this.timer) clearTimeout(this.timer)
    this.timer = null
  }

  /** The verdict did not go out; what is written is a draft again. */
  resume() {
    this.stopped = false
  }

  private set(state: DraftState) {
    this.state = state
    this.onState(state)
  }
}
