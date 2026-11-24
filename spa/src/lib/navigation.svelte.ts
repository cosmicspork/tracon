// The navigation list as one reactive value, shared by the rail, the phone
// bar, More, Settings › General › Navigation and Settings › Plugins, so every
// place that edits it stays in step. Also the installed plugins, which plugin
// entries are drawn from; read once the node is known and again whenever a
// screen that changes them asks.

import { api } from './api'
import {
  addPlugin,
  defaultNav,
  loadNav,
  moveEntry,
  removePlugin,
  saveNav,
  setHidden,
  type NavEntry,
} from './nav'
import type { PluginSummary } from './types'

class Navigation {
  /** The node the list belongs to; empty until the node is known. */
  node = $state('')
  list = $state<NavEntry[]>(defaultNav())
  plugins = $state<PluginSummary[] | null>(null)
  private reading = 0

  /** Switch to `node`'s list (and read its plugins), once per node. */
  use(node: string | undefined | null) {
    if (!node || node === this.node) return
    this.node = node
    this.list = loadNav(node)
    void this.refreshPlugins()
  }

  async refreshPlugins() {
    const n = ++this.reading
    try {
      const read = (await api.plugins()).plugins
      if (n === this.reading) this.plugins = read
    } catch {
      /* the plugin entries stay hidden until a read succeeds */
    }
  }

  private set(list: NavEntry[]) {
    this.list = list
    if (this.node) saveNav(this.node, list)
  }

  move(index: number, delta: number) {
    this.set(moveEntry(this.list, index, delta))
  }
  hide(index: number, hidden: boolean) {
    this.set(setHidden(this.list, index, hidden))
  }
  addPlugin(name: string) {
    this.set(addPlugin(this.list, name))
  }
  removePlugin(name: string) {
    this.set(removePlugin(this.list, name))
  }
  reset() {
    this.set(defaultNav())
  }
}

export const navigation = new Navigation()
