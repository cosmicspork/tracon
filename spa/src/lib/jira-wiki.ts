// Jira wiki markup is what the Jira tools send as descriptions and comments.
// Like markdown.ts, the source comes from agents, so every byte of text is
// escaped and only http(s)/mailto or in-app links survive.
import { escapeHtml, safeUrl } from './markdown'

type Cell = { head: boolean; text: string }
type Item = { marks: string[]; text: string }

const HOLE = /\u0000(\d+)\u0000/g
const LIST_ITEM = /^\s*([*#]+|-)\s+(.*)$/
const SPANS: [string, string][] = [
  ['\\*', 'strong'],
  ['_', 'em'],
  ['\\-', 'del'],
  ['\\+', 'ins'],
]

function link(inner: string): string | null {
  const bar = inner.indexOf('|')
  const text = bar < 0 ? null : inner.slice(0, bar)
  const target = (bar < 0 ? inner : inner.slice(bar + 1).split('|')[0]!).trim()
  if (target.startsWith('~')) return `<span class="mention">@${escapeHtml(target.slice(1))}</span>`
  if (!/^([a-z][a-z0-9+.-]*:|\/(?!\/))/i.test(target)) {
    return text === null ? null : inline(text)
  }
  const href = safeUrl(target)
  const label = text === null ? escapeHtml(target) : inline(text)
  return href === null ? label : `<a href="${href}">${label}</a>`
}

function inline(src: string): string {
  const held: string[] = []
  const hold = (html: string) => `\u0000${held.push(html) - 1}\u0000`
  let s = src.replace(/\u0000/g, '')
  s = s.replace(/\{\{(.+?)\}\}/g, (_, code: string) => hold(`<code>${escapeHtml(code)}</code>`))
  s = s.replace(/\[([^[\]]+)\]/g, (whole, inner: string) => {
    const html = link(inner)
    return html === null ? whole : hold(html)
  })
  s = s.replace(/\bhttps?:\/\/[^\s<>"'[\]|{}\u0000]+/g, (url) => {
    const tail = /[.,;:!?)]+$/.exec(url)?.[0] ?? ''
    const bare = url.slice(0, url.length - tail.length)
    return hold(`<a href="${safeUrl(bare)}">${escapeHtml(bare)}</a>`) + tail
  })
  s = escapeHtml(s)
  for (const [m, tag] of SPANS) {
    const span = new RegExp(`(^|[^\\w${m}])${m}(?=\\S)(.+?)(?<=\\S)${m}(?![\\w${m}])`, 'g')
    s = s.replace(span, (_, before: string, text: string) => `${before}<${tag}>${text}</${tag}>`)
  }
  return s.replace(HOLE, (_, i: string) => held[Number(i)]!)
}

function cells(row: string): Cell[] {
  const out: Cell[] = []
  let cur: Cell | null = null
  let brackets = 0
  let braces = 0
  for (let i = 0; i < row.length; i++) {
    const c = row[i]!
    if (c === '|' && brackets === 0 && braces === 0) {
      if (cur) out.push(cur)
      const head = row[i + 1] === '|'
      if (head) i++
      cur = { head, text: '' }
      continue
    }
    if (c === '[') brackets++
    else if (c === ']' && brackets > 0) brackets--
    else if (c === '{' && row[i + 1] === '{') braces++
    else if (c === '}' && row[i + 1] === '}' && braces > 0) braces--
    if (cur) cur.text += c
  }
  if (cur && cur.text.trim() !== '') out.push(cur)
  return out
}

function table(rows: string[]): string {
  const body = rows
    .map((row) => {
      const tds = cells(row.trim()).map(({ head, text }) => {
        const tag = head ? 'th' : 'td'
        return `<${tag}>${inline(text.trim())}</${tag}>`
      })
      return `<tr>${tds.join('')}</tr>`
    })
    .join('')
  return `<table><tbody>${body}</tbody></table>`
}

function list(items: Item[]): string {
  const tag = (mark: string) => (mark === '#' ? 'ol' : 'ul')
  const open: string[] = []
  let html = ''
  for (const { marks, text } of items) {
    let common = 0
    while (common < open.length && common < marks.length && open[common] === marks[common]) common++
    while (open.length > common) html += `</li></${tag(open.pop()!)}>`
    if (open.length > 0 && open.length === marks.length) html += '</li><li>'
    while (open.length < marks.length) {
      const mark = marks[open.length]!
      open.push(mark)
      html += `<${tag(mark)}><li>`
    }
    html += inline(text)
  }
  while (open.length) html += `</li></${tag(open.pop()!)}>`
  return html
}

/** Render Jira wiki markup to HTML that is safe to insert as-is. */
export function renderJiraWiki(src: string): string {
  const lines = src.replace(/\r\n?/g, '\n').split('\n')
  const out: string[] = []
  let para: string[] = []
  const flush = () => {
    if (para.length) out.push(`<p>${para.map(inline).join('<br>')}</p>`)
    para = []
  }

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]!

    const fence = /^\s*\{(code|noformat)(?::[^}]*)?\}(.*)$/.exec(line)
    if (fence) {
      flush()
      const close = `{${fence[1]}}`
      let buf = fence[2]!
      while (!buf.includes(close) && i + 1 < lines.length) buf += `\n${lines[++i]}`
      const end = buf.indexOf(close)
      const code = (end < 0 ? buf : buf.slice(0, end)).replace(/^\n/, '').replace(/\n$/, '')
      out.push(`<pre><code>${escapeHtml(code)}</code></pre>`)
      const after = end < 0 ? '' : buf.slice(end + close.length)
      if (after.trim()) para.push(after.trim())
      continue
    }

    if (!line.trim()) {
      flush()
      continue
    }

    const heading = /^\s*h([1-6])\.\s+(.*)$/.exec(line)
    if (heading) {
      flush()
      out.push(`<h${heading[1]}>${inline(heading[2]!)}</h${heading[1]}>`)
      continue
    }

    const quote = /^\s*bq\.\s+(.*)$/.exec(line)
    if (quote) {
      flush()
      out.push(`<blockquote>${inline(quote[1]!)}</blockquote>`)
      continue
    }

    if (/^\s*-{4,}\s*$/.test(line)) {
      flush()
      out.push('<hr>')
      continue
    }

    let item = LIST_ITEM.exec(line)
    if (item) {
      flush()
      const items: Item[] = []
      for (;;) {
        items.push({ marks: item[1] === '-' ? ['*'] : [...item[1]!], text: item[2]! })
        const next = LIST_ITEM.exec(lines[i + 1] ?? '')
        if (!next) break
        item = next
        i++
      }
      out.push(list(items))
      continue
    }

    if (line.trimStart().startsWith('|')) {
      flush()
      const rows = [line]
      while (lines[i + 1]?.trimStart().startsWith('|')) rows.push(lines[++i]!)
      out.push(table(rows))
      continue
    }

    para.push(line)
  }
  flush()
  return out.join('\n')
}
