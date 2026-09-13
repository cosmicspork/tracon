// The composer's unsent text lives on the node, not in this tab. What is left
// here is the timing: when a keystroke becomes a save, when a late fetch may
// still fill an empty box, and when a pending save must be dropped because the
// prompt has already gone and the node has cleared the draft itself.
//
// Kept out of the component so the rules can be read and tested on their own —
// the failure they prevent (a save landing after a send and resurrecting text
// the operator already sent) is a race, and races are not visible in markup.

/** Quiet time before typing becomes a save. Long enough not to write on every
 *  keystroke, short enough that a closed tab loses at most a word. */
export const DRAFT_DEBOUNCE_MS = 500

export interface DraftTimers {
  set: (fn: () => void, ms: number) => unknown
  clear: (handle: unknown) => void
}

const realTimers: DraftTimers = {
  set: (fn, ms) => setTimeout(fn, ms),
  clear: (handle) => clearTimeout(handle as ReturnType<typeof setTimeout>),
}

export interface DraftBox {
  /** The operator typed. Claims the box and schedules a save. */
  typed(text: string): void
  /** The node's copy, offered to a box that may not have been touched yet.
   *  Returns the text to restore, or `null` to leave the box alone: whoever
   *  is typing outranks a fetch that started before they did. */
  restore(stored: string | null | undefined): string | null
  /** The prompt was sent. Any pending save is dropped: sending clears the
   *  draft on the node, and a save landing after it would put the sent text
   *  back into the box. */
  sent(): void
  /** Nothing is owed to the node. */
  idle(): boolean
  /** Give up any pending save without sending — leaving the screen. */
  dispose(): void
}

export function draftBox(
  save: (text: string) => void | Promise<void>,
  delay: number = DRAFT_DEBOUNCE_MS,
  timers: DraftTimers = realTimers,
): DraftBox {
  let handle: unknown = null
  let owned = false

  const cancel = () => {
    if (handle !== null) {
      timers.clear(handle)
      handle = null
    }
  }

  return {
    typed(text: string) {
      owned = true
      cancel()
      handle = timers.set(() => {
        handle = null
        void Promise.resolve(save(text)).catch(() => {})
      }, delay)
    },
    restore(stored) {
      if (owned) return null
      const text = stored ?? ''
      return text === '' ? null : text
    },
    sent() {
      cancel()
      owned = false
    },
    idle() {
      return handle === null
    },
    dispose() {
      cancel()
    },
  }
}
