<script lang="ts">
  // Every component in @tracon/ui/svelte, in the states worth looking at.
  // The scheme switch at the top shows them in light and dark; `?theme=light`
  // or `?theme=dark` opens on one.
  import { applyTheme, type Theme } from '../theme'
  import { Banner, Empty, LoadGate, Row, SectionHeader, Segmented, SideButton, Toggle, WordDiff } from '../svelte/index.js'

  let theme = $state<Theme>((new URLSearchParams(location.search).get('theme') as Theme) || 'auto')
  $effect(() => applyTheme(theme))

  let choice = $state<'one' | 'two' | 'three'>('two')
  let on = $state(true)
  let off = $state(false)
  let dismissed = $state(false)
</script>

<main>
  <header>
    <h1>@tracon/ui/svelte</h1>
    <Segmented
      label="Colour scheme"
      value={theme}
      options={[
        { value: 'auto', label: 'Match system' },
        { value: 'light', label: 'Light' },
        { value: 'dark', label: 'Dark' },
      ]}
      onchange={(t) => (theme = t)}
    />
  </header>

  <SectionHeader detail={3}>Row
    {#snippet aside()}<a href="#rows">all rows</a>{/snippet}
  </SectionHeader>
  <div class="rows" id="rows">
    <Row href="#open" title="Open">
      {#snippet lead()}<span class="dot ok"></span>{/snippet}
      <span class="t">A link row<small>personal · 12 calls · running</small></span>
      {#snippet actions()}<SideButton onclick={() => (dismissed = !dismissed)}>{dismissed ? 'undo' : 'dismiss'}</SideButton>{/snippet}
    </Row>
    <Row onclick={() => {}} expanded={false}>
      {#snippet lead()}<span class="dot"></span>{/snippet}
      <span class="t">A button row, full width with no side buttons<small>work · 1 call</small></span>
    </Row>
    <Row href="#plugin" minHeight={52}>
      <span class="t">A plugin page<small>workboard 0.4.0 · personal</small></span>
      {#snippet actions()}<SideButton href="#work" label="Open on work">work</SideButton><SideButton href="#ops" label="Open on ops">ops</SideButton>{/snippet}
    </Row>
    <Row off>
      <span class="t">Not available<small>not offered on any channel</small></span>
    </Row>
  </div>

  <SectionHeader>Banner</SectionHeader>
  <Banner detail="every external agent's calls on it are refused">broker access stopped on personal
    {#snippet action()}<button class="lnk" type="button">Start broker access</button>{/snippet}
  </Banner>
  <Banner tone="crit" role="alert" detail="database is locked">Could not load plugins</Banner>
  <Banner tone="ok">Hub reconnected</Banner>
  <Banner tone="dim" detail="local sessions continue">Hub unavailable</Banner>

  <SectionHeader>Empty</SectionHeader>
  <Empty>Nothing has ended yet.</Empty>
  <Empty loading>Loading sessions…</Empty>
  <Empty>No sessions running. {#snippet action()}<a href="#start">Start one.</a>{/snippet}</Empty>

  <SectionHeader>Segmented</SectionHeader>
  <Segmented
    label="Example"
    value={choice}
    options={[
      { value: 'one', label: 'One' },
      { value: 'two', label: 'Two' },
      { value: 'three', label: 'Three' },
    ]}
    onchange={(v) => (choice = v)}
  />

  <SectionHeader>Toggle</SectionHeader>
  <div class="toggles">
    <Toggle checked={on} title="Open the window at launch" hint="Off: menu bar only." onchange={(v) => (on = v)} />
    <Toggle checked={off} title="Hide the dock icon while closed" onchange={(v) => (off = v)} />
  </div>

  <SectionHeader>LoadGate</SectionHeader>
  <LoadGate status={{ loaded: false, error: null }} what="work">rows</LoadGate>
  <LoadGate status={{ loaded: false, error: 'database is locked' }} what="work" onretry={() => {}}>rows</LoadGate>
  <LoadGate status={{ loaded: true, error: 'connection reset' }} what="work"><Empty>The last answer, kept under the error.</Empty></LoadGate>

  <SectionHeader>WordDiff</SectionHeader>
  <WordDiff before="Rotate the gateway signing key before the March expiry." after="Rotate the gateway signing key before the 14 March expiry, then publish it." />
  <WordDiff before="unchanged" after="unchanged" />
</main>

<style>
  main {
    max-width: 760px;
    margin: 0 auto;
    padding: 24px 16px 64px;
    display: grid;
    gap: 10px;
  }
  header {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
  }
  h1 {
    font: 600 18px var(--sans);
    margin: 0;
  }
  .rows {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .dot {
    flex: none;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--dim);
  }
  .dot.ok {
    background: var(--ok);
  }
  .t {
    display: grid;
    gap: 2px;
    min-width: 0;
    font-weight: 500;
  }
  .t small {
    font: 12px var(--mono);
    color: var(--dim);
  }
</style>
