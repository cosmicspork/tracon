// Documents are markdown. `marked` renders them; what is rendered was written
// by the operator or approved by them (an agent's doc_write is asked), so the
// HTML is trusted the way the notebook trusted it.
import { marked } from 'marked'

marked.setOptions({ gfm: true, breaks: false })

export function render(md: string): string {
  return marked.parse(md, { async: false }) as string
}
