<script lang="ts">
  let health = $state<string>('…')
  fetch('/api/health')
    .then((r) => r.json())
    .then((j) => (health = `node ${j.version}`))
    .catch(() => (health = 'node unreachable'))
</script>

<main>
  <h1>tracon</h1>
  <p>{health}</p>
</main>
