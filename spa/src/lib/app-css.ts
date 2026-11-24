// The interface's whole stylesheet as one string, for tests that render a
// component in a real browser: app.css with its @tracon/ui imports
// (packages/ui) inlined, as Vite bundles it.

import { readFileSync } from 'node:fs'

const ui = new URL('../../../packages/ui/', import.meta.url)

export function appCss(): string {
  const css = readFileSync(new URL('../app.css', import.meta.url), 'utf8')
  return css.replace(/@import '@tracon\/ui\/([\w.-]+\.css)';/g, (_, file: string) =>
    readFileSync(new URL(file, ui), 'utf8'),
  )
}
