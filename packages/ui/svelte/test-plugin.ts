// For `bun test`: .svelte files compiled for the server, so a test renders a
// component to HTML with `svelte/server`.
import { plugin } from 'bun'

plugin({
  name: 'svelte',
  async setup(build) {
    const { compile } = await import('svelte/compiler')
    build.onLoad({ filter: /\.svelte$/ }, async ({ path }) => ({
      contents: compile(await Bun.file(path).text(), { filename: path, generate: 'server' }).js.code,
      loader: 'js',
    }))
  },
})
