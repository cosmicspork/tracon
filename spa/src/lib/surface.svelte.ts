// Which surface is reading. Capability is gated by surface, not by width: the
// phone directs work and does not edit, so the interface asks this rather than
// scattering media queries through logic.

class Surface {
  phone = $state(false)

  start() {
    const query = window.matchMedia('(max-width: 700px)')
    this.phone = query.matches
    query.addEventListener('change', (e) => (this.phone = e.matches))
  }
}

export const surface = new Surface()
