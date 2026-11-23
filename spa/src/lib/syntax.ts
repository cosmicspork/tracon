// Syntax highlighting for the review diff: which language a file is, by its
// name, and the colours its tokens take. Each language's parser is its own
// chunk, fetched the first time a file of that language shows. The colours
// are the interface's own tokens, so the diff reads the same in light and
// dark and its red and green stay the diff's.

import { HighlightStyle, LanguageDescription, syntaxHighlighting, type LanguageSupport } from '@codemirror/language'
import { languages } from '@codemirror/language-data'
import { tags as t } from '@lezer/highlight'

const found = new Map<string, Promise<LanguageSupport | null>>()

/** The language of a file by its name, loaded; null when there is none or it cannot be fetched. */
export function languageFor(path: string): Promise<LanguageSupport | null> {
  const name = path.split('/').pop() ?? path
  const desc = LanguageDescription.matchFilename(languages, name)
  if (!desc) return Promise.resolve(null)
  let loading = found.get(desc.name)
  if (!loading) {
    loading = desc.load().catch(() => null)
    found.set(desc.name, loading)
  }
  return loading
}

/** How tokens read: few colours, all of them the interface's. */
export const syntax = syntaxHighlighting(
  HighlightStyle.define([
    { tag: [t.keyword, t.controlKeyword, t.moduleKeyword, t.operatorKeyword, t.modifier], color: 'var(--acc)' },
    { tag: [t.string, t.special(t.string), t.regexp, t.character], color: 'var(--wait)' },
    { tag: [t.number, t.bool, t.null, t.atom], color: 'var(--wait)' },
    { tag: [t.comment, t.lineComment, t.blockComment, t.docComment], color: 'var(--dim)', fontStyle: 'italic' },
    { tag: [t.typeName, t.className, t.namespace], color: 'var(--ink2)', fontWeight: '600' },
    { tag: [t.definition(t.variableName), t.function(t.definition(t.variableName))], color: 'var(--ink)', fontWeight: '600' },
    { tag: [t.meta, t.processingInstruction, t.annotation], color: 'var(--dim)' },
    { tag: [t.tagName, t.attributeName, t.propertyName], color: 'var(--acc)' },
    { tag: t.heading, fontWeight: '700' },
    { tag: t.emphasis, fontStyle: 'italic' },
    { tag: t.strong, fontWeight: '700' },
    { tag: t.link, textDecoration: 'underline' },
  ]),
)
