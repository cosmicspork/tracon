// Documents can arrive from agents and mesh peers. Keep Markdown's generated
// markup, but treat embedded HTML and URL attributes as untrusted input.
import { Marked, marked, type Tokens } from 'marked'

marked.setOptions({ gfm: true, breaks: false })

export function escapeHtml(value: string): string {
  return value.replace(/[&<>"']/g, (c) => ({
    '&': '&amp;',
    '<': '&lt;',
    '>': '&gt;',
    '"': '&quot;',
    "'": '&#39;',
  })[c] as string)
}

export function safeUrl(value: string, image = false): string | null {
  const href = value.trim()
  // Browsers ignore ASCII whitespace/control characters around schemes. Use a
  // compact copy for the decision while retaining the original safe URL.
  const compact = href.replace(/[\u0000-\u0020\u007f]/g, '')
  const scheme = /^([a-z][a-z0-9+.-]*):/i.exec(compact)?.[1]?.toLowerCase()
  if (scheme && !['http', 'https', ...(image ? [] : ['mailto'])].includes(scheme)) return null
  return escapeHtml(href)
}

function link(text: string, href: string, title?: string | null): string {
  const clean = safeUrl(href)
  if (clean === null) return text
  return `<a href="${clean}"${title ? ` title="${escapeHtml(title)}"` : ''}>${text}</a>`
}

marked.use({
  renderer: {
    html({ text }: Tokens.HTML | Tokens.Tag) {
      return escapeHtml(text)
    },
    link({ href, title, tokens }: Tokens.Link) {
      return link(this.parser.parseInline(tokens), href, title)
    },
    image({ href, title, text, tokens }: Tokens.Image) {
      if (tokens) text = this.parser.parseInline(tokens, this.parser.textRenderer)
      const clean = safeUrl(href, true)
      if (clean === null) return escapeHtml(text)
      return `<img src="${clean}" alt="${escapeHtml(text)}"${title ? ` title="${escapeHtml(title)}"` : ''}>`
    },
  },
})

export function render(md: string): string {
  return marked.parse(md, { async: false }) as string
}

// An agent's account of its work is read with no network, like its pages: an
// image would be a request to wherever the agent pointed it, from the
// operator's browser and outside the session's egress. It is named instead.
const offline = new Marked({ gfm: true, breaks: false })
offline.use({
  renderer: {
    html({ text }: Tokens.HTML | Tokens.Tag) {
      return escapeHtml(text)
    },
    link({ href, title, tokens }: Tokens.Link) {
      return link(this.parser.parseInline(tokens), href, title)
    },
    image({ title, text, tokens }: Tokens.Image) {
      if (tokens) text = this.parser.parseInline(tokens, this.parser.textRenderer)
      const label = text || title || 'image'
      return `<span class="no-fetch">[${escapeHtml(label)}]</span>`
    },
  },
})

export function renderWithoutFetching(md: string): string {
  return offline.parse(md, { async: false }) as string
}
