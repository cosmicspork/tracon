// How a provider's reported capabilities read in the checks list.

import type { CapabilityLine, Elsewhere } from './types'

const NAMES: Record<string, string> = {
  runtime_socket: 'runtime socket',
  image_identity: 'image identity',
}

/** `runtime_socket` → `runtime socket`. */
export function capabilityName(capability: string): string {
  return NAMES[capability] ?? capability
}

/**
 * The chip a line wears: `off` for one its check did not establish, `warn`
 * for a value that is itself a caution (a route nobody verified, an open
 * resolver, a reachable runtime socket), plain otherwise.
 */
export function capabilityTone(line: CapabilityLine): 'off' | 'warn' | '' {
  if (line.value === null) return 'off'
  if (['unverified', 'provider-open', 'present'].includes(line.value)) return 'warn'
  return ''
}

/** Memory as an operator reads it: `630 Mi`, `1.5 Gi`. */
export function memoryWords(bytes: number): string {
  const mi = 1024 * 1024
  const gi = mi * 1024
  if (bytes >= gi) return `${(bytes / gi).toFixed(1).replace(/\.0$/, '')} Gi`
  return `${Math.floor(bytes / mi)} Mi`
}

/** CPU in cores from millicores: `2`, `0.75`. */
export function cpuWords(millis: number): string {
  return String(Number((millis / 1000).toFixed(2)))
}

/**
 * Why a Kubernetes runtime is full or has room, from a placement event's
 * `capacity` or the checks' `room`: the cluster's free and needed figures,
 * the scheduler's refusal of the session's pod, or why the room was not
 * read; a sandbox pool with no node yet has room to wait for
 * (`awaits_node`). Empty for a capacity that carries none of these (a count
 * alone).
 */
export function roomWords(room: Record<string, unknown> | null | undefined): string {
  if (!room) return ''
  if (typeof room.unschedulable === 'string') return `its pod ${room.unschedulable}`
  if (typeof room.unread === 'string') return `the cluster's room was not read: ${room.unread}`
  const n = (k: string) => (typeof room[k] === 'number' ? (room[k] as number) : null)
  const [freeCpu, freeMemory, needsCpu, needsMemory] = [n('free_cpu'), n('free_memory'), n('needs_cpu'), n('needs_memory')]
  if (freeCpu === null || freeMemory === null || needsCpu === null || needsMemory === null) return ''
  if (room.awaits_node === true) {
    return `the sandbox pool has no node now, and the cluster's autoscaler adds one when a pod asks; a session and its checks need ${memoryWords(needsMemory)} and ${cpuWords(needsCpu)} CPU`
  }
  if (room.fits === true) {
    return `the cluster has ${memoryWords(freeMemory)} and ${cpuWords(freeCpu)} CPU free; a session and its checks need ${memoryWords(needsMemory)} and ${cpuWords(needsCpu)} CPU`
  }
  if (freeMemory < needsMemory) {
    return `the cluster has ${memoryWords(freeMemory)} free, a session and its checks need ${memoryWords(needsMemory)}`
  }
  return `the cluster has ${cpuWords(freeCpu)} CPU free, a session and its checks need ${cpuWords(needsCpu)} CPU`
}

/** The room line a local Kubernetes runtime shows beside its capabilities. */
export function roomLine(room: Record<string, unknown> | null | undefined): ElsewhereLine | null {
  const text = roomWords(room)
  if (!text || !room) return null
  if (typeof room.unread === 'string') {
    return { label: 'room', tone: 'off', text: 'not read', detail: `${room.unread}; placement counts sessions only ([placement] local_limit)` }
  }
  return {
    label: 'room',
    tone: room.fits === false ? 'warn' : '',
    text: room.fits === false ? 'full' : 'room for a session',
    detail: room.fits === false ? `${text}: a new session overflows or is refused` : text,
  }
}

/** One line of what a provider elsewhere stands at, in the checks list. */
export interface ElsewhereLine {
  label: string
  tone: 'off' | 'warn' | 'bad' | ''
  text: string
  detail?: string
}

const REACH: Record<string, string> = {
  address: "the node's own address, as TLS",
  'hub relay': "the hub relay's listener, as TLS",
  'hub url': "the hub's own URL, as TLS in a WebSocket",
}

/**
 * A provider elsewhere as lines beside its capabilities: whether a session
 * can go there now and why not, how its sandboxes reach the node, when
 * placement sends sessions there and how many run, and which catalogue
 * services run in its sandboxes.
 */
export function elsewhereLines(e: Elsewhere): ElsewhereLine[] {
  const limit = e.capacity.limit === null ? '' : ` of ${e.capacity.limit}`
  const lines: ElsewhereLine[] = [
    e.healthy
      ? { label: 'health', tone: '', text: 'can take a session' }
      : { label: 'health', tone: 'bad', text: 'unavailable', detail: e.why ?? 'no reason given' },
    {
      label: 'reach',
      tone: e.reach === 'hub url' ? 'warn' : '',
      text: `${e.reach} · ${e.dial}`,
      detail: `${REACH[e.reach] ?? e.reach}; egress admits ${e.egress.join(', ') || 'nothing'}`,
    },
    {
      label: 'placement',
      tone: e.placement === 'off' ? 'off' : '',
      text: `${e.placement} · ${e.capacity.running}${limit} running`,
    },
    userLine(e),
  ]
  const size = defaultSizeLine(e)
  if (size) lines.push(size)
  for (const f of e.floors ?? []) {
    lines.push(
      f.size
        ? { label: `floor ${f.scope}`, tone: '', text: `${f.memory} → ${sized(f.size, f.memory_mb, f.price_per_hour_usd)}` }
        : { label: `floor ${f.scope}`, tone: 'bad', text: `${f.memory} → none`, detail: f.error },
    )
  }
  if (e.spend) {
    const s = e.spend
    const today = `$${s.today_usd.toFixed(2)}`
    lines.push({
      label: 'spend',
      tone: s.state === 'at' ? 'bad' : s.state === 'near' ? 'warn' : '',
      text: s.limit_usd === null ? `${today} today · no limit` : `${today} of $${s.limit_usd.toFixed(2)} today`,
      detail:
        s.state === 'at'
          ? 'limit reached: new sessions stay off DigitalOcean until the day turns ([spend] infra_usd_per_day)'
          : s.limit_usd === null
            ? 'set [spend] infra_usd_per_day in Configuration to cap it'
            : undefined,
    })
  }
  for (const s of e.services) {
    lines.push(
      s.ok
        ? { label: `service ${s.name}`, tone: '', text: 'runs inside' }
        : { label: `service ${s.name}`, tone: 'off', text: 'refused', detail: s.why },
    )
  }
  return lines
}

/** A size as `slug · 2048 MB · $0.107/h`, or unpriced when the catalog was not read. */
function sized(slug: string, memoryMb?: number, price?: number): string {
  if (price === undefined) return `${slug} · unpriced`
  const memory = memoryMb === undefined ? '' : ` · ${memoryMb} MB`
  return `${slug}${memory} · $${price.toFixed(3)}/h`
}

const BILLED = 'billed from create to destroy: sandboxes are kept warm, so idle time counts'

/**
 * The size a sandbox with no memory floor is made at, and why none can be:
 * a configured size the catalog does not list, or a catalog not read with
 * no size configured. Nothing from an older node that sent neither.
 */
function defaultSizeLine(e: Elsewhere): ElsewhereLine | null {
  const label = 'default size'
  if (e.size_error !== undefined) {
    if (e.catalog_error !== undefined) {
      return { label, tone: 'bad', text: 'catalog not read', detail: `${e.catalog_error}. ${e.size_error}` }
    }
    if (e.configured_size !== undefined) {
      return { label, tone: 'bad', text: `${e.configured_size} · not in DigitalOcean's catalog`, detail: e.size_error }
    }
    return { label, tone: 'bad', text: 'none', detail: e.size_error }
  }
  if (!e.size) return null
  const why = e.configured_size !== undefined ? 'set by [digitalocean] size' : 'the cheapest with at least 2 GiB'
  return {
    label,
    tone: e.catalog_error !== undefined ? 'warn' : '',
    text: sized(e.size, e.memory_mb, e.price_per_hour_usd),
    detail:
      e.catalog_error !== undefined ? `${why}, unpriced: catalog not read: ${e.catalog_error}` : `${why}; ${BILLED}`,
  }
}

/**
 * Who a sandbox's harness runs as, on the evidence of what its supervisor
 * said: not established until one has connected, and a caution when it
 * said another user (such a sandbox is refused).
 */
function userLine(e: Elsewhere): ElsewhereLine {
  const want = e.expected_uid === null ? 'its user' : `uid ${e.expected_uid}`
  if (!e.reported) {
    return {
      label: 'user',
      tone: 'off',
      text: 'not established',
      detail: `no sandbox has connected since the node started; one whose supervisor is not ${want} is refused`,
    }
  }
  const said = e.reported.uid === null ? 'no uid' : `uid ${e.reported.uid}`
  if (e.expected_uid === null || e.reported.uid === e.expected_uid) {
    return {
      label: 'user',
      tone: '',
      text: said,
      detail: `as tracon-sandbox ${e.reported.version} reported it; the harness and its services run as it`,
    }
  }
  return {
    label: 'user',
    tone: 'bad',
    text: said,
    detail: `tracon-sandbox ${e.reported.version} did not become ${want}, so its sandbox was refused: rebuild the template's image from this release`,
  }
}
