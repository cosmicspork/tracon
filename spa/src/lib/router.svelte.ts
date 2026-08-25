// A hand-rolled history router: four destinations do not need SvelteKit, and
// the node serves index.html for any path it does not own.

class Router {
  path = $state(location.pathname)

  start() {
    window.addEventListener('popstate', () => (this.path = location.pathname))
    document.addEventListener('click', (e) => {
      const a = (e.target as HTMLElement).closest('a')
      if (!a || a.origin !== location.origin || a.target || e.metaKey || e.ctrlKey) return
      e.preventDefault()
      this.go(a.pathname)
    })
  }

  go(path: string) {
    if (path === this.path) return
    history.pushState(null, '', path)
    this.path = path
  }
}

export const router = new Router()
