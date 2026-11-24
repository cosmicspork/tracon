// The keyboard behaviour of a radiogroup (`Segmented`): arrow keys move the
// choice and wrap, Home and End go to the ends; anything else is not ours.

/** The index a key moves the choice to from `current` of `count`, or null. */
export function segmentedKey(key: string, current: number, count: number): number | null {
  if (count <= 0) return null
  const from = current < 0 ? 0 : current
  switch (key) {
    case 'ArrowRight':
    case 'ArrowDown':
      return (from + 1) % count
    case 'ArrowLeft':
    case 'ArrowUp':
      return (from - 1 + count) % count
    case 'Home':
      return 0
    case 'End':
      return count - 1
    default:
      return null
  }
}
