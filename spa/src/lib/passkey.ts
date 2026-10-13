// Passkeys: what confirms an admin action made from off the node's machine.
//
// The node runs the ceremonies: it hands out the options (challenge included),
// keeps the state, and verifies the answer. This module only turns the node's
// JSON into what `navigator.credentials` takes, and the credential back into
// JSON, and maps a ceremony that ends without a credential into an error that
// says what happened. The ceremony error handling is svastha's
// (`web/src/lib/passkey.ts`), without its PRF vault parts.

/** The ceremony ended without a credential. WebAuthn reports a genuine user
 * cancel, a timeout, and most platform policy failures (especially on iOS) as
 * the same NotAllowedError, so callers can't tell them apart: `detail`
 * carries the underlying DOMException name and message so the interface can
 * show what actually happened instead of silently assuming a cancel. */
export class PasskeyCeremonyError extends Error {
  readonly detail: string
  constructor(cause: DOMException | null) {
    super('The passkey prompt closed without finishing.')
    this.name = 'PasskeyCeremonyError'
    this.detail = cause ? `${cause.name}${cause.message ? `: ${cause.message}` : ''}` : 'no credential returned'
  }
}

/** True when the WebAuthn API is present at all. */
export function passkeysSupported(): boolean {
  return (
    typeof window !== 'undefined' &&
    typeof window.PublicKeyCredential === 'function' &&
    typeof navigator !== 'undefined' &&
    !!navigator.credentials?.create &&
    !!navigator.credentials?.get
  )
}

// A ceremony that hangs (a documented iOS home-screen PWA failure mode) would
// otherwise leave the caller's busy state stuck forever; with a timeout the
// browser resolves it as an error instead.
export const CEREMONY_TIMEOUT_MS = 60_000

export function toBase64url(bytes: ArrayBuffer | Uint8Array): string {
  const view = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes)
  let s = ''
  for (const b of view) s += String.fromCharCode(b)
  return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')
}

export function fromBase64url(s: string): Uint8Array<ArrayBuffer> {
  const b64 = s.replace(/-/g, '+').replace(/_/g, '/')
  const padded = b64 + '='.repeat((4 - (b64.length % 4)) % 4)
  const raw = atob(padded)
  const out = new Uint8Array(new ArrayBuffer(raw.length))
  for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i)
  return out
}

/** A credential descriptor as the node serializes it. */
interface Descriptor {
  type: 'public-key'
  id: string
  transports?: AuthenticatorTransport[]
}

/** `options.publicKey` of a registration, as the node sends it. */
export interface CreationOptionsJSON {
  publicKey: {
    challenge: string
    rp: { id?: string; name: string }
    user: { id: string; name: string; displayName: string }
    pubKeyCredParams: PublicKeyCredentialParameters[]
    timeout?: number
    excludeCredentials?: Descriptor[]
    authenticatorSelection?: AuthenticatorSelectionCriteria
    attestation?: AttestationConveyancePreference
    extensions?: Record<string, unknown>
  }
}

/** `options.publicKey` of an assertion, as the node sends it. */
export interface RequestOptionsJSON {
  publicKey: {
    challenge: string
    rpId?: string
    timeout?: number
    allowCredentials?: Descriptor[]
    userVerification?: UserVerificationRequirement
    extensions?: Record<string, unknown>
  }
}

function descriptor(d: Descriptor): PublicKeyCredentialDescriptor {
  return { type: d.type, id: fromBase64url(d.id), ...(d.transports ? { transports: d.transports } : {}) }
}

/** The node's registration options, for `navigator.credentials.create`. The
 * node's choices (attestation none, user verification required, discoverable
 * preferred, no pinned attachment, its extensions) pass through unchanged;
 * only the binary fields are decoded, and the timeout is held to
 * {@link CEREMONY_TIMEOUT_MS}. */
export function creationOptions(json: CreationOptionsJSON): PublicKeyCredentialCreationOptions {
  const pk = json.publicKey
  return {
    challenge: fromBase64url(pk.challenge),
    rp: pk.rp,
    user: { ...pk.user, id: fromBase64url(pk.user.id) },
    pubKeyCredParams: pk.pubKeyCredParams,
    timeout: Math.min(pk.timeout ?? CEREMONY_TIMEOUT_MS, CEREMONY_TIMEOUT_MS),
    excludeCredentials: (pk.excludeCredentials ?? []).map(descriptor),
    ...(pk.authenticatorSelection ? { authenticatorSelection: pk.authenticatorSelection } : {}),
    attestation: pk.attestation ?? 'none',
    ...(pk.extensions ? { extensions: pk.extensions as AuthenticationExtensionsClientInputs } : {}),
  }
}

/** The node's assertion options, for `navigator.credentials.get`. */
export function requestOptions(json: RequestOptionsJSON): PublicKeyCredentialRequestOptions {
  const pk = json.publicKey
  return {
    challenge: fromBase64url(pk.challenge),
    ...(pk.rpId ? { rpId: pk.rpId } : {}),
    timeout: Math.min(pk.timeout ?? CEREMONY_TIMEOUT_MS, CEREMONY_TIMEOUT_MS),
    allowCredentials: (pk.allowCredentials ?? []).map(descriptor),
    userVerification: pk.userVerification ?? 'required',
    ...(pk.extensions ? { extensions: pk.extensions as AuthenticationExtensionsClientInputs } : {}),
  }
}

/** A new credential, as the node's verifier reads it. */
export function registrationJSON(cred: PublicKeyCredential): Record<string, unknown> {
  const response = cred.response as AuthenticatorAttestationResponse
  const transports = typeof response.getTransports === 'function' ? response.getTransports() : undefined
  return {
    id: cred.id,
    rawId: toBase64url(cred.rawId),
    type: cred.type,
    response: {
      attestationObject: toBase64url(response.attestationObject),
      clientDataJSON: toBase64url(response.clientDataJSON),
      ...(transports ? { transports } : {}),
    },
    extensions: cred.getClientExtensionResults?.() ?? {},
  }
}

/** An assertion, as the node's verifier reads it. */
export function assertionJSON(cred: PublicKeyCredential): Record<string, unknown> {
  const response = cred.response as AuthenticatorAssertionResponse
  return {
    id: cred.id,
    rawId: toBase64url(cred.rawId),
    type: cred.type,
    response: {
      authenticatorData: toBase64url(response.authenticatorData),
      clientDataJSON: toBase64url(response.clientDataJSON),
      signature: toBase64url(response.signature),
      userHandle: response.userHandle ? toBase64url(response.userHandle) : null,
    },
    extensions: cred.getClientExtensionResults?.() ?? {},
  }
}

// Thin wrappers that turn a no-credential outcome (NotAllowedError covers
// cancel, timeout, AND most platform policy failures) into a typed error that
// keeps the DOMException's detail instead of erasing it.

export async function create(publicKey: PublicKeyCredentialCreationOptions): Promise<PublicKeyCredential> {
  let cred: Credential | null
  try {
    cred = await navigator.credentials.create({ publicKey })
  } catch (err) {
    if (err instanceof DOMException && (err.name === 'NotAllowedError' || err.name === 'AbortError'))
      throw new PasskeyCeremonyError(err)
    throw err
  }
  if (!cred) throw new PasskeyCeremonyError(null)
  return cred as PublicKeyCredential
}

export async function get(publicKey: PublicKeyCredentialRequestOptions): Promise<PublicKeyCredential> {
  let cred: Credential | null
  try {
    cred = await navigator.credentials.get({ publicKey })
  } catch (err) {
    if (err instanceof DOMException && (err.name === 'NotAllowedError' || err.name === 'AbortError'))
      throw new PasskeyCeremonyError(err)
    throw err
  }
  if (!cred) throw new PasskeyCeremonyError(null)
  return cred as PublicKeyCredential
}

/** What to tell the operator when a ceremony failed: the browser's own
 * account for the ones WebAuthn reports opaquely, and plain words for the
 * ones it names. */
export function ceremonyProblem(err: unknown): string {
  if (err instanceof PasskeyCeremonyError) return `The passkey prompt closed without finishing (${err.detail}).`
  if (err instanceof DOMException) {
    if (err.name === 'InvalidStateError') return 'This authenticator already holds a passkey for this node.'
    if (err.name === 'SecurityError')
      return `This page's address is not the one the node's passkeys belong to (${err.message || err.name}).`
    if (err.name === 'NotSupportedError') return 'This authenticator cannot make a passkey the node accepts.'
    return `${err.name}${err.message ? `: ${err.message}` : ''}`
  }
  return err instanceof Error ? err.message : String(err)
}

/** Refresh a challenge this close to its expiry rather than answer it. */
export const EXPIRY_MARGIN_MS = 10_000

/** A registration the node has opened, kept so a ceremony the browser cut
 * short (iOS wants a fresh tap after the confirmation sheet) can be tried
 * again with the same challenge. */
export interface Enrollment {
  challenge_id: string
  options: CreationOptionsJSON
  expires_ms: number
}

export function enrollmentLive(e: Enrollment | null, now = Date.now()): e is Enrollment {
  return !!e && now < e.expires_ms - EXPIRY_MARGIN_MS
}

/** A name for a passkey made on this device, from its user agent. */
export function deviceLabel(userAgent: string): string {
  if (/iPhone/.test(userAgent)) return 'iPhone'
  if (/iPad/.test(userAgent)) return 'iPad'
  if (/Android/.test(userAgent)) return 'Android'
  if (/Macintosh|Mac OS X/.test(userAgent)) return 'Mac'
  if (/Windows/.test(userAgent)) return 'Windows'
  if (/Linux/.test(userAgent)) return 'Linux'
  return 'Passkey'
}

/** What the step-up prompt names as being confirmed: the node's own line for
 * it, with the request it guards beneath; or the request alone when the node
 * gave no line. */
export function confirming(stepUp: { summary?: string; action?: string }): { headline: string | null; detail: string | null } {
  const summary = stepUp.summary?.trim() || null
  const action = stepUp.action?.trim() || null
  return summary ? { headline: summary, detail: action } : { headline: action, detail: null }
}
