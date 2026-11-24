// `bun run dev`: the components' story page (dev/), in either scheme.
import { svelte } from '@sveltejs/vite-plugin-svelte'
import { defineConfig } from 'vite'

export default defineConfig({ root: 'dev', plugins: [svelte()] })
