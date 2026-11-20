// Plugins (/plugins) and one plugin's page (/plugins/:name): the pages
// installed plugins bring, each framed from its own loopback origin. The
// frame's own content is the plugin's, not this interface's, so the audit
// frames a blank page where the node would serve the plugin's.

const plugin = (name, title, ui) => ({
  name,
  repo: `cosmicspork/${name}`,
  state: 'active',
  active: { version: '0.2.0', tools: [], ui, ui_title: ui ? title : null },
  instances: [],
})

const PLUGINS = {
  plugins: [
    plugin('hoja', 'hoja', true),
    plugin('workboard', 'Workboard', true),
    { ...plugin('sql-review', 'SQL', false), active: { version: '0.3.1', tools: ['sql-review__query'], ui: false, ui_title: null } },
  ],
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
    title: 'Plugins: two with a page, one without',
    note: 'Only plugins whose manifest has [ui] are listed; installing is Settings › Plugins.',
    api: { '/api/plugins': PLUGINS },
  },
  {
    id: 'plugins-empty',
    area: 'plugins',
    route: '/plugins',
    title: 'Plugins: none with a page',
    api: { '/api/plugins': { plugins: [PLUGINS.plugins[2]] } },
  },
  {
    id: 'plugins-page',
    area: 'plugins',
    route: '/plugins/workboard',
    title: 'Plugin page: Workboard framed for a channel',
    note: 'The frame is sandboxed on the plugin origin; the channel picker re-opens it for another channel.',
    api: { '/api/plugins': PLUGINS, 'POST /api/plugins/workboard/ui': opened('workboard', 'Workboard') },
  },
  {
    id: 'plugins-page-refused',
    area: 'plugins',
    route: '/plugins/hoja',
    title: 'Plugin page: opened off the node’s machine',
    api: {
      '/api/plugins': PLUGINS,
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
]
