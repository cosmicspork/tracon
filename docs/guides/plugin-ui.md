# Make your plugin look native

A plugin page is framed on its own origin, full width under the interface's
navigation (Plugins › your plugin, or its own entry in the rail). It can look,
theme and behave like the rest of Tracon by building on `@tracon/ui`: the tokens,
base styles and theme bridge the interface itself uses, and, for a Svelte page, the
same components. This guide is how. The contract the page lives under (its origin,
the query, what it may call) is in [the plugin reference](../reference/plugins.md#plugin-pages).

## Setup

Each Tracon release attaches `tracon-ui-<version>.tgz`, at Tracon's version:

```sh
bun add https://github.com/cosmicspork/tracon/releases/download/v<version>/tracon-ui-<version>.tgz
```

Then, once, before anything renders:

```ts
// main.ts
import '@tracon/ui/index.css' // tokens.css, then base.css
import { connectTheme } from '@tracon/ui'

connectTheme()
```

`connectTheme()` applies the `tracon_theme` the interface puts in the frame's
query, so the first paint is right, then each theme message the interface posts
when the operator changes it. Opened outside Tracon it does nothing.

A Svelte 5 page can also use the components (Svelte is a peer dependency, so a page
that is not on Svelte keeps the CSS alone):

```svelte
<script lang="ts">
  import { Banner, Empty, LoadGate, Row, SectionHeader, SideButton } from '@tracon/ui/svelte'
</script>
```

| Component | For |
|---|---|
| `Row` | A list line: a link (`href`), a button (`onclick`) or plain (`off`), with a `lead` snippet (a status dot, an icon), its content, and side buttons in `actions`. With no side buttons it takes the whole line. |
| `SideButton` | The quiet button beside a row: dismiss, archive, open on another channel. |
| `Banner` | A line across the page: `tone` `crit` (refusal, failure), `ok`, `dim`, or plain; a bold `detail`; an `action` snippet. |
| `Empty` | Nothing to show, said in words, with an optional `action`; `loading` while it is not known yet. |
| `SectionHeader` | A section's heading with a count or detail in mono, and an `aside` on the right. |
| `Segmented` | A choice of a few, as a radiogroup; arrow keys, Home and End move it. |
| `Toggle` | A labelled switch with a title and a hint. Several go in a `<div class="toggles">`. |
| `LoadGate` | Loading, failed, or the answer, for a list you fetch (`status: { loaded, error }`). A refresh that fails after an answer keeps it under the error. |
| `WordDiff` | A text before and after, diffed by word, as an approval card shows a change. |

Run `bun run dev` in `packages/ui` of the Tracon repository to see each of them, in
either scheme. A component joins the package only once a second page needs it, and
anything that reads the node's API stays in the interface.

## Layout

- **The page owns the whole frame.** There is no margin around it: give your page
  its own padding (the interface uses 16px on a phone, 24px or so on a desktop).
- **Don't repeat what the interface already says.** The tab title and the
  navigation name the plugin, and the channel it was opened on is in its address;
  a heading that says "Workboard · personal" again only pushes the content down.
- **Leave the phone's tab bar to the interface.** The frame already sits above
  it; nothing of yours needs to reserve space for it or draw over it.
- **One column on a phone.** At 700px and below the interface stacks; a table
  that cannot fit becomes rows.

## Colour and state

- **Amber (`--wait`) means waiting on the operator, and nothing else.** Not
  "warning", not "in progress".
- **Red (`--crit`) is refusal and failure.** A thing that could not be done, or
  was not allowed.
- **State is a 3px bar plus a wash, never an outline or a border.** Use the
  `--wash-*` tokens for a row's background and a 3px bar of the matching colour at
  its left edge.
- **Surfaces step up by `--s1`, `--s2`, `--s3`,** not by borders. A row is `--s1`
  on the page's `--bg`, and `--s2` under the pointer.

## Type

- `--sans` for prose: titles, labels, sentences.
- `--mono` for what a person might copy or compare: ids, versions, counts, dates,
  paths, branch names. A row's second line is usually mono.

## Controls

- `.btn.p` for the one primary action on a screen, and only one.
- `.btn.d` only for what cannot be taken back (delete, discard).
- `.lnk` for an inline action in a sentence or a row.
- Confirm in the page, never with `confirm()`: a dialog from a framed origin is
  blocked or easy to mistake for the interface's own. Show the consequence inline
  with the action that commits it.

## Standalone development

Serve the page with your own dev server while you work. Outside Tracon
`connectTheme()` changes nothing, so the page follows the system scheme; switch
your system (or the browser's emulation) to see both. Anything that needs the
node, such as tool calls through the page's bridge, has to be stubbed there.

## Do and don't

A workboard-style list, before:

```svelte
<h2>Workboard — personal</h2>
<ul>
  {#each items as item}
    <li style="border: 1px solid orange">
      <a href={item.url}>{item.title}</a> ({item.state})
      <button onclick={() => remove(item)}>Delete</button>
    </li>
  {/each}
</ul>
```

After: no repeated title, rows with a state bar and a mono second line, a quiet
side button, and the loading and empty states said:

```svelte
<script lang="ts">
  import { Empty, LoadGate, Row, SideButton } from '@tracon/ui/svelte'
</script>

<LoadGate {status} what="the board">
  {#each items as item (item.id)}
    <Row href={item.url}>
      {#snippet lead()}<span class="bar {item.waiting ? 'wait' : ''}"></span>{/snippet}
      <span class="t">{item.title}<small>{item.key} · {item.state}</small></span>
      {#snippet actions()}<SideButton onclick={() => archive(item)}>archive</SideButton>{/snippet}
    </Row>
  {:else}
    <Empty>Nothing on the board.</Empty>
  {/each}
</LoadGate>

<style>
  .bar { align-self: stretch; width: 3px; border-radius: 2px; background: var(--dim); }
  .bar.wait { background: var(--wait); }
  .t { display: grid; gap: 2px; min-width: 0; }
  .t small { font: 12px var(--mono); color: var(--dim); }
</style>
```
