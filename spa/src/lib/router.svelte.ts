// A hand-rolled history router: four destinations do not need SvelteKit, and
// the node serves index.html for any path it does not own.

import { redirect } from './routes'

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

  /** Screens with unsaved edits, each saying what leaving would lose. */
  private guards = new Set<() => string | null>()

  /**
   * Ask before an in-app navigation, or Back, leaves while `guard` says
   * something would be lost (its question, or null when nothing would). A
   * reload or a closed tab is the screen's own `beforeunload`. Returns the
   * call that removes the guard.
   */
  guard(guard: () => string | null): () => void {
    this.guards.add(guard)
    return () => this.guards.delete(guard)
  }

  /** Whether leaving may go ahead: no guard objects, or the operator agreed. */
  private mayLeave(): boolean {
    for (const guard of this.guards) {
      const question = guard()
      if (question && !window.confirm(question)) return false
    }
    return true
  }

  start() {
    this.read()
    window.addEventListener('popstate', () => {
      if (!this.mayLeave()) {
        // Back already moved the address: put back the one being left.
        history.pushState(null, '', this.path + this.search + this.hash)
        return
      }
      this.previous = null
      this.read()
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
    if (path !== current && !this.mayLeave()) return
    if (path !== current) {
      this.previous = current
      history.pushState(null, '', path)
    }
    this.read()
    this.revision += 1
  }

  /**
   * Change the address without a new history entry: a search being typed is
   * one place to come back to, not one per keystroke.
   */
  replace(path: string) {
    if (path !== this.path + this.search + this.hash) history.replaceState(history.state, '', path)
    this.read()
  }

  /** Take the address bar as it is, first moving an old address to its new one. */
  private read() {
    const moved = redirect(location.pathname, location.search, location.hash)
    if (moved) history.replaceState(history.state, '', moved)
    this.path = location.pathname
    this.search = location.search
    this.hash = location.hash
  }
}

export const router = new Router()
