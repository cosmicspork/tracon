// A hand-rolled history router: four destinations do not need SvelteKit, and
// the node serves index.html for any path it does not own.

class Router {
  path = $state(location.pathname)
  /** The query, kept reactive: a screen addressed by `?item=` is navigated to
      in-app as often as it is loaded cold. */
  search = $state(location.search)
  hash = $state(location.hash)
  revision = $state(0)
  /**
   * The route the last in-app navigation left, so a screen can put the
   * operator back where they came from. Null on a cold load and after a
   * history jump: nothing is behind a page that was typed in, and what Back
   * left is ahead of where it landed, not behind it.
   *
   * Deliberately not reactive — it is read when a screen opens, and a screen
   * that re-read it on every navigation would be recording its own.
   */
  previous: string | null = null

  start() {
    window.addEventListener('popstate', () => {
      this.previous = null
      this.path = location.pathname
      this.search = location.search
      this.hash = location.hash
      this.revision += 1
    })
    document.addEventListener('click', (e) => {
      // Leave modified clicks, downloads, and already-handled events to the
      // browser: shift/alt/meta/ctrl open new windows or save, and hijacking
      // them would break that.
      if (e.defaultPrevented || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return
      const a = (e.target as HTMLElement).closest('a')
      if (!a || a.origin !== location.origin || a.target || a.hasAttribute('download')) return
      e.preventDefault()
      this.go(a.pathname + a.search + a.hash)
    })
  }

  go(path: string) {
    const current = this.path + this.search + this.hash
    if (path !== current) {
      this.previous = current
      history.pushState(null, '', path)
    }
    this.path = location.pathname
    this.search = location.search
    this.hash = location.hash
    this.revision += 1
  }
}

export const router = new Router()
