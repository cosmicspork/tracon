//! What preparing a repository would do, and what in it the node will not do,
//! said before a session is launched on it.
//!
//! Preparation runs a lockfile install with the package manager's scripts off,
//! then the operator's own `prepare` commands, in an image the operator chose.
//! A repository asks for more than that in places the node deliberately does
//! not read as instructions: a devcontainer's hooks, features, mounts and
//! privileges; a package's install scripts. Each is reported here with what it
//! asked for and where the same work belongs instead, so an incompatibility is
//! a sentence before launch rather than a failure (or, worse, a silent skip)
//! after it. Nothing here runs anything: the preview reads files.

use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::config::Config;
use crate::environment::{
    approved_image, non_empty, RepoEnvironment, LOCKFILES, UNSAFE_DEVCONTAINER_KEYS,
};

pub const DEVCONTAINER: &str = ".devcontainer/devcontainer.json";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Incompatibility {
    /// The file that asked, relative to the repository.
    pub source: String,
    /// What it asked for: a field or a script name.
    pub item: String,
    /// What the node does with it instead.
    pub reason: String,
    /// Where the same work belongs, when it has a home.
    pub instead: Option<String>,
    /// True when preparation would refuse the repository over it; false when it
    /// is passed over and preparation goes ahead without it.
    pub blocking: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PreparationPreview {
    pub repo: String,
    /// The image preparation and checks would run in, and whose it is.
    pub image: String,
    pub image_source: &'static str,
    /// A pinned image the devcontainer names, used only when the operator's
    /// entry names none.
    pub devcontainer_image: Option<String>,
    /// The lockfiles found, in the order preparation prefers them.
    pub lockfiles: Vec<String>,
    /// The install preparation would run, scripts off. None: no lockfile.
    pub install: Option<String>,
    /// The operator's own `prepare` commands, run before checks.
    pub prepare: Vec<String>,
    /// What preparation may reach, as the entry wrote it.
    pub egress: Vec<String>,
    pub incompatible: Vec<Incompatibility>,
    /// No incompatibility would stop preparation.
    pub ready: bool,
}

fn found(
    source: &str,
    item: &str,
    reason: &str,
    instead: Option<&str>,
    blocking: bool,
) -> Incompatibility {
    Incompatibility {
        source: source.into(),
        item: item.into(),
        reason: reason.into(),
        instead: instead.map(str::to_string),
        blocking,
    }
}

/// Why a devcontainer field is not honoured, and where its work belongs.
fn devcontainer_field(key: &str) -> (&'static str, Option<&'static str>) {
    match key {
        "initializeCommand" => (
            "would run on the host before any container exists; the node never runs repository code on the host",
            Some("a command in the [[repo]] entry's `prepare`"),
        ),
        "onCreateCommand" | "updateContentCommand" | "postCreateCommand" | "postStartCommand" => (
            "is a setup hook the repository controls; the node does not run it",
            Some("a command in the [[repo]] entry's `prepare`, which runs in the check image with only `egress` reachable"),
        ),
        "features" => (
            "installs software into the container at start; the node does not install features",
            Some("the image itself: a Dockerfile the [[repo]] entry names in `dockerfile`"),
        ),
        "privileged" | "capAdd" | "runArgs" => (
            "asks for container privileges the node does not grant",
            None,
        ),
        "mounts" | "workspaceMount" => (
            "asks for host paths in the container; the node mounts only the workspace and its cache",
            None,
        ),
        "containerEnv" | "remoteEnv" => (
            "sets environment the node does not pass into a session or a check",
            Some("the image's own ENV, or a `prepare` command that writes the configuration it needs"),
        ),
        _ => ("is not honoured", None),
    }
}

fn devcontainer(
    root: &Path,
    cfg: &Config,
    environment: &RepoEnvironment,
    incompatible: &mut Vec<Incompatibility>,
) -> Option<String> {
    let path = root.join(DEVCONTAINER);
    let source = std::fs::read_to_string(&path).ok()?;
    // When the operator's entry names the image, the devcontainer is not read
    // at all: what it asks for is passed over, and said so, but blocks nothing.
    let asked = !environment.names_image();
    let object = match serde_json::from_str::<Value>(&source) {
        Ok(Value::Object(object)) => object,
        Ok(_) | Err(_) => {
            incompatible.push(found(
                DEVCONTAINER,
                "devcontainer.json",
                "is not a JSON object the node can read (comments and trailing commas are not accepted)",
                None,
                asked,
            ));
            return None;
        }
    };
    for &key in UNSAFE_DEVCONTAINER_KEYS {
        if object.get(key).is_some_and(non_empty) {
            let (reason, instead) = devcontainer_field(key);
            incompatible.push(found(DEVCONTAINER, key, reason, instead, asked));
        }
    }
    if object.get("dockerComposeFile").is_some_and(non_empty) {
        incompatible.push(found(
            DEVCONTAINER,
            "dockerComposeFile",
            "describes services the node does not start from a compose file",
            None,
            false,
        ));
    }
    match object.get("image") {
        Some(Value::String(image)) if !image.trim().is_empty() => {
            if asked && approved_image(environment, cfg, Some(image)).is_err() {
                incompatible.push(found(
                    DEVCONTAINER,
                    "image",
                    &format!(
                        "{image} is neither pinned to a digest nor in the node's approved images"
                    ),
                    Some("the image by digest (`name@sha256:…`), or the [[repo]] entry's `image`"),
                    true,
                ));
            }
            Some(image.clone())
        }
        _ => {
            if object.contains_key("build") || object.contains_key("dockerFile") {
                incompatible.push(found(
                    DEVCONTAINER,
                    "build",
                    "builds its image; preparation does not build from a devcontainer",
                    Some("the [[repo]] entry's `dockerfile`, which the node builds from the default branch and pins"),
                    asked,
                ));
            }
            None
        }
    }
}

/// The install scripts a manifest declares that a scripts-off install skips.
fn scripts(
    root: &Path,
    file: &str,
    names: &[&str],
    key: &str,
    incompatible: &mut Vec<Incompatibility>,
) {
    let Ok(source) = std::fs::read_to_string(root.join(file)) else {
        return;
    };
    let Ok(value) = serde_json::from_str::<Value>(&source) else {
        return;
    };
    let Some(declared) = value.get(key).and_then(Value::as_object) else {
        return;
    };
    for &name in names {
        if declared.get(name).is_some_and(non_empty) {
            incompatible.push(found(
                file,
                &format!("{key}.{name}"),
                "runs during install; preparation installs with scripts off, so it does not run",
                Some("a command in the [[repo]] entry's `prepare`, if the project needs it"),
                false,
            ));
        }
    }
}

pub fn preview(cfg: &Config, repo: &Path, environment: &RepoEnvironment) -> PreparationPreview {
    let mut incompatible = Vec::new();
    let devcontainer_image = devcontainer(repo, cfg, environment, &mut incompatible)
        .filter(|_| !environment.names_image());
    let mut lockfiles = Vec::new();
    let mut install = None;
    for &(file, command) in LOCKFILES {
        if repo.join(file).is_file() {
            lockfiles.push(file.to_string());
            install.get_or_insert(command.to_string());
        }
    }
    if lockfiles
        .iter()
        .any(|f| f != "Cargo.lock" && f != "composer.lock")
    {
        scripts(
            repo,
            "package.json",
            &["preinstall", "install", "postinstall", "prepare"],
            "scripts",
            &mut incompatible,
        );
    }
    if lockfiles.iter().any(|f| f == "composer.lock") {
        scripts(
            repo,
            "composer.json",
            &["pre-install-cmd", "post-install-cmd", "post-autoload-dump"],
            "scripts",
            &mut incompatible,
        );
    }
    let ready = !incompatible.iter().any(|i| i.blocking);
    PreparationPreview {
        repo: repo.display().to_string(),
        image: devcontainer_image
            .clone()
            .unwrap_or_else(|| environment.image.clone()),
        image_source: if devcontainer_image.is_some() {
            "repository devcontainer"
        } else {
            environment.image_source
        },
        devcontainer_image,
        lockfiles,
        install,
        prepare: environment.prepare.clone(),
        egress: environment.egress.clone(),
        incompatible,
        ready,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    const PINNED: &str =
        "img@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn write(root: &Path, file: &str, body: &str) {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn items(p: &PreparationPreview) -> Vec<(&str, bool)> {
        p.incompatible
            .iter()
            .map(|i| (i.item.as_str(), i.blocking))
            .collect()
    }

    #[test]
    fn every_unhonoured_field_is_named_not_just_the_first() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            DEVCONTAINER,
            &serde_json::json!({
                "image": PINNED,
                "initializeCommand": "./host-setup.sh",
                "postCreateCommand": "npm run setup",
                "features": { "ghcr.io/devcontainers/features/node:1": {} },
                "runArgs": [],
                "mounts": ["source=/var/run/docker.sock,target=/var/run/docker.sock,type=bind"],
            })
            .to_string(),
        );
        write(dir.path(), "package-lock.json", "{}");
        write(
            dir.path(),
            "package.json",
            r#"{"scripts":{"postinstall":"node build.js","test":"vitest"}}"#,
        );
        let cfg = Config::default();
        let store = Store::open_in_memory().unwrap();
        let environment = crate::environment::environment_for(&cfg, &store, Some(dir.path()));
        let p = preview(&cfg, dir.path(), &environment);
        assert_eq!(
            items(&p),
            vec![
                ("mounts", true),
                ("initializeCommand", true),
                ("postCreateCommand", true),
                ("features", true),
                ("scripts.postinstall", false),
            ]
        );
        assert!(!p.ready);
        assert!(p.incompatible[1].reason.contains("host"));
        assert_eq!(p.install.as_deref(), Some("npm ci --ignore-scripts"));
        assert_eq!(p.image, PINNED);
        assert_eq!(p.image_source, "repository devcontainer");
    }

    #[test]
    fn an_operator_image_passes_over_the_devcontainer_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            DEVCONTAINER,
            r#"{"build":{"dockerfile":"Dockerfile"},"postCreateCommand":"make"}"#,
        );
        let cfg = Config {
            repo: vec![crate::config::Repo {
                path: dir.path().to_path_buf(),
                image: Some(PINNED.into()),
                prepare: vec!["make deps".into()],
                ..Default::default()
            }],
            ..Config::default()
        };
        let store = Store::open_in_memory().unwrap();
        let environment = crate::environment::environment_for(&cfg, &store, Some(dir.path()));
        let p = preview(&cfg, dir.path(), &environment);
        assert!(p.ready, "{:?}", p.incompatible);
        assert_eq!(
            items(&p),
            vec![("postCreateCommand", false), ("build", false)]
        );
        assert_eq!(p.image, PINNED);
        assert_eq!(p.prepare, vec!["make deps"]);
        assert!(p.install.is_none() && p.lockfiles.is_empty());
    }

    #[test]
    fn a_build_or_an_unpinned_image_says_where_it_belongs() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::default();
        let store = Store::open_in_memory().unwrap();
        let environment = crate::environment::environment_for(&cfg, &store, Some(dir.path()));

        write(
            dir.path(),
            DEVCONTAINER,
            r#"{"build":{"dockerfile":"Dockerfile"}}"#,
        );
        let p = preview(&cfg, dir.path(), &environment);
        assert_eq!(items(&p), vec![("build", true)]);
        assert!(p.incompatible[0]
            .instead
            .as_deref()
            .unwrap()
            .contains("dockerfile"));

        write(dir.path(), DEVCONTAINER, r#"{"image":"node:20"}"#);
        let p = preview(&cfg, dir.path(), &environment);
        assert_eq!(items(&p), vec![("image", true)]);

        write(dir.path(), DEVCONTAINER, "{ // a comment\n}");
        let p = preview(&cfg, dir.path(), &environment);
        assert_eq!(items(&p), vec![("devcontainer.json", true)]);
    }
}
