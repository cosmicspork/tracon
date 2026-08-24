# Phase 1 spikes

Run 2026-08-24 on macOS (aarch64) with Podman 6.1.0, applehv machine, rootless, SELinux
enabled in the VM. Each spike was a throwaway driver, not code that ships.

## 1. Setting the model over ACP

`session/set_config_option {sessionId, configId: "model", value: "<option value>"}` against
`omp acp` 18.0.4 returns the full `configOptions` with `currentValue` updated. `session/close`
returns `{}` and the process exits 0 after stdin closes. Only `openai-codex/*` models appear
in `configOptions[id=model]` at `session/new`; the list changed between two runs
(`gpt-daybreak-blue-latest` appeared), so the node treats `config_option_update` as
authoritative and re-probes on demand.

## 2. Boundary on a Podman machine

- `podman network create --internal --disable-dns --subnet 10.89.0.0/24 tracon-int`.
- Gateway (`containers/gateway`) on `podman` + `tracon-int:ip=10.89.0.2`; tinyproxy with the
  reference config and an anchored-regex `allow.txt`; socat forwarding `10.89.0.2:7421` to
  `TCP:host.containers.internal:7421`.
- A host listener bound to `127.0.0.1:7421` **is** reachable from the internal network through
  the gateway under applehv. No non-loopback bind needed.
- From a `--cap-drop=ALL --security-opt=no-new-privileges` container on `tracon-int` only:
  direct HTTPS egress fails; via `HTTPS_PROXY` an allowlisted host connects, an unlisted host
  is refused (`000`, no upstream connection in the log); `git ls-remote https://github.com`
  is blocked.
- `/private/tmp` and `/Users` are visible inside the VM; bind mounts work without
  `--security-opt label=disable` despite SELinux being enabled.
- Bind-mounting a file over `~/.omp/agent/AGENTS.md` does **not** mask it when the target is a
  symlink (the dangling link survives). Mounting the whole `~/.omp` is therefore wrong. The
  runner mounts a node-owned, otherwise empty state dir as `/root/.omp` and bind-mounts only
  `agent.db`, `agent.db-wal`, `agent.db-shm` into `/root/.omp/agent/`, alongside a
  materialized `config.yml`. Verified: no `AGENTS.md` inside the container.
- The host `~/.bun/bin/omp` is a darwin Mach-O binary and cannot be mounted into the Linux
  VM. The harness image downloads `omp-linux-<arch>` from the GitHub release at the pinned
  version instead (`containers/harness/Containerfile`).

## 3. Allowlist under a real prompt

`omp acp` in the harness image, inside the boundary, `podman run --rm -i` driven from the
host over the remote socket: initialize → session/new → set model
`openai-codex/gpt-5.6-luna` → prompt "reply pong" → `end_turn`, usage
`{inputTokens: 15019, outputTokens: 5, totalTokens: 15024}` → close. Stdin/stdout framing
through the remote client behaved.

Gateway log for the run: `CONNECT chatgpt.com:443` ×4 (established), one `CONNECT
zenmux.ai:443` refused. omp probes that provider at startup; it is not needed and stays
denied. Starting allowlist:

```
^api\.anthropic\.com$
^api\.openai\.com$
^chatgpt\.com$
^auth\.openai\.com$
```
