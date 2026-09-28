// The node was upgraded while this page stayed open: its data is current,
// but the interface rendering it is the release that was loaded. Only a
// reload fetches the new one.
export function staleInterface(built: string, served: string | null | undefined): boolean {
  return Boolean(built && served && built !== served)
}
