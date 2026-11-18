// The device approval shown in place of the interface. One path for every
// way one is opened: an approval address loaded cold or opened in a tab
// already showing the interface (`App.svelte`), and a code scanned in the app
// (`QrScanner.svelte`). Opening one never touches `location.hash`, which the
// router reads for the Settings section.
//
// The ID is also stashed (`device.ts`), so an approval opened before signing
// in is still there once the operator has, and Login can say why it is shown.

import { stashApproval, stashedApproval } from './device'

class Approving {
  /** The approval being shown, or null. */
  id = $state<string | null>(stashedApproval())
}

export const approving = new Approving()

/** Show the approval `id` in place of the interface. */
export function openApproval(id: string): void {
  stashApproval(id)
  approving.id = id
}

/** Put the interface back: the approval was decided or closed. */
export function closeApproval(): void {
  stashApproval(null)
  approving.id = null
}
