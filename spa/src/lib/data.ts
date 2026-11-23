// How the data pane says what a node holds.

/** A size the way a person reads it: 0 B, 812 B, 4.2 KB, 1.3 GB. */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  let n = bytes
  let unit = 0
  while (n >= 1000 && unit < units.length - 1) {
    n /= 1000
    unit++
  }
  if (unit === 0) return `${Math.round(n)} B`
  return `${n < 10 ? n.toFixed(1) : Math.round(n)} ${units[unit]}`
}

/** "1 session", "12 work items", "no memories". */
export function countLabel(count: number, unit: string): string {
  const plural = unit.endsWith('y') && !unit.endsWith('ey') ? `${unit.slice(0, -1)}ies` : `${unit}s`
  if (count === 0) return `no ${plural}`
  return `${count.toLocaleString('en-US')} ${count === 1 ? unit : plural}`
}

/** What the operator is told of one service volume, and whether it can be
 *  reset from here: only at the node itself, and not while a session holds it. */
export function serviceVolumeLine(
  volume: { service: string; bytes: number; used_ms: number; live: boolean; mounted: boolean },
  now: number,
): string {
  const parts = [`${volume.service} · ${formatBytes(volume.bytes)}`]
  if (volume.live) parts.push('held by a running session')
  else parts.push(`last used ${ago(now - volume.used_ms)}`)
  if (!volume.mounted) parts.push('its entry no longer names this service')
  return parts.join(' · ')
}

export function resetRefusal(volume: { live: boolean }, local: boolean): string | null {
  if (!local) return 'Service data is reset on the node itself.'
  if (volume.live) return 'A running session holds it; reset it once that session ends.'
  return null
}

function ago(ms: number): string {
  const minutes = Math.floor(Math.max(0, ms) / 60_000)
  if (minutes < 1) return 'just now'
  if (minutes < 60) return `${minutes} min ago`
  const hours = Math.floor(minutes / 60)
  if (hours < 48) return `${hours} h ago`
  return `${Math.floor(hours / 24)} days ago`
}
