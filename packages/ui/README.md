# @tracon/ui

Tracon's design tokens, base styles and theme bridge. The Tracon interface is
built on them, and so can a plugin page, so that it looks and themes like the
rest of the interface. Plain CSS and small TypeScript modules with no framework
dependency, plus optional Svelte 5 components built on them.

## Install

Each Tracon release attaches `tracon-ui-<version>.tgz`; the package's version is
Tracon's. Install it from the release:

```sh
bun add https://github.com/cosmicspork/tracon/releases/download/v<version>/tracon-ui-<version>.tgz
```

## Use

```ts
// main.ts
import '@tracon/ui/index.css' // tokens.css, then base.css
import { connectTheme } from '@tracon/ui'

connectTheme()
```

- `tokens.css`: the custom properties (`--bg`, `--s1`..`--s3`, `--ink`, `--ink2`,
  `--dim`, `--rule`, `--acc`, `--acc-ink`, `--wait`, `--crit`, `--ok`, the
  `--wash-*` and `--sans` / `--mono`). Dark by default; light follows the system
  unless `:root[data-theme]` says otherwise.
- `base.css`: the reset and shared primitives (`.btn`, `.lnk`, `.banner`,
  `.empty`, `.note`, `.segmented`, `.toggle`, `.chip`, `.tag`, `kbd`, ...).
- `theme`: `applyTheme(theme)`, `connectTheme()` and `THEME_MESSAGE`.
- `word-diff`, `load`, `segmented`: the framework-free helpers behind the
  components (`wordDiff` / `wordCounts`, `loadPhase`, `segmentedKey`).
- `svelte` (needs `svelte` ^5): the interface's own `Row`, `SideButton`,
  `Banner`, `Empty`, `SectionHeader`, `Segmented`, `Toggle`, `LoadGate` and
  `WordDiff`. The interface uses these same files, so a plugin page built with
  them matches it.

```svelte
<script lang="ts">
  import { Row, SideButton } from '@tracon/ui/svelte'
</script>

<Row href="/items/1">Item one{#snippet actions()}<SideButton href="/items/1/edit">edit</SideButton>{/snippet}</Row>
```

`connectTheme()` applies the `tracon_theme` query parameter the interface adds to
a plugin frame's URL, then every `{ type: "tracon/theme", theme: "light" | "dark" }`
message posted by the parent window. Anything from another source is ignored.
Opened standalone, it changes nothing and the page follows the system setting.

Colour rules: amber (`--wait`) means waiting on the operator and nothing else, red
(`--crit`) is refusal and failure, and state is a 3px bar plus a wash, never an
outline.

See docs/guides/plugin-ui.md, "Make your plugin look native", for which
component to use where, and docs/reference/plugins.md, "Looking native".

## Compatibility

Renaming or removing a token, a base class or the theme message is a breaking
change and is listed in Tracon's changelog under `@tracon/ui`. Adding one is not.

## Developing

```sh
bun install && bun test && bun run check
bun run dev    # every component in each state, dark and light, at localhost:5173
just ui-pack   # from the repository root: target/tracon-ui-<version>.tgz
```

## License

MIT (this directory only; see LICENSE here). The rest of Tracon is AGPL-3.0-only.
