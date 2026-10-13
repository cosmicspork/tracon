// The passkey prompt behind every admin action made from off the node's
// machine. `api.call` hands a `step_up_required` refusal here; the prompt
// asks the operator to confirm (a tap is what lets iOS show the passkey
// sheet at all), runs the assertion, and hands the proof back for the retry.
// At the node itself nothing is ever refused this way, so the desktop app
// never sees it there. Against a remote node the desktop app's webview has no
// passkeys to offer, so the prompt hands the challenge to a device approval
// instead: another browser, signed in with a passkey, confirms it, and this
// window collects the proof (`device.ts`).

import { isTauri } from '@tauri-apps/api/core'

import { api, setStepUpHandler, type StepUp } from './api'
import {
  confirmsElsewhere,
  DeviceCancelled,
  deviceProblem,
  handOver,
  pollForApproval,
  type DeviceProof,
  type DeviceStart,
} from './device'
import {
  assertionJSON,
  ceremonyProblem,
  create,
  creationOptions,
  enrollmentLive,
  EXPIRY_MARGIN_MS,
  get,
  passkeysSupported,
  PasskeyCeremonyError,
  registrationJSON,
  requestOptions,
  type Enrollment,
} from './passkey'

/** The operator closed the prompt: the action was not confirmed. */
export class StepUpCancelled extends Error {
  constructor() {
    super('Not confirmed: the action was not taken.')
    this.name = 'StepUpCancelled'
  }
}


interface Pending {
  stepUp: StepUp
  message: string
  resolve: (proof: string) => void
  reject: (err: Error) => void
}

/** Hand the pending action's challenge to a device approval. Either way the
    challenge is spent: the next try needs a fresh one. */
async function askElsewhere(pending: Pending): Promise<DeviceStart> {
  try {
    return await handOver(pending.stepUp, {
      assertOptions: (opHash) => api.passkeyAssertOptions(opHash),
      deviceStepUp: (challengeId) => api.deviceStepUp(challengeId),
    })
  } finally {
    pending.stepUp = { ...pending.stepUp, expires_ms: 0 }
  }
}

class StepUpPrompt {
  pending = $state<Pending | null>(null)
  busy = $state(false)
  problem = $state('')
  readonly supported = passkeysSupported()
  /** This window confirms in another browser rather than with its own
      passkey: the desktop app, or a browser without WebAuthn. */
  readonly elsewhere = confirmsElsewhere({
    tauri: typeof window !== 'undefined' && isTauri(),
    passkeys: this.supported,
  })
  /** The device approval this prompt is waiting on, once asked for. */
  device = $state<DeviceStart | null>(null)
  private abort: AbortController | null = null
  /** Prompts wait their turn: one sheet at a time. */
  private queue: Promise<unknown> = Promise.resolve()

  ask(stepUp: StepUp, message: string): Promise<string> {
    const turn = this.queue.then(
      () =>
        new Promise<string>((resolve, reject) => {
          this.problem = ''
          this.device = null
          this.pending = { stepUp, message, resolve, reject }
          // Nothing to tap for here: ask straight away, so the button that
          // opens the browser is a plain link the tap follows.
          if (this.elsewhere) void this.confirmElsewhere()
        }),
    )
    this.queue = turn.catch(() => undefined)
    return turn
  }

  /** Have another browser confirm the pending action, and wait for it. */
  async confirmElsewhere(): Promise<void> {
    const pending = this.pending
    if (!pending || this.busy || this.abort) return
    this.problem = ''
    this.device = null
    const abort = new AbortController()
    this.abort = abort
    try {
      const started = await askElsewhere(pending)
      if (abort.signal.aborted) {
        void api.deviceDeny(started.approval_id).catch(() => undefined)
        return
      }
      this.device = started
      const collected = await pollForApproval(started, (code) => api.deviceToken<DeviceProof>(code), {
        signal: abort.signal,
      })
      if (this.pending !== pending) return
      this.pending = null
      this.device = null
      pending.resolve(collected.proof)
    } catch (err) {
      if (err instanceof DeviceCancelled || this.pending !== pending) return
      this.device = null
      this.problem = deviceProblem(err)
    } finally {
      if (this.abort === abort) this.abort = null
    }
  }

  /** Run the assertion for the pending action. Called from the prompt's
      button, so the ceremony starts inside a user gesture. */
  async confirm(): Promise<void> {
    const pending = this.pending
    if (!pending || this.busy) return
    this.busy = true
    this.problem = ''
    try {
      let stepUp = pending.stepUp
      if (Date.now() > stepUp.expires_ms - EXPIRY_MARGIN_MS) {
        stepUp = { ...stepUp, ...(await api.passkeyAssertOptions(stepUp.op_hash)) }
        pending.stepUp = stepUp
      }
      const credential = await get(requestOptions(stepUp.options))
      let finished
      try {
        finished = await api.passkeyAssertFinish(stepUp.challenge_id, assertionJSON(credential))
      } catch (err) {
        // The node spends a challenge whether or not it verifies; the next
        // try needs a fresh one.
        pending.stepUp = { ...stepUp, expires_ms: 0 }
        throw err
      }
      this.pending = null
      pending.resolve(finished.proof)
    } catch (err) {
      // A ceremony that ended without a credential leaves the challenge
      // unspent: the operator can try again from the same prompt.
      this.problem = ceremonyProblem(err)
    } finally {
      this.busy = false
    }
  }

  cancel(): void {
    const pending = this.pending
    if (!pending) return
    // A device approval still waiting is refused, so the other browser does
    // not go on offering it.
    const device = this.device
    this.abort?.abort()
    this.abort = null
    if (device) void api.deviceDeny(device.approval_id).catch(() => undefined)
    this.device = null
    this.pending = null
    this.problem = ''
    pending.reject(new StepUpCancelled())
  }
}

export const stepUp = new StepUpPrompt()
setStepUpHandler((s, message) => stepUp.ask(s, message))


/** Open a registration. Off the node, after the pairing window, this is an
    admin action and the step-up prompt runs first. */
export function beginEnrollment(label: string): Promise<Enrollment> {
  return api.passkeyRegisterOptions(label)
}

/** Create the passkey and enroll it. Throws {@link PasskeyCeremonyError} when
    the browser's sheet closed without one; the enrollment can be retried
    until it expires. */
export async function finishEnrollment(e: Enrollment): Promise<{ id: string; label: string }> {
  const credential = await create(creationOptions(e.options))
  return api.passkeyRegisterFinish(e.challenge_id, registrationJSON(credential))
}


/** One "add a passkey" control's state: busy, what went wrong, and the
    registration held for a retry. */
export class Enroller {
  busy = $state(false)
  problem = $state('')
  held = $state<Enrollment | null>(null)

  /** Enroll a passkey named `label`; true once it is enrolled. A sheet the
      browser closed keeps the registration, so the next call (a fresh tap)
      tries the same one again. */
  async run(label: string): Promise<boolean> {
    if (this.busy) return false
    this.busy = true
    this.problem = ''
    try {
      const enrollment = enrollmentLive(this.held) ? this.held : await beginEnrollment(label)
      this.held = enrollment
      try {
        await finishEnrollment(enrollment)
      } catch (err) {
        // Only a sheet that closed leaves the challenge unspent.
        if (!(err instanceof PasskeyCeremonyError)) this.held = null
        throw err
      }
      this.held = null
      return true
    } catch (err) {
      this.problem = err instanceof StepUpCancelled ? '' : ceremonyProblem(err)
      return false
    } finally {
      this.busy = false
    }
  }
}

export { deviceLabel, PasskeyCeremonyError } from './passkey'
