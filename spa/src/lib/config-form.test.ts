import { expect, test } from 'bun:test'
import { ApiError } from './api'
import {
  arrayOps,
  blankEntry,
  changed,
  declareOps,
  entriesOps,
  entryStates,
  rebaseEntries,
  rebaseMap,
  arrayEdit,
  modelOps,
  editability,
  errorsByField,
  fieldFor,
  initialDraft,
  keyPath,
  keySegs,
  labelOf,
  magnitude,
  mapOps,
  mapRows,
  opFor,
  parseDraft,
  rebase,
  rematch,
  saveOps,
  sectionOps,
  sections,
  type Draft,
  type EntryState,
  type FieldContext,
} from './config-form'
import type { ConfigKeySpec, ConfigOp, ConfigShape } from './types'

function spec(key: string, shape: Partial<ConfigShape> & { kind: ConfigShape['kind'] }, over: Partial<ConfigKeySpec> = {}): ConfigKeySpec {
  return {
    key,
    apply: 'restart',
    admin: true,
    exec_path: false,
    fs_path: false,
    path: null,
    secret: false,
    editable: true,
    editable_at_node: true,
    shape: { optional: false, ...shape },
    ...over,
  }
}

const KEYS: ConfigKeySpec[] = [
  spec('node_name', { kind: 'string' }),
  spec('session.budget_tokens', { kind: 'integer' }, { apply: 'live' }),
  spec('session.permission_timeout_secs', { kind: 'integer' }, { apply: 'live' }),
  spec('session.default_channel', { kind: 'string' }, { apply: 'live' }),
  spec('publish.gh', { kind: 'string' }, { apply: 'live', exec_path: true, path: 'executable' }),
  spec('publish.commits', { kind: 'enum', choices: ['squash', 'keep'] }, { apply: 'live' }),
  spec('publish.style.conventional', { kind: 'bool' }, { apply: 'live' }),
  spec('publish.style.types', { kind: 'list', item: 'string' }, { apply: 'live' }),
  spec('github.dispatch.*', { kind: 'list', item: 'string' }, { apply: 'live' }),
  spec('mesh.hub_url', { kind: 'string', optional: true }),
  spec('embed.min_similarity', { kind: 'float' }),
  spec('gateway.harness_listen', { kind: 'any' }, { editable: false, editable_at_node: true, why_not: 'is the harness socket’s path' }),
  spec('runtime.kubernetes.state_mount', { kind: 'string' }, { editable: false, editable_at_node: false, why_not: 'decides where agents can write' }),
  spec('qa.token', { kind: 'string' }, { secret: true }),
  spec('repo.path', { kind: 'string' }, { apply: 'live', array: 'repo' }),
  spec('service.name', { kind: 'string' }, { array: 'service' }),
  spec('service.port', { kind: 'integer' }, { array: 'service' }),
  spec('providers.*.upstream', { kind: 'string' }),
]

const DOCUMENT = {
  node_name: 'laptop',
  session: { budget_tokens: 2000000 },
  publish: { gh: '/usr/bin/gh' },
  github: { dispatch: { 'op/homelab': ['renovate.yaml'] } },
  qa: { token: '<redacted>' },
  service: [{ name: 'browser', port: 9222 }],
}
const EFFECTIVE = {
  node_name: 'laptop',
  session: { budget_tokens: 2000000, permission_timeout_secs: 900, default_channel: 'personal' },
  publish: { gh: '/usr/bin/gh', commits: 'squash', style: { conventional: true, types: ['feat', 'fix'] } },
  github: { dispatch: { 'op/homelab': ['renovate.yaml'] } },
  mesh: {},
  embed: { min_similarity: 0.3 },
  gateway: { harness_listen: '/run/user/1000/tracon/harness.sock' },
  runtime: { kubernetes: { state_mount: '/state' } },
  qa: { token: '<redacted>' },
  service: [{ name: 'browser', port: 9222 }],
}

function ctx(local = false): FieldContext {
  return { keys: KEYS, document: DOCUMENT, effective: EFFECTIVE, redacted: '<redacted>', local }
}

function field(key: string, local = false) {
  const f = fieldFor(ctx(local), keySegs(key))
  if (!f) throw new Error(`no field ${key}`)
  return f
}

test('key paths are written as node.toml spells them, and read back', () => {
  expect(keyPath(['publish', 'gh'])).toBe('publish.gh')
  expect(keyPath(['repo', 2, 'egress'])).toBe('repo[2].egress')
  expect(keyPath(['providers', 'my.provider', 'upstream'])).toBe('providers."my.provider".upstream')
  expect(keyPath(['github', 'dispatch', 'op/homelab'])).toBe('github.dispatch."op/homelab"')
  expect(keySegs('providers."my.provider".upstream')).toEqual(['providers', 'my.provider', 'upstream'])
  expect(keySegs('repo[2].egress')).toEqual(['repo', 'egress'])
  expect(keySegs('a."q\\"x"')).toEqual(['a', 'q"x'])
})

test('a label is the key in words, its unit said apart', () => {
  expect(labelOf('permission_timeout_secs')).toEqual({ label: 'Permission timeout', unit: 'seconds' })
  expect(labelOf('budget_tokens')).toEqual({ label: 'Budget tokens', unit: 'tokens' })
  expect(labelOf('stream_max_body_bytes')).toEqual({ label: 'Stream max body', unit: 'bytes' })
  expect(labelOf('gh')).toEqual({ label: 'Gh', unit: undefined })
  expect(magnitude(86400, 'seconds')).toBe('1 day')
  expect(magnitude(900, 'seconds')).toBe('15 minutes')
  expect(magnitude(52428800, 'bytes')).toBe('50 MiB')
  expect(magnitude(2000000, 'tokens')).toBe('2M tokens')
  expect(magnitude(30, 'seconds')).toBe('')
})

test('each key gets the control its shape asks for', () => {
  expect(field('node_name').control).toBe('text')
  expect(field('session.budget_tokens').control).toBe('number')
  expect(field('session.budget_tokens').integer).toBe(true)
  expect(field('embed.min_similarity').integer).toBe(false)
  expect(field('publish.style.conventional').control).toBe('bool')
  expect(field('publish.style.types').control).toBe('lines')
  const commits = field('publish.commits')
  expect(commits.control).toBe('select')
  expect(commits.choices).toEqual(['squash', 'keep'])
  // Read in its own way: what it holds decides.
  expect(field('gateway.harness_listen').control).toBe('text')
  expect(field('mesh.hub_url').optional).toBe(true)
  // Choices the node holds rather than the schema.
  const withChannels = fieldFor({ ...ctx(), choices: (k) => (k === 'session.default_channel' ? ['', 'personal', 'work'] : undefined) }, ['session', 'default_channel'])
  expect(withChannels?.control).toBe('select')
  expect(field('publish.gh').help).toContain('program the node runs')
  expect(field('session.budget_tokens').written).toBe(true)
  expect(field('session.permission_timeout_secs').written).toBe(false)
})

test('a key the host keeps is disabled with its reason, and at the node only the state mount stays shut', () => {
  const remote = field('gateway.harness_listen')
  expect(remote.editable).toBe(false)
  expect(remote.whyNot).toStartWith('Edit at the node:')
  expect(field('gateway.harness_listen', true).editable).toBe(true)
  const mount = field('runtime.kubernetes.state_mount', true)
  expect(mount.editable).toBe(false)
  expect(mount.whyNot).not.toStartWith('Edit at the node')
  expect(editability(spec('x', { kind: 'string' }), false)).toEqual({ editable: true })
})

test('a secret is never prefilled and is written only when replaced', () => {
  const token = field('qa.token')
  expect(token.control).toBe('secret')
  expect(token.secretSet).toBe(true)
  expect(token.value).toBeUndefined()
  expect(initialDraft(token)).toEqual({ v: '' })
  expect(opFor(token, initialDraft(token))).toBeNull()
  expect(opFor(token, { v: 'sk-new' })).toEqual({ op: 'set', key: 'qa.token', value: 'sk-new' })
  expect(opFor(token, { v: '', reset: true })).toEqual({ op: 'remove', key: 'qa.token' })
  // Not set: clearing it changes nothing.
  const unset = fieldFor({ ...ctx(), document: {}, effective: {} }, ['qa', 'token'])!
  expect(unset.secretSet).toBe(false)
  expect(opFor(unset, { v: '', reset: true })).toBeNull()
})

test('each changed key is one operation, and an untouched one none', () => {
  const budget = field('session.budget_tokens')
  expect(opFor(budget, initialDraft(budget))).toBeNull()
  expect(opFor(budget, { v: '2500000' })).toEqual({ op: 'set', key: 'session.budget_tokens', value: 2500000 })
  expect(opFor(budget, { v: '2000000', reset: true })).toEqual({ op: 'remove', key: 'session.budget_tokens' })
  // A default the file does not hold has nothing to reset.
  const timeout = field('session.permission_timeout_secs')
  expect(opFor(timeout, { v: '900', reset: true })).toBeNull()
  expect(parseDraft(timeout, { v: '' }).error).toBe('A number is needed here.')
  expect(parseDraft(budget, { v: '1.5' }).error).toBe('A whole number is needed here.')
  expect(parseDraft(field('embed.min_similarity'), { v: '0.45' })).toEqual({ value: 0.45 })
  const types = field('publish.style.types')
  expect(opFor(types, { v: 'feat\n fix \n\n' })).toBeNull()
  expect(opFor(types, { v: 'feat\nfix\nchore' })).toEqual({ op: 'set', key: 'publish.style.types', value: ['feat', 'fix', 'chore'] })
  const conventional = field('publish.style.conventional')
  expect(opFor(conventional, { v: false })).toEqual({ op: 'set', key: 'publish.style.conventional', value: false })
  // An optional key emptied is taken out, never set to "".
  const hub = field('mesh.hub_url')
  expect(opFor(hub, { v: 'https://hub.example.net' })).toEqual({ op: 'set', key: 'mesh.hub_url', value: 'https://hub.example.net' })
  expect(opFor(hub, { v: '' })).toBeNull()
  const written = fieldFor({ ...ctx(), document: { mesh: { hub_url: 'https://h' } }, effective: { mesh: { hub_url: 'https://h' } } }, ['mesh', 'hub_url'])!
  expect(opFor(written, { v: '' })).toEqual({ op: 'remove', key: 'mesh.hub_url' })
  expect(changed(written, { v: 'https://h' })).toBe(false)
})

test('a section saves only what changed, key by key, and holds back what does not parse', () => {
  const session = sections(ctx()).find((s) => s.id === 'session')!
  const drafts: Record<string, Draft> = {
    'session.budget_tokens': { v: '3000000' },
    'session.permission_timeout_secs': { v: 'soon' },
    'session.default_channel': { v: 'personal' },
  }
  const { ops, errors } = sectionOps(session.fields, drafts)
  expect(ops).toEqual([{ op: 'set', key: 'session.budget_tokens', value: 3000000 }])
  expect(errors).toEqual({ 'session.permission_timeout_secs': 'soon is not a number.' })
})

test('sections follow the schema, leave the bespoke tables out, and gather maps and arrays', () => {
  const all = sections(ctx())
  const ids = all.map((s) => s.id)
  expect(ids).toEqual(['', 'session', 'publish', 'publish.style', 'github', 'mesh', 'embed', 'gateway', 'runtime.kubernetes', 'qa', 'service'])
  expect(all.some((s) => s.fields.some((f) => f.key.startsWith('repo') || f.key.startsWith('providers')))).toBe(false)
  const github = all.find((s) => s.id === 'github')!
  expect(github.maps[0].key).toBe('github.dispatch')
  expect(github.maps[0].control).toBe('lines')
  expect(all.find((s) => s.id === '')!.fields.map((f) => f.key)).toEqual(['node_name'])
  const service = all.find((s) => s.id === 'service')!.arrays[0]
  expect(service.key).toBe('service')
  expect(service.fields).toEqual(['name', 'port'])
  expect(service.entries).toEqual([{ name: 'browser', port: 9222 }])
})

test('a map sets the names that changed and removes the ones that went', () => {
  const map = sections(ctx()).find((s) => s.id === 'github')!.maps[0]
  const rows = mapRows(map)
  expect(rows).toEqual([{ name: 'op/homelab', v: 'renovate.yaml' }])
  expect(mapOps(map, rows)).toEqual([])
  expect(mapOps(map, [{ name: 'op/homelab', v: 'renovate.yaml\nci.yml' }, { name: 'op/site', v: 'deploy.yml' }])).toEqual([
    { op: 'set', key: 'github.dispatch."op/homelab"', value: ['renovate.yaml', 'ci.yml'] },
    { op: 'set', key: 'github.dispatch."op/site"', value: ['deploy.yml'] },
  ])
  expect(mapOps(map, [])).toEqual([{ op: 'remove', key: 'github.dispatch."op/homelab"' }])
})

test('an array of tables is edited by entry: remove, per-field set, insert, reorder', () => {
  const original = [
    { path: '/a', egress: ['crates'] },
    { path: '/b', egress: [] },
    { path: '/c', egress: [], session_egress: true },
  ]
  // Nothing touched: nothing sent.
  expect(arrayOps(['repo'], original, original.map((entry, origin) => ({ origin, entry })))).toEqual([])
  // One field of one entry.
  expect(
    arrayOps(['repo'], original, [
      { origin: 0, entry: original[0] },
      { origin: 1, entry: { path: '/b', egress: ['npm'] } },
      { origin: 2, entry: { path: '/c', egress: [] } },
    ]),
  ).toEqual([
    { op: 'set', key: 'repo[1].egress', value: ['npm'] },
    { op: 'remove', key: 'repo[2].session_egress' },
  ])
  // Remove the first, edit the last (now at 1), add one, and put the new one first.
  const ops = arrayOps(['repo'], original, [
    { origin: null, entry: { path: '/d', egress: [] } },
    { origin: 2, entry: { path: '/c', egress: ['pypi'], session_egress: true } },
    { origin: 1, entry: original[1] },
  ])
  expect(ops).toEqual([
    { op: 'remove', key: 'repo[0]' },
    { op: 'set', key: 'repo[1].egress', value: ['pypi'] },
    { op: 'insert', array: 'repo', entry: { path: '/d', egress: [] } },
    // After the removal and the append: [/b, /c, /d]; wanted [/d, /c, /b].
    { op: 'reorder', array: 'repo', order: [2, 1, 0] },
  ])
  // Removals go last first, so no index moves under another.
  expect(arrayOps(['repo'], original, [{ origin: 1, entry: original[1] }])).toEqual([
    { op: 'remove', key: 'repo[2]' },
    { op: 'remove', key: 'repo[0]' },
  ])
  // A nested array is addressed by its quoted table.
  expect(arrayOps(['providers', 'my.p', 'models'], [{ id: 'a' }], [{ origin: 0, entry: { id: 'b' } }])).toEqual([
    { op: 'set', key: 'providers."my.p".models[0].id', value: 'b' },
  ])
})

test('drafts re-anchor on a table read again by identity', () => {
  const drafts = [
    { origin: 0, entry: { path: '/a' } },
    { origin: 1, entry: { path: '/b' } },
    { origin: null, entry: { path: '/new' } },
  ]
  const now = [{ path: '/b' }, { path: '/x' }, { path: '/a' }]
  expect(rematch(drafts, now, (e) => String(e.path ?? '')).map((d) => d.origin)).toEqual([2, 0, null])
})

test('a refusal names the fields it is about', () => {
  const ops: ConfigOp[] = [
    { op: 'set', key: 'publish.gh', value: '/tmp/gh' },
    { op: 'set', key: 'repo[1].dockerfile', value: '../x' },
    { op: 'insert', array: 'repo', entry: { path: '/x', prepare: ['a\u200bb'] } },
  ]
  const keys = ['publish.gh', 'publish.git', 'repo[1].dockerfile', 'repo']
  expect(errorsByField([{ field: 'ops[1].value', message: 'm' }, { field: 'ops[2].entry.prepare[0]', message: 'n' }], ops, keys).fields).toEqual({
    'repo[1].dockerfile': ['m'],
    repo: ['n'],
  })
  const out = errorsByField(
    [
      { field: 'ops[0]', message: 'not a program' },
      { field: 'repo.dockerfile', message: 'must be inside the repository' },
      { field: 'qa.browser', message: 'not a key' },
    ],
    ops,
    keys,
  )
  expect(out.fields).toEqual({ 'publish.gh': ['not a program'], 'repo[1].dockerfile': ['must be inside the repository'] })
  expect(out.general).toEqual(['not a key'])
  // A hidden character in a map's name lands on the map.
  const map = errorsByField([{ field: 'github.dispatch."op/x\u202e"', message: 'holds a hidden character' }], [], ['github.dispatch'])
  expect(map.fields).toEqual({ 'github.dispatch': ['holds a hidden character'] })
})

test('a save reports a conflict, a refusal by field, a cancelled step-up, or the result', async () => {
  const ops: ConfigOp[] = [{ op: 'set', key: 'publish.gh', value: '/tmp/gh' }]
  const keys = ['publish.gh']
  const sent: [string, ConfigOp[]][] = []
  const ok = await saveOps(
    async (hash, o) => {
      sent.push([hash, o])
      return { changed: ['publish.gh'], hash: 'h2', restart_required: false }
    },
    'h1',
    ops,
    keys,
  )
  expect(sent).toEqual([['h1', ops]])
  expect(ok).toEqual({ kind: 'saved', result: { changed: ['publish.gh'], hash: 'h2', restart_required: false } })

  const conflict = await saveOps(async () => {
    throw new ApiError(409, 'node.toml changed since it was read')
  }, 'h1', ops, keys)
  expect(conflict.kind).toBe('conflict')

  const refused = await saveOps(async () => {
    throw new ApiError(422, 'refused', undefined, [{ field: 'publish.gh', message: '"/tmp/gh": an agent could write it' }], 'refused')
  }, 'h1', ops, keys)
  expect(refused).toEqual({ kind: 'refused', reason: 'refused', message: 'refused', fields: { 'publish.gh': ['"/tmp/gh": an agent could write it'] }, general: [] })

  const invalid = await saveOps(async () => {
    throw new ApiError(422, 'node.toml would not load', undefined, undefined, 'invalid')
  }, 'h1', ops, keys)
  expect(invalid).toEqual({ kind: 'refused', reason: 'invalid', message: 'node.toml would not load', fields: {}, general: ['node.toml would not load'] })

  // A hidden character is named at the place in the body: the key of that operation.
  const hidden = await saveOps(async () => {
    throw new ApiError(422, 'ops[0].value holds U+202E, a control, format or separator character a confirmation could not show; it is refused', undefined, [{ field: 'ops[0].value', message: 'holds U+202E' }], 'hidden_character')
  }, 'h1', ops, keys)
  expect(hidden.kind === 'refused' && hidden.fields).toEqual({ 'publish.gh': ['Holds U+202E: an invisible or control character. Retype the value.'] })
  expect(hidden.kind === 'refused' && hidden.general).toEqual([])

  // Too much to confirm at once: no field, and what to do about it.
  const large = await saveOps(async () => {
    throw new ApiError(422, 'this edit changes more sensitive keys than one confirmation can show (9); split it into smaller edits', undefined, undefined, 'too_large')
  }, 'h1', ops, keys)
  expect(large.kind === 'refused' && large.general).toEqual([
    'this edit changes more sensitive keys than one confirmation can show (9); split it into smaller edits',
    'Save fewer keys at a time: undo some of these edits, save the rest, then make them again.',
  ])

  // A key only the host changes: the node's reason lands on the key.
  const hostOnly = await saveOps(async () => {
    throw new ApiError(422, 'refused', undefined, [{ field: 'boundary.gateway_image', message: 'boundary.gateway_image is an image the node runs with its own sockets' }], 'refused')
  }, 'h1', [{ op: 'set', key: 'boundary.gateway_image', value: 'x' }], ['boundary.gateway_image'])
  expect(hostOnly.kind === 'refused' && Object.keys(hostOnly.fields)).toEqual(['boundary.gateway_image'])

  const cancelled = await saveOps(async () => {
    throw new Error('Not confirmed: the action was not taken.')
  }, 'h1', ops, keys)
  expect(cancelled).toEqual({ kind: 'failed', message: 'Not confirmed: the action was not taken.' })
})

test('a step-up refusal is confirmed inside the call and the same request is sent again', async () => {
  const { call, setStepUpHandler } = await import('./api')
  const bodies: string[] = []
  const proofs: (string | null)[] = []
  const real = globalThis.fetch
  globalThis.fetch = (async (_url: string, init: RequestInit) => {
    bodies.push(String(init.body))
    const proof = (init.headers as Record<string, string>)['x-tracon-step-up'] ?? null
    proofs.push(proof)
    if (!proof) {
      return new Response(
        JSON.stringify({
          error: { code: 403, kind: 'step_up_required', message: 'confirm' },
          step_up: { op_hash: 'o', challenge_id: 'c', options: {}, expires_ms: Date.now() + 60000, summary: 'Change node.toml: publish.gh' },
        }),
        { status: 403 },
      )
    }
    return new Response(JSON.stringify({ changed: ['publish.gh'], hash: 'h2', restart_required: false }), { status: 200 })
  }) as typeof fetch
  const summaries: string[] = []
  setStepUpHandler(async (stepUp) => {
    summaries.push(stepUp.summary ?? '')
    return 'proof'
  })
  try {
    const ops: ConfigOp[] = [{ op: 'set', key: 'publish.gh', value: '/usr/bin/gh' }]
    const out = await saveOps((hash, o) => call('PATCH', '/api/config', { expected_hash: hash, ops: o }), 'h1', ops, ['publish.gh'])
    expect(out.kind).toBe('saved')
    expect(summaries).toEqual(['Change node.toml: publish.gh'])
    expect(proofs).toEqual([null, 'proof'])
    // The very bytes the confirmation was bound to.
    expect(bodies[0]).toBe(bodies[1])
  } finally {
    setStepUpHandler(null)
    globalThis.fetch = real
  }
})

test('after a conflict, drafts carry over and those whose key moved are named', () => {
  const before = sections(ctx()).find((s) => s.id === 'session')!.fields
  const theirs = { ...EFFECTIVE, session: { ...EFFECTIVE.session, budget_tokens: 1000000, permission_timeout_secs: 600 } }
  const after = sections({ ...ctx(), effective: theirs }).find((s) => s.id === 'session')!.fields
  const drafts: Record<string, Draft> = {
    'session.budget_tokens': { v: '3000000' },
    'session.default_channel': { v: 'work' },
    // Untouched: dropped, so the reloaded value shows.
    'session.permission_timeout_secs': { v: '900' },
  }
  const out = rebase(before, after, drafts)
  expect(out.drafts).toEqual({ 'session.budget_tokens': { v: '3000000' }, 'session.default_channel': { v: 'work' } })
  expect(out.clashes).toEqual([{ key: 'session.budget_tokens', theirs: 1000000, yours: { v: '3000000' } }])
})

test('an array of tables in the generic form edits its entries one field at a time', () => {
  const service = sections(ctx()).find((s) => s.id === 'service')!.arrays[0]
  const before = entryStates(ctx(), service)
  expect(before[0].fields.map((f) => f.key)).toEqual(['service[0].name', 'service[0].port'])
  expect(entriesOps(service, before, before).ops).toEqual([])
  const edited: EntryState[] = [{ ...before[0], drafts: { port: { v: '9333' } } }, { ...blankEntry(ctx(), service, 1), drafts: { name: { v: 'postgres' }, port: { v: '5432' } } }]
  expect(entriesOps(service, before, edited)).toEqual({
    ops: [
      { op: 'set', key: 'service[0].port', value: 9333 },
      { op: 'insert', array: 'service', entry: { name: 'postgres', port: 5432 } },
    ],
    screen: ['service[0].port', 'service[1]'],
    errors: {},
  })
  const bad = [{ ...before[0], drafts: { port: { v: 'ninety' } } }]
  expect(entriesOps(service, before, bad).errors).toEqual({ 'service[0].port': 'ninety is not a number.' })
  expect(entriesOps(service, before, []).ops).toEqual([{ op: 'remove', key: 'service[0]' }])
})

const PROVIDER_KEYS = [
  spec('providers.*.credential', { kind: 'string' }),
  spec('providers.*.upstream', { kind: 'string' }),
  spec('providers.*.shape', { kind: 'string' }),
  spec('providers.*.login', { kind: 'string', optional: true }),
  spec('providers.*.models.id', { kind: 'string' }, { array: 'providers.*.models' }),
  spec('providers.*.models.name', { kind: 'string' }, { array: 'providers.*.models' }),
  spec('providers.*.models.reasoning', { kind: 'bool' }, { array: 'providers.*.models' }),
]
const ANTHROPIC = {
  credential: 'anthropic',
  upstream: 'https://api.anthropic.com',
  shape: 'anthropic',
  login: 'anthropic',
  models: [
    { id: 'sonnet', name: 'Claude Sonnet', reasoning: true },
    { id: 'haiku', name: 'Claude Haiku' },
  ],
}

test('a built-in provider the file does not name is written key by key before anything of it', () => {
  const c: FieldContext = { keys: PROVIDER_KEYS, document: {}, effective: { providers: { anthropic: ANTHROPIC } }, redacted: '<redacted>', local: false }
  expect(declareOps(c, 'anthropic', ['providers.anthropic.upstream'])).toEqual([
    { op: 'set', key: 'providers.anthropic.credential', value: 'anthropic' },
    { op: 'set', key: 'providers.anthropic.shape', value: 'anthropic' },
    { op: 'set', key: 'providers.anthropic.login', value: 'anthropic' },
  ])
  const rows = ANTHROPIC.models.map((model, origin) => ({ origin, model }))
  expect(modelOps(c, 'anthropic', rows)).toEqual([])
  const ops = modelOps(c, 'anthropic', [...rows, { origin: null, model: { id: 'opus', name: '', reasoning: true } }])
  expect(ops.slice(0, 4).map((o) => o.op)).toEqual(['set', 'set', 'set', 'set'])
  expect(ops.at(-1)).toEqual({
    op: 'set',
    key: 'providers.anthropic.models',
    value: [{ id: 'sonnet', name: 'Claude Sonnet', reasoning: true }, { id: 'haiku', name: 'Claude Haiku' }, { id: 'opus', reasoning: true }],
  })
  // Named in the file: nothing to declare.
  expect(declareOps({ ...c, document: { providers: { anthropic: { upstream: 'x' } } } }, 'anthropic')).toEqual([])
})

test('a provider whose models the file lists edits each model where it is', () => {
  const written = { providers: { 'my.p': { ...ANTHROPIC, models: [{ id: 'sonnet', name: 'Claude Sonnet', reasoning: true, context: 0 }, { id: 'haiku', name: 'Claude Haiku' }] } } }
  const c: FieldContext = { keys: PROVIDER_KEYS, document: written, effective: { providers: { 'my.p': ANTHROPIC } }, redacted: '<redacted>', local: false }
  const rows = ANTHROPIC.models.map((model, origin) => ({ origin, model: { ...model } }))
  expect(modelOps(c, 'my.p', rows)).toEqual([])
  rows[0].model.reasoning = false
  rows[1].model.name = 'Haiku'
  expect(modelOps(c, 'my.p', [rows[1], rows[0]])).toEqual([
    { op: 'set', key: 'providers."my.p".models[1].name', value: 'Haiku' },
    { op: 'remove', key: 'providers."my.p".models[0].reasoning' },
    { op: 'reorder', array: 'providers."my.p".models', order: [1, 0] },
  ])
})

test('parallel tool calls are on unless a model turns them off, so only off is written', () => {
  const written = { providers: { 'my.p': { ...ANTHROPIC, models: [{ id: 'sonnet' }] } } }
  const c: FieldContext = { keys: PROVIDER_KEYS, document: written, effective: { providers: { 'my.p': { ...ANTHROPIC, models: [{ id: 'sonnet' }] } } }, redacted: '<redacted>', local: false }
  // Ticking the box the model already shows as ticked writes nothing.
  expect(modelOps(c, 'my.p', [{ origin: 0, model: { id: 'sonnet', parallel_tools: true } }])).toEqual([])
  expect(modelOps(c, 'my.p', [{ origin: 0, model: { id: 'sonnet', parallel_tools: false } }])).toEqual([
    { op: 'set', key: 'providers."my.p".models[0].parallel_tools', value: false },
  ])
  const off = { providers: { 'my.p': { ...ANTHROPIC, models: [{ id: 'sonnet', parallel_tools: false }] } } }
  const c2: FieldContext = { ...c, document: off, effective: off }
  expect(modelOps(c2, 'my.p', [{ origin: 0, model: { id: 'sonnet', parallel_tools: true } }])).toEqual([
    { op: 'remove', key: 'providers."my.p".models[0].parallel_tools' },
  ])
})

test('a key with no value, in a table the file leaves out, writes nothing until it is given one', () => {
  const keys = [spec('publish.style.max_subject', { kind: 'integer' }), spec('publish.style.imperative', { kind: 'bool' }), spec('publish.style.types', { kind: 'list', item: 'string' })]
  const c: FieldContext = { keys, document: {}, effective: { publish: {} }, redacted: '<redacted>', local: false }
  const [max, imperative, types] = sections(c)[0].fields
  expect(parseDraft(max, initialDraft(max))).toEqual({ remove: true })
  expect(opFor(max, { v: '' })).toBeNull()
  expect(opFor(imperative, { v: false })).toBeNull()
  expect(opFor(types, { v: '' })).toBeNull()
  expect(opFor(max, { v: '72' })).toEqual({ op: 'set', key: 'publish.style.max_subject', value: 72 })
  expect(opFor(imperative, { v: true })).toEqual({ op: 'set', key: 'publish.style.imperative', value: true })
})

function serviceCtx(entries: Record<string, unknown>[]): FieldContext {
  return { ...ctx(), document: { ...DOCUMENT, service: entries }, effective: { ...EFFECTIVE, service: entries } }
}
function serviceArray(c: FieldContext) {
  return sections(c).find((s) => s.id === 'service')!.arrays[0]
}

test('after another save, the drafts of an array land on the same entries, and no entry added elsewhere is removed', () => {
  const browser = { name: 'browser', port: 9222 }
  const mail = { name: 'mail', port: 1025 }
  const cache = { name: 'cache', port: 6379 }
  const before = serviceCtx([browser, mail])
  const loaded = serviceArray(before)
  const states = entryStates(before, loaded)
  // The operator changes mail's port and removes browser.
  const edited: EntryState[] = [{ ...states[1], drafts: { port: { v: '1026' } } }]
  // Meanwhile (or by a save of another table here) the node gains cache, first.
  const after = serviceCtx([cache, browser, mail])
  const now = serviceArray(after)
  const carried = rebaseEntries(after, now, loaded.entries, edited)
  expect(carried.clashes).toEqual([])
  const { ops } = entriesOps(now, entryStates(after, now), carried.states)
  expect(ops).toEqual([
    { op: 'remove', key: 'service[1]' },
    { op: 'set', key: 'service[1].port', value: 1026 },
  ])
  // Before the fix the same drafts, sent against the new file, removed mail.
  const stale = entriesOps(now, entryStates(after, now), edited).ops
  expect(stale).toContainEqual({ op: 'remove', key: 'service[2]' })
})

test('an array that did not change keeps its drafts as they are, even with the first field edited', () => {
  const c = serviceCtx([{ name: 'browser', port: 9222 }])
  const array = serviceArray(c)
  const states: EntryState[] = [{ ...entryStates(c, array)[0], drafts: { name: { v: 'chromium' } } }]
  expect(rebaseEntries(c, array, array.entries, states)).toEqual({ states, clashes: [] })
})

test('an entry changed on both sides is named; one removed on the node is named with what saving does', () => {
  const before = serviceCtx([{ name: 'browser', port: 9222 }, { name: 'mail', port: 1025 }])
  const loaded = serviceArray(before)
  const states = entryStates(before, loaded)
  const edited: EntryState[] = [
    { ...states[0], drafts: { port: { v: '9333' } } },
    { ...states[1], drafts: { port: { v: '1026' } } },
  ]
  const after = serviceCtx([{ name: 'browser', port: 9444 }])
  const out = rebaseEntries(after, serviceArray(after), loaded.entries, edited)
  expect(out.clashes.map((c) => c.key)).toEqual(['service[0].port', 'service[1]'])
  expect(out.clashes[1].says).toBe('mail was removed on the node; saving adds it back with your edits.')
  expect(out.states.map((s) => s.origin)).toEqual([0, null])
})

test('a map keeps names others added and values others changed; a name both changed is named', () => {
  const map = sections(ctx()).find((s) => s.id === 'github')!.maps[0]
  const was = map.entries
  const rows = [{ name: 'op/homelab', v: 'renovate.yaml\nci.yml' }, { name: 'op/new', v: 'deploy.yml' }]
  const theirs = { ...map, entries: [['op/homelab', ['renovate.yaml']], ['op/site', ['pages.yml']]] as [string, unknown][] }
  const out = rebaseMap(theirs, was, rows)
  expect(out.clashes).toEqual([])
  expect(mapOps(theirs, out.rows)).toEqual([
    { op: 'set', key: 'github.dispatch."op/homelab"', value: ['renovate.yaml', 'ci.yml'] },
    { op: 'set', key: 'github.dispatch."op/new"', value: ['deploy.yml'] },
  ])
  const both = { ...map, entries: [['op/homelab', ['other.yml']]] as [string, unknown][] }
  const clash = rebaseMap(both, was, rows)
  expect(clash.clashes.map((c) => c.says)).toEqual(['op/homelab is now other.yml on the node; saving sets renovate.yaml, ci.yml.'])
})

test('after a save, a draft the node now holds is dropped rather than named', () => {
  const before = sections(ctx()).find((s) => s.id === 'session')!.fields
  const saved = { ...EFFECTIVE, session: { ...EFFECTIVE.session, budget_tokens: 3000000 } }
  const after = sections({ ...ctx(), effective: saved }).find((s) => s.id === 'session')!.fields
  expect(rebase(before, after, { 'session.budget_tokens': { v: '3000000' } })).toEqual({ drafts: {}, clashes: [] })
})

test('a refusal of a field set lands on the entry shown, after removals and a reorder', () => {
  const original = [{ name: 'a', port: 1 }, { name: 'b', port: 2 }, { name: 'c', port: 3 }]
  // Remove a; show c first with a new port, then b.
  const { ops, screen } = arrayEdit(['service'], original, [
    { origin: 2, entry: { name: 'c', port: 30 } },
    { origin: 1, entry: original[1] },
  ])
  expect(ops).toEqual([
    { op: 'remove', key: 'service[0]' },
    { op: 'set', key: 'service[1].port', value: 30 },
    { op: 'reorder', array: 'service', order: [1, 0] },
  ])
  const keys = ['service[0].name', 'service[0].port', 'service[1].name', 'service[1].port']
  expect(errorsByField([{ field: 'ops[1].value', message: 'too low' }], ops, keys, screen).fields).toEqual({ 'service[0].port': ['too low'] })
  // Named by the operation's own key, as the node may: the same field.
  expect(errorsByField([{ field: 'service[1].port', message: 'taken' }], ops, keys, screen).fields).toEqual({ 'service[0].port': ['taken'] })
})
