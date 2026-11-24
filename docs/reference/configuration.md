# Configuration

`node.toml` is optional; every key has a default. When the node writes it (Settings,
`tracon repo set`, `tracon mesh init`, an approved egress ask), it edits the file in place:
your comments, key order and formatting stay, only the keys that changed are touched, and a
result that would not load is refused before anything is written. Each write is recorded
with who made it and which keys it changed, and pushed to your subscribed devices
("node.toml changed: ... by ...").

Keys that name a program the node runs (`podman`, `gh`, `glab`, `git`, the
builder's `buildctl`) or a file or directory it reads, sends on or writes
(`embed.api_key_file`, the builder's certificate, registry and token paths,
`docs.export_dir`, `worktree_root`, `repo_roots`,
`state_mount`) are held to a path guard, so neither an agent nor a stolen session can
point one at something an agent wrote or at a secret. In short:

- a program is an absolute path or a bare name on the node's `PATH`, is called exactly
  what its key runs, and lives where no agent can write and only root can, or under a
  package manager's prefix (Homebrew, `~/.local/bin`, `~/.cargo/bin`), where your own
  user may;
- a secret file lives in `~/.config/tracon/secrets/` (on macOS, `secrets/` beside
  `node.toml`) or `/etc/tracon/`;
- documents export into the node's own export area (`~/.local/share/tracon/exports` on
  Linux), or into a directory you marked by creating a `.tracon-export` file in it,
  which must be outside anywhere an agent can write.

An edit that breaks a rule is refused with the reason. A file that already breaks one
still starts, with a warning naming the key; the node refuses to run such a program or
write such a directory until it is fixed, and still reads a key file you placed outside
the secrets directory, until you move it. The rules in full are in
`docs/ARCHITECTURE.md` ("The path guard").

Some keys apply to the running node at once: `[[repo]]`, the `[session]` defaults (budget,
timeouts, default channel, claim grace, suspend-after), `[publish]`, `[github] dispatch`,
`[supervision]`, `[review]`, `[notify] contact`, `[memory] promote_at`, provider prices,
`[runtime] approved_images` and the `[docs]` export keys, from the next session, turn,
check or push. The rest wait for a restart, and every screen of the interface says which
until then ("node.toml changed: restart to apply ..."), with one **Restart**. The same
control is in Settings › System › Restart on every node, whether or not a change is
waiting (a restart into an unchanged file clears what the node holds in memory); the card
also says how this node restarts, what the restart applies, and why it cannot where it
cannot. With nothing running and no session live, **Restart** restarts at once. Otherwise
one step names the running turns, checks, reviews and plugin instances and the live
sessions a restart ends, and offers **Restart when they finish** (it stops taking new
sessions, turns and review submissions and waits for the running work, 30 minutes at
most, then gives up and says why), **End them and restart now**, or Cancel. A restart
that is waiting can be withdrawn (**Don't restart**) or hurried along. What is running is
kept current as turns start and end. The same is `POST /api/admin/restart` with
`{"mode": "when_idle" | "now", "expected_hash": "<GET /api/restart's config.hash>"}`. It
is an administrator's action, confirmed with a passkey from anywhere but the node itself,
and works under the user service and in a container restarted whenever it exits
(`TRACON_RESTART_BY_EXIT=1`, set by the Kubernetes manifests), where the node exits for
the kubelet to start it again.

A node keeps the last file it ran well on as `node.toml.last-good` (following live edits
once they have run), and puts it back after three starts in a row that died before
serving, when the file is to blame: it does not load, or a key that waits for a restart
differs from last-good's. The failed one is kept as `node.toml.failed-<ms>`; see
[RECOVERY.md](../RECOVERY.md). The full set, with defaults:

```toml
node_name = "<hostname>"            # how this node is named in the mesh

[harness]
id = "claude"                       # "claude" or "native": the harness for a model the node cannot
                                    # classify; the rest follow their credential. An unknown id
                                    # refuses to start; a retired one (omp, opencode) is migrated to
                                    # "claude" as the file loads, with the original kept beside it
                                    # (docs/RECOVERY.md).
version = "2.1.295"                 # pinned; empty means the version this node's release
                                    # carries. Checked twice — `--version` in the runner, and the
                                    # handshake's own report — and a session whose harness reports
                                    # another version, or a protocol revision this node was not
                                    # written against, fails with that reason rather than running.
tools = []                          # the only tools a session may use; empty is the harness's own set
                                    # (a list without the harness's shell leaves nothing to commit,
                                    #  so nothing to review)

[boundary]                          # the rootless-Podman boundary a Linux host establishes
podman = ""                         # empty: found on PATH, then the usual install locations
# stop_timeout_secs = 10            # between a stopped harness container's SIGTERM and SIGKILL; also
                                    # the pod's terminationGracePeriodSeconds
gateway_container = "tracon-gw"     # the name a sandbox reaches the gateways by, on its own
                                    # loopback. network, subnet, gateway_ip, gateway_image,
                                    # harness_image and start_machine configured what older
                                    # releases ran; they are ignored, with a line in the log, but
                                    # for naming what an upgraded node removes
auto_reconcile = true               # set up the boundary at start and when what it fetches
                                    # changes; false leaves it to `tracon setup`
resource_prefix = "tracon"          # every runtime volume and container this node names starts
                                    # with it and a `-`: the shared harness state, the boundary
                                    # probes, each session's and check's own. Only for a second
                                    # node on the same container host (see "A scratch node"); set
                                    # it before that node first runs, since a change strands the
                                    # volumes the old one named. It may not begin with "tracon-"

[gateway]
allow_hosts = ['^api\.anthropic\.com$', '^api\.openai\.com$', '^chatgpt\.com$',
               '^auth\.openai\.com$']  # harness egress; subscription sign-in runs in the node
proxy_port = 8888
forward_port = 7421                 # the harness's way to the node: models, tools, and the
                                    # workspace's read-only `origin` (/git/<channel>/<owner>/<name>.git)
# harness_listen = "127.0.0.1:7421" # or a socket path; the platform default is right
# qa_proxy_port = 8890              # where a container reaches the node's per-client egress
                                    # proxy: preparation and sessions. Each presents its own
                                    # credentials; the name is from the retired QA browser
# egress_port = 7424                # where the node serves that proxy when harness_listen is a
                                    # TCP address; with a socket path it is a socket beside it.
                                    # A pod-hosted node's sandboxes reach it through their
                                    # tunnels, and its BuildKit daemon at the node pod's address

[session]
budget_tokens = 2000000             # per session
permission_timeout_secs = 900       # a harness's own unanswered ask is a deny
approval_expiry_secs = 86400        # a brokered call held for the operator expires unanswered
# harness_api_timeout_secs = 30     # a call the gateway forwards to a harness that takes longer is
                                    # recorded as unknown, and reconciled by asking the harness
# default_channel = "work"          # the channel the composer starts on; empty is no preference
claim_grace_secs = 60               # a review claim lapses this long after the client vanishes
suspend_published_after_secs = 1800 # a published session idle this long is suspended; 0 never
auth_wait_secs = 86400              # a session refused by an expired sign-in waits this long for a new one; 0 always
# worktree_root = "/private/tmp"    # on macOS; the system temp directory elsewhere

[runtime]
kind = "podman"                     # Linux; or "kubernetes", for a pod-hosted node, or "apple",
                                    # a Mac's default and the only kind it runs: its sandboxes
                                    # are VMs (see "On a Mac with Apple Containerization").
                                    # "podman" on a Mac is refused
# [runtime.apple]
# container = ""                    # Apple's container CLI; empty for /usr/local/bin/container
# limit = 4                         # most sandboxes at once; unset for no limit of its own
# cpus = 4                          # each sandbox VM's CPUs and memory ("4G"); unset or empty
# memory = ""                       # for container's defaults
# build_timeout_secs = 1800         # how long one container build of a repository image may take
# [runtime.podman]                  # every harness, check and preparation runs under tracon-sandbox
                                    # from the harness bundle, over the sandbox protocol, with
                                    # --network none: its only route the protocol's socket, and the
                                    # MCP and model gateway, the allowlist proxy (served by the
                                    # node) and the per-client egress proxy tunnels to the node on
                                    # the gateway's ports. Workspaces and harness state are on the
                                    # sandbox's own disk, sent in at start and read back over the
                                    # protocol (exports live; the session's own state at each turn
                                    # end, every 60 s in a turn, at exit). Sandboxes run the stock
                                    # base ([sandbox] base_image), or a repository's image built
                                    # FROM ${TRACON_SANDBOX_BASE} (an `image` or another Dockerfile
                                    # is refused), as the base's user 65532, mapped to the node's own
                                    # (--userns=keep-id), with HOME=/home/harness; a repository's
                                    # prepare runs once and is snapshotted (podman commit). Each
                                    # [[service]] runs inside the session's sandbox. The
                                    # sandbox_protocol, sandbox_files, sandbox_tunnels,
                                    # sandbox_images and sandbox_services switches that chose this
                                    # are gone: one set to false is refused at load
# sandbox_init = false              # start tracon-sandbox under podman's --init, not as PID 1
# [sandbox]
# bundle = "/path/to/<version>/amd64" # the harness bundle (scripts/harness-bundle.sh --sandbox)
                                    # holding bin/tracon-sandbox, mounted read-only at /opt/tracon;
                                    # set at the node only. With no bundle, the node fetches its release's bundle into <state>/bundles,
                                    # checked against its .sha256 and manifest
# base_image = "ghcr.io/cosmicspork/tracon-sandbox-base:<version>" # pulled for this host's
                                    # architecture only and pinned by that platform's manifest
                                    # digest (asked of the registry; an image it cannot resolve is
                                    # pinned as the runtime holds it); a single-platform digest is
                                    # used as given;
                                    # a repository Dockerfile stands on it with
                                    # FROM ${TRACON_SANDBOX_BASE}
# release_url = "https://github.com/cosmicspork/tracon/releases/download" # where bundles come from
# resume_within_secs = 120          # how long a run over the sandbox protocol whose connection
                                    # dropped waits for its sandbox to redial (see "When a
                                    # sandbox's connection drops"); 0 ends the run with the
                                    # connection, as before resuming existed
# [runtime.kubernetes]              # namespace (for sandbox pods; empty, the node pod's own),
                                    # state_mount (where the node's own claim is mounted),
                                    # gateway_host (the name a sandbox resolves to its loopback).
                                    # harness_image, state_claim, uid and harness_home configured
                                    # the harness pods, which were removed: ignored, with a line in
                                    # the log. Every sandbox runs as the base's user 65532
# start_timeout_secs = 900          # (under [runtime.kubernetes]) how long a pod may take to be
                                    # scheduled, pull its image and start before the run is given
                                    # up with the reason it was waiting. Only the start: a started
                                    # check or preparation runs until it ends or its own timeout
# unschedulable_grace_secs = 60     # (under [runtime.kubernetes]) how long a sandbox pod may stay
                                    # unschedulable before the node acts: a session's pod is placed
                                    # again with this runtime full, a check's or preparation's fails
                                    # with "could not be scheduled: <the scheduler's message>"
# image_pull_secrets = []           # (under [runtime.kubernetes]) Secrets sandbox pods pull with; empty
                                    # leaves them to the namespace's default service account
# shm_size = "1Gi"                  # (under [runtime.kubernetes]) memory-backed /dev/shm for browsers;
                                    # counts against the memory limit, so it must fit within it
# [runtime.kubernetes.resources.session]   # a session's pod; [...resources.command] is a check's
# cpu_request = "250m"              # or a preparation's (500m, 2Gi, 4Gi), [...resources.probe] a
# memory_request = "1Gi"            # tool probe's (100m, 128Mi, 512Mi, no /dev/shm).
# memory_limit = "4Gi"              # Every pod renders its requests and memory limit, so a
# cpu_limit = "2"                   # LimitRange never fills them in; no CPU limit unless one is
                                    # set (cpu_limit). A limit below its request is refused at load
                                    # Each pod is labelled tracon.dev/kind (session, check, prepare,
                                    # probe) and tracon.dev/sizing (the table that sized it), plus
                                    # repo, channel, harness and phase where they apply
# [runtime.kubernetes.pool]         # the nodes sandbox pods run on, in place of the node pod's own
                                    # machine ("Sandbox pool"); left out, they stay beside it
# node_selector = { workload = "tracon-sandbox" } # labels a node must carry; the node adds
                                    # kubernetes.io/arch for its base's architecture
# tolerations = [{ key = "workload", value = "tracon-sandbox", effect = "NoSchedule" }]
                                    # the pool's taints; operator "Equal" (default) or "Exists"
# scale_up_grace_secs = 600         # how long a pod in the pool may stay unschedulable, from the
                                    # start, in place of unschedulable_grace_secs, so a pool at zero
                                    # has time to add a node; a NotTriggerScaleUp from the cluster
                                    # autoscaler still gives it up after unschedulable_grace_secs
# internal_cidrs = []               # (under [runtime.kubernetes]) address blocks the per-client
                                    # egress proxy refuses besides the private ones (RFC 1918,
                                    # 100.64.0.0/10, fc00::/7), which it always refuses in a
                                    # cluster: name a pod, service or node range that is public
                                    # (on a cloud, the nodes' and load balancers' public IPs),
                                    # and any NAT64 (64:ff9b::/96) or 6to4 (2002::/16) range the
                                    # cluster uses, since an IPv4 address inside one is not decoded
                                    # sandbox_protocol is always on: `false` is refused at load,
                                    # naming the way over ("As a Kubernetes pod"); `true` can go
# sandbox_port = 7426               # (under [runtime.kubernetes]) where sandboxes dial the node pod,
                                    # and where their init containers fetch the bundle
# [runtime.kubernetes.builder]      # build repository images on a cluster ("Images on Kubernetes");
                                    # left out, a pod-hosted node builds nothing
# addr = "tcp://buildkitd.tracon-build.svc:1234"  # BuildKit's mutual-TLS listener
# tls_dir = "/etc/tracon/buildkit"  # the client certificate: ca.crt, tls.crt, tls.key
# registry = "registry.digitalocean.com/<registry>"  # host/namespace built images are pushed to
# docker_config = "/etc/tracon/registry"  # a directory holding config.json with the push credential
# timeout_secs = 2700               # one build's limit; left out, 45 minutes
# buildctl = "buildctl"             # the client to run; the node image carries one on PATH
# no_cache = false                  # true: build without BuildKit's cache, for a daemon shared with others
# do_token_file = "/etc/tracon/do/token"  # DigitalOcean only: an API token for removing old images,
                                    # when the registry refuses a delete; left out, the credential's own
# approved_images = []              # digest-pinned project images preparation may use besides the stock base

# [[repo]]                          # one repository's environment (also Settings → Repositories and
                                    # `tracon repo set`, which apply without a restart); everything but
                                    # `path` is optional and
                                    # falls back to the node-wide answer. Yours, never the repository's:
                                    # a candidate that could edit it could pick its own checks
# path = "github.com/owner/name"    # an absolute path is that repository's root, a relative one a path
                                    # suffix, so a managed clone is named without the clone root. The
                                    # first matching entry wins
# image = "localhost/toolchain@sha256:…"  # what its checks and preparation run in. Digest-pinned, always:
                                    # a check's evidence is keyed on the image identity the runtime
                                    # confirmed, and a tag is not an identity. A locally built image has
                                    # a digest too (`podman image inspect` reports `RepoDigests`); the
                                    # node refuses to start on a tag. Without one, the stock base,
                                    # which carries no project toolchain. Refused on every runtime,
                                    # since each runs every repository on the stock base: use `build`
                                    # steps and `prepare` (Settings → Repositories names each refusal)
# dockerfile = ".devcontainer/Dockerfile"  # in place of `image`: the node builds this from the
                                    # repository's default branch, pins what it built, and rebuilds
                                    # when the file or its context changes there. Refused unless
                                    # its first FROM is ${TRACON_SANDBOX_BASE}
# context = "."                     # the build context; left out, the Dockerfile's own directory
# build = ["apt-get update", "apt-get install -y --no-install-recommends php8.5-cli"]
                                    # in place of `image` and `dockerfile`: steps run as root, one
                                    # `RUN` each, on the pinned stock base ([sandbox] base_image),
                                    # into one image that ends as the base's user 65532 in /work.
                                    # They reach only `egress`, over HTTPS. Rebuilt when the base,
                                    # a step or the egress changes (see "Repository recipes on the
                                    # stock base")
# checks = ["just check"]           # in place of [supervision] checks. With `prepare`, the warm-up
                                    # also runs them on the default branch to build it, keeping the
                                    # output under /cache (CARGO_TARGET_DIR=/cache/target,
                                    # GOCACHE=/cache/go-build) in the snapshot; each check starts from
                                    # it, read-only beneath a writable layer of its own
# timeout_secs = 1800               # in place of [supervision] timeout_secs; covers preparation, and
                                    # each command of the warm-up's build
# prepare = ["bun install --frozen-lockfile"]  # run once before a run's checks, on a copy each check
                                    # is then copied from, with a cache writable and `egress` reachable;
                                    # and on a session's workspace before its harness starts
# egress = ["npm"]                  # what `prepare` and `build` may reach: crates, npm, pypi, packagist,
                                    # github, apt (the stock base's apt sources and Ubuntu's mirrors),
                                    # or a literal host name. Empty is no egress at all
# session_egress = true             # open `egress` to this repository's sessions too, so an agent can
                                    # add a dependency. Off by default: a session that can reach a host
                                    # that accepts uploads can upload to it
# prepare_credentials = ["nova"]    # broker credentials (kind `env`) whose environment is lent to the
                                    # default branch's preparation when the image is built, and to
                                    # nothing else. Needs `dockerfile` or `build`; each credential must also list
                                    # this repository, or a `group/**` above it, in its own `repos`
# service_volumes = ["postgres"]    # catalogue services whose data this repository keeps between
                                    # sessions: each gets a volume of its own, mounted writable at
                                    # /var/lib/tracon-services/<name> in every session's sandbox on every
                                    # runtime, and taken back when the sandbox ends, after its services
                                    # stop. A service not named starts empty in each session. Only the
                                    # operator resets one (Settings → System → Service data,
                                    # `tracon repo reset-volume`); a sweep removes it with the
                                    # repository's data once no entry names the repository

# [upstreams]                       # credentials the node attaches to requests for one declared host,
                                    # so the client never holds them (see "Attaching a credential
                                    # instead of lending it")
# preparation = "env"               # how prepare_credentials reach the warm-up: "env" lends their
                                    # environment, "attach" lends nothing and attaches each through
                                    # its [[upstreams.host]]
# [[upstreams.host]]
# host = "nova.laravel.com"         # exactly this host, reached over the node's own TLS on 443
# credential = "nova"               # a broker credential of kind `env`
# attach = "composer"               # "basic" (username, password), "header" (header, value), or
                                    # "composer" (its COMPOSER_AUTH entry for the host); values name
                                    # the credential's variables as ${NAME}, never a secret itself
# sessions = false                  # attach it for sessions too, in channels the credential is bound to

# [placement]                       # where sessions may run and what each channel and repository
                                    # requires of the sandbox provider. Decided as each session starts
                                    # once DigitalOcean is on or a requirement is set; the session's
                                    # log says where it runs and why. A review's checks overflow the
                                    # same way when the node has no room for them, client work never
# digitalocean = "off"              # off | overflow (only once every local provider is full) | prefer.
                                    # A node whose own boundary is not verified asks first: a card
                                    # offers DigitalOcean, and nothing moves without it
# local_limit = 4                   # sessions on this node's own runtime at once; one more is full.
                                    # On Kubernetes an extra cap: the cluster's room counts too
                                    # (see "Room and overflow" under "As a Kubernetes pod")
# [placement.channel.acme]          # one channel's requirements
# client = true                     # client work: route must be node-only, and no open resolver
# forbid_digitalocean = true
# [[placement.repo]]                # one repository's, matched as a [[repo]] path is
# path = "github.com/acme/site"
# client = true
# [placement.repo.require]          # each capability's accepted values; left out, any. Where a
                                    # channel, a repository and client work all speak, the
                                    # stricter wins
# tier = ["microvm"]                # container | microvm
# route = ["node-only"]             # node-only | unverified
# dns = ["none", "node"]            # none | node | provider-open
# privilege = ["unprivileged"]      # unprivileged | root-in-vm (asked only of a container)
# runtime_socket = ["absent"]       # absent | present
# image_identity = ["exact"]        # exact | derived
# memory = "4Gi"                    # not a capability but a floor: the least memory each sandbox
                                    # is made with, as a Kubernetes quantity ("4Gi", not "4GiB").
                                    # The larger of channel's and repository's wins. Read by
                                    # DigitalOcean alone (the cheapest size in its catalog with at
                                    # least this much, raising [digitalocean] size or its default);
                                    # it never refuses a provider, and local runtimes ignore it.
                                    # Settings shows the size each floor resolves to

# [digitalocean]                    # DigitalOcean's Harness Runtime, for sessions [placement] sends
                                    # there. Off without a credential. Each sandbox is a microVM whose
                                    # egress admits one destination: node_addr, so its only route is
                                    # this node, or (relay with no node_addr) the hub's host, which is
                                    # not this node alone (see relay). DNS is the runtime's, open, so
                                    # client work never goes. The runtime's init starts the supervisor
                                    # as root; it becomes the stock base's user (uid 65532, home
                                    # /home/harness, working in /work) before it starts anything, as
                                    # on Podman and Apple, with no-new-privileges; a sandbox whose
                                    # supervisor says it runs as anyone else (an image built from an
                                    # older release) is refused. Every process gets the node's allowlist
                                    # proxy through the tunnel, not the runtime's. A [[service]] with a
                                    # bundle runs inside, from the template's copy; one with only an
                                    # image is refused there. Settings' capability view shows how it is
                                    # reached and why it cannot take a session. Nothing is mounted
                                    # there: its trees and gateways go over the sandbox protocol
# credential = "do"                 # a broker credential holding DIGITALOCEAN_ACCESS_TOKEN
# registry = "registry.digitalocean.com/acme"  # where the node pushes the template sources it
                                    # builds: its pinned base and each repository's prepared snapshot,
                                    # with the bundle it verified, each [[service]] bundle it holds
                                    # (the release's browser is fetched for it) and the tracon-sandbox
                                    # service (containers/do-sandbox). A Podman node builds and pushes
                                    # with the token; a Kubernetes node over the sandbox protocol
                                    # builds on [runtime.kubernetes.builder]'s BuildKit and pushes with
                                    # its credential, so this must be that builder's registry. amd64
                                    # nodes only (all the runtime runs); Apple takes them from a
                                    # peer. Each is registered as a template once per image digest
                                    # and what goes
                                    # into it (about seven minutes), as part of preparing it, never per
                                    # session, so a release or a new browser bundle makes a new one and
                                    # the old is swept; until a snapshot's is ready its sessions there
                                    # run in the base's
                                    # A node with registry also makes templates for peers that ask
                                    # (do_template_prepare) by portable key: the base by its release
                                    # (index digest), the repository by forge identity, its recipe
                                    # (build or Dockerfile, prepare, egress, lockfiles), the harness
                                    # release and each service bundle's name and version. It refuses
                                    # a key its own recipe does not make (accept the peer's recipe
                                    # offer first), lists each such template in its hello, names it
                                    # <prefix>-tp-<key> (its own sessions in that image start from it
                                    # too) and keeps it 14 days past the last peer's request
                                    # On a Podman node on a mesh, a registry under
                                    # registry.digitalocean.com also takes each repository image it
                                    # builds (<prefix>-repo-images:<recipe>-<arch>), whose digest it
                                    # signs for its channels' other members: a Podman co-member with a
                                    # DigitalOcean credential pulls that digest for the same recipe
                                    # instead of building it, and builds it itself, saying why in the
                                    # build log, without access or on any mismatch
# templates = "own"                 # own (image or registry) | peer: a node that cannot build amd64,
                                    # which is all the runtime runs (an Apple node), takes ready
                                    # templates a peer with registry lists for its keys, and asks it
                                    # to make one it lacks; meanwhile sessions there run in the base's,
                                    # and Settings says "preparing on <peer>". Name neither image nor
                                    # registry; needs [mesh] hub_url
# builder = ""                      # with templates = "peer": the one peer (node name or id) to take
                                    # templates from; empty for any that lists them
# image = "…@sha256:…"              # or one image of your own for every session there, built with
                                    # containers/do-sandbox from this release's bundle (and any
                                    # service bundles, which the node takes on trust); name this or
                                    # registry, not both
# base_template = "coding-base"     # the runtime's base the image is rebased onto (DigitalOcean
                                    # no longer accepts the old name "sandbox")
# region = ""                       # the API host's region; empty for the default
# api_url = ""                      # the API's base URL, for a stand-in; empty for DigitalOcean's
# size = ""                         # a sandbox size slug; empty for the cheapest size in the
                                    # runtime's catalog with at least 2 GiB (mars-2vcpu-2gb today),
                                    # never the runtime's own 1 GB default. A smaller slug set here
                                    # is kept; a [placement] memory floor raises either. A slug the
                                    # catalog does not list, a floor no size meets, or (with no slug
                                    # set, or a floor) a catalog the node has never read refuses the
                                    # sandbox with why; a slug set here lets sandboxes start without
                                    # the catalog, unpriced. Settings shows the size, its memory and
                                    # price
# limit = 4                         # sandboxes there at once
# listen = "0.0.0.0:8443"           # where the node takes its sandboxes' TLS connections
# node_addr = "203.0.113.7:443"     # what they dial as raw TLS: an address, not a name. A load
                                    # balancer's 443 forwarding to listen, or the hub relay listener's
                                    # address. Empty with relay: they dial the hub's own URL instead
# relay = false                     # reached through the hub relay ([mesh.sandbox_relay], whose
                                    # target is listen); its name for this node is the SNI. With no
                                    # node_addr, sandboxes open wss://<[mesh] hub_url host>/v0/relay/connect
                                    # through the runtime's proxy, and egress admits that host alone.
                                    # That host also reaches the hub's own public routes (/v0/info,
                                    # /v0/enroll/{code}) and, behind a shared front (a CDN), any other
                                    # site on it, so the route reads as unverified. Needs a hub that
                                    # serves /v0/relay/connect (tracon-hub 0.49.0 or later); an older
                                    # one is named in the provider's health and DigitalOcean is not used
# server_name = ""                  # SNI to send otherwise; empty is tracon-node

# [[service]]                       # something a session may start in its sandbox by name
                                    # (`service_start`), never by path or command. The sandbox's
                                    # supervisor runs it on the sandbox's loopback: reached on its
                                    # 127.0.0.1, reaching only what the session does, stopped with the
                                    # session's sandbox. The policy bundle decides each by `name`; the
                                    # shipped one runs `browser` unattended and asks about any other
# name = "browser"
# bundle = "release"                # required: what is at /opt/tracon-services/<name>, read-only, in
                                    # every sandbox (mounted on Podman and Apple; on Kubernetes a
                                    # session's pod copies it in from the node, "As a Kubernetes
                                    # pod"; on DigitalOcean it is baked into the templates the node
                                    # builds). A port one of the sandbox's tunnels listens on is
                                    # refused at service_start. An entry with only an `image`, which
                                    # ran as a container beside the sandbox, is refused at load; an
                                    # `image` beside a bundle is ignored, with a line in the log.
                                    # "release", on `browser` only: the browser bundle of the node's
                                    # own release (tracon-browser-<chrome>-<arch>.tar.gz, <chrome> the
                                    # headless-shell containers/browser/Containerfile pins), which the
                                    # node fetches from [sandbox] release_url when it sets up its
                                    # boundary, checks against its .sha256 and manifest, and keeps in
                                    # <state>/bundles/browser/<chrome>/<arch>, removing other Chromes.
                                    # Otherwise an absolute path to a directory of binaries for this
                                    # host (one <version>/<arch> of scripts/browser-bundle.sh, say, or
                                    # of scripts/database-bundle.sh for `postgres` on 5432 and `redis`
                                    # on 6379, whose `run` keeps its data in $TRACON_SERVICE_DATA when
                                    # the repository keeps it: see `service_volumes` under [[repo]]).
                                    # Set at the node only
# command = []                      # arguments after `run`
# port = 9222                       # where the session reaches it
# ready = "/json/version"           # an HTTP path answering 2xx once ready; left out, a TCP connect
# timeout_secs = 60                 # how long each start may take to answer before the supervisor
                                    # counts it failed
# run = []                          # what to start; default /opt/tracon-services/<name>/run,
                                    # followed by `command`
# env = {}                          # environment added to the sandbox's own, and
                                    # TRACON_SERVICE_DATA=/var/lib/tracon-services/<name>, which exists
                                    # only when the session's repository keeps the service's data
# restart = "on-failure"            # "never", "on-failure" or "always"; a restart is not a failure

# [[mcp_server]]                    # an MCP server outside the node (streamable HTTP). Sessions see
                                    # its tools on the node's own MCP endpoint as `<name>__<tool>`,
                                    # deferred behind tool search like the node's other tools; only
                                    # the node speaks to the server. Applies without a restart
# name = "linear"                   # 1-24 lowercase letters, digits or '-'
# url = "https://mcp.linear.app/mcp"
# channels = ["personal"]           # who is offered its tools; empty means nobody
# credential = "linear"             # a broker credential whose TOKEN is sent as
                                    # `Authorization: Bearer <TOKEN>`; bound to the calling channel too
# header = "X-Api-Key"              # send TOKEN as it is in this header instead
# allow = ["list_*", "get_*"]       # by the server's own tool names, `*` a wildcard: run unattended
                                    # unless a signed policy rule denies them (and the bundle is valid)
# ask = ["get_secret"]              # always put to the operator, with the exact arguments
# deny = ["delete_*"]               # refused. Deny wins over ask, ask over allow; a tool no list names
                                    # is asked unless a policy rule allows `<name>__<tool>` by name
# timeout_secs = 60                 # how long one call may take

# [[provider_cli]]                  # a provider's command-line tool the node runs on its own host for
                                    # an agent (`provider_cli_run`), which names only the tool and its
                                    # arguments. Applies without a restart
# name = "aws"                      # 1-24 lowercase letters, digits or '-'
# binary = "/usr/local/bin/aws"     # an absolute path or a bare name on the node's PATH, held to the
                                    # rules for a program the node runs (checked again before every
                                    # run); never a shell or an interpreter, by name or by link
# credential = "aws-ops"            # a broker credential of kind `env`, bound to the calling channel
                                    # too: its variables are the program's environment, never its
                                    # arguments. Left out, it runs with no credential
# channels = ["ops"]                # who is offered it; empty means nobody
# allow = ["s3 ls", "cloudfront create-invalidation"]  # argument prefixes, word by word: a run that
                                    # begins with one is decided by the signed policy and its grants
                                    # like any tool call; any other run is put to the operator. A
                                    # signed denial refuses either way
# timeout_secs = 120                # how long one run may take before it is killed

# [plugins]
# owners = ["cosmicspork"]          # GitHub owners whose releases `tracon plugin add` may install;
                                    # changed at the node only. A local archive is not held to it
# ui_port_first = 7440              # loopback ports plugin pages are served on, one per plugin with
# ui_port_last = 7459               # a UI, kept for it once given; needs a restart

# [[plugin]]                        # your settings for an installed plugin, by name; may come before
                                    # the install
# name = "sql-review"
# idle_timeout_secs = 900           # how long it may sit idle before it is stopped; 0 is the default
# timeout_secs = 60                 # how long one call may take; 0 is the default (60)
# allow = ["query"]                 # by the plugin's own tool names, as [[mcp_server]]'s lists
# ask = []
# deny = ["drop_*"]
# require_preview = true            # Allow on its cards needs the plugin's preview of the call as it
                                    # would run to have been shown; false allows without one
                                    # (for a plugin with no previews). Reject is always open
# channels = ["personal"]           # where it is offered and run, within what its credentials allow;
                                    # unset is every such channel, [] is none. Elsewhere its tools are
                                    # not listed, calls, pages, previews and format conversions are
                                    # refused, and no process starts. Applies without a restart; a
                                    # process already running on a channel dropped from it stops when idle

[providers.anthropic]               # anthropic, openai and openai-codex are built in; add others the same way
credential = "anthropic"
upstream = "https://api.anthropic.com"
shape = "anthropic"                 # or "openai", "openai-codex"; the shape also fixes which
                                    # methods and paths the gateway will lend the credential to:
                                    # inference, token counting, embeddings and the models list,
                                    # never the provider account behind the key
# login = "anthropic"               # the subscription sign-in the node runs: "anthropic", or "openai" for ChatGPT/Codex;
                                    # a sign-in per set of channels is held under `credential`, and
                                    # each further one under `<credential>@<channel>`
# models = [                        # what the picker offers and tracon-agent is told; a built-in provider
#   { id = "claude-opus-5-5", name = "Claude Opus 5.5", context = 1000000, output = 128000, reasoning = true, attachment = true },
#   # parallel_tools = false, reasoning_effort = "high": read by the native harness only (parallel is on by default)
# ]                                 # left without models gets the current generation; `models = []` declares none
# [providers.anthropic.price]
# input_per_mtok = 3.0
# output_per_mtok = 15.0

[publish]                           # the binaries the node runs to publish an approved review
gh = "gh"                           # the node image carries pinned gh and glab releases on PATH
glab = "glab"                       # (gh 2.49 or later, as `gh attestation verify` needs); a node
git = "git"                         # on a host runs whichever its service's PATH finds
forge_timeout_secs = 15             # how long one gh or glab call may run
commits = "squash"                  # one commit of the reviewed tree; "keep" pushes the agent's commits
warn_direct = true                  # warn when a review pushes straight to its base, no change opened
# [publish.style]                   # every rule off unless set; a repository entry may carry its own
# conventional = true               # type(scope): subject, types from `types` or the usual set
# imperative = true                 # "add", not "added" or "adds"
# max_subject = 72
# kebab_branch = true               # feat/lowercase-words
# no_ticket_keys = true             # no PROJ-123 in the subject or branch

[github.dispatch]                   # the workflows run_dispatch may start, by repository; none unless listed
# "owner/name" = ["renovate.yaml"]  # file names in .github/workflows; read at startup

[mesh]
# hub_url = "https://hub.example.com"   # set by tracon mesh init / enroll
heartbeat_secs = 60                 # a hello when something changed; snapshots every 10th; the member list every 5th
poll_secs = 30
command_timeout_secs = 15
# stream_open_timeout_secs = 30     # an owner stream that has not answered its open is closed, never re-sent
# stream_idle_secs = 120            # a stream with no frame this long is closed by either end
# stream_max_body_bytes = 8388608   # the largest request body a stream carries to an owner (8 MiB)
# stream_max_response_bytes = 268435456  # the largest response it carries back, an event stream included (256 MiB)
# stream_max_concurrent = 16        # owner streams this node may have open at once
# renew_credentials = true          # on an always-on node: renew shared subscription sign-ins first
# admin_peers = ["<node id>"]       # peers that may read and edit this node.toml and sign its providers in,
                                    # each action confirmed with a passkey there; changed here, never by a peer

[mesh.sandbox_relay]                # reached by remote sandboxes through the hub; off unless enabled.
                                    # They dial the hub's relay listener, or its own URL as a WebSocket;
                                    # either way the TLS inside is theirs to this node
# enabled = true
# target = "127.0.0.1:7443"         # the local sandbox listener the hub's relayed TLS is bridged to
# max_streams = 16

[memory]
promote_at = "02:00"                # the nightly promotion batch

[supervision]
checks = ["just check"]             # run against a snapshot of the workspace before a review is accepted
timeout_secs = 900
# max_snapshot_bytes = 536870912    # the largest Git snapshot imported for one check run (512 MiB)
# dependency_inputs = {}            # named identities of inputs a check depends on ("node" = "22.11");
                                    # changing one deliberately invalidates reuse of a check's result

[work]
default_priority = 2                # what a new item gets when nobody says
# [[work.priority]]                 # the named levels people pick from; set, the list replaces the default
# name = "Urgent"                   # one whole (Urgent 4, Priority 3, Planned 2, Backlog 1)
# value = 4

# [spend]                           # what sandboxes on DigitalOcean may cost
# infra_usd_per_day = 10.0          # US dollars of sandbox time per local day; once reached, no new
                                    # session goes there until the day turns (running ones are not
                                    # stopped). Unset or 0: no limit

[review]
max_diff_lines = 10000              # added lines (removed ones do not count, though the whole diff is held to
                                    # ten times this); a bigger submission is refused before any check runs.
                                    # An update to an open change counts only what it adds to the change's branch
max_files = 200
generated_details = false           # a description an approval sends opens with words you write; the agent's
                                    # text follows, collapsed under "Generated details"

[notify]
# contact = "mailto:you@example.com"    # what a push service may write to about this sender; falls back to [auth] public_origin

[auth]
# public_origin = "https://tracon.example.org"  # where remote browsers reach this node; https: on a domain name (http: for localhost)

[external]                          # a harness you run yourself, using this node's tools; off unless enabled
enabled = false
repo_roots = ["~/src"]              # a worktree put up for review must belong to a repository under one of these
check_run = false                   # true: offer the check_run tool, which runs the repository's checks or one
                                    # command in its image on the node's own copy of the commit and hands back
                                    # their output, attached to nothing; needs a [[repo]] entry. Nothing else
check_submissions = false           # true: run the required checks behind each submission, as evidence for the
                                    # reviewer (a failed one shows on the card and never refuses the submission),
                                    # and make `auto` the external review trigger where a channel sets none.
                                    # Off, a reviewer started on an external submission runs them first and waits
                                    # for them (none with no [[repo]] entry: the card says why, and it reads the diff)
                                    # run_containers (0.60) is still read, with a warning, as setting both; a new key
                                    # set beside it wins for that key. run_checks before it likewise

[docs]
# export_dir = "~/notes"            # Markdown only: <slug>.md, archived ones under archive/;
                                    # the node's export area, or a directory holding a
                                    # .tracon-export file you created (the path guard)
export_channel = ""                 # empty: [session] default_channel
export_every_secs = 1800            # at startup, then this often (at least 60)
preview_listen = "127.0.0.1:7422"   # isolated HTML capability origin
# preview_url = "https://preview.example.com" # public origin when a reverse proxy terminates TLS

# Document kinds: a slug's prefix files it (`runbook-deploy` is a runbook). Written
# out, the list replaces the default one whole: guide, ref, architecture, proposal,
# and the working notes repo, note, meeting and inbox.
# [[docs.kind]]
# prefix = "runbook"                # lowercase letters and digits
# label = "Runbook"                 # what the Documents screen calls it
# description = "how to run something here"   # what doc_write tells an agent it is for
# working_note = false              # true: the shipped policy writes it unasked

# [gc]                              # tracon gc --apply on a timer; off unless every_secs is set
# every_secs = 86400                # 0 or unset: off; less than 3600 is 3600
# archive_ended_after_secs = 86400  # archive this node's ended sessions untouched this long
                                    # first; unset: free only what is archived already
# caches = false                    # true: dependency caches too, as tracon gc --caches

# [metrics]                         # a Prometheus scrape target, GET /metrics; unset, nothing listens
# listen = "127.0.0.1:7425"         # unauthenticated and read-only: loopback on a host; "0.0.0.0:7425"
                                    # only in a pod behind a NetworkPolicy that admits the scraper alone.
                                    # A port another listener of the node has is refused at load

[embed]                             # semantic search; off unless enabled
enabled = false
base_url = "http://127.0.0.1:8080"  # an OpenAI-shaped /v1/embeddings
model = "bge-m3"
dim = 1024                          # must match the model; changing it rebuilds the index
# api_key_file = "<home>/.config/tracon/secrets/llama-server.key"  # only under secrets/ or /etc/tracon
# provider = "anthropic"            # instead of base_url: through the gateway, so the channel ceiling applies
batch = 16
timeout_secs = 60
min_similarity = 0.45               # cosine floor for a semantic match; model-dependent
context_tokens = 2048               # tokens one input may hold: --ctx-size / --parallel, at most --ubatch-size
```

For `[embed]`, a local `llama-server --embedding -m <model>.gguf` is enough; BGE-M3
and Qwen3-Embedding-0.6B (which wants `pooling = last`) are the models it was built
against. Without an endpoint, search is text-only and the Documents screen says so.
`context_tokens` caps every chunk at 7/8 of that many bytes (a tokenizer makes no
more tokens than bytes), so a paragraph too long for the cap is split at line ends,
and a line too long for it is cut, rather than refused by the server on every sweep.
Set it to what one llama-server slot holds: `--ctx-size` divided by `--parallel`,
and no more than `--ubatch-size`.

`[metrics] listen` serves `GET /metrics` in the OpenMetrics text format on a
listener of its own: no credential, no input, nothing of the operator's API, and
the shipped harness NetworkPolicy does not admit its port. The families: pod
refusals and start failures by kind, sizing and reason, pod start time, runs in
flight, sessions held by state, session starts, ends, readiness and duration,
checks and preparations with their durations, repository image builds, the
gateway's model calls and tokens by provider, model and channel, and
`tracon_build_info`. Every label value comes from an enum, a configured
repository, channel or provider, or the model catalogue (any other model is
`other`); never a session id, pod name, branch, prompt, path or error text, so
every series recurs. Per-session detail is the cluster's: join its per-pod
series to `kube_pod_labels{label_tracon_dev_session=...}`. On Kubernetes,
kube-state-metrics needs
`--metric-labels-allowlist=pods=[tracon.dev/kind,tracon.dev/sizing,tracon.dev/repo,tracon.dev/channel,tracon.dev/harness,tracon.dev/phase,tracon.dev/session]`
for those labels to reach it.

The shipped node NetworkPolicy (`deploy/kubernetes/base/networkpolicy-node.yaml`)
admits ingress only from the node's own sandboxes, and only on 7426, so an
in-cluster Prometheus cannot reach 7425 until an overlay adds a rule admitting the
scraper alone, for example:

```yaml
# overlay patch to the tracon-node NetworkPolicy: append one ingress rule
- op: add
  path: /spec/ingress/-
  value:
    from:
      - namespaceSelector:
          matchLabels:
            kubernetes.io/metadata.name: monitoring
        podSelector:
          matchLabels:
            app.kubernetes.io/name: prometheus
    ports:
      - protocol: TCP
        port: 7425
```

HTML bundle previews are served from `preview_listen`, never the operator
origin, and embedded with `sandbox="allow-scripts"`. Set `preview_url` when a
reverse proxy exposes that listener; it must be an HTTP(S) origin distinct from
the operator origin. Preview capabilities expire after an hour idle and are
invalidated when their document is replaced or deleted. The preview policy
permits inline and bundle-local scripts, styles, images, and media, but denies
network connections, forms, nested frames, workers, and plugins. Fonts that
must work in the opaque-origin sandbox should be embedded as `data:` URLs.

In Documents, import an HTML file or folder, replace it, open a full-window
preview, or download the original file or ZIP bundle. Imports allow up to 256
files, 20 MiB total, and 16 MiB per file. A folder needs a root `index.html` or
one unambiguous HTML entry; browsers without folder selection can import a
single file. External fonts, CDNs, and live API connections are intentionally
blocked, not fetched automatically. HTTPS operator access needs a distinct HTTPS
preview origin configured with `preview_url`.

A document's kind is its slug's prefix, read against `[[docs.kind]]` whenever the
document is listed, searched or exported, not taken from what it was filed as when
written. So a kind added, renamed or removed applies at once to every document
already there, with no step to reclassify them: a removed kind's documents show as
`other`, and the export leaves their files alone, as it does any file without a
kind's prefix. The reverse holds too: adding a kind puts the `<prefix>-*.md` files
already in the export directory under the export's management, so a file there that
is not a document is deleted at the next export. Two kinds are the node's own and cannot be configured: `shown` (what
`show_work` wrote) and `plan`.
An entry that names one of them, `other`, or a prefix already listed, is refused.
`doc_write` and `doc_search` offer agents the list as it is, and the Documents screen
groups and filters by it (`GET /api/docs/kinds`).

CLI import and export are Markdown-only. Agents manage documents of either
format: `doc_write` takes `format` (`markdown`, the default, or `html`), and
writing the other format converts the document in place, keeping its id,
archived and pinned state. HTML from `doc_write`, `PUT /api/docs/{ch}/{slug}`
with `"format": "html"`, or `tracon doc put --format html` is one page, stored
through the same writer, path checks and limits as an import and served only
from the preview origin; replacing a multi-file bundle drops its other files,
and the approval's title says how many. Markdown over an HTML document retires
its bundle. `doc_edit` changes a Markdown document by exact-match replacements
(`edits`, 1 to 64 `{old_str, new_str}`, applied in order, all or none, each
`old_str` matching exactly once), so a one-line change does not resend the whole
body; it requires `if_hash`, refuses HTML (use `doc_write`), is approved as
`doc_write` is, and its approval shows the diff the edits make. `doc_delete` removes
a document of either format with its bundle, is always asked, and refuses shown
work. `doc_archive` (`{slug, archived, if_hash?}`) archives a document, or restores
it with `archived: false`, by the same flag-only path as `PUT /api/docs/{ch}/{slug}`
with `archived` and no body (and `tracon doc archive|unarchive <slug>`): body, hash
and bundle are unchanged. It is always asked, even for a working note, its card
names the slug, the action (archive or restore) and the document's title, and it
refuses shown work. All four take `if_hash` from `doc_read`, across a change of
format; a held call without one is pinned to the version it was asked about. A
reserved slug, an unknown format, an HTML body over the per-file limit, a
`doc_archive` without a boolean `archived`, or a `doc_edit` that does not apply is
refused before the call is held.

## Policy

What runs unasked, what is refused with a reason, and what reaches your queue is a
signed bundle, `policy.toml`, beside the node's state. `tracon policy init` writes
the shipped one (`node/src/policy/working-agreements.toml`) and signs it, replacing
whatever was there; to keep rules of your own, edit the file and run
`tracon policy sign` instead. The node reads it at startup, so restart it
(`tracon service restart`) after either, and `tracon policy push` hands it to the
rest of the mesh: it lists each peer as queued or not, and the running node
delivers to any peer that was offline when it comes back. A peer that refused the
bundle is not sent it again; once you have fixed that peer, run `tracon policy push`
again to resend. A bundle that is missing, unsigned or malformed has no rules, and
no rules means everything is asked.

A rule allows, denies, or is left to ask. Deny wins over allow whatever the order,
and anything no rule names is asked. A deny matches its patterns anywhere in the
call, arguments included, except the `title` and `body` of `submit_review` and
`submit_report`: those only reach your queue, and a write-up of a production
incident has to be able to say so. For a brokered tool an allow names the tool
exactly and can narrow it by argument, with `*` as the only wildcard:

```toml
[[rule]]
id = "unattended-notes"
verdict = "allow"
reason = "Keeping working notes current changes nothing the operator has not agreed to."
kinds = ["tool"]
matches = ["doc_write"]
args = { slug = ["note-*", "repo-*", "meeting-*", "inbox-*"] }
```

The shipped bundle's `unattended-notes` rule names no slugs. It says
`args = { slug_working_note = true }` instead, which matches a `doc_write` or `doc_edit` whose slug
is of a kind `[[docs.kind]]` marks `working_note = true`, so which documents are
working notes is said once, beside the kinds. The trade-off is deliberate: part of
what runs unattended is now decided by `node.toml`, which is not signed. An edit of
`node.toml` is already an administrative act, gated by a passkey, so this asks no
more trust of it than its other keys do. A bundle of your own that keeps slug globs
is matched exactly as before.

The shipped bundle lets the boundary do the containing. Inside a session's
container, reading, editing and running shell commands go unasked: the container
has no route out but the egress it is granted and no credentials, and nothing it
makes reaches anyone until you approve the diff. What the bundle guards is what
crosses that boundary or speaks in your name. It refuses publishing (`git push`,
`gh pr create` and the like) anywhere in a line; a session holds no forge or tracker
credential, and merging or moving a ticket is left to the brokered tools, which are
asked and need a narrow authority grant. It also refuses an edit
to the harness's settings (`.claude/settings…`), which would configure the harness of
whoever checks the branch out, and an edit tool's write into Git's internals
(`.git/`). It runs the brokered reads unattended — queries, lookups, searches,
pipeline and check status, job logs, documents, memory, the ledger —
and asks before every brokered write that others see: a comment, an edited or new
ticket, a pipeline run, a document other than working notes, a rewritten or deleted work item.
A tool whose kind the node does not know is asked. The narrative a call carries (a
review's or report's title and body, a review's forge description and comment, a ticket's summary and description, a comment,
a reply, a document's body) is text, not an action, so it is left out of deny
matching: a comment can say "production" or quote `gh pr merge`. The fields that say
where it goes are still matched.

A bundle of your own can be narrower. An allow can name commands by their leading
words (`git status`, `cat`), and then a line that chains commands with `|`, `&&`,
`||` or `;` is allowed only when every command in it would be on its own, with a
redirection, substitution or variable asked. For a file action, an allow pattern
ending in `/` covers what is under it.

Two things are allowed without a rule, which matters under a narrower bundle. One
of your required checks (`[supervision] checks`), run exactly as configured, does
not ask before it runs. And a card offers **Allow … for this session** beside Allow
and Deny: the call is answered once, and later calls in the same scope run unasked
until the session ends — a command by its tool and subcommand (`cargo test …`), a
one-word command or a chain only exactly as written, a brokered tool by name. Both
are recorded in the session's log with the rule that let the call through, and
neither reaches past a signed deny.

### Fetching pages

`web_fetch` is decided URL by URL under the kind `fetch`, with deny by default: a URL
is fetched unasked only when an allow rule naming `kinds = ["fetch"]` covers it (an
allow rule that names no kind never allows a fetch), or a pattern saved to the
channel's fetch allow list does. A pattern with a scheme is a glob over the whole URL;
one without is a glob over the host, on any scheme, port and path:

```toml
[[rule]]
id = "documentation"
verdict = "allow"
reason = "Reference documentation the agents read."
kinds = ["fetch"]
matches = ["https://docs.rs/*", "*.python.org"]

[[rule]]
id = "no-paste-sites"
verdict = "deny"
reason = "Nothing is fetched from paste sites."
kinds = ["fetch"]
matches = ["*.pastebin.com"]
```

A deny matches its patterns as globs and, as every deny does, as substrings. The
channel's own list is authority grants of the action `fetch`, whose target is a
pattern: **Save to channel** on a fetch card adds one, and **Settings → Permissions &
policies** lists, adds and revokes them like any grant. A grant denying a pattern
refuses it on that channel even where the bundle allows it; a signed deny wins over
every grant, and a saved allow fetches only while the signed bundle is intact. A
pattern that names no host of its own (`*`, `https://*`, `*.com`) is refused as a
grant: opening the whole web is a signed rule. Anything else is asked.

## Granting authority

Merging, publishing, deploying, and calling a plugin tool its manifest marks
`consequential` (jira-review's `transition`) are not something a policy rule's
argument text matches unattended — they are scoped grants, made and revoked one at
a time in **Settings → Permissions & policies**, beside the signed rules they sit
under. A grant names one target (a pull request, a merge request, a deployment job,
or one plugin call, `plugin_call` on `plugin:<plugin>__<tool>:<sha256 of its
arguments>`, which the call's card names) and, for merge, publish, and deploy, the
one commit it covers; the branch moving to a later sha makes the grant no
longer apply, rather than carrying it forward. A publish grant also binds what the
forge will show: an update's target names the change (`…:change:<n>:prose:<hash>`),
and the hash covers any separate description or comment. A publish that rewrites
a branch's history, whether or not a change is open for it, is never granted
around: it always asks. A signed policy deny always wins over a local grant,
however narrow it is scoped: what it refuses stays refused until the signed
bundle itself changes, never by granting around it. Revoking a grant, or letting it expire, takes effect on the next
action the node reads it for, not one already dispatched.

## Where things live

| | macOS | Linux |
|---|---|---|
| `node.toml` | `~/Library/Application Support/tracon/` | `~/.config/tracon/` |
| database, credentials, identity, harness volume, managed repos, plugins, scratch | `~/Library/Application Support/tracon/` | `~/.local/state/tracon/` |
| harness socket | (TCP `127.0.0.1:7421`) | `$XDG_RUNTIME_DIR/tracon/harness.sock` |
| the CLI, and the binary the service runs | `~/.local/bin/tracon` | `~/.local/bin/tracon` |
| the service | `~/Library/LaunchAgents/com.tracon.node.plist`, logs in `~/Library/Logs/tracon.log` | `~/.config/systemd/user/tracon.service` |

`TRACON_STATE_DIR` overrides the state directory outright and `TRACON_CONFIG_DIR` the
directory holding `node.toml`. `TRACON_LISTEN` or `serve --listen` moves the API off
`127.0.0.1:7420`. A node beside a real one needs all three and its own runtime names
(see [A scratch node](../guides/install.md#a-scratch-node)).
