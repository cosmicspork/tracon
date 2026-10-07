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
