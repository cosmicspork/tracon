use tracon::{boundary, config, http};

use std::net::SocketAddr;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "tracon", version, about = "tracon node")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the node: boundary check, harness supervision, HTTP API and interface.
    Serve {
        /// Address for the operator API and SPA.
        #[arg(long, env = "TRACON_LISTEN", default_value = "127.0.0.1:7420")]
        listen: SocketAddr,
    },
    /// Fetch the harness bundle and pull the sandbox base this node runs, and
    /// remove the gateway an older release made.
    Setup {
        /// Accepted and ignored: an older release rebuilt its gateway and
        /// harness images with it, and the node builds none now.
        #[arg(long, hide = true)]
        rebuild: bool,
    },
    /// Verify the harness boundary and exit non-zero naming the first failed check.
    CheckBoundary {
        /// Also probe egress from inside the boundary.
        #[arg(long)]
        deep: bool,
    },
    /// Manage the signed policy bundle.
    #[command(subcommand)]
    Policy(PolicyCommand),
    /// Join a hub from an invitation URL or code printed by `tracon mesh invite`.
    Enroll {
        invitation: String,
        /// This node's name as other nodes will see it (default: hostname).
        #[arg(long)]
        name: Option<String>,
        /// The hub URL, if the invitation is a bare code rather than a URL.
        #[arg(long)]
        hub: Option<String>,
    },
    /// The mesh: this node's identity and its hub.
    #[command(subcommand)]
    Mesh(MeshCommand),
    /// Channels: the unit of tenancy, separated by key.
    #[command(subcommand)]
    Channel(ChannelCommand),
    /// The credential store: what the node brokers, sealed under its identity.
    #[command(subcommand)]
    Credential(CredentialCommand),
    /// Documents on the running node's corpus (talks to `tracon serve`).
    #[command(subcommand)]
    Doc(DocCommand),
    /// Skills in a channel's launch manifest (talks to `tracon serve`).
    #[command(subcommand)]
    Skill(SkillCommand),
    /// Sessions: a portable package this machine already holds, and a
    /// session's own harness state.
    #[command(subcommand)]
    Session(SessionCommand),
    /// Everything a node holds, as one versioned file (data contract v1):
    /// export it from the running node, and read one with nothing running.
    #[command(subcommand)]
    Data(DataCommand),
    /// Memories on the running node's corpus (talks to `tracon serve`).
    #[command(subcommand)]
    Memory(MemoryCommand),
    /// The work ledger: items, dependencies, ready work.
    #[command(subcommand)]
    Work(WorkCommand),
    /// Names for records: what the interface and every node show in place of
    /// an id. A record with no name of the operator's reads by its generated
    /// one (`session-loving-bubble-21`).
    #[command(subcommand)]
    Name(NameCommand),
    /// Who may reach this node's API from off this machine.
    #[command(subcommand)]
    Auth(AuthCommand),
    /// A harness you run yourself, outside the boundary, using this node's tools.
    #[command(subcommand)]
    External(ExternalCommand),
    /// The phones this node pushes to.
    #[command(subcommand)]
    Push(PushCommand),
    /// Run the node under the platform's supervisor, so it survives a logout,
    /// a crash, and a reboot.
    #[command(subcommand)]
    Service(ServiceCommand),
    /// Approvals and tokens per accepted change, human and agent time.
    Metrics {
        #[arg(long)]
        channel: Option<String>,
        /// Window, in days (default 30).
        #[arg(long, default_value_t = 30)]
        days: i64,
    },
    /// The repositories the node keeps an environment for, and the images it
    /// built for them (talks to `tracon serve`).
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    /// Plugins: programs released on GitHub in tracon's plugin shape, which
    /// the node installs once the operator allows an install card (talks to
    /// `tracon serve`).
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// The trail behind a commit: model, prompts, approval, policy version.
    Provenance { sha: String },
    /// Runtime volumes, state directories and images whose session,
    /// workspace, check or build is over. Lists them, images by class with
    /// their sizes; `--apply` removes them (talks to `tracon serve`).
    Gc {
        /// Remove what is listed for removal.
        #[arg(long)]
        apply: bool,
        /// Include dependency caches, which are rebuilt when next needed.
        #[arg(long)]
        caches: bool,
        /// Include the images older nodes made that nothing runs any more:
        /// harness images, and layered (`-claude`, `-native`) and superseded
        /// `-src` repository images; and untag other releases'
        /// `tracon-sandbox-base` tags on a base that stays.
        #[arg(long)]
        legacy: bool,
        /// Take images named as this node's that carry no owner label (made
        /// before the labels) for this node's own.
        #[arg(long)]
        adopt_unlabelled: bool,
        /// Also list what is kept, and why.
        #[arg(long)]
        all: bool,
    },
    /// Sandbox providers, outside any session.
    Sandbox {
        #[command(subcommand)]
        command: SandboxCommand,
    },
}

#[derive(Subcommand)]
enum SandboxCommand {
    /// Make one sandbox, connect to it, run a command in it, read its
    /// isolation back from the provider, and remove it.
    Probe {
        /// The provider to probe (apple, kubernetes, podman). Kubernetes is
        /// probed from inside the node's pod, before the node serves there.
        #[arg(long)]
        provider: String,
        /// The harness bundle for the sandbox's architecture, with
        /// bin/tracon-sandbox in it. Left out, this release's for this host,
        /// fetched and checked as the node fetches its own.
        #[arg(long)]
        bundle: Option<std::path::PathBuf>,
        /// The image to run. Left out: ubuntu:24.04, or on Kubernetes the
        /// stock sandbox base, whose own tools fetch the bundle.
        #[arg(long)]
        image: Option<String>,
        /// Leave the sandbox running afterwards.
        #[arg(long)]
        keep: bool,
        /// What to call the sandbox (Podman's container name). Refused on
        /// Kubernetes, where the probe names its pod itself.
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Subcommand)]
enum PluginCommand {
    /// Fetch the latest release of `owner/repo` (an owner in `[plugins]
    /// owners`), or read a local archive, check it, and raise its install
    /// card. Nothing is installed until the card is allowed.
    Add {
        /// `owner/repo`, or the path of a `<name>-<version>-<target>.tar.gz`.
        source: String,
        /// The channel the install card goes to. Left out:
        /// `[session] default_channel`.
        #[arg(long)]
        channel: Option<String>,
    },
    /// The plugins installed, their versions, and any waiting on a card.
    Ls,
    /// Fetch the latest release of one plugin, or of every one installed
    /// from a release, and raise a card for each that changed.
    Update {
        name: Option<String>,
        #[arg(long)]
        channel: Option<String>,
    },
    /// Remove a plugin's builds. Its data is kept unless `--purge`.
    Rm {
        name: String,
        /// Delete its data and forget it too.
        #[arg(long)]
        purge: bool,
    },
    /// Go back to the version installed before the active one.
    Rollback { name: String },
    /// The last lines a plugin's instance on a channel wrote, and what the
    /// node said of it.
    Logs {
        name: String,
        #[arg(long)]
        channel: String,
        /// How many lines.
        #[arg(short = 'n', long, default_value_t = 200)]
        lines: usize,
    },
    /// Stop a plugin's instance on a channel and start it afresh, clearing
    /// its backoff: what brings an unhealthy one back. On a channel it has
    /// not run on yet, start it there.
    Restart {
        name: String,
        #[arg(long)]
        channel: String,
    },
}

#[derive(Subcommand)]
// Parsed once per invocation; boxing `Set` would buy nothing.
#[allow(clippy::large_enum_variant)]
enum RepoCommand {
    /// The `[[repo]]` entries and the image each repository last built into.
    Ls,
    /// Build a repository's image from the Dockerfile on its default branch,
    /// and wait for it. Rebuilds an image that is current, and retries a
    /// build that failed.
    Build {
        /// A checkout's path, or the end of a managed clone's (`owner/name`).
        /// The current directory when left out.
        repo: Option<String>,
    },
    /// One entry as `node.toml` would hold it, and what it was built into.
    Show {
        /// The entry's `path`, exactly as written.
        path: String,
    },
    /// Add an entry, or change the fields named on one that exists. It
    /// applies to the next session and the next check; no restart.
    Set {
        /// The entry's `path`: an absolute repository root, or the end of a
        /// managed clone's (`github.com/owner/name`).
        path: String,
        /// A digest-pinned image, in place of a Dockerfile.
        #[arg(long, conflicts_with = "dockerfile")]
        image: Option<String>,
        /// The repository's Dockerfile, for the node to build.
        #[arg(long)]
        dockerfile: Option<String>,
        /// The build context, when it is not the Dockerfile's directory.
        #[arg(long)]
        context: Option<String>,
        /// A step run as root on the stock base, in place of an image or a
        /// Dockerfile; repeat for several. Replaces the list; one empty value
        /// clears it.
        #[arg(long = "build", conflicts_with_all = ["image", "dockerfile"])]
        build: Vec<String>,
        /// A required check; repeat for several. Replaces the entry's list.
        #[arg(long = "check")]
        checks: Vec<String>,
        /// Go back to the node-wide `[supervision] checks`.
        #[arg(long, conflicts_with = "checks")]
        inherit_checks: bool,
        /// A preparation command; repeat for several. Replaces the entry's
        /// list; one empty value clears it.
        #[arg(long = "prepare")]
        prepare: Vec<String>,
        /// A broker credential lent to the default branch's preparation;
        /// repeat for several. Replaces the list; one empty value clears it.
        #[arg(long = "prepare-credential")]
        prepare_credentials: Vec<String>,
        /// A preset (crates, npm, pypi, packagist, github, apt) or a host name;
        /// repeat for several. Replaces the list; one empty value clears it.
        #[arg(long = "egress")]
        egress: Vec<String>,
        /// Whether the entry's egress is open to its sessions too.
        #[arg(long)]
        session_egress: Option<bool>,
        /// Seconds preparation, and each check, may take.
        #[arg(long)]
        timeout_secs: Option<u64>,
        /// A catalogue service (postgres, redis) whose data the repository
        /// keeps between sessions; repeat for several. Replaces the list;
        /// one empty value clears it (the data stays until it is reset).
        #[arg(long = "service-volume")]
        service_volumes: Vec<String>,
    },
    /// Remove an entry. The repository goes back to the node-wide answer.
    Rm {
        /// The entry's `path`, exactly as written.
        path: String,
    },
    /// Each repository's persistent service volumes: what each weighs, when
    /// a session last used it, and whether one holds it now.
    Volumes,
    /// Reset a repository's data for one service: its next session starts
    /// the service empty. Refused while a session holds it.
    ResetVolume {
        /// The repository's path, or the end of it (`owner/name`).
        repo: String,
        /// The catalogue service (postgres, redis).
        service: String,
    },
}

#[derive(Subcommand)]
enum SkillCommand {
    /// Copy a skill package into the node's own storage and put it in a
    /// channel's launch manifest. `<dir>` is a directory holding a SKILL.md;
    /// `<dir>#<git-rev>` imports it as that revision recorded it. A URL is
    /// refused: the node stages bytes it has read, never a fetch the harness
    /// performs.
    Import {
        source: String,
        #[arg(long, default_value = "personal")]
        channel: String,
    },
    /// Show a channel's effective selection: what is imported, what the next
    /// launch would build from it, and what the sessions running now have.
    Ls {
        #[arg(long, default_value = "personal")]
        channel: String,
    },
    /// Remove a skill from a channel's manifest. Running sessions keep the
    /// manifest they launched with.
    Rm {
        name: String,
        #[arg(long, default_value = "personal")]
        channel: String,
    },
}

#[derive(Subcommand)]
enum DocCommand {
    /// Import Markdown from a docs directory: flat `<kind>-<slug>.md` files,
    /// and those under `archive/` as archived.
    Import {
        dir: std::path::PathBuf,
        #[arg(long, default_value = "personal")]
        channel: String,
    },
    /// List documents, optionally on one channel.
    Ls {
        #[arg(long)]
        channel: Option<String>,
    },
    /// Print a document's body.
    Get {
        slug: String,
        #[arg(long, default_value = "personal")]
        channel: String,
    },
    /// Create or replace a document from a file (or stdin with `-`). Writing
    /// the other format converts it in place; `html` is one self-contained
    /// page, stored as a single-file bundle.
    Put {
        slug: String,
        file: std::path::PathBuf,
        #[arg(long, default_value = "personal")]
        channel: String,
        #[arg(long, value_parser = ["markdown", "html"], default_value = "markdown")]
        format: String,
    },
    /// Delete a document.
    Rm {
        slug: String,
        #[arg(long, default_value = "personal")]
        channel: String,
    },
    /// Archive a document: kept, and read by slug, but out of the list,
    /// search and the pinned orientation. Its body and hash are unchanged.
    Archive {
        slug: String,
        #[arg(long, default_value = "personal")]
        channel: String,
    },
    /// Restore an archived document.
    Unarchive {
        slug: String,
        #[arg(long, default_value = "personal")]
        channel: String,
    },
    /// Mirror every Markdown document on a channel into `<slug>.md`, with
    /// archived ones under `archive/`. HTML documents are reported and skipped.
    /// A removed document's old file is removed; unrelated files remain.
    Export {
        dir: std::path::PathBuf,
        #[arg(long, default_value = "personal")]
        channel: String,
    },
    /// Throw this node's vector index away and build it again from the corpus.
    /// Vectors are derived data: never backed up, never replicated, and always
    /// reconstructible from the documents and memories this node holds.
    Reindex,
}

#[derive(Subcommand)]
enum DataCommand {
    /// Write everything the running node holds to a file, as data contract
    /// v1: one JSON record per line, signed by the node. Credentials and
    /// private keys are left out; transcripts are not, and can carry
    /// anything that passed through a session.
    Export {
        /// Where to write it. Left out: `tracon-export-<ms>.jsonl` here.
        #[arg(short, long)]
        out: Option<std::path::PathBuf>,
    },
    /// Read an export without a node, a store, or a harness: a count of each
    /// kind and whether the signature holds, `--jsonl` for the records, or
    /// `--documents DIR` to write its documents out as document export does.
    /// Reading grants nothing and imports nothing.
    Show {
        export: std::path::PathBuf,
        /// Print the records, one JSON object per line.
        #[arg(long)]
        jsonl: bool,
        /// Only records of this kind (with `--jsonl`).
        #[arg(long)]
        kind: Option<String>,
        /// Write the export's Markdown documents to `<DIR>/<channel>/<slug>.md`.
        #[arg(long)]
        documents: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
enum SessionCommand {
    /// Read a portable session package (the file a session export downloads)
    /// without a node, a store, or a harness: a summary, or `--jsonl` for one
    /// tagged JSON record per line.
    Show {
        package: std::path::PathBuf,
        /// One JSON object per line, each with a `kind`.
        #[arg(long)]
        jsonl: bool,
    },

    /// Put every session that ran a retired harness away read-only.
    ///
    /// The migration step when a harness is retired. Each session keeps its
    /// harness identity and version, its transcript, its evidence and its
    /// workspace; what it loses is the ability to run, which it had already
    /// lost when the harness was removed. Pending approvals on them are
    /// closed, because nothing can answer them. Idempotent.
    ArchiveLegacy {
        /// The retired harness id. Defaults to every harness this build no
        /// longer has.
        #[arg(long)]
        harness: Option<String>,
    },
    /// Carry an archived session's work forward under a supported harness.
    ///
    /// A new session, with a new id, on the same workspace and branch,
    /// recording where it came from and opening with an explicit handoff
    /// note. The archived session is untouched and stays readable.
    Reopen {
        id: String,
        /// The harness the new session runs: either supported one, since the
        /// node holds an image for each.
        #[arg(long, default_value = "claude")]
        harness: String,
        /// The model it runs (`provider/model`). Defaults to the archived
        /// session's.
        #[arg(long)]
        model: Option<String>,
    },
    /// Carry on a session a node restart cut off: a new session on the same
    /// workspace, branch and harness, opening with a handoff note.
    Continue { id: String },
    /// What a session saved before each compaction and restart of its
    /// harness: its full-text conversation, its harness's transcript and its
    /// scratchpad. With an index, that save's conversation; with `--export`,
    /// that save whole as one JSON file.
    Saved {
        id: String,
        /// The save to read, as the list numbers it.
        index: Option<u32>,
        /// Write the save, every file in it, as JSON to this path.
        #[arg(long, requires = "index")]
        export: Option<std::path::PathBuf>,
    },
    /// Fork a session: a new session from its branch and context, on a
    /// branch of its own, here or on another node of its channel, while the
    /// session itself keeps running with its own task.
    ///
    /// The fork opens knowing it is a fork of the session at its head
    /// commit, with your note. It names its source, and the source records
    /// the fork.
    Fork {
        id: String,
        /// The node to start the fork on (a name or id). Defaults to this
        /// node.
        #[arg(long)]
        to: Option<String>,
        /// The fork's branch. Defaults to the source's with `-fork`.
        #[arg(long)]
        branch: Option<String>,
        /// What the fork is for, given to it in its first prompt.
        #[arg(long, default_value = "")]
        note: String,
        /// The model it runs. Defaults to the source's.
        #[arg(long)]
        model: Option<String>,
        /// Its token budget. Defaults to the source's.
        #[arg(long)]
        budget: Option<i64>,
        /// An existing task for the fork (the source keeps its own).
        #[arg(long, conflicts_with = "new_task")]
        task: Option<String>,
        /// Make a new task with this title for the fork.
        #[arg(long)]
        new_task: Option<String>,
    },
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// Write the unit and start the node. Safe to run again after an upgrade.
    Install,
    /// Stop the node and remove the unit. State and credentials are untouched.
    Uninstall,
    /// What the supervisor says about it.
    Status,
    /// Restart the node under its supervisor, onto the binary the unit names.
    Restart,
}

#[derive(Subcommand)]
enum PushCommand {
    /// Every device subscribed at this node, and whether it still answers.
    Ls,
    /// Forget a device; the phone stops hearing from this node.
    Rm { id: String },
    /// Push "notifications are on" to every device, and say what came back.
    Test,
}

#[derive(Subcommand)]
enum AuthCommand {
    /// Issue an operator token, printed once. Until one exists this node
    /// answers only to loopback; with one, a client that presents it gets a
    /// cookie and can reach the node from anywhere.
    ///
    /// The node changed is the one `TRACON_URL` names, else `TRACON_LISTEN`,
    /// else the one serving this state directory, else 127.0.0.1:7420.
    Issue {
        /// The address clients use to reach this node (an ingress, a tailnet
        /// HTTPS name), printed in the login link and QR; it does not choose
        /// which node is changed (set TRACON_URL for that).
        #[arg(long, value_name = "URL")]
        public_url: Option<String>,
        /// The old name of --public-url, kept for one release.
        #[arg(long, hide = true, conflicts_with = "public_url")]
        url: Option<String>,
        /// Change the token without asking; needed without a terminal.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Remove the operator token and log every client out: loopback only again.
    Revoke {
        /// Remove the token without asking; needed without a terminal.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// What is logged in, and whether a token is set at all.
    Sessions,
}

#[derive(Subcommand)]
enum WorkCommand {
    /// Add an item.
    Add {
        title: String,
        #[arg(long, default_value = "personal")]
        channel: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value = "")]
        body: String,
        /// Items this one waits on, comma-separated ids (prefixes are fine).
        #[arg(long, value_delimiter = ',')]
        dep: Vec<String>,
        /// A level's name or a number; higher runs first. Unset: the
        /// node's `[work] default_priority`.
        #[arg(long)]
        priority: Option<String>,
    },
    /// List the ledger with derived readiness, in the ready order.
    Ls {
        #[arg(long, default_value = "personal")]
        channel: String,
        /// A project's name or id, or `none`.
        #[arg(long)]
        project: Option<String>,
        /// ready | blocked | in_session | published | open | closed
        #[arg(long)]
        state: Option<String>,
        /// Words every item must contain in its title or body.
        #[arg(long)]
        query: Option<String>,
    },
    /// Change an item: only the fields given change.
    Edit {
        id: String,
        #[arg(long, default_value = "personal")]
        channel: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long, conflicts_with = "body_file")]
        body: Option<String>,
        /// Read the body from a file, or `-` for stdin.
        #[arg(long)]
        body_file: Option<std::path::PathBuf>,
        /// A level's name or a number; higher runs first.
        #[arg(long)]
        priority: Option<String>,
        /// A project's name or id, or `none` to clear it.
        #[arg(long)]
        project: Option<String>,
    },
    /// Ready work: what a session may pick, in order.
    Ready {
        #[arg(long, default_value = "personal")]
        channel: String,
        #[arg(long)]
        project: Option<String>,
    },
    /// Show one item, its sessions, and what was discovered from it.
    /// Ids here and below may be a unique prefix or the `task-…` name `ls` prints.
    Show {
        id: String,
        #[arg(long, default_value = "personal")]
        channel: String,
    },
    /// Close an item; a session holding it ends at its next turn end.
    Close {
        id: String,
        #[arg(long, default_value = "personal")]
        channel: String,
    },
    /// Add a dependency: `dep <id> --on <id>`.
    Dep {
        id: String,
        #[arg(long)]
        on: String,
        #[arg(long, default_value = "personal")]
        channel: String,
    },
    /// Delete an item.
    Rm {
        id: String,
        #[arg(long, default_value = "personal")]
        channel: String,
    },
    /// Start a plan or execute session on an item, here or on a peer: what
    /// the Composer does, from a terminal. The session starts in the item's
    /// project repository; the node still refuses an item that is not ready,
    /// is held by another session, or (for execute) has no plan.
    Start {
        id: String,
        #[arg(long, default_value = "personal")]
        channel: String,
        /// Unset: `plan` when the item has no plan document yet, else
        /// `execute`.
        #[arg(long, value_parser = ["plan", "execute"])]
        phase: Option<String>,
        /// A node's name, a unique prefix of its id, or `self`. Unset: the
        /// node this command talks to.
        #[arg(long)]
        node: Option<String>,
        /// Unset: the branch of the item's latest session, so execute
        /// carries on where the plan left off; with none, the node names a
        /// new `feat/tracon-…` branch.
        #[arg(long)]
        branch: Option<String>,
        /// The forge branch a new branch starts from instead of the default.
        #[arg(long)]
        base: Option<String>,
        /// `provider/model`. Unset: the channel's binding for the phase.
        #[arg(long)]
        model: Option<String>,
        /// Unset: the node's `[harness] id`.
        #[arg(long)]
        harness: Option<String>,
        /// The session's token budget. Unset: the node's default.
        #[arg(long, value_name = "TOKENS")]
        budget: Option<i64>,
        /// The forge URL to start in. Unset: the item's project's remote.
        #[arg(long, value_name = "URL")]
        repo: Option<String>,
        /// Print the session row the node returned.
        #[arg(long)]
        json: bool,
        /// The id this start is sent with. Unset: a new one. Give the one an
        /// earlier start printed when its node did not answer in time: the
        /// node answers with the session that start made, if it made one,
        /// instead of starting a second.
        #[arg(long, value_name = "ID")]
        request_id: Option<String>,
    },
}

#[derive(Subcommand)]
enum NameCommand {
    /// Name a record (a session, review, report, approval, manifest,
    /// transfer or check); every node in the mesh shows it.
    Set {
        /// session, review, report, approval, manifest, transfer or check.
        kind: String,
        /// The record: its id, a prefix of six or more characters, or a name.
        record: String,
        /// The new name: one line, at most 64 characters.
        name: String,
    },
    /// Put a record back to its generated name.
    Rm {
        kind: String,
        /// The record: its id, a prefix of six or more characters, or a name.
        record: String,
    },
    /// What an id, a prefix or a name refers to, of any kind.
    Show {
        /// An id, a prefix of six or more characters, or a name.
        record: String,
    },
}

#[derive(Subcommand)]
enum MemoryCommand {
    /// List memories on a channel.
    Ls {
        #[arg(long, default_value = "personal")]
        channel: String,
        #[arg(long)]
        state: Option<String>,
    },
    /// Add a directive (or another kind with --kind).
    Add {
        body: String,
        #[arg(long, default_value = "personal")]
        channel: String,
        #[arg(long, default_value = "directive")]
        kind: String,
        #[arg(long, default_value = "global")]
        scope: String,
        #[arg(long)]
        scope_ref: Option<String>,
    },
    /// Remove a memory by id, or a prefix unique on the channel.
    Rm {
        id: String,
        #[arg(long, default_value = "personal")]
        channel: String,
    },
    /// Recall, as a session would.
    Recall {
        query: String,
        #[arg(long, default_value = "personal")]
        channel: String,
    },
    /// Build the promotion batches now instead of tonight.
    Batch {
        #[arg(long)]
        now: bool,
    },
}

#[derive(Subcommand)]
enum CredentialCommand {
    /// Names, kinds, and bindings. Never values.
    Ls,
    /// Add the credentials from a plaintext TOML file (owner-only), sealing
    /// them into the store. Existing names are replaced. A node running on
    /// this machine is handed them, and uses them at once.
    Import { file: std::path::PathBuf },
    /// Remove one credential from this node. A node running on this machine
    /// is asked to remove it, and stops brokering it at once. A handoff of it
    /// back to this node is refused from then on. When this node is the
    /// origin of its sign-in (signed in, shared or first renewed it here),
    /// every other node it was shared or handed off to is sent a revoke,
    /// redelivered until that node answers; a copy another node handed here
    /// is removed here only.
    Rm { name: String },
    /// Hand a credential to another member node, direct-sealed over the hub.
    /// The credential must list that node in `nodes`, or it is refused there.
    /// A copy another node shared here is that node's to share, and refused.
    Share {
        name: String,
        #[arg(long)]
        to: String,
    },
    /// The revokes removed credentials are owed: which peers dropped their
    /// copy, which kept one, which are still pending, and which were given
    /// up on.
    Revokes {
        /// Stop sending a revoke and forget it, by the id (or its start)
        /// this listing shows: for a peer that has left the mesh.
        #[arg(long, value_name = "ID")]
        cancel: Option<String>,
    },
}

#[derive(Subcommand)]
enum MeshCommand {
    /// Print this node's id and fingerprint, generating the identity if needed.
    /// The id is what a hub's TRACON_HUB_ADMIT takes.
    Id,
    /// The first node: create the `@mesh` channel key here and point this node
    /// at a hub that admits it (`TRACON_HUB_ADMIT=<node id>` on the hub).
    Init {
        /// The hub's URL, e.g. https://hub.example.com
        #[arg(long)]
        hub: String,
    },
    /// List the hub's members and their channels.
    Members,
    /// Revoke a member on the hub (a lost or retired node). Allowed for the
    /// node that admitted it, or for the node itself.
    Remove {
        /// The member's node id.
        node_id: String,
    },
    /// Invite another node: prints a code and URL, waits for it to answer,
    /// shows its fingerprint for you to confirm, then admits it and hands off
    /// the channel keys and this node's policy bundle.
    Invite {
        /// Channels to hand off besides `@mesh`, comma-separated.
        #[arg(long, value_delimiter = ',')]
        channels: Vec<String>,
        /// How long the invitation stays open, in seconds (hub caps it).
        #[arg(long, default_value_t = 600)]
        ttl: u64,
        /// Admit without asking, once the fingerprint is shown. For scripts.
        #[arg(long)]
        yes: bool,
    },
    /// Hand channel keys to a node that is already a member.
    Admit {
        node_id: String,
        #[arg(long, value_delimiter = ',')]
        channels: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ChannelCommand {
    /// Set bindings on a channel: `key=value` pairs, dotted keys nest
    /// (`phases.review.model=m`, `ceiling_tokens_per_day=2000000`,
    /// `key=` removes). Re-handed to every member on a mesh.
    Bind {
        name: String,
        #[arg(required = true)]
        pairs: Vec<String>,
    },
    /// Create a channel: mint its key here. Other nodes get it by enrollment.
    Create { name: String },
    /// The channels this node holds keys for.
    List,
    /// Leave a channel: off this node's hub record, then its key and
    /// membership here. Its sessions and documents stay as history.
    Leave {
        name: String,
        /// Drop the key too. If no other node holds it, what the channel
        /// sealed cannot be opened again.
        #[arg(long)]
        forget_key: bool,
    },
    /// Share channels with the hub's replica: it can then open and index
    /// them, and run the nightly batch for them. Never do this for a work
    /// channel; the hub is rented infrastructure.
    Share {
        #[arg(value_delimiter = ',')]
        channels: Vec<String>,
        #[arg(long)]
        hub: bool,
    },
}

#[derive(Subcommand)]
enum ExternalCommand {
    /// How to point your own harness at this node, and the lanes that
    /// called in the last day.
    Show,
    /// Print the headers that label a harness's calls, for Claude Code's
    /// `headersHelper`: the repository and branch it was started in, and the
    /// harness process, so the interface can say it is still running. Needs
    /// no running node.
    Lane {
        /// Seconds your harness's MCP client gives one call, when you have
        /// lifted its timeout: status tools then hold for longer per call.
        #[arg(long, env = "TRACON_WAIT_BUDGET")]
        wait_budget: Option<u64>,
    },
    /// Refuse every external harness on a channel, until `start`.
    Stop { channel: String },
    /// Allow external harnesses on a channel again after `stop`.
    #[command(alias = "clear")]
    Start { channel: String },
}

#[derive(Subcommand)]
enum PolicyCommand {
    /// Generate a signing key. The private half stays on this machine and never
    /// reaches the hub, so a compromised hub can serve stale policy but not new.
    Keygen {
        /// Overwrite an existing key. The old one stops being able to sign.
        #[arg(long)]
        force: bool,
    },
    /// Write the five working agreements as a starting bundle, and sign it.
    Init,
    /// Sign the current bundle, after editing it.
    Sign,
    /// Hand the signed bundle to every peer as a rollout the running node
    /// keeps delivering. Exits non-zero if any peer's copy was not queued.
    Push,
    /// Verify the bundle and print what it decides. Exits non-zero if the
    /// signature does not check out.
    Show,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tracon=info,tower_http=info".into()),
        )
        .init();

    let command = Cli::parse().command;
    // A listing piped into `head` is cut off on purpose: end quietly, as
    // other tools do, rather than panic in `println!`. Not for the node,
    // which writes to its harnesses' pipes and must outlive any one of them.
    #[cfg(unix)]
    if !matches!(command, Command::Serve { .. }) {
        // SAFETY: `signal` only sets the process's disposition, back to the
        // default the Rust runtime replaced with "ignore" before `main`.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }

    match command {
        Command::Serve { listen } => {
            http::serve(listen).await?;
            // Stopped to be started again (a restart in a pod): exit now,
            // whatever background work is left, so the supervisor sees the
            // process go and starts it.
            if tracon::restart::exit_was_requested() {
                std::process::exit(0);
            }
            Ok(())
        }
        Command::Setup { rebuild } => {
            let cfg = boundary_config()?;
            let backend = boundary::backend_for(&cfg).await;
            // Never beside the node's own setup of the same boundary.
            boundary::reconcile::exclusive(backend.setup(&cfg, rebuild)).await??;
            println!("{} boundary is in place", backend.kind());
            // A credential with no client left is a credential nobody is
            // watching. Retired by name, and said out loud rather than
            // quietly: the broker is untouched.
            for artifact in tracon::legacy::retire_credentials() {
                if artifact.removed {
                    println!("removed {} — {}", artifact.path.display(), artifact.what);
                }
            }
            Ok(())
        }
        Command::Credential(cmd) => credential_command(cmd).await,
        Command::Doc(cmd) => doc_command(cmd).await,
        Command::Skill(cmd) => skill_command(cmd).await,
        Command::Session(cmd) => session_command(cmd).await,
        Command::Data(cmd) => data_command(cmd).await,
        Command::Memory(cmd) => memory_command(cmd).await,
        Command::Work(cmd) => work_command(cmd).await,
        Command::Name(cmd) => name_command(cmd).await,
        Command::Auth(cmd) => auth_command(cmd).await,
        Command::External(cmd) => external_command(cmd).await,
        Command::Push(cmd) => push_command(cmd).await,
        Command::Service(cmd) => match cmd {
            ServiceCommand::Install => tracon::service::install(),
            ServiceCommand::Uninstall => tracon::service::uninstall(),
            ServiceCommand::Status => tracon::service::status(),
            ServiceCommand::Restart => tracon::service::restart(),
        },
        Command::Metrics { channel, days } => {
            use reqwest::Method;
            let since = tracon::store::now_ms() - days.max(1) * 86_400_000;
            let mut q = format!("/api/metrics?since_ms={since}");
            if let Some(c) = channel {
                q.push_str(&format!("&channel={c}"));
            }
            let v = node_call(Method::GET, &q, None, None).await?;
            println!("{}", v["note"].as_str().unwrap_or(""));
            for c in v["channels"].as_array().cloned().unwrap_or_default() {
                let f = |k: &str| {
                    c[k].as_f64()
                        .map(|x| format!("{x:.1}"))
                        .unwrap_or_else(|| "—".into())
                };
                println!(
                    "{:<12} accepted {:>3}  rejected {:>3}  approvals/accepted {:>6}  tokens/accepted {:>10}  tokens {:>10}  cost {}  human {}s  agent {}s  sessions {}",
                    c["channel"].as_str().unwrap_or(""),
                    c["accepted_changes"],
                    c["rejected_changes"],
                    f("approvals_per_accepted_change"),
                    f("tokens_per_accepted_change"),
                    c["tokens"],
                    c["cost_usd"]
                        .as_f64()
                        .map(|x| format!("${x:.2}"))
                        .unwrap_or_else(|| "unpriced".into()),
                    f("human_seconds"),
                    f("agent_seconds"),
                    c["sessions"],
                );
            }
            Ok(())
        }
        Command::Gc {
            apply,
            caches,
            legacy,
            adopt_unlabelled,
            all,
        } => {
            use reqwest::Method;
            let v = node_call(
                Method::POST,
                "/api/maintenance/storage",
                Some(serde_json::json!({
                    "apply": apply,
                    "caches": caches,
                    "legacy": legacy,
                    "adopt_unlabelled": adopt_unlabelled,
                })),
                None,
            )
            .await?;
            let items = v["items"].as_array().cloned().unwrap_or_default();
            // Images last, grouped by class.
            let (images, others): (Vec<_>, Vec<_>) =
                items.iter().partition(|item| item["kind"] == "image");
            let mut images = images;
            images.sort_by_key(|item| {
                (
                    item["class"].as_str().unwrap_or("").to_string(),
                    !item["remove"].as_bool().unwrap_or(false),
                )
            });
            let mut listed = 0;
            let mut failed = 0;
            for item in others.iter().chain(images.iter()) {
                let remove = item["remove"].as_bool().unwrap_or(false);
                if !remove && !all {
                    continue;
                }
                let status = match (remove, apply, item["error"].as_str()) {
                    (false, _, _) => "keep".to_string(),
                    (true, false, _) => "remove".to_string(),
                    (true, true, Some(error)) => {
                        failed += 1;
                        format!("FAILED ({error})")
                    }
                    (true, true, None) => "removed".to_string(),
                };
                listed += usize::from(remove);
                let class = match item["class"].as_str() {
                    Some(class) => format!(
                        "{class:<10} {:>9} ",
                        human_bytes(item["size"].as_u64().unwrap_or(0))
                    ),
                    None => String::new(),
                };
                println!(
                    "{status:<8} {:<9} {class}{}  — {}",
                    item["kind"].as_str().unwrap_or(""),
                    item["name"].as_str().unwrap_or(""),
                    item["reason"].as_str().unwrap_or("")
                );
            }
            print_image_classes(&images, apply);
            if listed == 0 {
                println!("nothing to remove");
            } else if !apply {
                println!("{listed} to remove; run again with --apply to remove them");
            }
            if let Some(line) = v["scheduled_line"].as_str() {
                println!("{line}");
            }
            if failed > 0 {
                anyhow::bail!("{failed} could not be removed");
            }
            Ok(())
        }
        Command::Repo { command } => repo_command(command).await,
        Command::Plugin { command } => plugin_command(command).await,
        Command::Sandbox {
            command:
                SandboxCommand::Probe {
                    provider,
                    bundle,
                    image,
                    keep,
                    name,
                },
        } => {
            let provider = match provider.as_str() {
                "apple" => tracon::provider::ProviderKind::Apple,
                "kubernetes" => tracon::provider::ProviderKind::Kubernetes,
                "podman" => tracon::provider::ProviderKind::Podman,
                other => {
                    anyhow::bail!(
                        "unknown provider {other:?}; the probe knows apple, kubernetes and podman"
                    )
                }
            };
            tracon::provider::probe::run(tracon::provider::probe::ProbeOptions {
                provider,
                image,
                bundle,
                keep,
                name,
            })
            .await
        }
        Command::Provenance { sha } => {
            use reqwest::Method;
            let v = node_call(Method::GET, &format!("/api/provenance/{sha}"), None, None).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        Command::Policy(PolicyCommand::Push) => {
            let cfg = node_config()?;
            let hub = cfg
                .mesh
                .hub_url
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no hub configured"))?;
            let (id, _) = tracon::mesh::identity::load_or_generate()?;
            let store = tracon::store::Store::open(&config::Config::db_path())?;
            let push = tracon::mesh::enroll::push_policy(&store, &id, &hub).await?;
            let width = push.targets.iter().map(|t| t.name.len()).max().unwrap_or(0);
            for target in &push.targets {
                match &target.queued {
                    Ok(()) => println!("{:<width$}  queued", target.name),
                    Err(why) => println!("{:<width$}  not queued: {why}", target.name),
                }
            }
            println!(
                "rollout {}: the node retries undelivered peers when they reconnect",
                push.rollout_id
            );
            let missed = push.targets.iter().filter(|t| t.queued.is_err()).count();
            if missed > 0 {
                anyhow::bail!(
                    "{missed} of {} peer{} not queued",
                    push.targets.len(),
                    if push.targets.len() == 1 { "" } else { "s" }
                );
            }
            Ok(())
        }
        Command::Policy(cmd) => policy_command(cmd),
        Command::Enroll {
            invitation,
            name,
            hub,
        } => enroll_command(invitation, name, hub).await,
        Command::Mesh(cmd) => mesh_command(cmd).await,
        Command::Channel(cmd) => channel_command(cmd).await,
        Command::CheckBoundary { deep } => check_boundary(deep).await,
    }
}

/// `tracon check-boundary`: the checks run here, or, where this host's
/// serving node holds what they need (a Kubernetes node's sandbox port), by
/// that node, which is asked over its loopback API. Either way the same
/// lines and the same exit status.
async fn check_boundary(deep: bool) -> Result<()> {
    let cfg = boundary_config()?;
    let checked = match sandbox_port_held(&cfg) {
        Some(port) => match running_node().await? {
            Some(url) => {
                eprintln!(
                    "port {port} is taken by the node serving its sandboxes here: \
                     asking it ({url}) to run the checks"
                );
                node_call(
                    reqwest::Method::POST,
                    "/api/boundary/check",
                    Some(serde_json::json!({ "deep": deep })),
                    None,
                )
                .await?
            }
            None => local_boundary_check(&cfg, deep).await,
        },
        None => local_boundary_check(&cfg, deep).await,
    };
    let (lines, failed) = boundary_lines(&checked);
    for line in lines {
        println!("{line}");
    }
    match failed {
        None => Ok(()),
        // Refusal is the honest state: name the check and exit non-zero.
        Some(id) => anyhow::bail!("boundary check failed: {id}"),
    }
}

/// The checks run in this process, shaped as `POST /api/boundary/check`
/// answers them.
async fn local_boundary_check(cfg: &config::Config, deep: bool) -> serde_json::Value {
    let backend = boundary::backend_for(cfg).await;
    let report = backend.check_all(cfg, deep).await;
    let providers: Vec<_> = tracon::provider::backend_report(backend.kind(), &report)
        .into_iter()
        .collect();
    serde_json::json!({ "checks": report, "providers": providers })
}

/// The sandbox port a serving node on this host holds, which this process
/// can then not listen on to run the checks itself. Only Kubernetes listens
/// on a fixed port: Podman's and Apple's sandboxes each get a socket of
/// their own, which a serving node does not hold.
fn sandbox_port_held(cfg: &config::Config) -> Option<u16> {
    if cfg.runtime.kind != config::RuntimeKind::Kubernetes {
        return None;
    }
    let port = cfg.runtime.kubernetes.sandbox_port;
    port_in_use(SocketAddr::from(([0, 0, 0, 0], port))).then_some(port)
}

fn port_in_use(addr: SocketAddr) -> bool {
    addr.port() != 0
        && matches!(
            std::net::TcpListener::bind(addr),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse
        )
}

/// The lines `check-boundary` prints for a check's answer (`checks`, then
/// each provider's capabilities), and the first failed check's id.
fn boundary_lines(checked: &serde_json::Value) -> (Vec<String>, Option<String>) {
    let mut lines = Vec::new();
    let mut failed = None;
    for c in checked["checks"]["checks"].as_array().into_iter().flatten() {
        let ok = c["ok"].as_bool().unwrap_or(false);
        let pending = c["pending"].as_bool().unwrap_or(false);
        let id = c["id"].as_str().unwrap_or("?");
        if !ok && failed.is_none() {
            failed = Some(id.to_string());
        }
        lines.push(format!(
            "{} {:<22} {}",
            match (ok, pending) {
                (true, _) => "ok  ",
                (false, true) => "WAIT",
                (false, false) => "FAIL",
            },
            id,
            c["detail"].as_str().unwrap_or_default()
        ));
    }
    for caps in checked["providers"].as_array().into_iter().flatten() {
        lines.push(String::new());
        lines.push(format!(
            "capabilities ({}, reported, not enforced):",
            caps["provider"].as_str().unwrap_or("?")
        ));
        for line in caps["lines"].as_array().into_iter().flatten() {
            lines.push(format!(
                "  {:<16} {}",
                line["capability"].as_str().unwrap_or("?"),
                line["value"].as_str().unwrap_or("not established")
            ));
        }
    }
    (lines, failed)
}

fn policy_command(cmd: PolicyCommand) -> Result<()> {
    use tracon::policy::{bundle, Policy, WORKING_AGREEMENTS};

    match cmd {
        PolicyCommand::Keygen { force } => {
            if bundle::Paths::signing_key().exists() && !force {
                anyhow::bail!(
                    "a signing key already exists at {}; pass --force to replace it",
                    bundle::Paths::signing_key().display()
                );
            }
            let (signing, verifying) = bundle::generate_key();
            bundle::write_key(&signing)?;
            println!("signing key: {}", bundle::Paths::signing_key().display());
            println!("public key:  {}", hex::encode(verifying.to_bytes()));
            Ok(())
        }
        PolicyCommand::Init => {
            let path = bundle::Paths::bundle();
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, WORKING_AGREEMENTS)?;
            std::fs::write(
                bundle::Paths::signature(),
                bundle::sign(WORKING_AGREEMENTS)?,
            )?;
            println!("wrote and signed {}", path.display());
            Ok(())
        }
        PolicyCommand::Sign => {
            let text = std::fs::read_to_string(bundle::Paths::bundle())?;
            // Refuse to sign something that will not load, rather than shipping
            // a signed bundle the node then ignores.
            let _: Policy = toml::from_str(&text)
                .map_err(|e| anyhow::anyhow!("the bundle is not valid policy: {e}"))?;
            std::fs::write(bundle::Paths::signature(), bundle::sign(&text)?)?;
            println!("signed {}", bundle::Paths::bundle().display());
            Ok(())
        }
        // Handled in `main`, where the async hub call lives.
        PolicyCommand::Push => unreachable!("push is dispatched before policy_command"),
        PolicyCommand::Show => {
            let policy = bundle::load()?;
            println!("{} rules, version {}", policy.rules.len(), policy.version);
            for rule in &policy.rules {
                println!(
                    "  {:<24} {:?}  {}",
                    rule.id,
                    rule.verdict,
                    if rule.matches.is_empty() {
                        format!("kinds {:?}", rule.kinds)
                    } else {
                        format!("{} patterns", rule.matches.len())
                    }
                );
            }
            Ok(())
        }
    }
}

async fn mesh_command(cmd: MeshCommand) -> Result<()> {
    use tracon::mesh::identity;
    match cmd {
        MeshCommand::Id => {
            let (id, fresh) = identity::load_or_generate()?;
            if fresh {
                eprintln!(
                    "generated a new identity at {}",
                    identity::seed_path().display()
                );
            }
            println!("node id:     {}", id.node_id());
            println!(
                "fingerprint: {}",
                proto::enroll::fingerprint(&id.verifying_key().to_bytes())
            );
            Ok(())
        }
        MeshCommand::Init { hub } => {
            let (id, _) = identity::load_or_generate()?;
            let store =
                std::sync::Arc::new(tracon::store::Store::open(&config::Config::db_path())?);
            tracon::mesh::channels::create(&store, &id, proto::frame::MESH_CHANNEL)?;
            let hub = hub.trim_end_matches('/').to_string();
            config::update(&store, "tracon mesh init", |_| {
                Ok::<_, config::EditError>(vec![config::Op::set(
                    config::KeyPath::keys(&["mesh", "hub_url"]),
                    hub.as_str(),
                )])
            })?
            .settle()
            .await;
            println!("hub:         {hub}");
            println!("node id:     {}", id.node_id());
            println!("admit it on the hub with TRACON_HUB_ADMIT={}", id.node_id());
            println!("then `tracon serve`; other nodes join with `tracon mesh invite`");
            Ok(())
        }
        MeshCommand::Remove { node_id } => {
            let cfg = node_config()?;
            let hub = cfg.mesh.hub_url.clone().ok_or_else(|| {
                anyhow::anyhow!("no hub configured; run tracon mesh init or tracon enroll")
            })?;
            let (id, _) = identity::load_or_generate()?;
            tracon::mesh::enroll::remove_member(&id, &hub, &node_id).await?;
            println!("removed {node_id}");
            Ok(())
        }
        MeshCommand::Members => {
            let cfg = node_config()?;
            let hub = cfg.mesh.hub_url.clone().ok_or_else(|| {
                anyhow::anyhow!("no hub configured; run tracon mesh init or tracon enroll")
            })?;
            let (id, _) = identity::load_or_generate()?;
            let v = tracon::mesh::client::MeshClient::get_once(&id, &hub, "/v0/members").await?;
            for line in member_lines(&v, &id.node_id()) {
                println!("{line}");
            }
            Ok(())
        }
        MeshCommand::Invite { channels, ttl, yes } => invite_command(channels, ttl, yes).await,
        MeshCommand::Admit { node_id, channels } => {
            let cfg = node_config()?;
            let hub = cfg
                .mesh
                .hub_url
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no hub configured"))?;
            let (id, _) = identity::load_or_generate()?;
            let store = tracon::store::Store::open(&config::Config::db_path())?;
            let members =
                tracon::mesh::client::MeshClient::get_once(&id, &hub, "/v0/members").await?;
            let m = members
                .as_array()
                .and_then(|a| {
                    a.iter()
                        .find(|m| m["node_id"].as_str() == Some(node_id.as_str()))
                })
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{node_id} is not a member of the hub"))?;
            tracon::mesh::enroll::admit(
                &store,
                &id,
                &hub,
                &node_id,
                m["x25519_pub"].as_str().unwrap_or(""),
                m["binding_sig"].as_str().unwrap_or(""),
                m["name"].as_str().unwrap_or(""),
                &channels,
                &admission_handoff(&id, &node_id).await?,
            )
            .await?;
            println!(
                "handed {} the keys for {}",
                tracon::names::node(&node_id, m["name"].as_str()),
                channels.join(", ")
            );
            Ok(())
        }
    }
}

/// The credentials admitting `to` hands it, by the share rules
/// ([`tracon::broker::Broker::admission_handoff`]), with any sign-in minted
/// for them sealed before anything is sent. A running node would write its
/// own copy of the store back over that, so with one running a credential
/// that needs a sign-in minted is left for `tracon credential share`.
async fn admission_handoff(
    id: &proto::keys::Identity,
    to: &str,
) -> Result<Vec<(String, tracon::broker::Credential)>> {
    let key = id.credential_store_key();
    let mut broker = tracon::broker::Broker::load(&key)?;
    let (mut handoff, minted) = broker.admission_handoff(&id.node_id(), to);
    if minted.is_empty() {
        return Ok(handoff);
    }
    if running_node().await?.is_some() {
        handoff.retain(|(name, _)| !minted.contains(name));
        eprintln!(
            "not handed over now: {}; with the node running, share {} with `tracon credential share <name> --to {}`",
            minted.join(", "),
            if minted.len() == 1 { "it" } else { "them" },
            &to[..16.min(to.len())]
        );
        return Ok(handoff);
    }
    broker.save(&key)?;
    Ok(handoff)
}

struct Stderr;
impl tracon::mesh::enroll::Progress for Stderr {
    fn say(&self, line: &str) {
        eprintln!("{line}");
    }
}

async fn enroll_command(
    invitation: String,
    name: Option<String>,
    hub: Option<String>,
) -> Result<()> {
    let (hub_from_url, code) = proto::enroll::parse_invite(&invitation)
        .ok_or_else(|| anyhow::anyhow!("that is not an invitation URL or code"))?;
    let hub = hub_from_url
        .or(hub)
        .ok_or_else(|| anyhow::anyhow!("pass the full invitation URL, or --hub with the code"))?;
    let (id, fresh) = tracon::mesh::identity::load_or_generate()?;
    if fresh {
        eprintln!("generated this node's identity");
    }
    let cfg = config::Config::try_load().map_err(|e| anyhow::anyhow!(e))?;
    let name = name.unwrap_or_else(|| cfg.node_name.clone());
    let store = std::sync::Arc::new(tracon::store::Store::open(&config::Config::db_path())?);
    let audit_store = store.clone();
    let facts = format!("{} {}", std::env::consts::ARCH, std::env::consts::OS);
    let channels = tracon::mesh::enroll::accept(
        store,
        &id,
        &hub,
        &code,
        &name,
        &facts,
        std::time::Duration::from_secs(600),
        &Stderr,
    )
    .await?;
    config::update(&audit_store, "tracon enroll", |_| {
        Ok::<_, config::EditError>(vec![config::Op::set(
            config::KeyPath::keys(&["mesh", "hub_url"]),
            hub.trim_end_matches('/'),
        )])
    })?
    .settle()
    .await;
    println!("enrolled as {name}; channels: {}", channels.join(", "));
    println!("next: tracon setup, then tracon serve");
    Ok(())
}

/// This node's configuration, for a command that acts as this node (its
/// name, its hub, its runtime). A missing `node.toml` is the defaults; one
/// that is there and does not load is refused with the reason, never
/// replaced by them.
fn node_config() -> Result<config::Config> {
    config::Config::load().map_err(|error| anyhow::anyhow!("node.toml does not load: {error}"))
}

/// The configuration `setup` and `check-boundary` act on, loaded strictly:
/// on the defaults a file that does not parse would fall back to, setup could
/// act on names the file does not give (it removes the gateway an older
/// release made, by the names the file gives it). A missing file is the
/// defaults.
fn boundary_config() -> Result<config::Config> {
    config::Config::try_load().map_err(|error| {
        anyhow::anyhow!("node.toml does not load, so the boundary is left as it is: {error}")
    })
}

async fn invite_command(channels: Vec<String>, ttl: u64, yes: bool) -> Result<()> {
    use tracon::mesh::enroll;
    let cfg = node_config()?;
    let hub = cfg
        .mesh
        .hub_url
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no hub configured; run tracon mesh init first"))?;
    let (id, _) = tracon::mesh::identity::load_or_generate()?;
    let store = tracon::store::Store::open(&config::Config::db_path())?;
    for c in &channels {
        if store.channel_get(c)?.is_none() {
            anyhow::bail!(
                "this node holds no key for channel {c}; tracon channel create {c} first"
            );
        }
    }
    let inv = enroll::open_invite(&id, &hub, &channels, Some(ttl)).await?;
    println!("invitation code: {}", inv.display_code());
    println!("on the new machine, one line — install, enroll, set up, and run:");
    println!(
        "  curl -fsSL https://raw.githubusercontent.com/cosmicspork/tracon/main/install.sh | TRACON_ENROLL='{}' sh",
        inv.url
    );
    println!("or with tracon already installed:");
    println!("  tracon enroll {}", inv.url);
    if let Some(qr) = enroll::qr_text(&inv.url) {
        println!("{qr}");
    }
    println!(
        "this node's fingerprint: {}   (the other side prints its own)",
        proto::enroll::fingerprint(&id.verifying_key().to_bytes())
    );
    println!("waiting for the other node… (expires in {ttl}s)");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(ttl);
    let req = loop {
        if std::time::Instant::now() > deadline {
            anyhow::bail!("the invitation expired");
        }
        if let Some(r) = enroll::poll_invite(&id, &hub, &inv.code).await? {
            break r;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    };
    let fp = proto::enroll::fingerprint_hex(&req.node_id).unwrap_or_default();
    println!();
    println!("received: {}  ({})", req.name, req.facts);
    println!("its fingerprint: {fp}");
    if !yes {
        eprint!("does that match what the other node printed? [y/N] ");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if !matches!(line.trim(), "y" | "Y" | "yes") {
            enroll::cancel_invite(&id, &hub, &inv.code).await.ok();
            anyhow::bail!("not admitted");
        }
    }
    enroll::admit(
        &store,
        &id,
        &hub,
        &req.node_id,
        &req.x25519_pub,
        &req.binding_sig,
        &req.name,
        &inv.channels,
        &admission_handoff(&id, &req.node_id).await?,
    )
    .await?;
    println!("admitted {} with {}", req.name, inv.channels.join(", "));
    Ok(())
}

async fn channel_command(cmd: ChannelCommand) -> Result<()> {
    let store = tracon::store::Store::open(&config::Config::db_path())?;
    match cmd {
        ChannelCommand::Share { channels, hub } => {
            if !hub {
                anyhow::bail!("only --hub is supported; a node gets channels by enrollment");
            }
            let cfg = node_config()?;
            let hub_url = cfg
                .mesh
                .hub_url
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no hub configured"))?;
            let (id, _) = tracon::mesh::identity::load_or_generate()?;
            let n = tracon::mesh::enroll::share_with_hub(&store, &id, &hub_url, &channels).await?;
            println!("shared {} with the hub's replica", n.join(", "));
            Ok(())
        }
        ChannelCommand::Create { name } => {
            let (id, _) = tracon::mesh::identity::load_or_generate()?;
            let cfg = node_config()?;
            let (created, note) =
                tracon::mesh::channels::create_and_sync(&store, &id, &name, &cfg).await?;
            if !created.minted {
                println!("channel {name} already exists here");
            }
            match note {
                None if cfg.mesh.hub_url.is_some() => println!("hub record updated"),
                None => {}
                Some(why) => println!("{why}"),
            }
            println!("created channel {name}; hand its key to other nodes with tracon mesh invite");
            Ok(())
        }
        ChannelCommand::Leave { name, forget_key } => {
            let (id, _) = tracon::mesh::identity::load_or_generate()?;
            let cfg = node_config()?;
            tracon::mesh::channels::leave(&store, &id, &name, &cfg, forget_key).await?;
            println!("left channel {name}");
            Ok(())
        }
        ChannelCommand::Bind { name, pairs } => {
            use reqwest::Method;
            use serde_json::Value;
            let mut patch = serde_json::Map::new();
            for pair in pairs {
                let (k, v) = pair
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("{pair}: expected key=value"))?;
                let value = if v.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.to_string()))
                };
                patch.insert(k.to_string(), value);
            }
            let v = node_call(
                Method::PUT,
                &format!("/api/channels/{name}/bindings"),
                Some(Value::Object(patch)),
                None,
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&v["bindings"])?);
            if let Some(n) = v["handed_to"].as_u64() {
                println!("handed to {n} member{}", if n == 1 { "" } else { "s" });
            }
            Ok(())
        }
        ChannelCommand::List => {
            for c in store.channel_list()? {
                let ring = proto::keyring::Keyring::from_bytes(&c.keyring)
                    .map(|r| r.entries().len())
                    .unwrap_or(0);
                println!(
                    "{:<24} {} epoch{}",
                    c.name,
                    ring,
                    if ring == 1 { "" } else { "s" }
                );
            }
            Ok(())
        }
    }
}

/// The running node's operator API. The CLI goes through it rather than the
/// store so every write is published to the mesh like any other.
/// What came of offering a plugin: the card to answer, or nothing to do.
fn print_offered(v: &serde_json::Value) {
    if v["up_to_date"].as_bool() == Some(true) {
        println!("up to date at {}", v["version"].as_str().unwrap_or("?"));
        return;
    }
    let name = v["name"].as_str().unwrap_or("?");
    let version = v["version"].as_str().unwrap_or("?");
    let what = match v["from_version"].as_str() {
        Some(from) => format!("update {name} {from} → {version}"),
        None => format!("activate {name} {version}"),
    };
    let waiting = match v["new"].as_bool() {
        Some(false) => " (already waiting)",
        _ => "",
    };
    println!(
        "approve the install card (approval {}{waiting}) on {} to {what}",
        v["approval_id"].as_str().unwrap_or("?"),
        v["channel"].as_str().unwrap_or("?"),
    );
}

/// Plugins go through the running node: it fetches, checks and keeps them,
/// and only it raises the card that installs one.
async fn plugin_command(command: PluginCommand) -> Result<()> {
    use reqwest::Method;
    match command {
        PluginCommand::Add { source, channel } => {
            let path = std::path::Path::new(&source);
            let v = if path.exists() || source.ends_with(".tar.gz") {
                let bytes =
                    std::fs::read(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let mut query = format!("name={}", urlencoding(&name));
                if let Some(channel) = &channel {
                    query.push_str(&format!("&channel={}", urlencoding(channel)));
                }
                node_upload(&format!("/api/plugins/archive?{query}"), bytes).await?
            } else {
                node_call(
                    Method::POST,
                    "/api/plugins",
                    Some(serde_json::json!({ "repo": source, "channel": channel })),
                    None,
                )
                .await?
            };
            print_offered(&v);
            Ok(())
        }
        PluginCommand::Ls => {
            let v = node_call(Method::GET, "/api/plugins", None, None).await?;
            let rows = v["plugins"].as_array().cloned().unwrap_or_default();
            if rows.is_empty() {
                println!("no plugins");
            }
            for row in &rows {
                let version = |b: &serde_json::Value| b["version"].as_str().map(str::to_string);
                let state = row["state"].as_str().unwrap_or("?");
                let active = version(&row["active"]).unwrap_or_else(|| "-".into());
                println!(
                    "{}  {active}  {state}  from {}",
                    row["name"].as_str().unwrap_or("?"),
                    row["repo"].as_str().unwrap_or("?"),
                );
                if let Some(line) = offered_line(row) {
                    println!("  {line}");
                }
                for tool in row["tools"].as_array().into_iter().flatten() {
                    println!("  {}", plugin_tool_line(tool));
                }
                if let Some(previous) = version(&row["previous"]) {
                    println!("  previous: {previous}");
                }
                let pending = &row["pending"];
                if let Some(pending_version) = version(pending) {
                    let card = pending["approval_state"].as_str().unwrap_or("gone");
                    let card = match card {
                        "pending" => format!(
                            "approval {}",
                            pending["approval_id"].as_str().unwrap_or("?")
                        ),
                        other => format!("card {other}"),
                    };
                    println!("  pending: {pending_version} ({card})");
                }
                for instance in row["instances"].as_array().into_iter().flatten() {
                    println!("  {}", instance_line(instance));
                }
            }
            Ok(())
        }
        PluginCommand::Update { name, channel } => {
            let names: Vec<String> = match name {
                Some(name) => vec![name],
                None => node_call(Method::GET, "/api/plugins", None, None).await?["plugins"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|r| r["state"] != "removed")
                    .filter(|r| !r["repo"].as_str().unwrap_or("").starts_with("local:"))
                    .filter_map(|r| r["name"].as_str().map(str::to_string))
                    .collect(),
            };
            if names.is_empty() {
                println!("no plugins installed from a release");
            }
            let mut failed = 0;
            for name in names {
                let v = node_call(
                    Method::POST,
                    &format!("/api/plugins/{}/update", urlencoding(&name)),
                    Some(serde_json::json!({ "channel": channel })),
                    None,
                )
                .await;
                match v {
                    Ok(v) => {
                        print!("{name}: ");
                        print_offered(&v);
                    }
                    Err(e) => {
                        failed += 1;
                        eprintln!("{name}: {e}");
                    }
                }
            }
            if failed > 0 {
                anyhow::bail!("{failed} plugin(s) not updated");
            }
            Ok(())
        }
        PluginCommand::Rm { name, purge } => {
            node_call(
                Method::DELETE,
                &format!("/api/plugins/{}?purge={purge}", urlencoding(&name)),
                None,
                None,
            )
            .await?;
            match purge {
                true => println!("removed {name} and its data"),
                false => println!("removed {name}; its data is kept (--purge deletes it)"),
            }
            Ok(())
        }
        PluginCommand::Rollback { name } => {
            let v = node_call(
                Method::POST,
                &format!("/api/plugins/{}/rollback", urlencoding(&name)),
                None,
                None,
            )
            .await?;
            println!(
                "{name}: {} → {} (the previous version is now {})",
                v["previous"]["version"].as_str().unwrap_or("?"),
                v["active"]["version"].as_str().unwrap_or("?"),
                v["previous"]["version"].as_str().unwrap_or("?"),
            );
            Ok(())
        }
        PluginCommand::Logs {
            name,
            channel,
            lines,
        } => {
            let v = node_call(
                Method::GET,
                &format!(
                    "/api/plugins/{}/logs?channel={}&lines={lines}",
                    urlencoding(&name),
                    urlencoding(&channel)
                ),
                None,
                None,
            )
            .await?;
            for line in v["lines"].as_array().into_iter().flatten() {
                println!("{}", line.as_str().unwrap_or(""));
            }
            Ok(())
        }
        PluginCommand::Restart { name, channel } => {
            let v = node_call(
                Method::POST,
                &format!("/api/plugins/{}/restart", urlencoding(&name)),
                Some(serde_json::json!({ "channel": channel })),
                None,
            )
            .await?;
            println!("{name} {}", instance_line(&v["instance"]));
            if let Some(error) = v["error"].as_str() {
                anyhow::bail!("{error}");
            }
            Ok(())
        }
    }
}

/// Where an installed plugin is offered, in `tracon plugin ls`: `offered
/// on: personal, work`, with `(channels = [...])` when its `[[plugin]]`
/// entry narrows it. `None` from a node too old to say, or for a plugin with
/// no active build.
fn offered_line(row: &serde_json::Value) -> Option<String> {
    let names = |v: &serde_json::Value| -> Option<Vec<String>> {
        v.as_array().map(|a| {
            a.iter()
                .filter_map(|c| c.as_str().map(str::to_string))
                .collect()
        })
    };
    let offered = names(&row["offered_on"])?;
    let mut line = match offered.is_empty() {
        true => "offered on: no channel".to_string(),
        false => format!("offered on: {}", offered.join(", ")),
    };
    if let Some(listed) = names(&row["channels"]) {
        line.push_str(&format!(" (channels = [{}])", listed.join(", ")));
    }
    Some(line)
}

/// One of a plugin's tools in `tracon plugin ls`: how a call to it is
/// answered now, and by which rule:
/// `tool <plugin>__<tool>: <runs|asked|refused> (rule <id>); says it <hint>s`.
fn plugin_tool_line(tool: &serde_json::Value) -> String {
    let answer = match tool["decision"].as_str().unwrap_or("?") {
        "allow" => "runs",
        "ask" => "asked",
        "deny" => "refused",
        other => other,
    };
    let rule = tool["rule_id"]
        .as_str()
        .map(|r| format!(" (rule {r})"))
        .unwrap_or_default();
    let consequential = if tool["consequential"] == true {
        ", consequential"
    } else {
        ""
    };
    format!(
        "tool {}: {answer}{rule}; says it {}s{consequential}",
        tool["name"].as_str().unwrap_or("?"),
        tool["hint"].as_str().unwrap_or("?"),
    )
}

/// One plugin instance, as `tracon plugin ls` shows it:
/// `<channel>: <state> pid <pid> up <uptime> restarts <n> — <last error>`.
fn instance_line(instance: &serde_json::Value) -> String {
    let mut line = format!(
        "{}: {}",
        instance["channel"].as_str().unwrap_or("?"),
        instance["state"].as_str().unwrap_or("?").replace('_', " "),
    );
    if let Some(pid) = instance["pid"].as_u64() {
        line.push_str(&format!(" pid {pid}"));
    }
    if let Some(up) = instance["uptime_secs"].as_u64() {
        line.push_str(&format!(" up {}", short_duration(up)));
    }
    if let Some(ms) = instance["backoff_ms"].as_u64() {
        line.push_str(&format!(" next try in {}", short_duration(ms / 1000)));
    }
    line.push_str(&format!(
        " restarts {}",
        instance["restarts"].as_u64().unwrap_or(0)
    ));
    if let Some(error) = instance["last_error"].as_str() {
        line.push_str(&format!(" — {error}"));
    }
    line
}

fn short_duration(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{}s", s / 60, s % 60),
        s => format!("{}h{}m", s / 3600, s % 3600 / 60),
    }
}

async fn repo_command(command: RepoCommand) -> Result<()> {
    use reqwest::Method;
    let list = || node_call(Method::GET, "/api/repos/environments", None, None);
    match command {
        RepoCommand::Ls => {
            let v = list().await?;
            let entries = v["entries"].as_array().cloned().unwrap_or_default();
            if entries.is_empty() {
                println!("no [[repo]] entries in node.toml");
            }
            for item in &entries {
                let entry = &item["entry"];
                let steps = entry["build"].as_array().map_or(0, Vec::len);
                let image = match (entry["dockerfile"].as_str(), entry["image"].as_str()) {
                    _ if steps > 0 => format!("builds {steps} step(s) on the stock base"),
                    (Some(dockerfile), _) => format!("builds {dockerfile}"),
                    (None, Some(image)) => image.to_string(),
                    (None, None) => "harness image".into(),
                };
                println!("{}  — {image}", entry["path"].as_str().unwrap_or(""));
                for build in item["builds"].as_array().into_iter().flatten() {
                    println!("  {}", build_line(build));
                }
                for note in item["unbuilt"].as_array().into_iter().flatten() {
                    println!("  {}", unbuilt_line(note));
                }
            }
            Ok(())
        }
        RepoCommand::Show { path } => {
            let v = list().await?;
            let item = v["entries"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|item| item["entry"]["path"].as_str() == Some(path.as_str()))
                .ok_or_else(|| anyhow::anyhow!("no [[repo]] entry has path {path}"))?;
            let entry: toml::Value = serde_json::from_value(item["entry"].clone())?;
            print!("[[repo]]\n{}", toml::to_string(&entry)?);
            for build in item["builds"].as_array().into_iter().flatten() {
                println!("\n{}", build_line(build));
                for command in build["warnings"].as_array().into_iter().flatten() {
                    println!(
                        "  warning: the image has no tool for `{}`",
                        command.as_str().unwrap_or("")
                    );
                }
            }
            for note in item["unbuilt"].as_array().into_iter().flatten() {
                println!("\n{}", unbuilt_line(note));
            }
            Ok(())
        }
        RepoCommand::Set {
            path,
            image,
            dockerfile,
            context,
            build,
            checks,
            inherit_checks,
            prepare,
            prepare_credentials,
            egress,
            session_egress,
            timeout_secs,
            service_volumes,
        } => {
            let (mut entries, hash) = repo_entries().await?;
            let at = entries
                .iter()
                .position(|entry| entry["path"].as_str() == Some(path.as_str()));
            let mut entry = match at {
                Some(at) => entries[at].clone(),
                None => serde_json::json!({ "path": path, "prepare": [], "egress": [] }),
            };
            let fields = entry.as_object_mut().expect("an entry is a table");
            // An image, a Dockerfile and build steps answer the same
            // question; naming one withdraws the others.
            if let Some(image) = image {
                fields.insert("image".into(), image.into());
                fields.remove("dockerfile");
                fields.remove("context");
                fields.remove("build");
            }
            if let Some(dockerfile) = dockerfile {
                fields.insert("dockerfile".into(), dockerfile.into());
                fields.remove("image");
                fields.remove("build");
            }
            if !build.is_empty() {
                let steps = flag_list(build);
                if steps.as_array().is_some_and(|steps| !steps.is_empty()) {
                    fields.remove("image");
                    fields.remove("dockerfile");
                    fields.remove("context");
                }
                fields.insert("build".into(), steps);
            }
            if let Some(context) = context {
                fields.insert("context".into(), context.into());
            }
            if !checks.is_empty() {
                fields.insert("checks".into(), flag_list(checks));
            }
            if inherit_checks {
                fields.remove("checks");
            }
            if !prepare.is_empty() {
                fields.insert("prepare".into(), flag_list(prepare));
            }
            if !prepare_credentials.is_empty() {
                fields.insert("prepare_credentials".into(), flag_list(prepare_credentials));
            }
            if !egress.is_empty() {
                fields.insert("egress".into(), flag_list(egress));
            }
            if let Some(open) = session_egress {
                fields.insert("session_egress".into(), open.into());
            }
            if let Some(seconds) = timeout_secs {
                fields.insert("timeout_secs".into(), seconds.into());
            }
            if !service_volumes.is_empty() {
                fields.insert("service_volumes".into(), flag_list(service_volumes));
            }
            match at {
                Some(at) => entries[at] = entry,
                None => entries.push(entry),
            }
            save_repo_entries(entries, &hash).await?;
            println!("saved; it applies to the next session and the next check");
            Ok(())
        }
        RepoCommand::Volumes => {
            let v = node_call(Method::GET, "/api/maintenance/service-volumes", None, None).await?;
            let volumes = v["volumes"].as_array().cloned().unwrap_or_default();
            if volumes.is_empty() {
                println!("no service volumes; a [[repo]] entry's service_volumes makes one at its next session");
            }
            for volume in &volumes {
                let mut notes = Vec::new();
                if volume["live"].as_bool() == Some(true) {
                    notes.push("held by a running session".to_string());
                }
                if volume["mounted"].as_bool() == Some(false) {
                    notes.push("its entry no longer names the service".to_string());
                }
                let notes = match notes.is_empty() {
                    true => String::new(),
                    false => format!("  ({})", notes.join("; ")),
                };
                println!(
                    "{}  {:<10} {:>9}  {}{notes}",
                    volume["repo"].as_str().unwrap_or(""),
                    volume["service"].as_str().unwrap_or(""),
                    human_bytes(volume["bytes"].as_u64().unwrap_or(0)),
                    volume["volume"].as_str().unwrap_or(""),
                );
            }
            Ok(())
        }
        RepoCommand::ResetVolume { repo, service } => {
            let repo = match std::path::Path::new(&repo).is_dir() {
                true => std::fs::canonicalize(&repo)?.to_string_lossy().into_owned(),
                false => repo,
            };
            let v = node_call(
                Method::POST,
                "/api/maintenance/service-volumes/reset",
                Some(serde_json::json!({ "repo": repo, "service": service })),
                None,
            )
            .await?;
            println!(
                "reset {}'s {} data; its next session starts it empty",
                v["repo"].as_str().unwrap_or(&repo),
                v["service"].as_str().unwrap_or(&service)
            );
            Ok(())
        }
        RepoCommand::Rm { path } => {
            let (mut entries, hash) = repo_entries().await?;
            let before = entries.len();
            entries.retain(|entry| entry["path"].as_str() != Some(path.as_str()));
            if entries.len() == before {
                anyhow::bail!("no [[repo]] entry has path {path}");
            }
            save_repo_entries(entries, &hash).await?;
            println!(
                "removed; its images stay until `tracon gc --apply` reclaims them, and a managed clone of it stays on disk"
            );
            Ok(())
        }
        RepoCommand::Build { repo } => {
            // A directory that exists is named by where it is; anything else
            // is left for the node to match against the clones it manages.
            let repo = match repo {
                Some(repo) if !std::path::Path::new(&repo).is_dir() => repo,
                Some(repo) => std::fs::canonicalize(repo)?.to_string_lossy().into_owned(),
                None => std::env::current_dir()?.to_string_lossy().into_owned(),
            };
            let started = node_call(
                Method::POST,
                "/api/repos/environments/build",
                Some(serde_json::json!({ "repo": repo })),
                None,
            )
            .await?;
            let repo = started["repo"].as_str().unwrap_or_default().to_string();
            let since = started["since_ms"].as_i64().unwrap_or(0);
            if started["cloned"].as_bool() == Some(true) {
                println!("cloned {repo}");
            }
            println!("{}", build_started(&started, &repo));
            let mut idle = std::time::Duration::ZERO;
            loop {
                tokio::time::sleep(BUILD_POLL).await;
                let v = list().await?;
                let build = match awaited_build(&v, &repo, since) {
                    AwaitedBuild::Row(build) => build,
                    AwaitedBuild::Waiting => {
                        idle = std::time::Duration::ZERO;
                        continue;
                    }
                    AwaitedBuild::Unbuilt(note) => {
                        if let Some(image) = note["using"].as_str() {
                            eprintln!("runs keep using {image}");
                        }
                        anyhow::bail!(
                            "nothing was built: {}",
                            note["reason"].as_str().unwrap_or("the node gave no reason")
                        );
                    }
                    AwaitedBuild::Nothing => {
                        idle += BUILD_POLL;
                        if idle >= BUILD_START_WITHIN {
                            anyhow::bail!(
                                "the node recorded no build of {repo} within {}s and is not \
                                 working on one; see the node's log",
                                BUILD_START_WITHIN.as_secs()
                            );
                        }
                        continue;
                    }
                };
                match build["status"].as_str() {
                    Some("building") => continue,
                    Some("ready") => {
                        println!("{}", build["image"].as_str().unwrap_or(""));
                        for command in build["warnings"].as_array().into_iter().flatten() {
                            println!(
                                "warning: the image has no tool for `{}`",
                                command.as_str().unwrap_or("")
                            );
                        }
                        return Ok(());
                    }
                    _ => {
                        eprintln!("{}", build["log_tail"].as_str().unwrap_or(""));
                        anyhow::bail!("{}", build["error"].as_str().unwrap_or("the build failed"));
                    }
                }
            }
        }
    }
}

/// How often `tracon repo build` reads the build back.
const BUILD_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// How long `tracon repo build` waits for a build the node is neither
/// recording nor working on before it gives up.
const BUILD_START_WITHIN: std::time::Duration = std::time::Duration::from_secs(60);

/// The line `tracon repo build` starts with: what it builds, and from which
/// commit. A repository with nothing committed yet (an empty repository) has
/// no commit to name, and says so.
fn build_started(started: &serde_json::Value, repo: &str) -> String {
    let what = match started["joined"].as_bool() {
        Some(true) => "joining the build already running for",
        _ => "building",
    };
    let source_ref = started["source_ref"].as_str().unwrap_or("");
    let commit = started["source_commit"].as_str().unwrap_or("");
    let from = match commit.is_empty() {
        true => "an empty repository, with nothing committed yet".to_string(),
        false => format!("{source_ref} ({commit:.12})"),
    };
    format!("{what} {repo} from {from}; a first build can take several minutes")
}

/// What `/api/repos/environments` says of a build asked for at `since`.
#[derive(Debug, PartialEq)]
enum AwaitedBuild {
    /// Its row, building or settled.
    Row(serde_json::Value),
    /// No row yet, and nothing built: why, as the node noted it.
    Unbuilt(serde_json::Value),
    /// No row yet, but the node is working on the repository: waiting for
    /// the build lock behind another build, say.
    Waiting,
    /// No row, no reason, and nothing under way.
    Nothing,
}

fn awaited_build(v: &serde_json::Value, repo: &str, since: i64) -> AwaitedBuild {
    let items: Vec<&serde_json::Value> = v["entries"].as_array().into_iter().flatten().collect();
    let ours = |item: &serde_json::Value| item["repo_path"].as_str() == Some(repo);
    let at_or_after =
        |item: &serde_json::Value, key: &str| item[key].as_i64().unwrap_or(0) >= since;
    if let Some(build) = items
        .iter()
        .flat_map(|item| item["builds"].as_array().into_iter().flatten())
        .find(|build| ours(build) && at_or_after(build, "started_ms"))
    {
        return AwaitedBuild::Row(build.clone());
    }
    // Waiting is asked before the note: a note is the repository's, not this
    // request's, and another attempt that settled while this one waits for
    // the build lock writes one too. This one's own outcome shows once it
    // stops ensuring, which it does as soon as it has settled.
    let working = items
        .iter()
        .flat_map(|item| item["ensuring"].as_array().into_iter().flatten())
        .any(|path| path.as_str() == Some(repo));
    if working {
        return AwaitedBuild::Waiting;
    }
    match items
        .iter()
        .flat_map(|item| item["unbuilt"].as_array().into_iter().flatten())
        .find(|note| ours(note) && at_or_after(note, "at_ms"))
    {
        Some(note) => AwaitedBuild::Unbuilt(note.clone()),
        None => AwaitedBuild::Nothing,
    }
}

/// The table as the node holds it, for a command that changes one entry of
/// it, with `node.toml`'s hash: the version the save is decided against.
async fn repo_entries() -> Result<(Vec<serde_json::Value>, String)> {
    let v = node_call(reqwest::Method::GET, "/api/repos/environments", None, None).await?;
    let entries = v["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| item["entry"].clone())
        .collect();
    let hash = v["hash"].as_str().unwrap_or_default().to_string();
    Ok((entries, hash))
}

/// The table saved whole, refused if `node.toml` changed since `hash` was read.
/// A refused save is reported, not retried: the change was decided against
/// the file as it was, and another edit landed in between.
async fn save_repo_entries(entries: Vec<serde_json::Value>, hash: &str) -> Result<()> {
    node_call(
        reqwest::Method::PUT,
        "/api/repos/environments",
        Some(serde_json::json!({ "entries": entries, "expected_hash": hash })),
        None,
    )
    .await
    .map_err(|error| match error.downcast_ref::<NodeRefusal>() {
        Some(refusal) if refusal.is_stale_version() => anyhow::anyhow!(
            "node.toml changed since this command read it, so nothing was saved; \
             check `tracon repo show` and run the command again if it still applies"
        ),
        _ => error,
    })?;
    Ok(())
}

/// Bytes as a person reads them, in powers of 1000 as `podman images` shows
/// sizes.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes} B"),
        _ => format!("{value:.1} {}", UNITS[unit]),
    }
}

/// `tracon gc`'s images, class by class: how many go and about what that
/// frees, the total, and what is held back for a flag.
fn print_image_classes(images: &[&serde_json::Value], applied: bool) {
    if images.is_empty() {
        return;
    }
    let mut classes: Vec<&str> = images
        .iter()
        .filter_map(|item| item["class"].as_str())
        .collect();
    classes.dedup();
    let verb = if applied { "removed" } else { "to remove" };
    println!("images by class (what removing frees, about; shared layers counted once):");
    let (mut count, mut bytes) = (0, 0);
    for class in classes {
        let of: Vec<_> = images
            .iter()
            .filter(|item| item["class"] == class)
            .collect();
        let going: Vec<_> = of
            .iter()
            .filter(|item| item["remove"].as_bool().unwrap_or(false))
            .filter(|item| !applied || item["error"].is_null())
            .collect();
        let size: u64 = going
            .iter()
            .map(|item| item["size"].as_u64().unwrap_or(0))
            .sum();
        let kept = of.len() - going.len();
        let note = match (class, kept) {
            (_, 0) => String::new(),
            ("unowned", _) => ", no owner label; --adopt-unlabelled takes them".into(),
            ("legacy", _) => ", --legacy removes what nothing uses".into(),
            _ => String::new(),
        };
        println!(
            "  {class:<10} {:>3} {verb:<9} {:>9}   {kept} kept{note}",
            going.len(),
            human_bytes(size)
        );
        count += going.len();
        bytes += size;
    }
    println!(
        "  {:<10} {count:>3} {verb:<9} {:>9}",
        "total",
        human_bytes(bytes)
    );
}

/// A repeated flag as the list it stands for: one empty value is an empty
/// list, which is how a list is cleared.
fn flag_list(values: Vec<String>) -> serde_json::Value {
    match values.as_slice() {
        [only] if only.is_empty() => serde_json::json!([]),
        _ => serde_json::json!(values),
    }
}

fn build_line(build: &serde_json::Value) -> String {
    let text = |key: &str| build[key].as_str().unwrap_or("");
    let detail = match text("status") {
        "ready" => text("image").to_string(),
        "failed" => text("error").to_string(),
        _ => String::new(),
    };
    // `base` is the image checks run in, `trial` one a draft was tried in,
    // and `snapshot` a prepared snapshot of `base` or the stock base.
    format!(
        "{:<8} {:<14} {}  {detail}",
        text("status"),
        text("kind"),
        text("repo_path"),
    )
}

/// A repository whose last attempt built nothing, as `repo ls` lists it
/// beside its builds.
fn unbuilt_line(note: &serde_json::Value) -> String {
    let text = |key: &str| note[key].as_str().unwrap_or("");
    let using = match note["using"].as_str() {
        Some(image) => format!(" (runs use {image})"),
        None => String::new(),
    };
    format!(
        "{:<8} {:<14} {}  {}{using}",
        "unbuilt",
        "base",
        text("repo_path"),
        text("reason"),
    )
}

/// The node at [`node_url`], when it is on this machine, answers, and is the
/// node this state directory belongs to: the one whose credential store a
/// command acting on this machine also writes. A different node answering
/// there (a port-forward, another state directory's node) holds some other
/// store, so the command acts on this one itself.
async fn running_node() -> Result<Option<String>> {
    let url = node_url()?;
    if !tracon::node_target::is_loopback(&url) {
        return Ok(None);
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()?;
    if client.get(format!("{url}/api/awake")).send().await.is_err() {
        return Ok(None);
    }
    if let Some(local) = tracon::mesh::identity::load()? {
        if let Ok(facts) = node_facts().await {
            if facts.id != local.node_id() {
                return Ok(None);
            }
        }
    }
    Ok(Some(url))
}

/// A request the node answered with an error status, and the message it gave.
#[derive(Debug)]
struct NodeRefusal {
    status: reqwest::StatusCode,
    kind: Option<String>,
    message: String,
}

impl NodeRefusal {
    /// The edit named a version of `node.toml` that is no longer current:
    /// a 412 to an If-Match, or the 409 conflict an `expected_hash` gets.
    fn is_stale_version(&self) -> bool {
        self.status == reqwest::StatusCode::PRECONDITION_FAILED
            || (self.status == reqwest::StatusCode::CONFLICT
                && self.kind.as_deref() == Some("conflict"))
    }
}

impl std::fmt::Display for NodeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.status, self.message)
    }
}

impl std::error::Error for NodeRefusal {}

/// The node this command talks to: `TRACON_URL`, then `TRACON_LISTEN`, then
/// the state directory's `serve.json`, then the default (see
/// [`tracon::node_target`]). Resolved once, so a warning is given once.
fn node_target() -> Result<&'static tracon::node_target::Target> {
    static TARGET: std::sync::OnceLock<Result<tracon::node_target::Target, String>> =
        std::sync::OnceLock::new();
    let target = TARGET.get_or_init(|| {
        let env = |name: &str| std::env::var(name).ok();
        let target = tracon::node_target::resolve(
            env("TRACON_URL").as_deref(),
            env("TRACON_LISTEN").as_deref(),
            &config::Config::state_dir(),
            tracon::node_target::pid_alive,
        )?;
        for warning in &target.warnings {
            eprintln!("warning: {warning}");
        }
        Ok(target)
    });
    target.as_ref().map_err(|e| anyhow::anyhow!("{e}"))
}

fn node_url() -> Result<String> {
    Ok(node_target()?.url.clone())
}

/// What the target node says it is (`GET /api/node`).
struct NodeFacts {
    id: String,
    name: Option<String>,
}

impl NodeFacts {
    /// "node <name>", or its generated `node-…` name when it has none.
    fn describe(&self) -> String {
        match &self.name {
            Some(name) if !name.trim().is_empty() => format!("node {name}"),
            _ => tracon::names::node(&self.id, None),
        }
    }
}

async fn node_facts() -> Result<NodeFacts> {
    let v = node_request(reqwest::Method::GET, "/api/node", None, None).await?;
    let id = v["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{}: /api/node named no node id",
                node_url().unwrap_or_default()
            )
        })?;
    Ok(NodeFacts {
        id: id.to_string(),
        name: v["name"].as_str().map(str::to_string),
    })
}

/// Before the first write: the node about to be changed must be the one this
/// state directory belongs to. Two cases have nothing to compare: a node named
/// by `TRACON_URL` (or off this machine) with the state directory left at its
/// default, which is how another node is driven, through a port-forward say,
/// from a machine that runs its own; and a state directory with no identity
/// (a CLI used remotely with `TRACON_TOKEN`). A state directory that was
/// named but holds no identity writes only to a node `TRACON_URL` names, so
/// a scratch directory whose node never started doesn't fall through to the
/// default node.
///
/// The identity is only read, never generated: a check must not create the
/// thing it checks against.
async fn check_target_identity() -> Result<()> {
    static CHECKED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    CHECKED
        .get_or_try_init(|| async {
            let target = node_target()?;
            let state_dir_named =
                std::env::var_os("TRACON_STATE_DIR").is_some_and(|v| !v.is_empty());
            let named_by_url = target.source == tracon::node_target::Source::Url;
            let Some(local) = tracon::mesh::identity::load()? else {
                if state_dir_named && !named_by_url {
                    anyhow::bail!(
                        "the state directory {} holds no node identity, so {} ({}) can't be checked as its node: set TRACON_URL to the node you mean",
                        config::Config::state_dir().display(),
                        target.url,
                        target.source,
                    );
                }
                return Ok(());
            };
            if !state_dir_named
                && (named_by_url || !tracon::node_target::is_loopback(&target.url))
            {
                return Ok(());
            }
            let facts = node_facts().await?;
            let ours = local.node_id();
            if facts.id != ours {
                anyhow::bail!(
                    "{} points at {} at {}, but the state directory {} is {}: set TRACON_URL to the node you mean",
                    target.source,
                    facts.describe(),
                    target.url,
                    config::Config::state_dir().display(),
                    tracon::names::node(&ours, None),
                );
            }
            Ok(())
        })
        .await
        .map(|_| ())
}

async fn node_call(
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
    if_match: Option<&str>,
) -> Result<serde_json::Value> {
    if !matches!(method, reqwest::Method::GET | reqwest::Method::HEAD) {
        check_target_identity().await?;
    }
    node_request(method, path, body, if_match).await
}

/// One request to the target node, unchecked: [`node_call`] is the way in.
async fn node_request(
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
    if_match: Option<&str>,
) -> Result<serde_json::Value> {
    node_send(method, path, body.map(NodeBody::Json), if_match).await
}

/// What a request to the node carries: JSON, or bytes the node reads as
/// they are (an archive).
enum NodeBody {
    Json(serde_json::Value),
    Bytes(Vec<u8>),
}

/// [`node_call`] with bytes for its body.
async fn node_upload(path: &str, bytes: Vec<u8>) -> Result<serde_json::Value> {
    check_target_identity().await?;
    node_send(
        reqwest::Method::POST,
        path,
        Some(NodeBody::Bytes(bytes)),
        None,
    )
    .await
}

async fn node_send(
    method: reqwest::Method,
    path: &str,
    body: Option<NodeBody>,
    if_match: Option<&str>,
) -> Result<serde_json::Value> {
    let url = format!("{}{path}", node_url()?);
    let mut req = reqwest::Client::new().request(method, &url);
    // A node on this machine knows the caller is the operator by the loopback
    // address. One reached over the network wants the token; the CLI holds no
    // cookie jar, so it presents the token itself.
    if let Ok(token) = std::env::var("TRACON_TOKEN") {
        if !token.is_empty() {
            req = req.bearer_auth(token);
        }
    }
    match body {
        Some(NodeBody::Json(b)) => req = req.json(&b),
        Some(NodeBody::Bytes(b)) => {
            req = req
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .body(b)
        }
        None => {}
    }
    if let Some(h) = if_match {
        req = req.header("if-match", h);
    }
    let res = req.send().await.map_err(|e| {
        let source = node_target()
            .map(|t| t.source.to_string())
            .unwrap_or_default();
        anyhow::anyhow!("{url} (from {source}): {e} (is `tracon serve` running? set TRACON_URL)")
    })?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        return Err(NodeRefusal {
            status,
            kind: v["error"]["kind"].as_str().map(str::to_string),
            message: v["error"]["message"].as_str().unwrap_or(&text).to_string(),
        }
        .into());
    }
    Ok(v)
}

/// `export` asks the running node, which owns the store; `show` reads a file
/// and talks to nothing, so an export stays readable with nothing running.
async fn data_command(cmd: DataCommand) -> Result<()> {
    match cmd {
        DataCommand::Export { out } => {
            let path = out.unwrap_or_else(|| {
                format!("tracon-export-{}.jsonl", tracon::store::now_ms()).into()
            });
            let url = format!("{}/api/maintenance/export", node_url()?);
            let mut req = reqwest::Client::new().get(&url);
            if let Ok(token) = std::env::var("TRACON_TOKEN") {
                if !token.is_empty() {
                    req = req.bearer_auth(token);
                }
            }
            let mut res = req.send().await.map_err(|e| {
                anyhow::anyhow!("{url}: {e} (is `tracon serve` running? set TRACON_URL)")
            })?;
            if !res.status().is_success() {
                let status = res.status();
                let text = res.text().await.unwrap_or_default();
                let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                anyhow::bail!(
                    "{status}: {}",
                    v["error"]["message"].as_str().unwrap_or(&text)
                );
            }
            // Written beside its name and renamed into it, so a file at the
            // name is always a whole export.
            let partial = path.with_extension("jsonl.partial");
            let mut file = {
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&partial)?
            };
            let written: Result<()> = async {
                use std::io::Write;
                while let Some(chunk) = res.chunk().await? {
                    file.write_all(&chunk)?;
                }
                file.sync_all()?;
                Ok(())
            }
            .await;
            if let Err(error) = written {
                let _ = std::fs::remove_file(&partial);
                return Err(error);
            }
            std::fs::rename(&partial, &path)?;
            let reading = tracon::contract::read(
                std::io::BufReader::new(std::fs::File::open(&path)?),
                |_| {},
            )?;
            println!("Wrote {} ({} record(s)).", path.display(), reading.records);
            if let Err(error) = &reading.verified {
                println!("It does not verify: {error}");
            }
            println!("\n{}", tracon::contract::WARNING);
            Ok(())
        }
        DataCommand::Show {
            export,
            jsonl,
            kind,
            documents,
        } => {
            let file = std::fs::File::open(&export)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", export.display()))?;
            let mut docs = Vec::new();
            let reading = tracon::contract::read(std::io::BufReader::new(file), |record| {
                let k = record["kind"].as_str().unwrap_or_default();
                if documents.is_some() && k == "document" && record["table"] == "document" {
                    docs.push(record.clone());
                }
                if jsonl && kind.as_deref().is_none_or(|want| want == k) {
                    println!("{record}");
                }
            })?;
            if let Some(dir) = &documents {
                let (written, skipped) = tracon::contract::write_documents(dir, &docs)?;
                eprintln!(
                    "Wrote {written} document(s) to {}{}.",
                    dir.display(),
                    if skipped > 0 {
                        format!(", skipped {skipped} (HTML, or not a safe file name)")
                    } else {
                        String::new()
                    }
                );
            }
            if jsonl {
                if let Err(error) = &reading.verified {
                    eprintln!("signature   DOES NOT VERIFY: {error}");
                }
                return Ok(());
            }
            let header = &reading.header;
            println!("contract    {}", header["contract"].as_str().unwrap_or(""));
            println!("node        {}", header["node"].as_str().unwrap_or(""));
            println!(
                "tracon      {}",
                header["tracon_version"].as_str().unwrap_or("")
            );
            println!(
                "created     {}",
                header["created_ms"].as_i64().unwrap_or_default()
            );
            println!(
                "signature   {}",
                match &reading.verified {
                    Ok(()) => "verified: the file is whole and that node wrote it. \
                               It grants nothing."
                        .to_string(),
                    Err(error) => format!("DOES NOT VERIFY: {error}"),
                }
            );
            for (kind, n) in &reading.counts {
                println!("{kind:<12}{n}");
            }
            if reading.packages > 0 {
                println!(
                    "packages    {} session package(s), {} verify",
                    reading.packages, reading.packages_verified
                );
            }
            println!("\n{}", tracon::contract::WARNING);
            println!("`--jsonl` prints every record; `--kind` picks one kind.");
            Ok(())
        }
    }
}

/// Two different things live here, and the difference is the point.
///
/// `show` reads a portable package and talks to nothing: an archive stays
/// readable when there is nothing left running. The others are the opposite
/// — every one of them goes through the running node, because the node owns
/// the runtime volume and the store, and a CLI that reached around it would
/// be a second writer.
async fn session_command(cmd: SessionCommand) -> Result<()> {
    use reqwest::Method;
    match cmd {
        SessionCommand::Show { package, jsonl } => {
            let transfer = tracon::transfers::read_package(&package)?;
            let records = tracon::transfers::package_jsonl(&transfer);
            if jsonl {
                for record in &records {
                    println!("{record}");
                }
                return Ok(());
            }
            for record in &records {
                let kind = record["kind"].as_str().unwrap_or("");
                match kind {
                    "package" => {
                        println!("package     {}", record["id"].as_str().unwrap_or(""));
                        println!(
                            "candidate   {}",
                            record["candidate_id"].as_str().unwrap_or("")
                        );
                        println!("channel     {}", record["channel"].as_str().unwrap_or(""));
                        println!(
                            "origin      {}",
                            record["origin_node"].as_str().unwrap_or("")
                        );
                        println!(
                            "signature   {}",
                            match record["verify_error"].as_str() {
                                None => "verified".to_string(),
                                Some(error) => format!("DOES NOT VERIFY: {error}"),
                            }
                        );
                    }
                    "note" => println!("note        {}", record["text"].as_str().unwrap_or("")),
                    "document" => println!(
                        "document    {:<32} {}",
                        record["slug"].as_str().unwrap_or(""),
                        record["title"].as_str().unwrap_or("")
                    ),
                    "memory" => println!(
                        "memory      {:<32} {}",
                        record["memory_kind"].as_str().unwrap_or(""),
                        record["body"].as_str().unwrap_or("")
                    ),
                    "file" => println!(
                        "file        {:<32} {} bytes",
                        record["path"].as_str().unwrap_or(""),
                        record["bytes"].as_i64().unwrap_or_default()
                    ),
                    _ => {}
                }
            }
            println!("\n`--jsonl` prints every record, evidence included.");
            Ok(())
        }
        SessionCommand::ArchiveLegacy { harness } => {
            let body = harness.map(|h| serde_json::json!({ "harness": h }));
            let v = node_call(Method::POST, "/api/sessions/archive-legacy", body, None).await?;
            let harness = v["harness"].as_str().unwrap_or("");
            let archived = v["archived"].as_i64().unwrap_or_default();
            if archived == 0 {
                println!("No {harness} sessions left to archive.");
                return Ok(());
            }
            println!("Archived {archived} {harness} session(s), read-only:");
            for row in v["sessions"].as_array().cloned().unwrap_or_default() {
                println!(
                    "  {}  {:<12} {:<28} {}",
                    row["id"].as_str().unwrap_or(""),
                    row["channel"].as_str().unwrap_or(""),
                    row["branch"].as_str().unwrap_or(""),
                    row["harness"].as_str().unwrap_or("")
                );
            }
            let closed = v["approvals_closed"].as_i64().unwrap_or_default();
            if closed > 0 {
                println!("\nClosed {closed} pending approval(s): nothing can answer them now.");
            }
            let kept = v["workspaces_retained"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if !kept.is_empty() {
                println!("\nWorkspaces kept on this node ({}):", kept.len());
                for path in kept.iter().take(20) {
                    println!("  {}", path.as_str().unwrap_or(""));
                }
            }
            let elsewhere = v["workspaces_elsewhere"].as_u64().unwrap_or_default();
            if elsewhere > 0 {
                println!(
                    "\n{elsewhere} more workspace(s) recorded for these sessions are not on this node \
                     (held by another node, or gone)."
                );
            }
            println!(
                "\nThese sessions stay readable and can never be launched again.\n\
                 Carry one forward with `tracon session reopen <id> --harness claude` (or `native`)."
            );
            Ok(())
        }
        SessionCommand::Reopen { id, harness, model } => {
            let id = session_resolve(&id).await?;
            let v = node_call(
                Method::POST,
                &format!("/api/sessions/{id}/reopen"),
                Some(serde_json::json!({ "harness": harness, "model": model })),
                None,
            )
            .await?;
            println!(
                "reopened {id} as {} on {}",
                v["id"].as_str().unwrap_or("?"),
                v["branch"].as_str().unwrap_or("?")
            );
            println!("  harness      {harness}");
            println!("  model        {}", v["model"].as_str().unwrap_or(""));
            println!("  workspace    {}", v["repo_path"].as_str().unwrap_or(""));
            println!(
                "  continues    {}",
                v["continued_from"].as_str().unwrap_or("")
            );
            println!(
                "\nThe new session opens with a handoff note: it inherits the workspace, \
                 not the old session's context."
            );
            Ok(())
        }
        SessionCommand::Fork {
            id,
            to,
            branch,
            note,
            model,
            budget,
            task,
            new_task,
        } => {
            let id = session_resolve(&id).await?;
            let destination = match to.as_deref().map(str::trim).filter(|to| !to.is_empty()) {
                Some(to) => Some(
                    node_resolve(Some("node"), to, None)
                        .await
                        .ok()
                        .flatten()
                        .map(|hit| hit.id)
                        .unwrap_or_else(|| to.to_string()),
                ),
                None => None,
            };
            let v = node_call(
                Method::POST,
                &format!("/api/sessions/{id}/fork"),
                Some(serde_json::json!({
                    "destination_node": destination,
                    "branch": branch,
                    "note": note,
                    "model": model,
                    "budget_tokens": budget,
                    "work_item_id": task,
                    "new_item": new_task.map(|title| serde_json::json!({ "title": title })),
                })),
                None,
            )
            .await?;
            let head = v["head_sha"].as_str().unwrap_or("");
            let head = head.get(..12).unwrap_or(head);
            if v["delivery"] == "local" {
                println!(
                    "forked {id} at {head} as {} on {}",
                    v["session"]["id"].as_str().unwrap_or("?"),
                    v["branch"].as_str().unwrap_or("?")
                );
                println!(
                    "  model        {}",
                    v["session"]["model"].as_str().unwrap_or("")
                );
                println!(
                    "  task         {}",
                    v["work_item_id"].as_str().unwrap_or("none")
                );
            } else {
                println!(
                    "fork of {id} at {head} on {} queued for {}",
                    v["branch"].as_str().unwrap_or("?"),
                    v["destination_node"].as_str().unwrap_or("?")
                );
                println!("  transfer     {}", v["transfer_id"].as_str().unwrap_or(""));
                println!("\nIt starts on that node once the package arrives.");
            }
            println!(
                "\nThe source keeps running with its own task; the fork opens knowing it is a \
                 fork, with your note."
            );
            Ok(())
        }
        SessionCommand::Continue { id } => {
            let id = session_resolve(&id).await?;
            let v = node_call(
                Method::POST,
                &format!("/api/sessions/{id}/continue"),
                None,
                None,
            )
            .await?;
            println!(
                "continued {id} as {} on {}",
                v["id"].as_str().unwrap_or("?"),
                v["branch"].as_str().unwrap_or("?")
            );
            println!(
                "\nThe new session opens with a handoff note: it inherits the workspace, \
                 not the interrupted session's context."
            );
            Ok(())
        }
        SessionCommand::Saved { id, index, export } => {
            let id = session_resolve(&id).await?;
            let Some(index) = index else {
                let v = node_call(
                    Method::GET,
                    &format!("/api/sessions/{id}/saved"),
                    None,
                    None,
                )
                .await?;
                let saves = v["saves"].as_array().cloned().unwrap_or_default();
                if saves.is_empty() {
                    println!("{id} has saved nothing: it has not been compacted or restarted.");
                    return Ok(());
                }
                for save in saves {
                    println!(
                        "{:>4}  before {:<10} {:<9} {} transcript file(s), {} scratchpad file(s), \
                         through event {}",
                        save["index"],
                        save["before"].as_str().unwrap_or(""),
                        save["trigger"].as_str().unwrap_or(""),
                        save["transcripts"].as_array().map_or(0, Vec::len),
                        save["scratchpad"].as_array().map_or(0, Vec::len),
                        save["through_seq"],
                    );
                }
                println!("\n`tracon session saved {id} <index>` prints one's conversation.");
                return Ok(());
            };
            if let Some(path) = export {
                let v = node_call(
                    Method::GET,
                    &format!("/api/sessions/{id}/saved/{index}/export"),
                    None,
                    None,
                )
                .await?;
                std::fs::write(&path, serde_json::to_vec_pretty(&v)?)?;
                println!(
                    "wrote save {index} of {id} to {} ({} file(s) beside the conversation)",
                    path.display(),
                    v["files"].as_array().map_or(0, Vec::len)
                );
                return Ok(());
            }
            let v = node_call(
                Method::GET,
                &format!("/api/sessions/{id}/saved/{index}"),
                None,
                None,
            )
            .await?;
            print!("{}", v["conversation"].as_str().unwrap_or(""));
            Ok(())
        }
    }
}

/// The label `tracon external lane` prints: `<repository>:<branch>`. The
/// repository is named by its main checkout, so a linked worktree is still
/// its repository, and the lane survives the harness restarting or resuming.
fn external_lane() -> String {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let repo = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .map(std::path::PathBuf::from)
        .and_then(|common| {
            let checkout = if common.ends_with(".git") {
                common.parent()?.to_path_buf()
            } else {
                common
            };
            checkout
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .or_else(|| cwd.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "unknown".into());
    let branch =
        git(&["branch", "--show-current"]).or_else(|| git(&["rev-parse", "--short", "HEAD"]));
    match branch {
        Some(branch) => format!("{repo}:{branch}"),
        None => repo,
    }
}

async fn external_command(cmd: ExternalCommand) -> Result<()> {
    use reqwest::Method;
    // The helper runs on every connection a harness makes, node or no node.
    if let ExternalCommand::Lane { wait_budget } = cmd {
        // The harness runs the helper as its own child, so the parent is the
        // harness process: what the node checks to say a lane is running.
        let mut headers = serde_json::json!({
            "X-Tracon-Agent": external_lane(),
            "X-Tracon-Agent-Pid": std::os::unix::process::parent_id().to_string(),
        });
        if let Some(budget) = wait_budget {
            headers["X-Tracon-Wait-Budget"] = budget.to_string().into();
        }
        println!("{headers}");
        return Ok(());
    }
    let v = node_call(Method::GET, "/api/external?identities=true", None, None).await?;
    match cmd {
        ExternalCommand::Lane { .. } => {
            unreachable!("answered above, without asking the node")
        }
        ExternalCommand::Show => {
            if v["enabled"] != true {
                println!(
                    "This node does not answer harnesses outside the boundary.\n\
                     Set [external] enabled = true in node.toml and restart it."
                );
                return Ok(());
            }
            let base = node_url()?;
            // The variable, never its value: a token is shown once, where it
            // is issued, and this line is meant to be pasted and shared.
            let header = std::env::var("TRACON_TOKEN")
                .ok()
                .filter(|t| !t.is_empty())
                .map(|_| " --header \"Authorization: Bearer $TRACON_TOKEN\"".to_string())
                .unwrap_or_default();
            println!("Register this node with your harness, once per channel:\n");
            for c in v["channels"].as_array().unwrap_or(&vec![]) {
                let Some(name) = c.as_str() else { continue };
                println!("  claude mcp add --transport http tracon-{name} {base}/mcp/external/{name}{header}");
            }
            // Claude Code runs the helper once per connection, in the
            // directory it was started from, as the process's own child.
            let exe = std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "tracon".into());
            let auth = if header.is_empty() {
                String::new()
            } else {
                r#","headers":{"Authorization":"Bearer ${TRACON_TOKEN}"}"#.to_string()
            };
            // The timeout Claude Code gives a managed session's calls, and the
            // helper declaring it less a minute, so status tools hold longer.
            let timeout = tracon::adapter::claude::MCP_CALL_TIMEOUT_MS;
            let budget = timeout / 1000 - 60;
            println!("\nOr, so the interface can tell your Claude Code agents apart:\n");
            for c in v["channels"].as_array().unwrap_or(&vec![]) {
                let Some(name) = c.as_str() else { continue };
                println!(
                    "  claude mcp add-json --scope user tracon-{name} \
                     '{{\"type\":\"http\",\"url\":\"{base}/mcp/external/{name}\",\"timeout\":{timeout},\"headersHelper\":\"{exe} external lane --wait-budget {budget}\"{auth}}}'"
                );
            }
            // The node launches nothing for this harness, so the identity its
            // commits should carry is offered here, to apply in the repository.
            if let Some(identities) = v["identities"].as_object().filter(|m| !m.is_empty()) {
                println!(
                    "\nCommit as your forge account, in each repository you work on (per channel):\n"
                );
                for (channel, identity) in identities {
                    let name = identity["name"].as_str().unwrap_or_default();
                    let email = identity["email"].as_str().unwrap_or_default();
                    println!(
                        "  {channel}: git config user.name {} && git config user.email {}",
                        shell_quote(name),
                        shell_quote(email)
                    );
                }
            }
            let stopped = v["stopped"].as_array().cloned().unwrap_or_default();
            if !stopped.is_empty() {
                println!("\nStopped (tracon external start <channel> to allow again):");
                for c in stopped {
                    println!("  {}", c.as_str().unwrap_or(""));
                }
            }
            let lanes = v["lanes"].as_array().cloned().unwrap_or_default();
            if lanes.is_empty() {
                println!("\nNo calls in the last day.");
            } else {
                println!("\nCalled in the last day:");
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or_default();
                for l in lanes {
                    let ago = (now - l["last_ms"].as_i64().unwrap_or(now)).max(0) / 60_000;
                    println!(
                        "  {} · {} · {} calls · last {ago}m ago",
                        l["channel"].as_str().unwrap_or(""),
                        l["lane"].as_str().unwrap_or("unlabelled"),
                        l["calls"].as_i64().unwrap_or(0),
                    );
                }
            }
            Ok(())
        }
        ExternalCommand::Stop { channel } => {
            node_call(
                Method::POST,
                &format!("/api/external/{channel}/stop"),
                None,
                None,
            )
            .await?;
            println!("stopped broker access on {channel}");
            Ok(())
        }
        ExternalCommand::Start { channel } => {
            node_call(
                Method::POST,
                &format!("/api/external/{channel}/start"),
                None,
                None,
            )
            .await?;
            println!("broker access on {channel} is allowed again");
            Ok(())
        }
    }
}

/// Say which node's operator token is about to change, and ask. Without a
/// terminal to ask on, only `--yes` goes ahead. The identity check runs first,
/// so a node this state directory is not is refused before anything is asked.
async fn confirm_token_change(verb: &str, yes: bool) -> Result<()> {
    use std::io::{BufRead, IsTerminal, Write};
    check_target_identity().await?;
    let facts = node_facts().await?;
    eprintln!(
        "{verb} the operator token of {} at {}; every logged-in client is logged out.",
        facts.describe(),
        node_url()?
    );
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("nothing changed: there is no terminal to confirm on; pass --yes");
    }
    eprint!("Continue? [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        anyhow::bail!("nothing changed")
    }
}

async fn auth_command(cmd: AuthCommand) -> Result<()> {
    use reqwest::Method;
    match cmd {
        AuthCommand::Issue {
            public_url,
            url,
            yes,
        } => {
            use base64::Engine;
            if url.is_some() {
                eprintln!(
                    "warning: --url is now --public-url; it only sets the login link's address and never chooses the node (TRACON_URL does)"
                );
            }
            let public_url = public_url.or(url);
            confirm_token_change("Rotating", yes).await?;
            use rand::RngCore;
            let mut raw = [0u8; 32];
            rand::rng().fill_bytes(&mut raw);
            let token = format!(
                "trc1.{}",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
            );
            // Only the hash is sent. The node cannot show the token again, and
            // a database read cannot recover it.
            let hash = tracon::http::auth::hash(&token);
            node_call(
                Method::POST,
                "/api/auth/token",
                Some(serde_json::json!({ "token_hash": hash })),
                None,
            )
            .await?;
            println!("{token}");
            println!();
            println!("Shown once. Any client that presents it gets a cookie for this node.");
            println!("Every client logged in with the previous token has been logged out.");
            // A phone types this badly; a QR carries it. The token rides the
            // URL fragment, which browsers never send, so it stays out of
            // server and proxy logs.
            let base = public_url.or_else(|| {
                let u = node_url().ok()?;
                (!["127.0.0.1", "localhost", "[::1]", "::1"]
                    .iter()
                    .any(|h| u.contains(h)))
                .then_some(u)
            });
            match base {
                Some(b) => {
                    let login = format!("{}/#token={token}", b.trim_end_matches('/'));
                    println!();
                    println!("Scan to log in:");
                    if let Some(q) = tracon::mesh::enroll::qr_text(&login) {
                        println!("{q}");
                    }
                    println!("{login}");
                }
                None => {
                    println!("For a login QR a phone can scan, pass --public-url https://<how clients reach this node>.");
                }
            }
            Ok(())
        }
        AuthCommand::Revoke { yes } => {
            confirm_token_change("Removing", yes).await?;
            node_call(Method::DELETE, "/api/auth/token", None, None).await?;
            println!("token removed; this node answers only to loopback again");
            Ok(())
        }
        AuthCommand::Sessions => {
            let v = node_call(Method::GET, "/api/auth/sessions", None, None).await?;
            if v["configured"].as_bool() != Some(true) {
                println!("no operator token set; this node answers only to loopback");
            }
            let clients = v["clients"].as_array().cloned().unwrap_or_default();
            if clients.is_empty() {
                println!("no clients logged in");
            }
            for c in clients {
                // A session another browser approved (device approval) says
                // which one did.
                let delegated = c["delegated_from"]
                    .as_str()
                    .map(|by| format!("  approved by {by}"))
                    .unwrap_or_default();
                println!(
                    "{}  last seen {}{}  {}",
                    c["id"].as_str().unwrap_or(""),
                    c["last_seen_ms"].as_i64().unwrap_or(0),
                    delegated,
                    c["user_agent"].as_str().unwrap_or("-"),
                );
            }
            Ok(())
        }
    }
}

async fn push_command(cmd: PushCommand) -> Result<()> {
    match cmd {
        PushCommand::Ls => {
            let v = node_call(reqwest::Method::GET, "/api/push/subscriptions", None, None).await?;
            let devices = v["devices"].as_array().cloned().unwrap_or_default();
            if let Some(why) = v["not_sending"].as_str() {
                println!("not sending: {why}");
            }
            if devices.is_empty() {
                println!("no devices; enable notifications on the Nodes screen from the phone");
            }
            for d in devices {
                println!(
                    "{}  {}  last ok {}  failures {}  {}",
                    d["id"].as_str().unwrap_or(""),
                    if d["local"].as_bool() == Some(true) {
                        "this machine"
                    } else {
                        "logged-in client"
                    },
                    d["last_ok_ms"]
                        .as_i64()
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "never".into()),
                    d["fail_count"].as_i64().unwrap_or(0),
                    d["user_agent"].as_str().unwrap_or("-"),
                );
                if let Some(error) = d["last_error"].as_str() {
                    println!(
                        "    last error at {}: {error}",
                        d["last_error_ms"].as_i64().unwrap_or(0)
                    );
                }
            }
            Ok(())
        }
        PushCommand::Rm { id } => {
            node_call(
                reqwest::Method::DELETE,
                &format!("/api/push/subscriptions/{id}"),
                None,
                None,
            )
            .await?;
            println!("forgotten");
            Ok(())
        }
        PushCommand::Test => {
            let v = node_call(
                reqwest::Method::POST,
                "/api/push/test",
                Some(serde_json::json!({"all": true})),
                None,
            )
            .await?;
            let sent = v["sent"].as_array().cloned().unwrap_or_default();
            if let Some(why) = v["not_sending"].as_str() {
                println!("not sending: {why}");
            }
            if sent.is_empty() {
                println!("no devices to push to");
            }
            for r in sent {
                println!(
                    "{}  {}",
                    r["id"].as_str().unwrap_or(""),
                    r["outcome"].as_str().unwrap_or("")
                );
            }
            Ok(())
        }
    }
}

/// Skills go through the running node rather than the store, for the reason
/// `doc import` does: one place decides what a name means, hashes the bytes
/// and refuses a duplicate, and it is the place a session will read them from.
async fn skill_command(cmd: SkillCommand) -> Result<()> {
    use reqwest::Method;
    match cmd {
        SkillCommand::Import { source, channel } => {
            // An absolute path, because the node reads it: the CLI may be run
            // from anywhere, and the node's working directory is its own.
            let resolved = match source.split_once('#') {
                Some((path, rev)) => format!(
                    "{}#{rev}",
                    std::path::Path::new(path)
                        .canonicalize()
                        .unwrap_or_else(|_| path.into())
                        .display()
                ),
                None => std::path::Path::new(&source)
                    .canonicalize()
                    .unwrap_or_else(|_| (&source).into())
                    .display()
                    .to_string(),
            };
            let v = node_call(
                Method::POST,
                "/api/manifest/skills",
                Some(serde_json::json!({ "source": resolved, "channel": channel })),
                None,
            )
            .await?;
            println!(
                "{} imported into {channel} ({}…)",
                v["name"].as_str().unwrap_or("?"),
                &v["digest"].as_str().unwrap_or("")
                    [..8.min(v["digest"].as_str().unwrap_or("").len())]
            );
            for file in v["files"].as_array().into_iter().flatten() {
                println!("  {}", file.as_str().unwrap_or(""));
            }
            // Warnings are printed after the file list, and never suppressed:
            // a skill's scripts run in the runner and a harness may register
            // its body as a shell-interpolated slash command.
            for warning in v["warnings"].as_array().into_iter().flatten() {
                println!("! {}", warning.as_str().unwrap_or(""));
            }
            println!("Applies at the next launch on {channel}; running sessions keep theirs.");
            Ok(())
        }
        SkillCommand::Ls { channel } => {
            let v = node_call(
                Method::GET,
                &format!("/api/manifest?channel={channel}"),
                None,
                None,
            )
            .await?;
            for item in v["items"].as_array().into_iter().flatten() {
                println!(
                    "{:<12} {:<24} {}",
                    item["kind"].as_str().unwrap_or(""),
                    item["name"].as_str().unwrap_or(""),
                    item["source"].as_str().unwrap_or("")
                );
            }
            match (v["next"].as_object(), v["error"].as_str()) {
                (_, Some(error)) => println!("\nthis manifest will not build: {error}"),
                (Some(next), _) => println!(
                    "\nnext launch: revision {} ({}…), {} skill(s), {} plugin(s)",
                    next["revision"],
                    &next["digest"].as_str().unwrap_or("")
                        [..8.min(next["digest"].as_str().unwrap_or("").len())],
                    next["skills"].as_array().map(Vec::len).unwrap_or(0),
                    next["plugins"].as_array().map(Vec::len).unwrap_or(0),
                ),
                _ => {}
            }
            if let Some(recorded) = v["recorded"].as_object() {
                println!("last recorded: revision {}", recorded["revision"]);
            }
            Ok(())
        }
        SkillCommand::Rm { name, channel } => {
            node_call(
                Method::DELETE,
                &format!("/api/manifest/skill/{name}?channel={channel}"),
                None,
                None,
            )
            .await?;
            println!("{name} removed from {channel}");
            Ok(())
        }
    }
}

/// The node's configured document kinds, as `GET /api/docs/kinds` lists
/// them: the reserved ones and `other` are the node's own and left out.
async fn node_doc_kinds() -> Result<Vec<tracon::config::DocKind>> {
    let v = node_call(reqwest::Method::GET, "/api/docs/kinds", None, None).await?;
    Ok(v["kinds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|k| k["reserved"] != true)
        .filter_map(|k| {
            Some(tracon::config::DocKind {
                prefix: k["kind"].as_str()?.to_string(),
                label: k["label"].as_str().unwrap_or_default().to_string(),
                description: k["description"].as_str().unwrap_or_default().to_string(),
                working_note: k["working_note"] == true,
            })
        })
        .collect())
}

/// Archive or restore a document by its flag alone, as `doc_archive` does:
/// `PUT /api/docs/{channel}/{slug}` with `archived` and no body.
async fn doc_set_archived(channel: &str, slug: &str, archived: bool) -> Result<()> {
    node_call(
        reqwest::Method::PUT,
        &format!("/api/docs/{channel}/{slug}"),
        Some(serde_json::json!({ "archived": archived })),
        None,
    )
    .await?;
    println!("{} {slug}", if archived { "archived" } else { "restored" });
    Ok(())
}

async fn doc_command(cmd: DocCommand) -> Result<()> {
    use reqwest::Method;
    match cmd {
        DocCommand::Import { dir, channel } => {
            let kinds = node_doc_kinds().await?;
            let (docs, skipped) = tracon::corpus::import::read_dir(&dir, &kinds)?;
            for s in &skipped {
                eprintln!("skipped {s}");
            }
            let mut n = 0;
            for d in docs {
                node_call(
                    Method::PUT,
                    &format!("/api/docs/{channel}/{}", d.slug),
                    Some(serde_json::json!({ "body": d.body, "archived": d.archived })),
                    None,
                )
                .await?;
                println!(
                    "{:<14} {}{}",
                    d.kind,
                    d.slug,
                    if d.archived { " (archived)" } else { "" }
                );
                n += 1;
            }
            println!(
                "{n} document{} imported into {channel}",
                if n == 1 { "" } else { "s" }
            );
            Ok(())
        }
        DocCommand::Ls { channel } => {
            let q = channel.map(|c| format!("?channel={c}")).unwrap_or_default();
            let v = node_call(Method::GET, &format!("/api/docs{q}"), None, None).await?;
            for d in v["docs"].as_array().cloned().unwrap_or_default() {
                println!(
                    "{:<10} {:<14} {:<36} {}",
                    d["channel"].as_str().unwrap_or(""),
                    d["kind"].as_str().unwrap_or(""),
                    d["slug"].as_str().unwrap_or(""),
                    d["title"].as_str().unwrap_or("")
                );
            }
            Ok(())
        }
        DocCommand::Get { slug, channel } => {
            let v = node_call(
                Method::GET,
                &format!("/api/docs/{channel}/{slug}"),
                None,
                None,
            )
            .await?;
            print!("{}", v["body"].as_str().unwrap_or(""));
            Ok(())
        }
        DocCommand::Put {
            slug,
            file,
            channel,
            format,
        } => {
            let body = if file.as_os_str() == "-" {
                let mut s = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
                s
            } else {
                std::fs::read_to_string(&file)?
            };
            let v = node_call(
                Method::PUT,
                &format!("/api/docs/{channel}/{slug}"),
                Some(serde_json::json!({ "body": body, "format": format })),
                None,
            )
            .await?;
            println!("{slug} {}", v["hash"].as_str().unwrap_or(""));
            Ok(())
        }
        DocCommand::Rm { slug, channel } => {
            node_call(
                Method::DELETE,
                &format!("/api/docs/{channel}/{slug}"),
                None,
                None,
            )
            .await?;
            println!("removed {slug}");
            Ok(())
        }
        DocCommand::Archive { slug, channel } => doc_set_archived(&channel, &slug, true).await,
        DocCommand::Unarchive { slug, channel } => doc_set_archived(&channel, &slug, false).await,
        DocCommand::Export { dir, channel } => {
            let v = node_call(
                Method::GET,
                &format!("/api/docs?channel={channel}&archived=true"),
                None,
                None,
            )
            .await?;
            let mut docs = Vec::new();
            let mut skipped_html = 0;
            for d in v["docs"].as_array().cloned().unwrap_or_default() {
                if d["format"].as_str() == Some("html") {
                    skipped_html += 1;
                    continue;
                }
                let slug = d["slug"].as_str().unwrap_or("").to_string();
                let full = node_call(
                    Method::GET,
                    &format!("/api/docs/{channel}/{slug}"),
                    None,
                    None,
                )
                .await?;
                docs.push(tracon::corpus::export::ExportDoc {
                    slug,
                    body: full["body"].as_str().unwrap_or("").to_string(),
                    archived: d["archived"].as_i64() == Some(1),
                });
            }
            // Pruned by the node's kinds, which decide what the export
            // manages, not by this binary's defaults.
            let kinds = node_doc_kinds().await?;
            let r = tracon::corpus::export::sync_dir(&dir, &docs, &kinds)?;
            println!(
                "{} Markdown documents in {}: {} written, {} unchanged, {} removed, {} HTML skipped",
                docs.len(),
                dir.display(),
                r.written,
                r.unchanged,
                r.removed,
                skipped_html,
            );
            Ok(())
        }
        DocCommand::Reindex => {
            let v = node_call(Method::POST, "/api/docs/reindex", None, None).await?;
            println!(
                "rebuilt {} chunks from {} records with {} at dim {}",
                v["chunks"].as_i64().unwrap_or_default(),
                v["records"].as_i64().unwrap_or_default(),
                v["model"].as_str().unwrap_or(""),
                v["dim"].as_i64().unwrap_or_default(),
            );
            Ok(())
        }
    }
}

/// Resolve an id prefix, or a task's generated name as listings print it,
/// against the channel's ledger.
async fn work_resolve(channel: &str, prefix: &str) -> Result<String> {
    use reqwest::Method;
    if prefix.len() == 64 {
        return Ok(prefix.to_string());
    }
    if let Some(hit) = node_resolve(Some("task"), prefix, Some(channel)).await? {
        return Ok(hit.id);
    }
    // A node that predates `/api/resolve`, or a prefix shorter than it
    // resolves: the ledger, read here.
    let v = node_call(
        Method::GET,
        &format!("/api/work?channel={channel}"),
        None,
        None,
    )
    .await?;
    let items = v["items"].as_array().cloned().unwrap_or_default();
    resolve_task(
        items.iter().filter_map(|i| i["id"].as_str()),
        prefix,
        channel,
    )
}

/// A task by a unique id prefix or by its generated name (`task-…`), which is
/// how `tracon work` lists it. A generated name two tasks share lists both.
fn resolve_task<'a>(
    ids: impl Iterator<Item = &'a str>,
    given: &str,
    channel: &str,
) -> Result<String> {
    if !given.starts_with("task-") {
        return resolve_prefix(ids, given, "work item", channel);
    }
    let hits: Vec<&str> = ids.filter(|id| tracon::names::task(id) == given).collect();
    match hits.as_slice() {
        [one] => Ok(one.to_string()),
        [] => anyhow::bail!("no work item is named {given} on {channel}"),
        many => anyhow::bail!(
            "{given} names {} work items on {channel}: {}",
            many.len(),
            many.join(", ")
        ),
    }
}

/// A record the node resolved from what the operator typed.
struct ResolvedRecord {
    kind: String,
    id: String,
    name: String,
    channel: String,
}

/// What `q` names on the node: an operator name, a generated name, a full id,
/// or an id prefix of six or more characters, of `kind` (any kind when
/// `None`). `None` when nothing does, or the node predates resolution; an
/// error listing the candidates when several records answer.
async fn node_resolve(
    kind: Option<&str>,
    q: &str,
    channel: Option<&str>,
) -> Result<Option<ResolvedRecord>> {
    use reqwest::Method;
    let mut path = format!("/api/resolve?q={}", urlencoding(q.trim()));
    if let Some(kind) = kind {
        path.push_str(&format!("&kind={}", urlencoding(kind)));
    }
    if let Some(channel) = channel {
        path.push_str(&format!("&channel={}", urlencoding(channel)));
    }
    match node_call(Method::GET, &path, None, None).await {
        Ok(v) => Ok(v["id"].as_str().map(|id| ResolvedRecord {
            kind: v["kind"].as_str().unwrap_or_default().to_string(),
            id: id.to_string(),
            name: v["name"].as_str().unwrap_or_default().to_string(),
            channel: v["channel"].as_str().unwrap_or_default().to_string(),
        })),
        Err(e) => match e.downcast_ref::<NodeRefusal>() {
            Some(r) if r.status == reqwest::StatusCode::NOT_FOUND => Ok(None),
            _ => Err(e),
        },
    }
}

/// A session by id, prefix or name.
async fn session_resolve(given: &str) -> Result<String> {
    if uuid::Uuid::try_parse(given).is_ok() {
        return Ok(given.to_string());
    }
    match node_resolve(Some("session"), given, None).await? {
        Some(hit) => Ok(hit.id),
        None => anyhow::bail!("no session is named {given}"),
    }
}

async fn name_command(cmd: NameCommand) -> Result<()> {
    use reqwest::Method;
    match cmd {
        NameCommand::Set { kind, record, name } => {
            let Some(hit) = node_resolve(Some(&kind), &record, None).await? else {
                anyhow::bail!("no {kind} is named {record}");
            };
            let v = node_call(
                Method::PUT,
                &format!(
                    "/api/names/{}/{}",
                    urlencoding(&hit.kind),
                    urlencoding(&hit.id)
                ),
                Some(serde_json::json!({ "name": name })),
                None,
            )
            .await?;
            println!(
                "{} {} is now {} (was {})",
                hit.kind,
                hit.id,
                v["name"].as_str().unwrap_or(""),
                hit.name
            );
            Ok(())
        }
        NameCommand::Rm { kind, record } => {
            let Some(hit) = node_resolve(Some(&kind), &record, None).await? else {
                anyhow::bail!("no {kind} is named {record}");
            };
            let v = node_call(
                Method::DELETE,
                &format!(
                    "/api/names/{}/{}",
                    urlencoding(&hit.kind),
                    urlencoding(&hit.id)
                ),
                None,
                None,
            )
            .await?;
            println!(
                "{} {} is {} again",
                hit.kind,
                hit.id,
                v["name"].as_str().unwrap_or("")
            );
            Ok(())
        }
        NameCommand::Show { record } => {
            let Some(hit) = node_resolve(None, &record, None).await? else {
                anyhow::bail!("nothing here is named {record}");
            };
            let generated = tracon::names::generated(&hit.kind, &hit.id);
            println!("{:<10} {}", "kind", hit.kind);
            println!("{:<10} {}", "id", hit.id);
            println!("{:<10} {}", "name", hit.name);
            if hit.name != generated {
                println!("{:<10} {}", "generated", generated);
            }
            println!("{:<10} {}", "channel", hit.channel);
            Ok(())
        }
    }
}

/// Resolve a memory id prefix against the channel's newest memories; an older
/// one needs its full id. Memories have no names, generated or the
/// operator's, so there is no name to accept here.
async fn memory_resolve(channel: &str, prefix: &str) -> Result<String> {
    use reqwest::Method;
    if uuid::Uuid::try_parse(prefix).is_ok() {
        return Ok(prefix.to_string());
    }
    let v = node_call(
        Method::GET,
        &format!("/api/memories?channel={channel}"),
        None,
        None,
    )
    .await?;
    let memories = v["memories"].as_array().cloned().unwrap_or_default();
    resolve_prefix(
        memories.iter().filter_map(|m| m["id"].as_str()),
        prefix,
        "memory",
        channel,
    )
}

fn resolve_prefix<'a>(
    ids: impl Iterator<Item = &'a str>,
    prefix: &str,
    noun: &str,
    channel: &str,
) -> Result<String> {
    let hits: Vec<&str> = ids.filter(|id| id.starts_with(prefix)).collect();
    match hits.as_slice() {
        [one] => Ok(one.to_string()),
        [] => anyhow::bail!("no {noun} starts with {prefix} on {channel}"),
        _ => anyhow::bail!("{prefix} is ambiguous on {channel}"),
    }
}

/// The ledger's priority levels and the channel's project names, for
/// printing and for reading what the operator typed.
struct WorkNames {
    levels: Vec<(String, i64)>,
    projects: Vec<(String, String)>,
    /// Each project's forge remote, by id, where it has one.
    remotes: Vec<(String, String)>,
}

impl WorkNames {
    async fn load(channel: &str) -> Result<Self> {
        use reqwest::Method;
        let node = node_call(Method::GET, "/api/node", None, None).await?;
        let levels = node["work"]["priority_levels"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|l| Some((l["name"].as_str()?.to_string(), l["value"].as_i64()?)))
                    .collect()
            })
            .unwrap_or_default();
        let v = node_call(
            Method::GET,
            &format!("/api/projects?channel={}", urlencode(channel)),
            None,
            None,
        )
        .await?;
        let projects = v["projects"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| {
                        Some((
                            p["id"].as_str()?.to_string(),
                            p["name"].as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let remotes = v["projects"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| {
                        let id = p["id"].as_str()?;
                        let url = p["remote_url"].as_str()?.trim();
                        (!url.is_empty()).then(|| (id.to_string(), url.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            levels,
            projects,
            remotes,
        })
    }

    /// The project's forge remote, as the Composer's `projectRepo` reads it.
    fn remote(&self, id: &str) -> Option<String> {
        self.remotes
            .iter()
            .find(|(p, _)| p == id)
            .map(|(_, url)| url.clone())
    }

    fn priority(&self, text: &str) -> Result<i64> {
        let text = text.trim();
        if let Some((_, v)) = self
            .levels
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(text))
        {
            return Ok(*v);
        }
        text.parse().map_err(|_| {
            let names: Vec<&str> = self.levels.iter().map(|(n, _)| n.as_str()).collect();
            anyhow::anyhow!(
                "priority {text:?} is not a number{}",
                if names.is_empty() {
                    String::new()
                } else {
                    format!(" or one of {}", names.join(", "))
                }
            )
        })
    }

    fn priority_name(&self, value: i64) -> String {
        self.levels
            .iter()
            .find(|(_, v)| *v == value)
            .map(|(n, _)| n.clone())
            .unwrap_or_else(|| value.to_string())
    }

    /// `None` for `none`; otherwise the project's id, by name, id or prefix.
    fn project(&self, text: &str) -> Result<Option<String>> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("none") {
            return Ok(None);
        }
        let hits: Vec<&String> = self
            .projects
            .iter()
            .filter(|(id, name)| {
                id == text
                    || name.eq_ignore_ascii_case(text)
                    || (text.len() >= 6 && id.starts_with(text))
            })
            .map(|(id, _)| id)
            .collect();
        match hits.as_slice() {
            [one] => Ok(Some((*one).clone())),
            [] => {
                let names: Vec<&str> = self.projects.iter().map(|(_, n)| n.as_str()).collect();
                if names.is_empty() {
                    anyhow::bail!("no project {text:?}: none is known on this channel yet")
                }
                anyhow::bail!("no project {text:?}; known: {}", names.join(", "))
            }
            _ => anyhow::bail!("{text:?} names more than one project; give its id"),
        }
    }

    fn project_name(&self, id: &str) -> String {
        self.projects
            .iter()
            .find(|(p, _)| p == id)
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| id[..8.min(id.len())].to_string())
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn work_line(i: &serde_json::Value, names: &WorkNames) -> String {
    let id = i["id"].as_str().unwrap_or("");
    let state = match i["readiness"]["state"].as_str().unwrap_or("") {
        "ready" if i["session_id"].is_string() => "in session".to_string(),
        "blocked" => {
            let by: Vec<String> = i["readiness"]["by"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|b| match b["kind"].as_str() {
                            Some("cycle") => "cycle".to_string(),
                            _ => tracon::names::task(b["id"].as_str().unwrap_or("?")),
                        })
                        .collect()
                })
                .unwrap_or_default();
            format!("blocked by {}", by.join(","))
        }
        other => other.to_string(),
    };
    let project = i["project_id"]
        .as_str()
        .map(|p| format!("  [{}]", names.project_name(p)))
        .unwrap_or_default();
    format!(
        "{:<26} {:<9} {:<24} {}{project}",
        tracon::names::task(id),
        names.priority_name(i["priority"].as_i64().unwrap_or(0)),
        state,
        i["title"].as_str().unwrap_or("")
    )
}

async fn work_command(cmd: WorkCommand) -> Result<()> {
    use reqwest::Method;
    use serde_json::json;
    match cmd {
        WorkCommand::Add {
            title,
            channel,
            project,
            body,
            dep,
            priority,
        } => {
            let mut deps = Vec::new();
            for d in dep.iter().filter(|d| !d.trim().is_empty()) {
                deps.push(work_resolve(&channel, d.trim()).await?);
            }
            let names = WorkNames::load(&channel).await?;
            let project = match project {
                Some(p) => names.project(&p)?,
                None => None,
            };
            let priority = priority.map(|p| names.priority(&p)).transpose()?;
            let v = node_call(
                Method::POST,
                "/api/work",
                Some(json!({
                    "channel": channel, "project_id": project, "title": title, "body": body,
                    "deps": deps, "priority": priority,
                })),
                None,
            )
            .await?;
            println!("{}", v["id"].as_str().unwrap_or(""));
            Ok(())
        }
        WorkCommand::Ls {
            channel,
            project,
            state,
            query,
        } => {
            let names = WorkNames::load(&channel).await?;
            let mut q = format!("/api/work?channel={}", urlencode(&channel));
            if let Some(p) = project {
                let id = names.project(&p)?.unwrap_or_else(|| "none".into());
                q.push_str(&format!("&project_id={id}"));
            }
            if let Some(s) = state {
                q.push_str(&format!("&state={}", urlencode(&s)));
            }
            if let Some(words) = query {
                q.push_str(&format!("&q={}", urlencode(&words)));
            }
            let v = node_call(Method::GET, &q, None, None).await?;
            for i in v["items"].as_array().cloned().unwrap_or_default() {
                println!("{}", work_line(&i, &names));
            }
            Ok(())
        }
        WorkCommand::Ready { channel, project } => {
            let names = WorkNames::load(&channel).await?;
            let mut q = format!("/api/work/ready?channel={}", urlencode(&channel));
            if let Some(p) = project {
                if let Some(id) = names.project(&p)? {
                    q.push_str(&format!("&project_id={id}"));
                }
            }
            let v = node_call(Method::GET, &q, None, None).await?;
            for i in v["items"].as_array().cloned().unwrap_or_default() {
                println!("{}", work_line(&i, &names));
            }
            Ok(())
        }
        WorkCommand::Edit {
            id,
            channel,
            title,
            body,
            body_file,
            priority,
            project,
        } => {
            let id = work_resolve(&channel, &id).await?;
            let names = WorkNames::load(&channel).await?;
            let mut patch = serde_json::Map::new();
            if let Some(t) = title {
                patch.insert("title".into(), json!(t));
            }
            let body = match body_file {
                Some(path) if path.as_os_str() == "-" => {
                    let mut text = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
                    Some(text)
                }
                Some(path) => Some(std::fs::read_to_string(&path)?),
                None => body,
            };
            if let Some(b) = body {
                patch.insert("body".into(), json!(b));
            }
            if let Some(p) = priority {
                patch.insert("priority".into(), json!(names.priority(&p)?));
            }
            if let Some(p) = project {
                patch.insert("project_id".into(), json!(names.project(&p)?));
            }
            if patch.is_empty() {
                anyhow::bail!("nothing to change: give --title, --body, --priority or --project");
            }
            node_call(
                Method::PUT,
                &format!("/api/work/{id}"),
                Some(serde_json::Value::Object(patch)),
                None,
            )
            .await?;
            let v = node_call(Method::GET, &format!("/api/work/{id}"), None, None).await?;
            println!("{}", work_line(&v["item"], &names));
            Ok(())
        }
        WorkCommand::Show { id, channel } => {
            let id = work_resolve(&channel, &id).await?;
            let v = node_call(Method::GET, &format!("/api/work/{id}"), None, None).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        WorkCommand::Close { id, channel } => {
            let id = work_resolve(&channel, &id).await?;
            let v = node_call(
                Method::PUT,
                &format!("/api/work/{id}"),
                Some(json!({ "state": "closed" })),
                None,
            )
            .await?;
            println!(
                "closed {}",
                tracon::names::task(v["id"].as_str().unwrap_or(&id))
            );
            Ok(())
        }
        WorkCommand::Dep { id, on, channel } => {
            let id = work_resolve(&channel, &id).await?;
            let on = work_resolve(&channel, &on).await?;
            let cur = node_call(Method::GET, &format!("/api/work/{id}"), None, None).await?;
            let mut deps: Vec<String> = cur["item"]["deps"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|d| d.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            deps.push(on);
            node_call(
                Method::PUT,
                &format!("/api/work/{id}"),
                Some(json!({ "deps": deps })),
                None,
            )
            .await?;
            println!("ok");
            Ok(())
        }
        WorkCommand::Rm { id, channel } => {
            let id = work_resolve(&channel, &id).await?;
            node_call(Method::DELETE, &format!("/api/work/{id}"), None, None).await?;
            println!("removed {}", tracon::names::task(&id));
            Ok(())
        }
        WorkCommand::Start {
            id,
            channel,
            phase,
            node,
            branch,
            base,
            model,
            harness,
            budget,
            repo,
            json,
            request_id,
        } => {
            work_start(WorkStart {
                id,
                channel,
                phase,
                node,
                branch,
                base,
                model,
                harness,
                budget,
                repo,
                json,
                request_id,
            })
            .await
        }
    }
}

/// Whether the channel holds the document: a plan slug on an item whose
/// document was since removed is no plan.
async fn doc_exists(channel: &str, slug: &str) -> Result<bool> {
    match node_call(
        reqwest::Method::GET,
        &format!("/api/docs/{}/{}", urlencode(channel), urlencode(slug)),
        None,
        None,
    )
    .await
    {
        Ok(_) => Ok(true),
        Err(e) => match e.downcast_ref::<NodeRefusal>() {
            Some(r) if r.status == reqwest::StatusCode::NOT_FOUND => Ok(false),
            _ => Err(e),
        },
    }
}

/// The branch of an item's most recent session, from `GET /api/work/{id}`'s
/// `sessions`.
fn latest_branch(sessions: &serde_json::Value) -> Option<String> {
    sessions
        .as_array()?
        .iter()
        .filter(|s| s["branch"].as_str().is_some_and(|b| !b.is_empty()))
        .max_by_key(|s| s["created_ms"].as_i64().unwrap_or(0))
        .and_then(|s| s["branch"].as_str())
        .map(str::to_string)
}

/// A node from `GET /api/nodes` by `self`, its name or its generated
/// `node-…` name (any case), its full id or a unique prefix of it.
fn resolve_node<'a>(nodes: &'a [serde_json::Value], text: &str) -> Result<&'a serde_json::Value> {
    let text = text.trim();
    let hits: Vec<&serde_json::Value> = if text.eq_ignore_ascii_case("self") {
        nodes.iter().filter(|n| n["is_self"] == true).collect()
    } else {
        let by_name: Vec<_> = nodes
            .iter()
            .filter(|n| {
                n["name"]
                    .as_str()
                    .is_some_and(|m| m.eq_ignore_ascii_case(text))
                    || n["id"]
                        .as_str()
                        .is_some_and(|id| tracon::names::node(id, None).eq_ignore_ascii_case(text))
            })
            .collect();
        if by_name.is_empty() && !text.is_empty() {
            nodes
                .iter()
                .filter(|n| n["id"].as_str().is_some_and(|id| id.starts_with(text)))
                .collect()
        } else {
            by_name
        }
    };
    let known = || {
        let names: Vec<String> = nodes.iter().map(node_label).collect();
        if names.is_empty() {
            "none".to_string()
        } else {
            names.join(", ")
        }
    };
    match hits.as_slice() {
        [one] => Ok(one),
        [] => anyhow::bail!("no node {text:?}; known: {}", known()),
        _ => anyhow::bail!(
            "{text:?} names more than one node; give its name or more of its id. Known: {}",
            known()
        ),
    }
}

/// A node's name, or its generated one when it has none.
fn node_label(n: &serde_json::Value) -> String {
    let id = n["id"].as_str().unwrap_or("");
    tracon::names::node(id, n["name"].as_str().filter(|m| !m.is_empty()))
}

/// What `tracon work start` was given.
struct WorkStart {
    id: String,
    channel: String,
    phase: Option<String>,
    node: Option<String>,
    branch: Option<String>,
    base: Option<String>,
    model: Option<String>,
    harness: Option<String>,
    budget: Option<i64>,
    repo: Option<String>,
    json: bool,
    request_id: Option<String>,
}

/// A thin client over `POST /api/sessions`, resolving what the Composer
/// resolves: the repository from the item's project, the phase from whether
/// a plan exists, the branch from the item's latest session, and the node by
/// name. Readiness, the plan requirement and the model are the node's to
/// decide; its refusal is printed as it comes back.
async fn work_start(a: WorkStart) -> Result<()> {
    use reqwest::Method;
    use serde_json::json;
    let id = work_resolve(&a.channel, &a.id).await?;
    let v = node_call(Method::GET, &format!("/api/work/{id}"), None, None).await?;
    let item = &v["item"];
    if item.is_null() {
        anyhow::bail!("no work item {id}");
    }
    let channel = item["channel"].as_str().unwrap_or(&a.channel).to_string();

    let repo = match a.repo.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) => r.to_string(),
        None => {
            let names = WorkNames::load(&channel).await?;
            match item["project_id"].as_str() {
                None => anyhow::bail!(
                    "{} has no project, so there is no repository to start in: give --repo <forge url>",
                    tracon::names::task(&id)
                ),
                Some(p) => names.remote(p).ok_or_else(|| {
                    anyhow::anyhow!(
                        "project {} has no forge remote: give --repo <forge url>",
                        names.project_name(p)
                    )
                })?,
            }
        }
    };

    let (phase, phase_chosen) = match a.phase {
        Some(p) => (p, false),
        None => {
            let planned = match item["phase_plan_slug"].as_str() {
                Some(slug) => doc_exists(&channel, slug).await?,
                None => false,
            };
            (if planned { "execute" } else { "plan" }.to_string(), true)
        }
    };

    let branch = a.branch.or_else(|| latest_branch(&v["sessions"]));

    let nodes = node_call(Method::GET, "/api/nodes", None, None).await?;
    let nodes = nodes.as_array().cloned().unwrap_or_default();
    let target = match a.node.as_deref() {
        Some(text) => {
            let n = resolve_node(&nodes, text)?;
            if n["is_self"] != true && n["reachable"] == false {
                eprintln!(
                    "warning: node {} is not reachable now; the node forwarding to it will say if it cannot",
                    node_label(n)
                );
            }
            n["id"].as_str().map(str::to_string)
        }
        None => None,
    };

    let mut body = json!({
        "channel": channel,
        "repo_path": repo,
        "work_item_id": id,
        "phase": phase,
    });
    let fields = [
        ("branch", branch),
        ("base", a.base),
        ("model", a.model),
        ("harness", a.harness),
        ("node_id", target),
    ];
    for (key, value) in fields {
        if let Some(value) = value.filter(|v| !v.trim().is_empty()) {
            body[key] = json!(value);
        }
    }
    if let Some(b) = a.budget {
        body["budget_tokens"] = json!(b);
    }

    let request_id = a
        .request_id
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    body["request_id"] = json!(request_id);
    let row = post_start(&body, &id, &channel, &request_id).await?;
    if a.json {
        println!("{}", serde_json::to_string_pretty(&row)?);
        return Ok(());
    }
    let sid = row["id"].as_str().unwrap_or("?");
    let on = row["node_id"].as_str().unwrap_or("");
    let node_name = nodes
        .iter()
        .find(|n| n["id"] == on)
        .map(node_label)
        .unwrap_or_else(|| tracon::names::node(on, None));
    println!("started {sid}");
    println!(
        "  task         {} {}",
        tracon::names::task(&id),
        item["title"].as_str().unwrap_or("")
    );
    println!("  node         {node_name}");
    println!(
        "  phase        {}{}",
        row["phase"].as_str().unwrap_or(&phase),
        if phase_chosen {
            if phase == "plan" {
                " (no plan yet)"
            } else {
                " (the task has a plan)"
            }
        } else {
            ""
        }
    );
    println!("  branch       {}", row["branch"].as_str().unwrap_or(""));
    println!("  model        {}", row["model"].as_str().unwrap_or(""));
    println!(
        "  harness      {}",
        row["harness_id"].as_str().unwrap_or("")
    );
    println!(
        "  repository   {}",
        row["repo_path"].as_str().unwrap_or(&repo)
    );
    println!("  open         {}/sessions/{sid}", node_url()?);
    Ok(())
}

/// `POST /api/sessions` for `work start`. A node that forwarded the start to
/// a peer and heard nothing back in time cannot say whether the peer made
/// the session: then the item's sessions are read for one with this
/// request id, and the same body, id and all, is sent once more, which the
/// owner answers with the session the first made if it made one. Still
/// nothing, and what is printed says how to check and how to retry safely.
async fn post_start(
    body: &serde_json::Value,
    task: &str,
    channel: &str,
    request_id: &str,
) -> Result<serde_json::Value> {
    let mut unknown = String::new();
    for attempt in 0..2 {
        match node_call(
            reqwest::Method::POST,
            "/api/sessions",
            Some(body.clone()),
            None,
        )
        .await
        {
            Ok(row) => return Ok(row),
            Err(e) => match e.downcast_ref::<NodeRefusal>() {
                Some(r) if r.kind.as_deref() == Some("outcome_unknown") => {
                    unknown = r.message.clone();
                    if let Some(row) = session_by_request(task, request_id).await {
                        return Ok(row);
                    }
                    if attempt == 0 {
                        eprintln!(
                            "the session's node did not answer in time; sending the start again \
                             with the same request id ({request_id}), which cannot start a second session"
                        );
                    }
                }
                _ => return Err(e),
            },
        }
    }
    let task_name = tracon::names::task(task);
    anyhow::bail!(
        "{unknown}\n\nThe session may still be created. Check with `tracon work show {task_name} \
         --channel {channel}`, or retry safely with `tracon work start {task_name} --channel \
         {channel} --request-id {request_id}` and the same options: that returns the session if \
         one was made instead of starting another."
    )
}

/// The item's session made for `request_id`, among the node's own and the
/// ones it mirrors from peers, if it has it.
async fn session_by_request(task: &str, request_id: &str) -> Option<serde_json::Value> {
    let v = node_call(
        reqwest::Method::GET,
        &format!("/api/work/{task}"),
        None,
        None,
    )
    .await
    .ok()?;
    v["sessions"]
        .as_array()?
        .iter()
        .find(|s| s["request_id"] == request_id)
        .cloned()
}

async fn memory_command(cmd: MemoryCommand) -> Result<()> {
    use reqwest::Method;
    match cmd {
        MemoryCommand::Ls { channel, state } => {
            let q = state.map(|s| format!("&state={s}")).unwrap_or_default();
            let v = node_call(
                Method::GET,
                &format!("/api/memories?channel={channel}{q}"),
                None,
                None,
            )
            .await?;
            // In full: a UUIDv7 begins with its timestamp, so memories added
            // together share any short prefix.
            for m in v["memories"].as_array().cloned().unwrap_or_default() {
                println!(
                    "{}  {:<9} {:<9} {:<8} {:.2}  {}",
                    m["id"].as_str().unwrap_or(""),
                    m["kind"].as_str().unwrap_or(""),
                    m["state"].as_str().unwrap_or(""),
                    m["scope"].as_str().unwrap_or(""),
                    m["confidence"].as_f64().unwrap_or(0.0),
                    m["body"]
                        .as_str()
                        .unwrap_or("")
                        .lines()
                        .next()
                        .unwrap_or("")
                );
            }
            Ok(())
        }
        MemoryCommand::Add {
            body,
            channel,
            kind,
            scope,
            scope_ref,
        } => {
            let v = node_call(
                Method::POST,
                "/api/memories",
                Some(serde_json::json!({ "channel": channel, "kind": kind, "scope": scope, "scope_ref": scope_ref, "body": body })),
                None,
            )
            .await?;
            println!("{}", v["id"].as_str().unwrap_or(""));
            Ok(())
        }
        MemoryCommand::Rm { id, channel } => {
            let id = memory_resolve(&channel, &id).await?;
            node_call(Method::DELETE, &format!("/api/memories/{id}"), None, None).await?;
            println!("removed {id}");
            Ok(())
        }
        MemoryCommand::Batch { now } => {
            if !now {
                anyhow::bail!("pass --now; the nightly batch runs on its own");
            }
            let v = node_call(Method::POST, "/api/promotions/batch", None, None).await?;
            let n = v["created"].as_array().map(|a| a.len()).unwrap_or(0);
            println!("{n} batch{} created", if n == 1 { "" } else { "es" });
            Ok(())
        }
        MemoryCommand::Recall { query, channel } => {
            let q = urlencoding(&query);
            let v = node_call(
                Method::GET,
                &format!("/api/memories?channel={channel}&q={q}"),
                None,
                None,
            )
            .await?;
            for h in v["hits"].as_array().cloned().unwrap_or_default() {
                println!(
                    "{:<9} {:<28} {}",
                    h["kind"].as_str().unwrap_or(""),
                    h["slug"].as_str().unwrap_or(""),
                    h["text"]
                        .as_str()
                        .unwrap_or("")
                        .lines()
                        .next()
                        .unwrap_or("")
                );
            }
            Ok(())
        }
    }
}

/// The tail of a `credential rm` line: the other nodes a removed credential
/// listed, each owed a revoke none has confirmed yet, or the origin whose
/// sign-in a removed copy was, which nothing is sent to.
fn revoke_pending(others: &[String], origin: Option<&str>, when: &str) -> String {
    if let Some(origin) = origin {
        format!(
            "; this node's copy only: the sign-in is {}'s, and it and any other holders keep theirs",
            &origin[..16.min(origin.len())]
        )
    } else if others.is_empty() {
        String::new()
    } else {
        format!(
            "; a revoke is pending for {} ({when}; `tracon credential revokes` follows it)",
            others
                .iter()
                .map(|n| &n[..16.min(n.len())])
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// The warning a `credential import` gives for each credential it replaced
/// that this node had shared: the holders keep the old value, and are still
/// listed, so a later `rm` revokes their copies.
fn import_replaced_shared(name: &str, nodes: &[String]) -> String {
    format!(
        "warning: {name} replaced a credential shared with {}; they keep the old value until \
         `tracon credential share` sends this one, and `tracon credential rm {name}` still \
         revokes their copies",
        nodes
            .iter()
            .map(|n| tracon::names::node(n, None))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn credential_command(cmd: CredentialCommand) -> Result<()> {
    use tracon::broker::Broker;
    let (id, _) = tracon::mesh::identity::load_or_generate()?;
    let key = id.credential_store_key();
    let mut broker = Broker::load(&key)?;
    match cmd {
        CredentialCommand::Ls => {
            if broker.is_empty() {
                println!("no credentials ({})", Broker::path().display());
            }
            for (name, c) in broker.iter() {
                let mut line = format!("{name:<20} {:<8}", c.kind);
                if let Some(p) = &c.provider {
                    line.push_str(&format!(" provider={p}"));
                }
                line.push_str(&format!(" channels={}", c.channels.join(",")));
                if !c.repos.is_empty() {
                    line.push_str(&format!(" repos={}", c.repos.join(",")));
                }
                if !c.nodes.is_empty() {
                    line.push_str(&format!(
                        " nodes={}",
                        c.nodes
                            .iter()
                            .map(|n| &n[..8.min(n.len())])
                            .collect::<Vec<_>>()
                            .join(",")
                    ));
                }
                if let Some(i) = &c.identity {
                    line.push_str(&format!(" identity={i}"));
                }
                if let Some(e) = c.expires_ms {
                    line.push_str(&format!(" expires_ms={e}"));
                }
                println!("{line}");
            }
            Ok(())
        }
        CredentialCommand::Import { file } => {
            let text = std::fs::read_to_string(&file)?;
            let incoming = Broker::parse_plain(&file, &text)?;
            // A running node reads the store once, at start: sealing around
            // it would leave it brokering the old copy until it restarts.
            // Handed to it instead, it seals them itself and uses them now.
            if let Some(url) = running_node().await? {
                let v = node_call(
                    reqwest::Method::POST,
                    "/api/credentials/import",
                    Some(serde_json::json!({ "toml": text })),
                    None,
                )
                .await?;
                let n = v["imported"].as_array().map_or(0, Vec::len);
                println!(
                    "{n} credential{} imported into the running node at {url}",
                    if n == 1 { "" } else { "s" }
                );
                for (name, nodes) in v["shared"].as_object().into_iter().flatten() {
                    let nodes: Vec<String> = nodes
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|n| n.as_str().map(str::to_string))
                        .collect();
                    eprintln!("{}", import_replaced_shared(name, &nodes));
                }
                return Ok(());
            }
            let here = id.node_id();
            let mut n = 0;
            let mut shared = Vec::new();
            for (name, c) in incoming.iter() {
                let carried = broker.import(name, c.clone(), &here);
                if !carried.is_empty() {
                    shared.push((name.to_string(), carried));
                }
                n += 1;
            }
            broker.save(&key)?;
            println!(
                "{n} credential{} sealed into {}",
                if n == 1 { "" } else { "s" },
                Broker::path().display()
            );
            for (name, nodes) in &shared {
                eprintln!("{}", import_replaced_shared(name, nodes));
            }
            Ok(())
        }
        CredentialCommand::Rm { name } => {
            // As with import: a running node brokers the copy it read at
            // start, and would write it back the next time it saved. Removed
            // through it, it stops brokering the credential now.
            if let Some(url) = running_node().await? {
                let v = node_call(
                    reqwest::Method::DELETE,
                    &format!("/api/credentials/{}", urlencoding(&name)),
                    None,
                    None,
                )
                .await?;
                let others: Vec<String> = v["revoke"]["pending"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|node| node.as_str().map(str::to_string))
                    .collect();
                println!(
                    "removed {name} from the running node at {url}{}",
                    revoke_pending(&others, v["origin"].as_str(), "queued at the hub")
                );
                return Ok(());
            }
            let Some(removed) = broker.revoke(&name) else {
                anyhow::bail!("no credential named {name}");
            };
            let here = id.node_id();
            let others = removed.revoke_targets(&here);
            // Recorded before the store loses the credential, for the node
            // to send when it next runs and reaches the hub: a removal saved
            // without it would never be sent.
            let mut recorded = None;
            if !others.is_empty() {
                let store = tracon::store::Store::open(&config::Config::db_path())?;
                let revoke_id = store.create_credential_revoke(
                    &name,
                    removed.grant_id.as_deref(),
                    removed.grant_version,
                    &others,
                )?;
                recorded = Some((store, revoke_id));
            }
            if let Err(error) = broker.save(&key) {
                if let Some((store, revoke_id)) = recorded {
                    let _ = store.cancel_credential_revoke(&revoke_id);
                }
                return Err(error.into());
            }
            let origin = (!removed.originates_at(&here))
                .then(|| removed.origin())
                .flatten();
            println!(
                "removed {name}{}",
                revoke_pending(&others, origin, "sent when the node next runs")
            );
            Ok(())
        }
        CredentialCommand::Revokes { cancel } => {
            let store = tracon::store::Store::open(&config::Config::db_path())?;
            if let Some(prefix) = cancel {
                let matching = store.credential_revoke_ids(&prefix)?;
                if matching.len() > 1 {
                    anyhow::bail!(
                        "{prefix} names {} revokes; give more of the id",
                        matching.len()
                    );
                }
                let Some(gone) = store.cancel_credential_revoke(&prefix)?.pop() else {
                    anyhow::bail!("no credential revoke {prefix}");
                };
                println!(
                    "cancelled revoke {}; nothing more is sent for it",
                    &gone[..8.min(gone.len())]
                );
                return Ok(());
            }
            let rows = store.credential_revokes()?;
            if rows.is_empty() {
                println!("no credential revokes");
            }
            let mut last = None;
            for t in rows {
                if last.as_deref() != Some(t.revoke_id.as_str()) {
                    println!(
                        "{} (revoke {})",
                        t.name,
                        &t.revoke_id[..8.min(t.revoke_id.len())]
                    );
                    last = Some(t.revoke_id.clone());
                }
                let state = match t.outcome.as_deref() {
                    _ if t.abandoned() => "abandoned",
                    _ if !t.confirmed() => "pending",
                    Some("dropped") => "dropped",
                    Some("kept") => "kept",
                    Some("not_held") => "not held",
                    _ => "confirmed",
                };
                let mut line = format!("  {:<24} {state}", store.node_display_name(&t.node_id));
                if !t.confirmed() || t.kept() {
                    if let Some(detail) = &t.detail {
                        line.push_str(&format!(" ({detail})"));
                    }
                }
                println!("{line}");
            }
            Ok(())
        }
        CredentialCommand::Share { name, to } => {
            let mut cred = broker
                .get(&name)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no credential named {name}"))?;
            if !cred.nodes.iter().any(|n| n == &to) {
                anyhow::bail!("{name} does not list {to} in `nodes`; it would be refused there");
            }
            if let Some(origin) = cred.shared_from(&id.node_id()) {
                anyhow::bail!(
                    "{name} is a copy of {}'s sign-in; only that node can share it, so that its revoke reaches every copy",
                    &origin[..16.min(origin.len())]
                );
            }
            // A running node holds the store and would write its own copy
            // over one sealed around it, so it shares; it names the sign-in
            // this share is, as it would here.
            if running_node().await?.is_some() {
                node_call(
                    reqwest::Method::POST,
                    &format!("/api/credentials/{}/share", urlencoding(&name)),
                    Some(serde_json::json!({ "to": to })),
                    None,
                )
                .await?;
                println!("handed {name} to {}", &to[..16.min(to.len())]);
                return Ok(());
            }
            // A share names its sign-in, and this node as its origin, so a
            // revoke and a tombstone can tell it from a later one.
            if cred.ensure_grant(&id.node_id()) {
                broker.put(&name, cred.clone());
                broker.save(&key)?;
            }
            let cfg = node_config()?;
            let hub = cfg
                .mesh
                .hub_url
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no hub configured"))?;
            let members =
                tracon::mesh::client::MeshClient::get_once(&id, &hub, "/v0/members").await?;
            let m = members
                .as_array()
                .and_then(|a| {
                    a.iter()
                        .find(|m| m["node_id"].as_str() == Some(to.as_str()))
                })
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{to} is not a member of the hub"))?;
            let grantee = proto::keys::key32(m["x25519_pub"].as_str().unwrap_or(""))
                .map(x25519_dalek::PublicKey::from)
                .ok_or_else(|| anyhow::anyhow!("the node's sealing key is malformed"))?;
            let payload = proto::frame::Payload::CredentialHandoff {
                credentials: Broker::handoff_rows(&[(name.clone(), cred)]),
            };
            tracon::mesh::enroll::post_direct(&id, &hub, &to, &grantee, &payload).await?;
            println!("handed {name} to {}", &to[..16.min(to.len())]);
            Ok(())
        }
    }
}

/// One shell word, single-quoted, for a line meant to be pasted.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// One line per hub member for `tracon mesh members`: the full node id (the
/// form every command and API that takes a node id accepts), the name padded
/// so the channels line up across rows, the channels, and `(you)` on this
/// node's row.
fn member_lines(members: &serde_json::Value, me: &str) -> Vec<String> {
    let members = members.as_array().map(Vec::as_slice).unwrap_or_default();
    let field = |m: &serde_json::Value, k: &str| m[k].as_str().unwrap_or("").to_string();
    let id_width = members
        .iter()
        .map(|m| field(m, "node_id").chars().count())
        .max()
        .unwrap_or(0);
    let name_width = members
        .iter()
        .map(|m| field(m, "name").chars().count())
        .max()
        .unwrap_or(0)
        .max(16);
    members
        .iter()
        .map(|m| {
            let channels: Vec<&str> = m["channels"]
                .as_array()
                .map(|a| a.iter().filter_map(|c| c.as_str()).collect())
                .unwrap_or_default();
            let nid = field(m, "node_id");
            format!(
                "{nid:<id_width$}  {:<name_width$} {}{}",
                field(m, "name"),
                channels.join(", "),
                if nid == me { "  (you)" } else { "" }
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::member_lines;
    use super::plugin_tool_line;
    use super::resolve_prefix;
    use super::{awaited_build, build_started, offered_line, AwaitedBuild};
    use serde_json::json;

    /// `tracon mesh members` prints each member's whole node id, since no
    /// command or API takes a prefix, and the columns still line up.
    #[test]
    fn mesh_members_print_whole_node_ids_in_aligned_columns() {
        let me = "2b310f36c18605ac".to_string() + &"a".repeat(48);
        let peer = "ca5dddf6756ad8fa".to_string() + &"b".repeat(48);
        let lines = member_lines(
            &json!([
                { "node_id": me, "name": "bazzite", "channels": ["@mesh", "personal"] },
                { "node_id": peer, "name": "a-name-longer-than-16", "channels": ["work"] },
            ]),
            &me,
        );
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].starts_with(&format!("{me}  bazzite ")),
            "{}",
            lines[0]
        );
        assert!(lines[0].ends_with("@mesh, personal  (you)"), "{}", lines[0]);
        assert!(lines[1].starts_with(&format!("{peer}  a-name-longer-than-16 work")));
        assert!(!lines[1].contains("(you)"));
        assert_eq!(lines[0].find("@mesh"), lines[1].find("work"));
        assert!(member_lines(&json!({}), &me).is_empty());
    }

    /// Where a plugin is offered, with the entry's channels when they narrow
    /// it, and nothing from a node too old to say.
    #[test]
    fn a_plugin_says_where_it_is_offered() {
        assert_eq!(
            offered_line(&json!({ "offered_on": ["personal", "work"], "channels": null })),
            Some("offered on: personal, work".into())
        );
        assert_eq!(
            offered_line(&json!({ "offered_on": [], "channels": [] })),
            Some("offered on: no channel (channels = [])".into())
        );
        assert_eq!(
            offered_line(&json!({ "offered_on": ["work"], "channels": ["work", "gone"] })),
            Some("offered on: work (channels = [work, gone])".into())
        );
        assert_eq!(offered_line(&json!({ "name": "old" })), None);
    }

    /// Each of a plugin's tools says how a call to it is answered, and by
    /// which rule when one decides it.
    #[test]
    fn a_plugin_tool_says_how_it_is_answered() {
        assert_eq!(
            plugin_tool_line(&json!({
                "name": "sql-review__query", "hint": "read", "consequential": false,
                "decision": "allow", "rule_id": "brokered-reads",
            })),
            "tool sql-review__query: runs (rule brokered-reads); says it reads"
        );
        assert_eq!(
            plugin_tool_line(&json!({
                "name": "sql-review__write", "hint": "write", "consequential": true,
                "decision": "ask", "rule_id": null,
            })),
            "tool sql-review__write: asked; says it writes, consequential"
        );
    }

    /// The build's first line names the commit it reads, and says when the
    /// repository has none rather than printing empty fields (#937).
    #[test]
    fn a_build_of_an_empty_repository_says_it_is_empty() {
        let started = json!({
            "source_ref": "origin/main",
            "source_commit": "0123456789abcdef",
            "joined": false,
        });
        assert_eq!(
            build_started(&started, "/r"),
            "building /r from origin/main (0123456789ab); a first build can take several minutes"
        );
        let empty = json!({ "source_ref": "", "source_commit": "", "joined": true });
        assert_eq!(
            build_started(&empty, "/r"),
            "joining the build already running for /r from an empty repository, with nothing \
             committed yet; a first build can take several minutes"
        );
    }

    /// `tracon repo build` follows its build's row, stops with the node's
    /// reason when nothing was built, waits while the node is still working
    /// on the repository, and otherwise has nothing to wait for.
    #[test]
    fn a_build_that_never_started_is_reported_not_awaited() {
        let list = |item: serde_json::Value| json!({ "entries": [item] });
        let repo = "/src/app";

        let row = json!({ "repo_path": repo, "status": "building", "started_ms": 100 });
        assert_eq!(
            awaited_build(&list(json!({ "builds": [row.clone()] })), repo, 100),
            AwaitedBuild::Row(row)
        );

        let note = json!({ "repo_path": repo, "reason": "the token service answered 403", "using": null, "at_ms": 101 });
        // An earlier build's row is not this one's; the note is.
        let earlier = json!({ "repo_path": repo, "status": "ready", "started_ms": 50 });
        assert_eq!(
            awaited_build(
                &list(json!({ "builds": [earlier.clone()], "unbuilt": [note.clone()] })),
                repo,
                100
            ),
            AwaitedBuild::Unbuilt(note.clone())
        );
        // A note from before the request, or for another repository, is not.
        assert_eq!(
            awaited_build(
                &list(json!({ "builds": [earlier.clone()], "unbuilt": [note.clone()] })),
                repo,
                200
            ),
            AwaitedBuild::Nothing
        );
        assert_eq!(
            awaited_build(
                &list(json!({ "unbuilt": [note], "ensuring": ["/src/other"] })),
                "/src/other",
                100
            ),
            AwaitedBuild::Waiting
        );
        assert_eq!(
            awaited_build(
                &list(json!({ "builds": [earlier], "ensuring": [repo] })),
                repo,
                100
            ),
            AwaitedBuild::Waiting
        );
        // A note written since the request by another attempt doesn't end
        // the wait while this one is still working on the repository.
        let other = json!({ "repo_path": repo, "reason": "the registry could not be asked", "using": "ghcr.io/o/app:1", "at_ms": 150 });
        assert_eq!(
            awaited_build(
                &list(json!({ "unbuilt": [other], "ensuring": [repo] })),
                repo,
                100
            ),
            AwaitedBuild::Waiting
        );
        assert_eq!(
            awaited_build(&json!({ "entries": [] }), repo, 100),
            AwaitedBuild::Nothing
        );
    }

    /// A serving node's answer prints as a check run here does, and its
    /// first failed check is what the command exits on.
    #[test]
    fn a_nodes_boundary_answer_prints_as_a_local_check_does() {
        let checked = serde_json::json!({
            "state": "refused",
            "checks": {"checks": [
                {"id": "runtime", "ok": true, "detail": "kubernetes 1.35"},
                {"id": "egress", "ok": false, "detail": "reached example.com"},
                {"id": "network_isolated", "ok": false, "detail": "later"},
            ]},
            "providers": [{
                "provider": "kubernetes",
                "lines": [
                    {"capability": "tier", "value": "container"},
                    {"capability": "route", "value": null},
                ],
            }],
        });
        let (lines, failed) = super::boundary_lines(&checked);
        assert_eq!(
            lines,
            [
                "ok   runtime                kubernetes 1.35",
                "FAIL egress                 reached example.com",
                "FAIL network_isolated       later",
                "",
                "capabilities (kubernetes, reported, not enforced):",
                "  tier             container",
                "  route            not established",
            ]
        );
        assert_eq!(failed.as_deref(), Some("egress"));

        let passed = serde_json::json!({"checks": {"checks": [
            {"id": "runtime", "ok": true, "detail": "ok"},
        ]}, "providers": []});
        assert_eq!(super::boundary_lines(&passed).1, None);

        // A check waiting on the node's own fetch says so, and still fails.
        let waiting = serde_json::json!({"checks": {"checks": [
            {"id": "runtime", "ok": false, "pending": true, "detail": "fetching the 0.52.0 harness bundle"},
        ]}, "providers": []});
        let (lines, failed) = super::boundary_lines(&waiting);
        assert_eq!(
            lines,
            ["WAIT runtime                fetching the 0.52.0 harness bundle"]
        );
        assert_eq!(failed.as_deref(), Some("runtime"));
    }

    /// A port something listens on is in use; a free one, or port 0, is not.
    #[test]
    fn a_held_sandbox_port_is_seen_as_held() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = held.local_addr().unwrap();
        assert!(super::port_in_use(addr));
        drop(held);
        assert!(!super::port_in_use(addr));
        assert!(!super::port_in_use("127.0.0.1:0".parse().unwrap()));
    }

    const IDS: [&str; 3] = ["45abb223aa", "45abc001bb", "0f80343ccc"];

    #[test]
    fn a_prefix_names_exactly_one_item_or_says_why_not() {
        let resolve = |prefix| resolve_prefix(IDS.into_iter(), prefix, "work item", "work");
        assert_eq!(resolve("0f80").unwrap(), "0f80343ccc");
        assert_eq!(resolve("45abb223aa").unwrap(), "45abb223aa");
        assert!(resolve("45ab")
            .unwrap_err()
            .to_string()
            .contains("ambiguous on work"));
        assert!(resolve("ffff")
            .unwrap_err()
            .to_string()
            .contains("no work item starts with ffff on work"));
        // The generated name `tracon work` lists a task by resolves too.
        let by_name = |given: &str| super::resolve_task(IDS.into_iter(), given, "work");
        let named = tracon::names::task(IDS[2]);
        assert_eq!(by_name(&named).unwrap(), IDS[2]);
        assert_eq!(by_name("0f80").unwrap(), IDS[2]);
        assert!(by_name("task-nope-none-00")
            .unwrap_err()
            .to_string()
            .contains("no work item is named"));
    }

    #[test]
    fn memories_added_together_need_more_than_eight_characters() {
        let ids = [
            "01999b3a-7c1e-7a00-8000-aaaaaaaaaaaa",
            "01999b3a-7d02-7b00-8000-bbbbbbbbbbbb",
        ];
        let resolve = |prefix| resolve_prefix(ids.into_iter(), prefix, "memory", "personal");
        assert!(resolve("01999b3a")
            .unwrap_err()
            .to_string()
            .contains("ambiguous"));
        assert_eq!(resolve("01999b3a-7d").unwrap(), ids[1]);
        assert!(resolve("0200")
            .unwrap_err()
            .to_string()
            .contains("no memory starts with 0200 on personal"));
    }
}
