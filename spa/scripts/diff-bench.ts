// How the review diff holds up on a large review, measured in a real
// Chromium: a few hundred source files with several hunks each, a lockfile
// thousands of lines long, a minified bundle and one large hand-written
// change. Run from spa/ with `bun scripts/diff-bench.ts`; TRACON_CHROMIUM
// points at a browser when Playwright has none of its own.
//
// It reports, for the page as the review screen mounts it:
//   mount       how long mounting the diff took (every file header is drawn by then)
//   firstBody   from the start of the mount to the first file's editor on screen
//   settled     to the end of the last long task after the mount
//   blocking    total blocking time over that load (sum of each long task past 50 ms)
//   longest     the longest single task over that load
//   editors     CodeMirror editors made by then
//   scroll      scrolling top to bottom in 800 px steps: the longest frame and
//               the blocking time over the whole scroll
//   heap        JS heap in use once settled

import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import { chromium } from 'playwright-core'
import { compile, compileModule } from 'svelte/compiler'

const here = fileURLToPath(new URL('../src/components/', import.meta.url))

// --- the review -------------------------------------------------------------

function sourceFile(i: number): string {
  const path = `src/module_${String(i).padStart(3, '0')}/handler.rs`
  const hunks: string[] = []
  for (const at of [12, 140, 310]) {
    const lines = [`@@ -${at},12 +${at},14 @@ fn handle_${i}()`]
    for (let k = 0; k < 4; k++) lines.push(`     let value_${at + k} = compute(${at + k}, "context ${i}");`)
    lines.push(`-    let total = sum(&values[${at}..]);`, `-    log::debug!("total {}", total);`)
    lines.push(`+    let total = values[${at}..].iter().copied().sum::<u64>();`)
    lines.push(`+    log::debug!(target: "module_${i}", "total {}", total);`)
    lines.push(`+    // The sum no longer allocates.`, `+    metrics::record(total);`)
    for (let k = 0; k < 4; k++) lines.push(`     let after_${at + k} = finish(${at + k});`)
    hunks.push(lines.join('\n'))
  }
  return `diff --git a/${path} b/${path}\nindex 1111111..2222222 100644\n--- a/${path}\n+++ b/${path}\n${hunks.join('\n')}\n`
}

function lockfile(lines: number): string {
  const body = Array.from({ length: lines }, (_, i) =>
    i % 4 === 0 ? `+[[package]]` : i % 4 === 1 ? `+name = "crate-${i}"` : i % 4 === 2 ? `+version = "1.${i % 97}.0"` : `+checksum = "${'ab'.repeat(32)}"`,
  )
  return `diff --git a/Cargo.lock b/Cargo.lock\nindex 3333333..4444444 100644\n--- a/Cargo.lock\n+++ b/Cargo.lock\n@@ -1,0 +1,${lines} @@\n${body.join('\n')}\n`
}

function minified(): string {
  const line = Array.from({ length: 4000 }, (_, i) => `function f${i}(a){return a+${i}}`).join(';')
  return `diff --git a/web/app.min.js b/web/app.min.js\nindex 5555555..6666666 100644\n--- a/web/app.min.js\n+++ b/web/app.min.js\n@@ -1 +1 @@\n-${line.slice(0, 60000)}\n+${line}\n`
}

function large(lines: number): string {
  const body = Array.from({ length: lines }, (_, i) => (i % 3 === 0 ? `-    old_${i}();\n+    new_${i}();` : `     same_${i}();`))
  return `diff --git a/src/big.rs b/src/big.rs\nindex 7777777..8888888 100644\n--- a/src/big.rs\n+++ b/src/big.rs\n@@ -1,${lines} +1,${lines} @@\n${body.join('\n')}\n`
}

const FILES = Number(process.env.FILES ?? 300)
const DIFF = [
  ...Array.from({ length: FILES }, (_, i) => sourceFile(i)),
  lockfile(8000),
  minified(),
  large(3000),
].join('')

// --- the page ---------------------------------------------------------------

const harness = `
  <script>
    import ReviewDiff from ${JSON.stringify(`${here}ReviewDiff.svelte`)}
    const { diff } = window.reviewDiff
  </script>
  <ReviewDiff {diff} />
`
const entry = `
  import { mount } from 'svelte'
  import Harness from 'harness.svelte'
  const start = performance.now()
  mount(Harness, { target: document.getElementById('app') })
  window.bench = { start, mounted: performance.now() }
`
const svelteClient = fileURLToPath(new URL('src/index-client.js', Bun.pathToFileURL(Bun.resolveSync('svelte/package.json', here))))
const compiled = (source: string, filename: string) => compile(source, { filename, generate: 'client', css: 'injected' }).js.code

const out = await Bun.build({
  entrypoints: ['entry'],
  target: 'browser',
  minify: true,
  plugins: [
    {
      name: 'svelte',
      setup(b) {
        b.onResolve({ filter: /^(entry|harness\.svelte)$/ }, ({ path }) => ({ path, namespace: 'harness' }))
        b.onResolve({ filter: /^svelte(\/|$)/ }, ({ path }) => ({ path: path === 'svelte' ? svelteClient : Bun.resolveSync(path, here) }))
        b.onLoad({ filter: /.*/, namespace: 'harness' }, ({ path }) => ({
          contents: path === 'entry' ? entry : compiled(harness, 'Harness.svelte'),
          loader: 'js',
        }))
        b.onLoad({ filter: /\.svelte$/ }, ({ path }) => ({ contents: compiled(readFileSync(path, 'utf8'), path), loader: 'js' }))
        b.onLoad({ filter: /\.svelte\.ts$/ }, ({ path }) => ({
          contents: compileModule(new Bun.Transpiler({ loader: 'ts' }).transformSync(readFileSync(path, 'utf8')), {
            filename: path,
            generate: 'client',
          }).js.code,
          loader: 'js',
        }))
      },
    },
  ],
})
if (!out.success) throw new Error(out.logs.join('\n'))
const js = (await out.outputs[0].text()).replaceAll('</script', '<\\/script')
const css = readFileSync(new URL('../src/app.css', import.meta.url), 'utf8')

async function launch() {
  const tries = [process.env.TRACON_CHROMIUM ? { executablePath: process.env.TRACON_CHROMIUM } : {}, { channel: 'chrome' }]
  for (const t of tries) {
    try {
      return await chromium.launch(t)
    } catch {
      /* the next place a browser might be */
    }
  }
  throw new Error('no Chromium to launch; set TRACON_CHROMIUM')
}
const browser = await launch()
const page = await browser.newPage({ viewport: { width: 1280, height: 900 } })
const html =
  `<!doctype html><meta charset="utf-8"><style>${css}</style><body style="margin:0;padding:0 16px"><div id="app"></div>` +
  `<script>
     window.longTasks = []
     new PerformanceObserver((l) => { for (const e of l.getEntries()) window.longTasks.push({ start: e.startTime, duration: e.duration }) })
       .observe({ type: 'longtask', buffered: true })
     ;(function first() {
       if (document.querySelector('.cm-editor')) window.firstBody = performance.now()
       else requestAnimationFrame(first)
     })()
     window.reviewDiff = ${JSON.stringify({ diff: DIFF })}
     localStorage.setItem('tracon-diff-mode', 'split')
   </script><script type="module">${js}</script></body>`
await page.route('http://tracon.test/**', (route) => route.fulfill({ contentType: 'text/html', body: html }))
await page.goto('http://tracon.test/', { waitUntil: 'commit', timeout: 600000 })
page.setDefaultTimeout(600000)
await page.waitForFunction(() => (window as any).firstBody, null, { timeout: 600000 })
// Settled: two seconds without a long task.
await page.waitForFunction(
  () => {
    const t = (window as any).longTasks as { start: number; duration: number }[]
    const last = t.length ? t[t.length - 1].start + t[t.length - 1].duration : 0
    return performance.now() - Math.max(last, (window as any).bench.mounted) > 2000
  },
  null,
  { timeout: 300000, polling: 200 },
)
const load = await page.evaluate(() => {
  const w = window as any
  const tasks = w.longTasks as { start: number; duration: number }[]
  const end = tasks.length ? tasks[tasks.length - 1].start + tasks[tasks.length - 1].duration : w.bench.mounted
  return {
    files: document.querySelectorAll('details.file').length,
    diffLines: w.reviewDiff.diff.split('\n').length,
    mount: Math.round(w.bench.mounted - w.bench.start),
    firstBody: Math.round(w.firstBody - w.bench.start),
    settled: Math.round(end - w.bench.start),
    blocking: Math.round(tasks.reduce((s, t) => s + Math.max(0, t.duration - 50), 0)),
    longest: Math.round(Math.max(0, ...tasks.map((t) => t.duration))),
    editors: document.querySelectorAll('.cm-editor').length,
    heapMB: Math.round(((performance as any).memory?.usedJSHeapSize ?? 0) / 1e6),
  }
})

const scroll = await page.evaluate(async () => {
  const w = window as any
  w.longTasks.length = 0
  const frames: number[] = []
  let last = performance.now()
  const t0 = last
  while (window.scrollY + window.innerHeight < document.documentElement.scrollHeight - 2) {
    window.scrollBy(0, 800)
    await new Promise((r) => requestAnimationFrame(r))
    const now = performance.now()
    frames.push(now - last)
    last = now
  }
  const tasks = w.longTasks as { duration: number }[]
  return {
    steps: frames.length,
    ms: Math.round(performance.now() - t0),
    longestFrame: Math.round(Math.max(...frames)),
    blocking: Math.round(tasks.reduce((s, t) => s + Math.max(0, t.duration - 50), 0)),
    editors: document.querySelectorAll('.cm-editor').length,
  }
})

console.log(JSON.stringify({ load, scroll }, null, 2))
await browser.close()
