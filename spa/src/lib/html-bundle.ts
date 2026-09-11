export const MAX_BUNDLE_BYTES = 20 * 1024 * 1024
export const MAX_FILE_BYTES = 16 * 1024 * 1024
export const MAX_FILES = 256

const KINDS = [
  'note',
  'repo',
  'meeting',
  'inbox',
  'proposal',
  'plan',
  'guide',
  'ref',
  'architecture',
]

export interface HtmlBundleFile {
  file: File
  path: string
}

export interface HtmlBundleSelection {
  sourceName: string
  entryPath: string
  files: HtmlBundleFile[]
  totalBytes: number
  suggestedSlug: string
}

export function selectHtmlFile(input: FileList | File[]): HtmlBundleSelection {
  const files = Array.from(input)
  if (files.length !== 1 || !isHtml(files[0].name)) {
    throw new Error('Choose exactly one .html file')
  }
  validateLimits(files)
  const file = files[0]
  const path = normalizePath(file.name)
  return {
    sourceName: file.name,
    entryPath: path,
    files: [{ file, path }],
    totalBytes: file.size,
    suggestedSlug: suggestSlug(file.name.replace(/\.html?$/i, '')),
  }
}

export function selectHtmlFolder(input: FileList | File[]): HtmlBundleSelection {
  const files = Array.from(input)
  if (files.length === 0) throw new Error('Choose a folder containing HTML')
  validateLimits(files)

  const pickerPaths = files.map((file) => file.webkitRelativePath)
  if (pickerPaths.some((path) => !path)) {
    throw new Error('Folder selection did not include relative paths')
  }
  const split = pickerPaths.map((path) => path.split('/'))
  const root = split[0][0]
  if (!root || split.some((segments) => segments.length < 2 || segments[0] !== root)) {
    throw new Error('Folder selection must have one common root')
  }
  const selected = files.map((file, index) => ({
    file,
    path: normalizePath(split[index].slice(1).join('/')),
  }))
  const rootIndex = selected.find((item) => item.path.toLowerCase() === 'index.html')
  const htmlEntries = selected.filter((item) => isHtml(item.path))
  const entry = rootIndex ?? (htmlEntries.length === 1 ? htmlEntries[0] : undefined)
  if (!entry) {
    if (htmlEntries.length === 0) throw new Error('The folder does not contain an HTML file')
    throw new Error('Choose a folder with index.html or exactly one HTML file')
  }

  return {
    sourceName: root,
    entryPath: entry.path,
    files: selected,
    totalBytes: files.reduce((sum, file) => sum + file.size, 0),
    suggestedSlug: suggestSlug(root),
  }
}

export function suggestSlug(name: string): string {
  const normalized = name
    .normalize('NFKD')
    .toLowerCase()
    .replace(/[^a-z0-9._-]+/g, '-')
    .replace(/-+/g, '-')
    .replace(/^[._-]+|[._-]+$/g, '')
  const base = normalized || 'document'
  const hasKind = KINDS.some((kind) => base === kind || base.startsWith(`${kind}-`))
  return hasKind ? base : `ref-${base}`
}

function normalizePath(path: string): string {
  if (!path || path.startsWith('/') || path.startsWith('\\') || path.includes('\\') || path.includes('\0')) {
    throw new Error(`Bundle path is invalid: ${path}`)
  }
  const segments = path.split('/')
  if (segments.some((segment) => !segment || segment === '.' || segment === '..')) {
    throw new Error(`Bundle path is invalid: ${path}`)
  }
  return segments.join('/')
}

function validateLimits(files: File[]): void {
  if (files.length > MAX_FILES) throw new Error(`Choose at most ${MAX_FILES} files`)
  let total = 0
  for (const file of files) {
    if (file.size > MAX_FILE_BYTES) {
      throw new Error(`${file.name} exceeds the ${MAX_FILE_BYTES}-byte file limit`)
    }
    total += file.size
    if (total > MAX_BUNDLE_BYTES) {
      throw new Error(`The selection exceeds the ${MAX_BUNDLE_BYTES}-byte bundle limit`)
    }
  }
}

function isHtml(path: string): boolean {
  return /\.html?$/i.test(path)
}
