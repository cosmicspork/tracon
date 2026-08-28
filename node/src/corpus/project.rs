//! Bank identity: a project is named by its channel and its canonical remote,
//! never by a checkout path or git root — worktrees move between paths, nodes,
//! and containers, and memory must follow the project, not the directory.

use std::path::Path;

use sha2::{Digest, Sha256};

/// `sha256(channel ‖ 0x1f ‖ canonical remote)`, hex. A repository without a
/// remote falls back to its directory name, which is weaker and logged.
pub fn project_id(channel: &str, canonical: &str) -> String {
    let mut h = Sha256::new();
    h.update(channel.as_bytes());
    h.update([0x1f]);
    h.update(canonical.as_bytes());
    hex::encode(h.finalize())
}

/// One spelling for every way a remote is written:
/// `git@github.com:o/r.git`, `ssh://git@github.com/o/r`, `https://github.com/o/r.git`
/// all become `github.com/o/r`.
pub fn canonical_remote(url: &str) -> Option<String> {
    let u = url.trim();
    if u.is_empty() {
        return None;
    }
    let rest = if let Some((_, tail)) = u.split_once("://") {
        tail
    } else if let Some((userhost, path)) = u.split_once(':') {
        // scp-like: user@host:path
        let host = userhost.rsplit('@').next().unwrap_or(userhost);
        return Some(finish(host, path));
    } else {
        return None;
    };
    let rest = rest.rsplit('@').next().unwrap_or(rest);
    let (host, path) = rest.split_once('/')?;
    let host = host.split(':').next().unwrap_or(host);
    Some(finish(host, path))
}

fn finish(host: &str, path: &str) -> String {
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    format!("{}/{}", host.to_ascii_lowercase(), path)
}

/// Resolve a repository's identity on the node's side of the boundary.
/// Returns `(id, name, remote)`; `remote` is `None` when the fallback applied.
pub async fn identify(channel: &str, repo: &Path, git: &str) -> (String, String, Option<String>) {
    let name = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let out = tokio::process::Command::new(git)
        .arg("-C")
        .arg(repo)
        .args(["config", "--get", "remote.origin.url"])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .await;
    let remote = out
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| canonical_remote(&String::from_utf8_lossy(&o.stdout)));
    match remote {
        Some(r) => (project_id(channel, &r), name, Some(r)),
        None => {
            tracing::warn!(repo = %repo.display(), "no remote; project identity falls back to the directory name");
            (project_id(channel, &format!("local/{name}")), name, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_canonicalise_to_one_spelling() {
        for u in [
            "git@github.com:cosmicspork/tracon.git",
            "ssh://git@github.com/cosmicspork/tracon",
            "https://github.com/cosmicspork/tracon.git",
            "https://user@GitHub.com/cosmicspork/tracon/",
        ] {
            assert_eq!(
                canonical_remote(u).as_deref(),
                Some("github.com/cosmicspork/tracon"),
                "{u}"
            );
        }
        assert_eq!(canonical_remote(""), None);
        assert_ne!(
            project_id("work", "github.com/o/r"),
            project_id("personal", "github.com/o/r")
        );
    }
}
