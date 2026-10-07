//! `tracon setup`: create the network, allowlist, and gateway the node owns.
//! Idempotent — running it again reconciles rather than failing.

use rust_embed::Embed;

use crate::config::{Config, HarnessListen};

use super::podman;
use crate::boundary::BoundaryError;

/// The gateway and harness definitions, carried inside the binary so a host
/// that only fetched the release can build the images it needs.
///
/// The exclusion is load-bearing. An operator doing an offline `Dockerfile.node`
/// build drops `opencode-ui-v<version>.tar.gz` into `containers/opencode-ui/`,
/// and that directory is embedded — so without this, a 10 MB release artefact
/// would be compiled into every binary built from that tree, silently, and the
/// node would carry a second unverified copy of the bundle it already verifies
/// by digest on disk.
#[derive(Embed)]
#[folder = "../containers"]
#[exclude = "opencode-ui/*.tar.gz"]
struct Containers;

/// The label a built image carries: a digest of the definitions it was built
/// from, so a node on a newer binary can tell its images are stale.
pub const DEFINITIONS_LABEL: &str = "io.tracon.definitions";

/// The directory the harness image is built from. Each harness has its own
/// definitions; another harness's under this one's tag would run the wrong CLI.
///
/// Only the two supported harnesses have definitions. An id outside them
/// never reaches here — `adapter_for` refuses it at startup, the retired
/// `omp` by name — so OpenCode's is the safe answer rather than a panic in
/// the setup path.
pub fn harness_dir(cfg: &Config) -> &'static str {
    harness_dir_for(&cfg.harness.id)
}

/// Each image the node runs, with the directory it is built from: the
/// gateway, and one harness image per supported harness. Both harnesses are
/// built because a session names its own harness and the node runs whichever
/// it names; the configured `[harness] id` is only the default.
pub fn images(cfg: &Config) -> Vec<(String, &'static str)> {
    let mut out = vec![(cfg.boundary.gateway_image.clone(), "gateway")];
    for id in crate::adapter::KNOWN {
        out.push((cfg.podman_harness_image(id), harness_dir_for(id)));
    }
    out
}

/// Each harness image and whether it is built from this build's
/// definitions, judged the way `check_runtime` judges it: an image without
/// the label cannot be judged, which is not the same as stale.
pub async fn harness_images(cfg: &Config) -> Vec<crate::boundary::HarnessImage> {
    let mut out = Vec::new();
    for id in crate::adapter::KNOWN {
        let image = cfg.podman_harness_image(id);
        let state = if podman(&["image", "exists", &image]).await.is_err() {
            "missing"
        } else {
            match image_digest(&image).await {
                Some(built) if built == definitions_digest(harness_dir_for(id)) => "current",
                Some(_) => "stale",
                None => "unknown",
            }
        };
        out.push(crate::boundary::HarnessImage {
            harness_id: id.to_string(),
            image,
            state,
        });
    }
    out
}

/// The definitions directory for a harness id.
pub fn harness_dir_for(harness_id: &str) -> &'static str {
    match harness_id {
        "claude" => "harness-claude",
        _ => "harness-opencode",
    }
}

/// A digest of one image's embedded definitions: every file under `dir`, in
/// path order.
pub fn definitions_digest(dir: &str) -> String {
    use sha2::{Digest, Sha256};
    let prefix = format!("{dir}/");
    let mut paths: Vec<_> = Containers::iter()
        .filter(|p| p.starts_with(&prefix))
        .collect();
    paths.sort();
    let mut hash = Sha256::new();
    for path in paths {
        if let Some(file) = Containers::get(&path) {
            hash.update(path.as_bytes());
            hash.update([0]);
            hash.update(file.data.as_ref());
            hash.update([0]);
        }
    }
    hex::encode(hash.finalize())
}

/// The definitions digest an image was built with, if it carries one. Images
/// built before the label existed, or by hand, carry none.
pub async fn image_digest(image: &str) -> Option<String> {
    let format = format!("{{{{ index .Labels \"{DEFINITIONS_LABEL}\" }}}}");
    podman(&["image", "inspect", "--format", &format, image])
        .await
        .ok()
        .and_then(|out| label_value(&out))
}

fn label_value(out: &str) -> Option<String> {
    let v = out.trim();
    (!v.is_empty() && v != "<no value>").then(|| v.to_string())
}

pub async fn setup(cfg: &Config, rebuild: bool) -> Result<(), BoundaryError> {
    super::ensure_machine(cfg).await?;
    ensure_images(cfg, rebuild).await?;
    ensure_network(cfg).await?;
    write_allowlist(cfg)?;
    ensure_gateway(cfg).await?;
    std::fs::create_dir_all(Config::harness_state_dir())?;
    Ok(())
}

/// Write the embedded container definitions out and build any image that is
/// missing or was built from definitions other than this binary's. The
/// harness image fetches the pinned harness release at build time, the one
/// network fetch a fresh node makes.
async fn ensure_images(cfg: &Config, rebuild: bool) -> Result<(), BoundaryError> {
    let root = Config::containers_dir();
    for path in Containers::iter() {
        let Some(file) = Containers::get(&path) else {
            continue;
        };
        let dest = root.join(path.as_ref());
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&dest, file.data.as_ref())?;
    }
    for (image, dir) in images(cfg) {
        let image = image.as_str();
        let digest = definitions_digest(dir);
        let current = podman(&["image", "exists", image]).await.is_ok()
            && image_digest(image).await.as_deref() == Some(digest.as_str());
        if current && !rebuild {
            tracing::info!(image, "image current");
            continue;
        }
        let ctx = root.join(dir);
        let label = format!("{DEFINITIONS_LABEL}={digest}");
        tracing::info!(image, context = %ctx.display(), "building image");
        podman(&[
            "build",
            "--label",
            &label,
            "-t",
            image,
            &ctx.to_string_lossy(),
        ])
        .await?;
    }
    Ok(())
}

async fn ensure_network(cfg: &Config) -> Result<(), BoundaryError> {
    if podman(&["network", "exists", &cfg.boundary.network])
        .await
        .is_ok()
    {
        tracing::info!(network = %cfg.boundary.network, "network exists");
        return Ok(());
    }
    // `--internal` removes the route out; `--disable-dns` stops the network's
    // resolver answering for every external name.
    podman(&[
        "network",
        "create",
        "--internal",
        "--disable-dns",
        "--subnet",
        &cfg.boundary.subnet,
        &cfg.boundary.network,
    ])
    .await?;
    tracing::info!(network = %cfg.boundary.network, "network created");
    Ok(())
}

fn write_allowlist(cfg: &Config) -> Result<(), BoundaryError> {
    let path = Config::allow_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // tinyproxy matches these as unanchored regexes, so `api.openai.com` would
    // also match `api.openai.com.evil.com`. Anchor every entry that is not
    // already anchored, so a hand-added host is an exact match rather than a
    // substring one. An operator who wants a pattern can still write `.*`.
    let body = cfg
        .gateway
        .allow_hosts
        .iter()
        .map(|h| anchor(h))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, format!("{body}\n"))?;
    tracing::info!(path = %path.display(), hosts = cfg.gateway.allow_hosts.len(), "allowlist written");
    Ok(())
}

/// Anchor one literal hostname as an exact-match tinyproxy filter entry.
/// Unlike [`anchor`] (for the operator-authored, regex-capable
/// `[gateway] allow_hosts`), every regex metacharacter in the host itself is
/// escaped: a host built from a parsed URL — a provider upstream's when `POST /api/providers` widens the allowlist for it — never
/// an operator writing a pattern, so a literal dot must never accidentally
/// match any character.
pub(crate) fn anchor_literal_host(host: &str) -> String {
    let mut escaped = String::with_capacity(host.len() + 2);
    escaped.push('^');
    for ch in host.chars() {
        if ".^$*+?()[]{}|\\".contains(ch) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped.push('$');
    escaped
}

/// Anchor a tinyproxy allowlist entry at both ends so it matches a whole host,
/// not a substring. Already-anchored entries are left as they are.
pub fn anchor(host: &str) -> String {
    let mut s = host.to_string();
    if !s.starts_with('^') {
        s.insert(0, '^');
    }
    if !s.ends_with('$') {
        s.push('$');
    }
    s
}

/// The gateway's own user service, on a Linux host whose systemd user manager
/// is running. Its own unit, so its own cgroup: restarting or stopping the
/// node's service, which ends everything in the node's cgroup, leaves the
/// boundary's gateway running, and systemd starts a gateway that stopped
/// rather than the operator rerunning setup.
pub const GATEWAY_UNIT: &str = "tracon-gateway.service";

/// The `podman run` arguments that make the gateway, without the verb and
/// without how it is supervised.
fn gateway_args(cfg: &Config, selinux: bool) -> Result<Vec<String>, BoundaryError> {
    let allow = Config::allow_file();
    let net_int = format!("{}:ip={}", cfg.boundary.network, cfg.boundary.gateway_ip);
    // Two forwards. On a Podman machine the node is outside the VM and gvproxy
    // reaches the host's loopback, so TCP via `host.containers.internal` works.
    // On a Linux host that name is a pasta interface address, not loopback, so
    // the node listens on a Unix socket and the gateway mounts its directory.
    //
    // Under SELinux a plain bind mount is unreadable from the container (the
    // gateway died on "allow.txt missing" on an SELinux host); `:z` relabels the node's
    // own files, which is fine for state tracon owns.
    let label = if selinux { ",z" } else { "" };
    let mount = format!("{}:/etc/tinyproxy/allow.txt:ro{label}", allow.display());
    // The per-client egress proxy is the node's, reached through a second
    // forward. It sits beside the harness listener — the same socket
    // directory, or the same host — so it needs no mount of its own.
    let egress = match cfg.egress_listen() {
        HarnessListen::Tcp(addr) => format!("TCP:host.containers.internal:{}", addr.port()),
        HarnessListen::Unix(path) => format!(
            "UNIX-CONNECT:/run/tracon/{}",
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "egress.sock".into())
        ),
    };
    let (upstream, socket_mount) = match &cfg.gateway.harness_listen {
        HarnessListen::Tcp(addr) => (
            format!("TCP:host.containers.internal:{}", addr.port()),
            None,
        ),
        HarnessListen::Unix(path) => {
            let dir = path
                .parent()
                .ok_or_else(|| BoundaryError::Podman("harness socket path has no parent".into()))?;
            std::fs::create_dir_all(dir)?;
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "harness.sock".into());
            let label = if selinux { ":z" } else { "" };
            (
                format!("UNIX-CONNECT:/run/tracon/{name}"),
                Some(format!("{}:/run/tracon{label}", dir.display())),
            )
        }
    };
    let mut args: Vec<String> = vec![
        "--name".into(),
        cfg.boundary.gateway_container.clone(),
        // The default network gives the gateway its own egress; the internal
        // one is how the harness reaches it.
        "--network".into(),
        "podman".into(),
        "--network".into(),
        net_int,
        "-v".into(),
        mount,
    ];
    if let Some(m) = socket_mount {
        args.push("-v".into());
        args.push(m);
        // SELinux forbids a confined container process from connecting to a
        // socket whose listener is unconfined (`connectto`), whatever the file
        // is labelled. The gateway is the trusted, node-owned piece — it exists
        // so the harness never touches the socket — so it runs unconfined; the
        // harness keeps its label.
        if selinux {
            args.push("--security-opt".into());
            args.push("label=disable".into());
        }
    }
    for env in [
        format!("TRACON_UPSTREAM={upstream}"),
        format!("TRACON_LISTEN_IP={}", cfg.boundary.gateway_ip),
        format!("TRACON_EGRESS_UPSTREAM={egress}"),
        format!("TRACON_EGRESS_PORT={}", cfg.gateway.qa_proxy_port),
    ] {
        args.push("-e".into());
        args.push(env);
    }
    args.push(cfg.boundary.gateway_image.clone());
    Ok(args)
}

/// Make the gateway, under its own user service where there is a user manager
/// to run one, and as a plain container where there is not (a Podman
/// machine, whose containers live in its VM and outside the node's cgroup
/// anyway). Either way the result replaces whatever gateway was there.
async fn ensure_gateway(cfg: &Config) -> Result<(), BoundaryError> {
    let args = gateway_args(cfg, super::selinux_enabled().await)?;
    if let Some(unit) = gateway_unit_path().filter(|_| !cfg!(target_os = "macos")) {
        if user_manager_running().await {
            let podman_bin = super::resolve_podman_env(cfg);
            let text = gateway_unit_text(&podman_bin, &args, &service_path());
            write_unit(&unit, &text)?;
            systemctl(&["daemon-reload"]).await?;
            systemctl(&["enable", GATEWAY_UNIT]).await?;
            // A restart, not a start: setup is also how a changed gateway
            // definition is applied.
            systemctl(&["restart", GATEWAY_UNIT]).await?;
            tracing::info!(unit = GATEWAY_UNIT, container = %cfg.boundary.gateway_container, "gateway started under its own user service");
            return Ok(());
        }
    }
    let _ = podman(&["rm", "-f", "-i", &cfg.boundary.gateway_container]).await;
    let mut run: Vec<&str> = vec!["run", "-d"];
    run.extend(args.iter().map(String::as_str));
    podman(&run).await?;
    tracing::info!(container = %cfg.boundary.gateway_container, "gateway started");
    Ok(())
}

/// What `recover_gateway` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// The gateway was running; nothing was touched.
    Running,
    /// It was stopped and was started again: under its own user service, put
    /// there now if setup had not already, or as the container it was.
    Started,
    /// There is no gateway to start. Setup makes one; recovery never does,
    /// because it has no image or network it can be sure of.
    Missing,
}

/// Bring back a gateway that stopped, without rerunning setup: at startup,
/// after a node-service restart took an older gateway down with it, or from a
/// re-check in the interface. A running gateway is never touched, so this is
/// safe while sessions are using it. The boundary checks that follow still
/// decide whether the node runs harnesses; this only gives them a gateway to
/// find.
pub async fn recover_gateway(cfg: &Config) -> Result<Recovery, BoundaryError> {
    let name = &cfg.boundary.gateway_container;
    let state = podman(&["inspect", name, "--format", "{{.State.Running}}"]).await;
    if matches!(&state, Ok(running) if running.trim() == "true") {
        return Ok(Recovery::Running);
    }
    let image_present = podman(&["image", "exists", &cfg.boundary.gateway_image])
        .await
        .is_ok();
    let network_present = podman(&["network", "exists", &cfg.boundary.network])
        .await
        .is_ok();
    if let Some(unit) = gateway_unit_path().filter(|_| !cfg!(target_os = "macos")) {
        if image_present && network_present && user_manager_running().await {
            // Written again whatever is there: an older setup left none (its
            // gateway was a container in the node's own cgroup, which is how
            // it came to be stopped), and a newer binary may describe it
            // differently. Both converge on the same unit.
            let args = gateway_args(cfg, super::selinux_enabled().await)?;
            let podman_bin = super::resolve_podman_env(cfg);
            let text = gateway_unit_text(&podman_bin, &args, &service_path());
            write_unit(&unit, &text)?;
            systemctl(&["daemon-reload"]).await?;
            systemctl(&["enable", GATEWAY_UNIT]).await?;
            systemctl(&["start", GATEWAY_UNIT]).await?;
            return Ok(Recovery::Started);
        }
    }
    if state.is_ok() {
        // A container exists and is stopped: start it as it was made.
        podman(&["start", name]).await?;
        return Ok(Recovery::Started);
    }
    Ok(Recovery::Missing)
}

/// Where the gateway's unit goes: beside the node's own.
fn gateway_unit_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|home| {
        std::path::PathBuf::from(home)
            .join(".config/systemd/user")
            .join(GATEWAY_UNIT)
    })
}

/// Whether there is a systemd user manager to hand the gateway to. A host
/// without one (a container, a session with no user bus) keeps the plain
/// container.
async fn user_manager_running() -> bool {
    tokio::process::Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

async fn systemctl(args: &[&str]) -> Result<(), BoundaryError> {
    let out = tokio::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .await?;
    if out.status.success() {
        Ok(())
    } else {
        Err(BoundaryError::Other(format!(
            "systemctl --user {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

fn write_unit(path: &std::path::Path, text: &str) -> Result<(), BoundaryError> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(text) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)?;
    Ok(())
}

/// The PATH the unit runs podman with: the node's own, which found podman and
/// its helpers (conmon, netavark, pasta) already.
fn service_path() -> String {
    std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into())
}

/// The gateway's unit, as Podman's own Quadlet generator would write it for a
/// container: `--cgroups=split` keeps conmon and the container in this unit's
/// cgroup rather than the caller's, `--sdnotify=conmon` makes it ready once the
/// container runs, and the cidfile is how stop finds it. `--replace` takes
/// over a gateway an older setup left as a bare container.
pub fn gateway_unit_text(podman_bin: &str, args: &[String], path: &str) -> String {
    let podman_bin = systemd_quote(podman_bin);
    let run = args
        .iter()
        .map(|a| systemd_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "# Written by `tracon setup` (and by the node, when it finds the gateway stopped).\n\
         # The gateway runs as its own user service so that restarting or stopping the\n\
         # node's service, which ends everything in that service's cgroup, leaves the\n\
         # boundary's gateway running, and so a gateway that stops is started again.\n\
         [Unit]\n\
         Description=tracon gateway\n\
         Documentation=https://github.com/cosmicspork/tracon\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=notify\n\
         NotifyAccess=all\n\
         Delegate=yes\n\
         KillMode=mixed\n\
         Environment={path}\n\
         ExecStart={podman_bin} run --cidfile=%t/%N.cid --replace --rm --cgroups=split --sdnotify=conmon -d {run}\n\
         ExecStop={podman_bin} rm -v -f -i --cidfile=%t/%N.cid\n\
         ExecStopPost=-{podman_bin} rm -v -f -i --cidfile=%t/%N.cid\n\
         Restart=always\n\
         RestartSec=5\n\
         TimeoutStopSec=30\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        path = env_quote(&format!("PATH={path}")),
    )
}

/// An `Environment=` assignment, quoted: systemd expands specifiers there but
/// not variables, so only `%` is doubled.
fn env_quote(assignment: &str) -> String {
    let escaped = assignment
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

/// One word of a unit's command line, quoted so systemd reads it back as
/// exactly that word: double quotes, with backslash and quote escaped, and
/// `%` and `$` doubled so neither specifier nor variable expansion touches it.
fn systemd_quote(word: &str) -> String {
    let mut out = String::with_capacity(word.len() + 2);
    out.push('"');
    for c in word.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gateway_unit_runs_it_in_its_own_cgroup_and_restarts_it() {
        let args: Vec<String> = ["--name", "tracon-gw", "-e", "A=b c", "img:1"]
            .map(String::from)
            .to_vec();
        let text = gateway_unit_text("/usr/bin/podman", &args, "/usr/bin:/bin");
        assert!(text.contains("--cgroups=split"), "{text}");
        assert!(text.contains("Restart=always"), "{text}");
        assert!(text.contains("Type=notify"), "{text}");
        assert!(text.contains("WantedBy=default.target"), "{text}");
        assert!(
            text.contains("Environment=\"PATH=/usr/bin:/bin\""),
            "{text}"
        );
        assert!(
            text.contains(
                "ExecStart=\"/usr/bin/podman\" run --cidfile=%t/%N.cid --replace --rm \
                 --cgroups=split --sdnotify=conmon -d \"--name\" \"tracon-gw\" \"-e\" \"A=b c\" \"img:1\""
            ),
            "{text}"
        );
        assert!(text.contains("ExecStop=\"/usr/bin/podman\" rm"), "{text}");
    }

    #[test]
    fn a_unit_word_reads_back_as_exactly_that_word() {
        assert_eq!(systemd_quote("plain"), "\"plain\"");
        assert_eq!(systemd_quote("a b"), "\"a b\"");
        assert_eq!(systemd_quote("50%"), "\"50%%\"");
        assert_eq!(systemd_quote("$HOME"), "\"$$HOME\"");
        assert_eq!(systemd_quote("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(systemd_quote("a\\b"), "\"a\\\\b\"");
        assert_eq!(env_quote("PATH=/a%b:$c"), "\"PATH=/a%%b:$c\"");
    }

    #[test]
    fn each_image_has_its_own_definitions_digest() {
        let gateway = definitions_digest("gateway");
        assert_eq!(gateway, definitions_digest("gateway"));
        assert_eq!(gateway.len(), 64);
        assert_ne!(gateway, definitions_digest("harness-opencode"));
        assert_ne!(
            definitions_digest("harness-opencode"),
            definitions_digest("harness-claude")
        );
        assert_ne!(gateway, definitions_digest("nonexistent"));
    }

    /// Each harness is built from its own definitions, and there are exactly
    /// two of them. The retired harness's `containers/harness` is gone; a
    /// `harness_dir` that still answered with it would name an empty
    /// directory and build an image with nothing in it.
    #[test]
    fn each_harness_is_built_from_its_own_definitions() {
        let mut cfg = Config::default();
        assert_eq!(harness_dir(&cfg), "harness-opencode");
        cfg.harness.id = "claude".into();
        assert_eq!(harness_dir(&cfg), "harness-claude");
        assert_eq!(definitions_digest("harness"), definitions_digest("gone"));
    }

    /// The recipes are carried; the artefact never is.
    ///
    /// An offline `Dockerfile.node` build wants the release tarball in
    /// `containers/opencode-ui/`, and that directory is what this embeds. A
    /// build from such a tree must not quietly gain 10 MB and a second,
    /// unverified copy of a bundle the node already pins by digest on disk.
    #[test]
    fn the_ui_bundle_recipe_is_embedded_and_the_artefact_is_not() {
        let paths: Vec<String> = Containers::iter().map(|p| p.to_string()).collect();
        for wanted in [
            "opencode-ui/build.sh",
            "opencode-ui/DIGEST",
            "opencode-ui/PINNED",
        ] {
            assert!(paths.iter().any(|p| p == wanted), "{wanted} is not carried");
        }
        let artefacts: Vec<&String> = paths.iter().filter(|p| p.ends_with(".tar.gz")).collect();
        assert!(
            artefacts.is_empty(),
            "a release artefact is compiled into the binary: {artefacts:?}"
        );
    }

    #[test]
    fn an_unlabelled_image_has_no_digest() {
        assert_eq!(label_value("<no value>\n"), None);
        assert_eq!(label_value(""), None);
        assert_eq!(label_value("abc123\n"), Some("abc123".into()));
    }

    #[test]
    fn allowlist_entries_are_anchored_exactly_once() {
        // A plain host becomes an exact match, so a suffix cannot slip past.
        assert_eq!(anchor("api.openai.com"), "^api.openai.com$");
        // An already-anchored regex is left alone.
        assert_eq!(anchor("^api\\.openai\\.com$"), "^api\\.openai\\.com$");
        // Half-anchored entries get only the missing end.
        assert_eq!(anchor("^api.openai.com"), "^api.openai.com$");
        assert_eq!(anchor("api.openai.com$"), "^api.openai.com$");
    }
}
