# @tracon/ui

Tracon's design tokens, base styles and theme bridge. The Tracon interface is
built on them, and so can a plugin page, so that it looks and themes like the
rest of the interface. Plain CSS and a small TypeScript module, with no
framework dependency.

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

`connectTheme()` applies the `tracon_theme` query parameter the interface adds to
a plugin frame's URL, then every `{ type: "tracon/theme", theme: "light" | "dark" }`
message posted by the parent window. Anything from another source is ignored.
Opened standalone, it changes nothing and the page follows the system setting.

Colour rules: amber (`--wait`) means waiting on the operator and nothing else, red
(`--crit`) is refusal and failure, and state is a 3px bar plus a wash, never an
outline.

See docs/reference/plugins.md, "Looking native".

## Compatibility

Renaming or removing a token, a base class or the theme message is a breaking
change and is listed in Tracon's changelog under `@tracon/ui`. Adding one is not.

## Developing

```sh
bun install && bun test && bun run check
just ui-pack   # from the repository root: target/tracon-ui-<version>.tgz
```

## License

MIT (this directory only; see LICENSE here). The rest of Tracon is AGPL-3.0-only.
