// Plugins (/plugins) and one plugin's page (/plugins/:name): the pages
// installed plugins bring, each framed from its own loopback origin. The
// frame's own content is the plugin's, not this interface's, so the audit
// frames a blank page where the node would serve the plugin's.

const plugin = (name, title, ui, { offered_on = ['personal'], icon = true } = {}) => ({
  name,
  repo: `cosmicspork/${name}`,
  state: 'active',
  active: { version: '0.2.0', digest: `sha256-${name}`, tools: [], ui, ui_title: ui ? title : null, icon: ui && icon },
  instances: [],
  offered_on,
})

// Workboard is offered on two channels, hoja on one, hora on none and with
// no icon (the generic glyph), sql-review has no page.
const PLUGINS = {
  plugins: [
    plugin('hoja', 'hoja', true),
    plugin('workboard', 'Workboard', true, { offered_on: ['personal', 'work'] }),
    plugin('hora', 'hora', true, { offered_on: [], icon: false }),
    { ...plugin('sql-review', 'SQL', false), active: { version: '0.3.1', tools: ['sql-review__query'], ui: false, ui_title: null } },
  ],
}

// A small square icon, as the node would serve a plugin's favicon.
const ICON = (fill) => ({
  status: 200,
  type: 'image/svg+xml',
  body: `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><rect width="16" height="16" rx="3" fill="${fill}"/></svg>`,
})

const NODE_ID = '9f31c6a870d24b5e8c1f0a6d3e7b2905a4c8d1e6f0b3a7c2d5e8f1a4b7c0d3e6'
// The navigation with Workboard and hora added, Workboard moved up and
// Memories hidden (lib/nav.ts, stored per node).
const CUSTOM_NAV = JSON.stringify([
  { kind: 'builtin', id: 'home' },
  { kind: 'plugin', name: 'workboard' },
  { kind: 'builtin', id: 'tasks' },
  { kind: 'builtin', id: 'sessions' },
  { kind: 'builtin', id: 'docs' },
  { kind: 'builtin', id: 'memories', hidden: true },
  { kind: 'builtin', id: 'plugins' },
  { kind: 'builtin', id: 'usage' },
  { kind: 'plugin', name: 'hora' },
])
const withNav = new Function(`try { localStorage.setItem('tracon-nav:${NODE_ID}', ${JSON.stringify(CUSTOM_NAV)}) } catch {}`)
const ICONS = {
  '/api/plugins/workboard/icon': ICON('#7fa6c9'),
  '/api/plugins/hoja/icon': ICON('#7fb59a'),
}

const opened = (name, title) => ({
  url: 'about:blank',
  origin: 'http://127.0.0.1:7440',
  title,
  channel: 'personal',
  expires_ms: 3_600_000,
})

export default [
  {
    id: 'plugins-list',
    area: 'plugins',
    route: '/plugins',
    title: 'Plugins: one on two channels, one on one, one on none',
    note: 'A row opens the plugin on its default channel; Workboard has a button for its other channel; hora is offered on no channel and shows the generic glyph. Only plugins whose manifest has [ui] are listed.',
    since: '#1150',
    api: { '/api/plugins': PLUGINS, ...ICONS },
  },
  {
    id: 'plugins-empty',
    area: 'plugins',
    route: '/plugins',
    title: 'Plugins: none with a page',
    api: { '/api/plugins': { plugins: [PLUGINS.plugins[3]] } },
  },
  {
    id: 'plugins-page',
    area: 'plugins',
    route: '/plugins/workboard',
    title: 'Plugin page: Workboard full page on its default channel',
    note: 'The frame fills main: no heading, no padding, no border. The tab title names the channel.',
    since: '#1150',
    api: { '/api/plugins': PLUGINS, 'POST /api/plugins/workboard/ui': opened('workboard', 'Workboard'), ...ICONS },
  },
  {
    id: 'plugins-page-channel',
    area: 'plugins',
    route: '/plugins/workboard?channel=work',
    title: 'Plugin page: Workboard on the channel its address names, from a nav entry',
    since: '#1150',
    init: withNav,
    api: { '/api/plugins': PLUGINS, 'POST /api/plugins/workboard/ui': opened('workboard', 'Workboard'), ...ICONS },
  },
  {
    id: 'plugins-page-not-offered',
    area: 'plugins',
    route: '/plugins/hoja?channel=work',
    title: 'Plugin page: a channel the plugin is not offered on',
    note: 'An error, not a silent fall back to another channel.',
    since: '#1150',
    api: { '/api/plugins': PLUGINS, ...ICONS },
  },
  {
    id: 'plugins-page-refused',
    area: 'plugins',
    route: '/plugins/hoja',
    title: 'Plugin page: opened off the node’s machine',
    api: {
      '/api/plugins': PLUGINS,
      ...ICONS,
      'POST /api/plugins/hoja/ui': {
        status: 409,
        body: {
          error: {
            code: 409,
            message: "plugin UIs are served on the node's loopback only: open them on the node's own machine",
          },
        },
      },
    },
  },
  {
    id: 'plugins-nav-custom',
    area: 'plugins',
    route: '/',
    title: 'Navigation: Workboard added and moved up, Memories hidden',
    note: 'A plugin entry shows its icon and title; hora is in the list but skipped while it is offered on no channel. On a phone the first four fill the bar.',
    since: '#1150',
    init: withNav,
    api: { '/api/plugins': PLUGINS, ...ICONS },
  },
  {
    id: 'plugins-nav-more',
    area: 'plugins',
    route: '/more',
    title: 'More: the visible entries the phone bar has no room for, then Settings',
    since: '#1150',
    sizes: ['phone'],
    init: withNav,
    api: { '/api/plugins': PLUGINS, ...ICONS },
  },
]
