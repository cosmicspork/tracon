import { expect, test } from 'bun:test'
import { readdirSync, readFileSync } from 'node:fs'

// The settings page's markup, read as source.
const settings = readFileSync(new URL('./Settings.svelte', import.meta.url), 'utf8')

test('the model list follows saves by itself: no Refresh models control is offered', () => {
  expect(settings).not.toContain('Refresh models')
  expect(settings).not.toContain('function refreshModels')
})

test('a failed probe is said, with a retry, only on the serving node', () => {
  const derived = settings.slice(settings.indexOf('const probeFailure = $derived('))
  expect(derived.slice(0, derived.indexOf('\n'))).toContain('selectedNodeIsServing ?')
  const markup = settings.slice(settings.indexOf('</script>'))
  const shown = markup.indexOf('{#if probeFailure}')
  expect(shown).toBeGreaterThan(-1)
  const block = markup.slice(shown, markup.indexOf('{/if}', shown))
  expect(block).toContain('{probeFailure}')
  expect(block).toContain('onclick={retryProbe}')
  expect(block).toContain("'Retry'")
})

test('no other screen says the probe failed', () => {
  const dirs = ['../routes/', '../components/', '../components/settings/']
  for (const dir of dirs) {
    const url = new URL(dir, import.meta.url)
    for (const file of readdirSync(url).filter((name) => name.endsWith('.svelte'))) {
      if (dir === '../routes/' && file === 'Settings.svelte') continue
      const text = readFileSync(new URL(file, url), 'utf8')
      expect(`${dir}${file}: ${text.includes('models_probe_failure') || text.includes('probeFailureLine')}`).toBe(`${dir}${file}: false`)
    }
  }
})
