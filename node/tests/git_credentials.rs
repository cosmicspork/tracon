//! What authenticates the node's Git commands, proven against a server that
//! actually demands authentication.
//!
//! A negative assertion about credentials is worthless unless a credential is
//! genuinely required: against a local path or a public remote, git never
//! asks anyone, so "the ambient helper was not used" would pass on a build
//! that consulted it happily. So every operation here runs against a host
//! that answers `401` with a Basic challenge — which is the moment git walks
//! its list of credential helpers — with an ambient helper planted in both
//! places git would find one: the home directory it is given, and the
//! repository's own config. The helper writes a marker file when it runs. It
//! must never exist, and the only credential the host may ever be offered is
//! the brokered one.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use tracon::forge::{self, Forge};
use tracon::git_remote::{self, Credential};

/// Every `Authorization` header the host was offered, in order.
type Offered = Arc<Mutex<Vec<Option<String>>>>;

/// A Git host that demands Basic auth and never accepts it, so a client keeps
/// asking whoever it can for a credential. It answers every route the smart
/// HTTP transport starts with.
async fn challenging_host(offered: Offered) -> SocketAddr {
    let app = axum::Router::new().fallback(move |headers: HeaderMap| {
        let offered = offered.clone();
        async move {
            offered.lock().unwrap().push(
                headers
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string),
            );
            let mut response: Response = StatusCode::UNAUTHORIZED.into_response();
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                "Basic realm=\"tracon\"".parse().unwrap(),
            );
            response
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// A credential helper that answers, and leaves a marker file behind saying
/// it was consulted. The value is quoted because `;` starts a comment in a
/// Git config file, and a helper truncated at the first `;` would be a plant
/// that could never run — a test that always passes.
fn ambient_helper(marker: &Path) -> String {
    format!(
        "[credential]\n\thelper = \"!f(){{ : > {} ; echo username=ambient; echo password=ambient; }}; f\"\n",
        marker.display()
    )
}

/// The helper the host would love the node to use: the kind `gh auth
/// git-credential` or `osxkeychain` installs. Planted in the home directory
/// the node gives git, which is where a helper on a real host lives.
fn plant_ambient_helper(marker: &Path) {
    for home in ["forge-home", "publish-home"] {
        let dir = git_remote::home(home);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".gitconfig"), ambient_helper(marker)).unwrap();
    }
}

/// The same helper, in the one place `GIT_CONFIG_GLOBAL=/dev/null` does not
/// reach: the repository's own config. Only the credential-helper reset keeps
/// this one from answering, so the plant is checked to be live before the
/// assertion that it never ran is allowed to mean anything.
fn plant_repo_helper(repo: &Path, marker: &Path) {
    let path = repo.join(".git").join("config");
    let path = if path.exists() {
        path
    } else {
        repo.join("config")
    };
    let mut config = std::fs::read_to_string(&path).unwrap_or_default();
    config.push_str(&ambient_helper(marker));
    std::fs::write(&path, config).unwrap();

    // Plain git, in that repository, with nothing suppressed: the helper must
    // run, or this file proves nothing.
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("printf 'protocol=https\\nhost=example.com\\n\\n' | git credential fill")
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        marker.exists(),
        "the planted helper never ran, so it cannot prove anything: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::remove_file(marker).unwrap();
}

fn sh(dir: &Path, script: &str) {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repository with one commit and `origin` pointing at the challenging host.
fn repo_with_remote(dir: &Path, url: &str) -> PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    sh(
        &repo,
        &format!(
            "git init -q -b main . && git config user.email t@e && git config user.name t \
             && echo hello > a.txt && git add -A && git commit -qm base \
             && git remote add origin {url}"
        ),
    );
    repo
}

const BROKERED: &str = "brokered-token-not-the-operators";

fn brokered() -> Credential<'static> {
    Credential::Brokered {
        user: "x-access-token",
        token: BROKERED,
    }
}

/// What Basic auth with the brokered token looks like on the wire.
fn expected_header() -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD
            .encode(format!("x-access-token:{BROKERED}").as_bytes())
    )
}

fn assert_only_the_brokered_credential(offered: &Offered, marker: &Path, what: &str) {
    assert!(
        !marker.exists(),
        "{what}: an ambient credential helper was run"
    );
    let seen = offered.lock().unwrap();
    assert!(
        !seen.is_empty(),
        "{what}: the host was never asked, so this proves nothing"
    );
    let authenticated: Vec<&String> = seen.iter().flatten().collect();
    assert!(
        !authenticated.is_empty(),
        "{what}: git never offered a credential at all, so the brokered one did not reach it"
    );
    for header in authenticated {
        assert_eq!(
            header,
            &expected_header(),
            "{what}: a credential other than the brokered one reached the forge"
        );
    }
}

/// `clone`, against a host that demands a credential. The URL is `http` only
/// because a test cannot hold a trusted certificate; `forge::clone` pins
/// `https` and builds its command through the same builder, which is what
/// decides who may answer the challenge.
#[tokio::test]
async fn cloning_offers_the_brokered_credential_and_never_an_ambient_helper() {
    state::isolate();
    let dir = state::scratch("git-credentials-clone");
    let marker = dir.join("ambient-ran");
    plant_ambient_helper(&marker);
    let offered: Offered = Default::default();
    let addr = challenging_host(offered.clone()).await;

    let mut command = git_remote::git("git", forge::FORGE_HOME, &brokered());
    command
        .args(["clone", &format!("http://{addr}/owner/name.git")])
        .arg(dir.join("clone"));
    let out = command.output().await.unwrap();
    assert!(!out.status.success(), "the host refuses every credential");

    assert_only_the_brokered_credential(&offered, &marker, "clone");
}

/// `fetch`, through the node's own managed-clone refresh.
#[tokio::test]
async fn fetching_a_managed_clone_offers_the_brokered_credential_only() {
    state::isolate();
    let dir = state::scratch("git-credentials-fetch");
    let marker = dir.join("ambient-ran");
    plant_ambient_helper(&marker);
    let offered: Offered = Default::default();
    let addr = challenging_host(offered.clone()).await;

    // A managed clone as the node holds one: under the managed root, with the
    // remote it was cloned from.
    let root = forge::managed_root(&tracon::config::Config::state_dir());
    let repo = forge::clone_dest(&root, "127.0.0.1", "owner", "name").unwrap();
    std::fs::create_dir_all(repo.parent().unwrap()).unwrap();
    let made = repo_with_remote(&dir, &format!("http://{addr}/owner/name.git"));
    std::fs::rename(&made, &repo).unwrap();
    plant_repo_helper(&repo, &marker);

    let error = forge::fetch_managed(&repo, &brokered()).await.unwrap_err();
    assert!(error.contains("git fetch failed"), "{error}");
    assert_only_the_brokered_credential(&offered, &marker, "fetch");
}

/// `ls-remote` — what publication asks the forge before and after it pushes,
/// and the one network call whose answer decides whether a push is repeated.
#[tokio::test]
async fn asking_what_a_ref_holds_offers_the_brokered_credential_only() {
    state::isolate();
    let dir = state::scratch("git-credentials-ls-remote");
    let marker = dir.join("ambient-ran");
    plant_ambient_helper(&marker);
    let offered: Offered = Default::default();
    let addr = challenging_host(offered.clone()).await;
    let url = format!("http://{addr}/owner/name.git");
    let repo = repo_with_remote(&dir, &url);
    plant_repo_helper(&repo, &marker);

    let mut command = git_remote::git_in("git", &repo, "publish-home", &brokered());
    command.args(["ls-remote", "origin", "refs/heads/main"]);
    let out = command.output().await.unwrap();
    assert!(!out.status.success(), "the host refuses every credential");

    assert_only_the_brokered_credential(&offered, &marker, "ls-remote");
}

/// `push` — the side effect the whole boundary exists for.
#[tokio::test]
async fn pushing_offers_the_brokered_credential_only() {
    state::isolate();
    let dir = state::scratch("git-credentials-push");
    let marker = dir.join("ambient-ran");
    plant_ambient_helper(&marker);
    let offered: Offered = Default::default();
    let addr = challenging_host(offered.clone()).await;
    let url = format!("http://{addr}/owner/name.git");
    let repo = repo_with_remote(&dir, &url);
    plant_repo_helper(&repo, &marker);

    let mut command = git_remote::git_in("git", &repo, "publish-home", &brokered());
    command.args(["push", "origin", "HEAD:refs/heads/feat/x"]);
    let out = command.output().await.unwrap();
    assert!(!out.status.success(), "the host refuses every credential");

    assert_only_the_brokered_credential(&offered, &marker, "push");
}

/// Without a brokered credential there is no second path to fall back to: an
/// anonymous command fails at the challenge rather than quietly finding a
/// helper on the host.
#[tokio::test]
async fn an_anonymous_command_has_nothing_to_fall_back_on() {
    state::isolate();
    let dir = state::scratch("git-credentials-anonymous");
    let marker = dir.join("ambient-ran");
    plant_ambient_helper(&marker);
    let offered: Offered = Default::default();
    let addr = challenging_host(offered.clone()).await;
    let url = format!("http://{addr}/owner/name.git");
    let repo = repo_with_remote(&dir, &url);
    plant_repo_helper(&repo, &marker);

    let mut command = git_remote::git_in("git", &repo, "publish-home", &Credential::Anonymous);
    command.args(["ls-remote", "origin"]);
    let out = command.output().await.unwrap();
    assert!(!out.status.success());
    assert!(!marker.exists(), "an ambient credential helper was run");
    let seen = offered.lock().unwrap();
    assert!(!seen.is_empty(), "the host was never asked");
    assert!(
        seen.iter().all(Option::is_none),
        "a credential nobody brokered reached the forge: {seen:?}"
    );
}

/// The forge a credential names is still where the clone URL comes from:
/// nothing in this file's plumbing lets a test — or a host — move it to
/// plain HTTP in production.
#[tokio::test]
async fn a_managed_clone_url_is_always_https() {
    state::isolate();
    let dir = state::scratch("git-credentials-url");
    let dest = dir.join("nowhere");
    // A host that does not resolve: the assertion is on the message, which
    // carries the URL git was given.
    let error = forge::clone(&brokered(), "127.0.0.1:1", "owner", "name", &dest)
        .await
        .unwrap_err();
    assert!(
        error.contains("https://127.0.0.1:1/owner/name.git"),
        "{error}"
    );
    assert!(!dest.exists(), "a failed clone leaves nothing behind");
}

/// Forge identities stay what their own tooling expects, because the helper
/// answers with them.
#[test]
fn both_forges_have_a_username_their_tokens_are_accepted_under() {
    state::isolate();
    assert_eq!(Forge::Github.git_user(), "x-access-token");
    assert_eq!(Forge::Gitlab.git_user(), "oauth2");
}
