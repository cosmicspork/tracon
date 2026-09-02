// A hand-rolled history router: four destinations do not need SvelteKit, and
// the node serves index.html for any path it does not own.

class Router {
  path = $state(location.pathname)
  /** The query, kept reactive: a screen addressed by `?item=` is navigated to
      in-app as often as it is loaded cold. */
  search = $state(location.search)

  start() {
    window.addEventListener('popstate', () => {
      this.path = location.pathname
      this.search = location.search
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
    if (path === this.path + location.search + location.hash) return
    history.pushState(null, '', path)
    this.path = location.pathname
    this.search = location.search
  }
}

export const router = new Router()
