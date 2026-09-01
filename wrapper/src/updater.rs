//! Unsigned desktop updates verified against GitHub's release digest: a
//! Linux AppImage, or a macOS `.app` swapped whole.
//!
//! The updater deliberately owns its selected asset. The webview can ask it to
//! check or install, but never chooses a URL, path, or checksum.

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::{self, Command},
    thread,
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use parking_lot::Mutex;
use semver::Version;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};
use tokio::{fs, io::AsyncWriteExt};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::{tray, State};

const RELEASE_URL: &str = "https://api.github.com/repos/cosmicspork/tracon/releases/latest";
const RELEASE_ACCEPT: &str = "application/vnd.github+json";
const RELEASE_API_VERSION: &str = "2022-11-28";
#[cfg(target_os = "macos")]
const UNSUPPORTED_MESSAGE: &str =
    "Self-update needs tracon.app in a folder you can write, such as Applications.";
#[cfg(not(target_os = "macos"))]
const UNSUPPORTED_MESSAGE: &str =
    "This install is managed by its package; self-update is available in the AppImage.";
const DIGEST_MISMATCH_MESSAGE: &str =
    "The download did not match GitHub; the current version was not changed.";
const REPLACE_MESSAGE: &str =
    "Cannot replace this install; its containing directory must be writable.";
const STAGED_MESSAGE: &str =
    "The downloaded app was not what the release describes; nothing was changed.";

/// The bundle being replaced, renamed aside so the running app keeps the code
/// it is executing. Swept at the next launch.
const OLD_PREFIX: &str = ".tracon-old-";
/// Where an update is assembled: beside the bundle, because a rename cannot
/// cross volumes.
const STAGE_PREFIX: &str = ".tracon-update-";

/// What this install can replace, and so which release asset replaces it.
/// Detection is the only platform-specific part of an update; selection,
/// verification, and the state machine are shared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// The AppImage file itself. Replacing it is one rename.
    #[cfg_attr(
        not(all(target_os = "linux", target_arch = "x86_64")),
        allow(dead_code)
    )]
    AppImage(PathBuf),
    /// The `.app` root. A bundle is a directory, and the ad-hoc signature an
    /// arm64 binary needs to run at all covers its contents, so it is swapped
    /// whole and never edited in place.
    #[cfg_attr(
        not(all(target_os = "macos", target_arch = "aarch64")),
        allow(dead_code)
    )]
    MacApp(PathBuf),
}

impl Target {
    pub fn detect(env: &tauri::Env) -> Option<Self> {
        detect_target(env)
    }

    fn asset_name(&self, version: &Version) -> String {
        match self {
            Target::AppImage(_) => format!("tracon_{version}_amd64.AppImage"),
            Target::MacApp(_) => format!("tracon_{version}_aarch64.app.tar.gz"),
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn detect_target(env: &tauri::Env) -> Option<Target> {
    env.appimage
        .as_ref()
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .map(Target::AppImage)
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn detect_target(_env: &tauri::Env) -> Option<Target> {
    let root = app_root_from(&std::env::current_exe().ok()?)?;
    // One check for three unsupported installs: a read-only dmg mount, a
    // translocated first run, and an /Applications this operator cannot write.
    writable(root.parent()?).then_some(Target::MacApp(root))
}

#[cfg(not(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
)))]
fn detect_target(_env: &tauri::Env) -> Option<Target> {
    None
}

/// The `.app` an executable is running out of, if it is running out of one.
/// A bundle renamed aside by an update has no `.app` ancestor, which is what
/// keeps a stale process from sweeping the tree it is executing.
fn app_root_from(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .find(|path| path.extension() == Some(OsStr::new("app")))
        .map(Path::to_path_buf)
}

#[cfg(target_os = "macos")]
fn writable(directory: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(directory.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
}

/// Remove what an earlier update left behind. A swap cannot delete the bundle
/// it replaces — this process is executing out of it — so the next launch does.
pub fn sweep_stale(target: &Target) {
    let Target::MacApp(root) = target else {
        return;
    };
    let Some(parent) = root.parent() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(OLD_PREFIX) || name.starts_with(STAGE_PREFIX) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Tauri restarts before the single-instance plugin releases its primary
/// socket, so a direct `request_restart` starts a second instance that exits
/// immediately. This minimal helper waits outside Tauri, then starts the
/// replaced install after the parent is gone.
const RESTART_HELPER_ARGUMENT: &str = "--tracon-restart-after";
/// A parent that never exits must not leave a helper spinning. Starting the
/// replacement early is harmless: the single-instance plugin folds a second
/// launch into the first.
const RESTART_WAIT: Duration = Duration::from_secs(30);

/// What has to happen after this process is gone: which binary waits, and
/// what it starts.
struct Restart {
    helper: PathBuf,
    relaunch: PathBuf,
}

struct RestartArgs {
    parent: u32,
    relaunch: OsString,
}

#[cfg(unix)]
fn detach_restart_helper() -> std::io::Result<()> {
    // A launcher can reap the app's whole process tree as soon as its main
    // process exits. Double-fork so the waiter survives long enough to start
    // the replacement after the single-instance socket has been released.
    unsafe {
        match libc::fork() {
            -1 => Err(std::io::Error::last_os_error()),
            0 => {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            }
            _ => process::exit(0),
        }
    }
}

#[cfg(not(unix))]
fn detach_restart_helper() -> std::io::Result<()> {
    Ok(())
}

/// `None` when this is an ordinary launch, so the caller can carry on.
fn parse_restart_args(
    mut arguments: impl Iterator<Item = OsString>,
) -> Option<Result<RestartArgs, String>> {
    if arguments.next().as_deref() != Some(OsStr::new(RESTART_HELPER_ARGUMENT)) {
        return None;
    }
    let Some(parent) = arguments
        .next()
        .and_then(|parent| parent.into_string().ok())
        .and_then(|parent| parent.parse::<u32>().ok())
    else {
        return Some(Err("invalid update restart parent".to_string()));
    };
    let Some(relaunch) = arguments.next() else {
        return Some(Err("missing target for the update restart".to_string()));
    };
    Some(Ok(RestartArgs { parent, relaunch }))
}

#[cfg(target_os = "linux")]
fn parent_alive(parent: u32) -> bool {
    Path::new(&format!("/proc/{parent}")).exists()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn parent_alive(parent: u32) -> bool {
    if unsafe { libc::kill(parent as libc::pid_t, 0) } == 0 {
        return true;
    }
    // ESRCH is the only answer that means gone; EPERM means alive and not ours.
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn parent_alive(_parent: u32) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn relaunch(target: &OsStr) {
    let mut restart = Command::new(target);
    // The AppImage runtime puts these extraction paths in its child
    // environment. Carrying them into a second AppImage makes its runtime
    // treat the already-deleted extraction directory as current.
    restart
        .env_remove("APPIMAGE")
        .env_remove("APPDIR")
        .env_remove("OWD");
    if let Err(error) = restart.spawn() {
        eprintln!("tracon: could not restart the updated AppImage: {error}");
    }
}

#[cfg(target_os = "macos")]
fn relaunch(target: &OsStr) {
    // `open` hands the bundle to LaunchServices, which is what gives the
    // replacement a GUI session; a detached helper exec'ing the inner binary
    // does not. Nothing quarantined the bundle — this app downloaded it — so
    // Gatekeeper has nothing to ask about.
    if let Err(error) = Command::new("/usr/bin/open").arg("-n").arg(target).spawn() {
        eprintln!("tracon: could not restart the updated app: {error}");
    }
    // This helper is a copy outside the bundle, so nothing else will remove it.
    if let Ok(helper) = std::env::current_exe() {
        let _ = std::fs::remove_file(helper);
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn relaunch(target: &OsStr) {
    if let Err(error) = Command::new(target).spawn() {
        eprintln!("tracon: could not restart the updated app: {error}");
    }
}

pub fn run_restart_helper() -> bool {
    let Some(parsed) = parse_restart_args(std::env::args_os().skip(1)) else {
        return false;
    };
    let arguments = match parsed {
        Ok(arguments) => arguments,
        Err(why) => {
            eprintln!("tracon: {why}");
            return true;
        }
    };
    if let Err(error) = detach_restart_helper() {
        eprintln!("tracon: could not detach update restart helper: {error}");
        return true;
    }
    let deadline = Instant::now() + RESTART_WAIT;
    while parent_alive(arguments.parent) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    relaunch(&arguments.relaunch);
    true
}

/// The helper cannot be the running executable on macOS: it lives inside the
/// bundle that is about to be renamed and later deleted. `fs::copy` keeps the
/// mode and the embedded ad-hoc signature the kernel insists on.
#[cfg(target_os = "macos")]
fn stage_restart_helper() -> Result<PathBuf, String> {
    let running = std::env::current_exe()
        .map_err(|error| format!("Could not prepare the restart: {error}"))?;
    let helper = std::env::temp_dir().join(format!("tracon-restart-{}", process::id()));
    let _ = std::fs::remove_file(&helper);
    std::fs::copy(&running, &helper)
        .map_err(|error| format!("Could not prepare the restart: {error}"))?;
    Ok(helper)
}

fn schedule_restart(helper: &Path, relaunch: &Path) -> Result<(), String> {
    Command::new(helper)
        .arg(RESTART_HELPER_ARGUMENT)
        .arg(process::id().to_string())
        .arg(relaunch)
        .spawn()
        .map_err(|error| format!("Could not restart the updated app: {error}"))?;
    Ok(())
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct UpdateStatus {
    pub state: &'static str,
    pub current_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug)]
struct Asset {
    version: Version,
    url: String,
    size: u64,
    digest: [u8; 32],
}

#[derive(Default)]
struct Release {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(serde::Deserialize)]
struct ReleaseWire {
    tag_name: String,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(serde::Deserialize)]
struct ReleaseAsset {
    name: String,
    size: u64,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

struct Inner {
    status: UpdateStatus,
    asset: Option<Asset>,
    announced: bool,
}

pub struct Updater {
    current_version: Version,
    target: Option<Target>,
    client: reqwest::Client,
    inner: Mutex<Inner>,
}

impl Updater {
    pub fn new(current_version: &str, target: Option<Target>) -> Self {
        let current_version =
            Version::parse(current_version).unwrap_or_else(|_| Version::new(0, 0, 0));
        let status = UpdateStatus {
            state: if target.is_some() {
                "idle"
            } else {
                "unsupported"
            },
            current_version: current_version.to_string(),
            available_version: None,
            message: target.is_none().then(|| UNSUPPORTED_MESSAGE.to_string()),
        };
        Self {
            current_version,
            target,
            client: reqwest::Client::new(),
            inner: Mutex::new(Inner {
                status,
                asset: None,
                announced: false,
            }),
        }
    }

    pub fn status(&self) -> UpdateStatus {
        self.inner.lock().status.clone()
    }

    /// Check once. A second check while either operation is active observes the
    /// current status rather than starting a second request or install.
    pub async fn check(&self, app: &AppHandle) -> UpdateStatus {
        let checking = self.status_with("checking", None, None);
        {
            let mut inner = self.inner.lock();
            if matches!(inner.status.state, "checking" | "downloading") {
                return inner.status.clone();
            }
            if self.target.is_none() {
                return inner.status.clone();
            }
            inner.status = checking;
            inner.asset = None;
        }
        self.refresh(app);

        let next = match self.fetch_release().await {
            Ok(asset) => match asset {
                Some(asset) => {
                    let version = asset.version.to_string();
                    (
                        self.status_with("available", Some(version), None),
                        Some(asset),
                    )
                }
                None => (self.status_with("current", None, None), None),
            },
            Err(message) => (self.status_with("failed", None, Some(message)), None),
        };

        let notify = {
            let mut inner = self.inner.lock();
            let notify = next.0.state == "available" && !inner.announced;
            if notify {
                inner.announced = true;
            }
            inner.status = next.0.clone();
            inner.asset = next.1;
            notify
        };
        self.refresh(app);
        if notify {
            use tauri_plugin_notification::NotificationExt;
            let version = self.status().available_version.unwrap_or_default();
            let _ = app
                .notification()
                .builder()
                .title(format!("tracon {version} is available"))
                .body("Install and restart from Settings or the tray.")
                .show();
        }
        self.status()
    }

    pub async fn install(&self, app: &AppHandle) -> UpdateStatus {
        let (asset, target) = {
            let mut inner = self.inner.lock();
            if inner.status.state == "downloading" || inner.status.state == "checking" {
                return inner.status.clone();
            }
            let Some(asset) = inner.asset.clone() else {
                return inner.status.clone();
            };
            let Some(target) = self.target.clone() else {
                return inner.status.clone();
            };
            inner.status = self.status_with("downloading", Some(asset.version.to_string()), None);
            (asset, target)
        };
        self.refresh(app);

        let restart = match self.replace(&asset, &target).await {
            Ok(restart) => restart,
            Err(message) => return self.failed(app, message),
        };
        if let Err(message) = schedule_restart(&restart.helper, &restart.relaunch) {
            return self.failed(app, message);
        }
        app.exit(0);
        self.status()
    }

    /// Put the new version where the old one is, and say what starts it once
    /// this process is gone: one file for an AppImage, a whole directory
    /// swapped by rename for a `.app`.
    async fn replace(&self, asset: &Asset, target: &Target) -> Result<Restart, String> {
        match target {
            Target::AppImage(path) => {
                download_and_replace(&self.client, asset, path).await?;
                let helper = std::env::current_exe()
                    .map_err(|error| format!("Could not prepare the restart: {error}"))?;
                Ok(Restart {
                    helper,
                    relaunch: path.clone(),
                })
            }
            Target::MacApp(root) => {
                #[cfg(target_os = "macos")]
                {
                    // Copied out before the swap: the running executable is
                    // inside the bundle that is about to be renamed.
                    let helper = stage_restart_helper()?;
                    if let Err(message) = download_and_swap(&self.client, asset, root).await {
                        let _ = std::fs::remove_file(&helper);
                        return Err(message);
                    }
                    Ok(Restart {
                        helper,
                        relaunch: root.clone(),
                    })
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = (asset, root);
                    Err(REPLACE_MESSAGE.to_string())
                }
            }
        }
    }

    fn failed(&self, app: &AppHandle, message: String) -> UpdateStatus {
        {
            let mut inner = self.inner.lock();
            inner.status = self.status_with("failed", None, Some(message));
            inner.asset = None;
        }
        self.refresh(app);
        self.status()
    }

    fn status_with(
        &self,
        state: &'static str,
        available_version: Option<String>,
        message: Option<String>,
    ) -> UpdateStatus {
        UpdateStatus {
            state,
            current_version: self.current_version.to_string(),
            available_version,
            message,
        }
    }

    async fn fetch_release(&self) -> Result<Option<Asset>, String> {
        let Some(target) = self.target.as_ref() else {
            return Ok(None);
        };
        let response = self
            .client
            .get(RELEASE_URL)
            .header(reqwest::header::ACCEPT, RELEASE_ACCEPT)
            .header("X-GitHub-Api-Version", RELEASE_API_VERSION)
            .header(
                reqwest::header::USER_AGENT,
                format!("tracon/{}", self.current_version),
            )
            .send()
            .await
            .map_err(|error| format!("Could not check GitHub releases: {error}"))?
            .error_for_status()
            .map_err(|error| format!("Could not check GitHub releases: {error}"))?;
        let release: ReleaseWire = response
            .json()
            .await
            .map_err(|error| format!("Could not read the GitHub release: {error}"))?;
        select_asset(
            &self.current_version,
            target,
            Release {
                tag_name: release.tag_name,
                assets: release.assets,
            },
        )
    }

    fn refresh(&self, app: &AppHandle) {
        if let Some(state) = app.try_state::<std::sync::Arc<State>>() {
            tray::refresh(app, &state);
        }
    }
}

fn select_asset(
    current: &Version,
    target: &Target,
    release: Release,
) -> Result<Option<Asset>, String> {
    let tag = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);
    let version = Version::parse(tag)
        .map_err(|_| format!("GitHub release tag '{}' is not a version", release.tag_name))?;
    if version <= *current {
        return Ok(None);
    }

    let expected_name = target.asset_name(&version);
    let Some(asset) = release
        .assets
        .into_iter()
        .find(|asset| asset.name == expected_name)
    else {
        return Err(format!("release v{version} is not ready; try again"));
    };
    if !asset.browser_download_url.starts_with("https://") {
        return Err(format!("release v{version} is not ready; try again"));
    }
    let Some(digest) = asset.digest.as_deref().and_then(parse_digest) else {
        return Err(format!("release v{version} is not ready; try again"));
    };
    Ok(Some(Asset {
        version,
        url: asset.browser_download_url,
        size: asset.size,
        digest,
    }))
}

fn parse_digest(value: &str) -> Option<[u8; 32]> {
    let hex = value.strip_prefix("sha256:")?;
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return None;
    }
    let mut digest = [0; 32];
    for (index, pair) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        digest[index] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(digest)
}

/// Stream the asset to `path`, refusing anything whose size or SHA-256 is not
/// exactly what the release said. Nothing else reads the file until this
/// returns `Ok`; on any failure the partial download is removed.
async fn download_verified(
    client: &reqwest::Client,
    asset: &Asset,
    path: &Path,
) -> Result<(), String> {
    // Removing a link removes the link itself; `create_new` below never opens
    // a stale target or overwrites any file in place.
    match fs::remove_file(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(REPLACE_MESSAGE.to_string()),
    }
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .map_err(|_| REPLACE_MESSAGE.to_string())?;

    let transfer = async {
        let response = client
            .get(&asset.url)
            .send()
            .await
            .map_err(|error| format!("Could not download the update: {error}"))?
            .error_for_status()
            .map_err(|error| format!("Could not download the update: {error}"))?;
        let mut stream = response.bytes_stream();
        let mut hasher = Sha256::new();
        let mut written = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| format!("Could not download the update: {error}"))?;
            written = written
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| DIGEST_MISMATCH_MESSAGE.to_string())?;
            hasher.update(&chunk);
            output
                .write_all(&chunk)
                .await
                .map_err(|error| format!("Could not write the update: {error}"))?;
        }
        if written != asset.size || hasher.finalize().as_slice() != asset.digest {
            return Err(DIGEST_MISMATCH_MESSAGE.to_string());
        }
        output
            .sync_all()
            .await
            .map_err(|_| REPLACE_MESSAGE.to_string())?;
        Ok(())
    }
    .await;

    drop(output);
    if transfer.is_err() {
        let _ = fs::remove_file(path).await;
    }
    transfer
}

/// An AppImage is one file, so the replacement is one rename.
async fn download_and_replace(
    client: &reqwest::Client,
    asset: &Asset,
    destination: &Path,
) -> Result<(), String> {
    let parent = destination
        .parent()
        .ok_or_else(|| REPLACE_MESSAGE.to_string())?;
    let name = destination
        .file_name()
        .ok_or_else(|| REPLACE_MESSAGE.to_string())?;
    let temporary = parent.join(format!(".{}.tracon-update", name.to_string_lossy()));
    let original_permissions = fs::metadata(destination)
        .await
        .map_err(|_| REPLACE_MESSAGE.to_string())?
        .permissions();

    download_verified(client, asset, &temporary).await?;

    let finish = async {
        fs::set_permissions(&temporary, original_permissions)
            .await
            .map_err(|_| REPLACE_MESSAGE.to_string())?;
        fs::rename(&temporary, destination)
            .await
            .map_err(|_| REPLACE_MESSAGE.to_string())
    }
    .await;

    if finish.is_err() {
        let _ = fs::remove_file(&temporary).await;
    }
    finish
}

/// A `.app` is a directory whose ad-hoc signature covers its contents, so the
/// update is assembled beside it and swapped in by rename. The bundle being
/// replaced is renamed aside rather than deleted: this process, and the node
/// it supervises, are executing out of it.
#[cfg(target_os = "macos")]
async fn download_and_swap(
    client: &reqwest::Client,
    asset: &Asset,
    app_root: &Path,
) -> Result<(), String> {
    let parent = app_root
        .parent()
        .ok_or_else(|| REPLACE_MESSAGE.to_string())?;
    // Beside the bundle, not in the temporary directory: a rename cannot
    // cross volumes.
    let staging = parent.join(format!("{STAGE_PREFIX}{}", process::id()));
    let _ = fs::remove_dir_all(&staging).await;
    fs::create_dir(&staging)
        .await
        .map_err(|_| REPLACE_MESSAGE.to_string())?;

    let staged = async {
        let archive = staging.join("update.tar.gz");
        download_verified(client, asset, &archive).await?;
        unpack(&archive, &staging).await?;
        let staged_app = staged_bundle_in(&staging)?;
        validate_staged_bundle(&staged_app, &asset.version)?;
        Ok::<PathBuf, String>(staged_app)
    }
    .await;

    let staged_app = match staged {
        Ok(staged_app) => staged_app,
        Err(message) => {
            let _ = fs::remove_dir_all(&staging).await;
            return Err(message);
        }
    };

    let old = parent.join(format!("{OLD_PREFIX}{}", process::id()));
    swap_bundles(app_root, &staged_app, &old)?;
    let _ = fs::remove_dir_all(&staging).await;
    Ok(())
}

/// bsdtar ships with macOS and keeps the symlinks and modes a bundle needs.
/// It only ever reads an archive whose digest already matched the release.
#[cfg(target_os = "macos")]
async fn unpack(archive: &Path, into: &Path) -> Result<(), String> {
    let status = tokio::process::Command::new("/usr/bin/tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .status()
        .await
        .map_err(|error| format!("Could not unpack the update: {error}"))?;
    if !status.success() {
        return Err("Could not unpack the update.".to_string());
    }
    Ok(())
}

/// The one `.app` an unpacked archive is expected to contain.
fn staged_bundle_in(staging: &Path) -> Result<PathBuf, String> {
    let mut bundles = std::fs::read_dir(staging)
        .map_err(|_| STAGED_MESSAGE.to_string())?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("app")));
    match (bundles.next(), bundles.next()) {
        (Some(bundle), None) => Ok(bundle),
        _ => Err(STAGED_MESSAGE.to_string()),
    }
}

/// What the archive contained has to be a runnable bundle of exactly the
/// version the release advertised, checked before anything is moved.
fn validate_staged_bundle(staged: &Path, expected: &Version) -> Result<(), String> {
    for executable in ["tracon-wrapper", "tracon"] {
        let path = staged.join("Contents/MacOS").join(executable);
        let metadata = std::fs::metadata(&path).map_err(|_| STAGED_MESSAGE.to_string())?;
        if !metadata.is_file() {
            return Err(STAGED_MESSAGE.to_string());
        }
        #[cfg(unix)]
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(STAGED_MESSAGE.to_string());
        }
    }
    let plist = std::fs::read_to_string(staged.join("Contents/Info.plist"))
        .map_err(|_| STAGED_MESSAGE.to_string())?;
    match plist_short_version(&plist) {
        Some(version) if version == expected.to_string() => Ok(()),
        _ => Err(STAGED_MESSAGE.to_string()),
    }
}

fn plist_short_version(plist: &str) -> Option<String> {
    let (_, rest) = plist.split_once("<key>CFBundleShortVersionString</key>")?;
    let (between, value) = rest.split_once("<string>")?;
    // The value is the next tag or the key is not the one being answered.
    if between.contains("<key>") {
        return None;
    }
    let (value, _) = value.split_once("</string>")?;
    Some(value.trim().to_string())
}

/// Two renames, both metadata operations on one volume. The window where no
/// bundle is installed is between them; if the second fails the first is
/// undone and the install is exactly as it was.
fn swap_bundles(app_root: &Path, staged: &Path, old: &Path) -> Result<(), String> {
    std::fs::rename(app_root, old).map_err(|_| REPLACE_MESSAGE.to_string())?;
    if std::fs::rename(staged, app_root).is_err() {
        let _ = std::fs::rename(old, app_root);
        return Err(REPLACE_MESSAGE.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    fn release(tag_name: &str, assets: Vec<ReleaseAsset>) -> Release {
        Release {
            tag_name: tag_name.to_string(),
            assets,
        }
    }

    fn asset(name: &str, digest: Option<&str>) -> ReleaseAsset {
        ReleaseAsset {
            name: name.to_string(),
            size: 3,
            browser_download_url: "https://example.test/tracon.AppImage".to_string(),
            digest: digest.map(str::to_string),
        }
    }

    async fn serve_once(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.write_all(&body).await.unwrap();
        });
        format!("http://{address}/tracon.AppImage")
    }

    fn appimage() -> Target {
        Target::AppImage(PathBuf::from("/opt/tracon.AppImage"))
    }

    fn bundle() -> Target {
        Target::MacApp(PathBuf::from("/Applications/tracon.app"))
    }

    fn scratch(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tracon-updater-{label}-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fabricate_bundle(root: &Path, version: &str) {
        std::fs::create_dir_all(root.join("Contents/MacOS")).unwrap();
        for executable in ["tracon-wrapper", "tracon"] {
            let path = root.join("Contents/MacOS").join(executable);
            std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
            #[cfg(unix)]
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(
            root.join("Contents/Info.plist"),
            format!(
                "<plist><dict><key>CFBundleName</key><string>tracon</string>\
                 <key>CFBundleShortVersionString</key><string>{version}</string></dict></plist>"
            ),
        )
        .unwrap();
    }

    #[test]
    fn selects_only_the_newer_exact_appimage() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let selected = select_asset(
            &Version::parse("0.9.1").unwrap(),
            &appimage(),
            release(
                "v0.9.2",
                vec![asset("tracon_0.9.2_amd64.AppImage", Some(&digest))],
            ),
        )
        .unwrap()
        .unwrap();
        assert_eq!(selected.version, Version::parse("0.9.2").unwrap());
        assert_eq!(selected.size, 3);
    }

    #[test]
    fn equal_and_older_releases_never_downgrade() {
        let current = Version::parse("0.9.1").unwrap();
        assert!(
            select_asset(&current, &appimage(), release("v0.9.1", vec![]))
                .unwrap()
                .is_none()
        );
        assert!(
            select_asset(&current, &appimage(), release("0.9.0", vec![]))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rejects_incomplete_or_untrusted_new_release_assets() {
        let current = Version::parse("0.9.1").unwrap();
        let wrong = asset("tracon_0.9.2_aarch64.AppImage", None);
        assert_eq!(
            select_asset(&current, &appimage(), release("v0.9.2", vec![wrong])).unwrap_err(),
            "release v0.9.2 is not ready; try again"
        );
        let malformed = asset("tracon_0.9.2_amd64.AppImage", Some("sha256:ABC"));
        assert_eq!(
            select_asset(&current, &appimage(), release("v0.9.2", vec![malformed])).unwrap_err(),
            "release v0.9.2 is not ready; try again"
        );
    }

    #[test]
    fn each_target_selects_only_its_own_asset() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let current = Version::parse("0.9.1").unwrap();
        let mac = || {
            release(
                "v0.9.2",
                vec![asset("tracon_0.9.2_aarch64.app.tar.gz", Some(&digest))],
            )
        };
        assert!(select_asset(&current, &bundle(), mac()).unwrap().is_some());
        assert_eq!(
            select_asset(&current, &appimage(), mac()).unwrap_err(),
            "release v0.9.2 is not ready; try again"
        );
    }

    #[test]
    fn an_app_bundle_is_found_only_from_inside_one() {
        assert_eq!(
            app_root_from(Path::new(
                "/Applications/tracon.app/Contents/MacOS/tracon-wrapper"
            )),
            Some(PathBuf::from("/Applications/tracon.app"))
        );
        // What an update renames aside: no `.app` ancestor, so a process still
        // running from it detects no target and never sweeps itself.
        assert_eq!(
            app_root_from(Path::new(
                "/Applications/.tracon-old-1/Contents/MacOS/tracon-wrapper"
            )),
            None
        );
        assert_eq!(app_root_from(Path::new("/usr/local/bin/tracon")), None);
    }

    #[test]
    fn reads_the_advertised_version_from_a_plist() {
        assert_eq!(
            plist_short_version("<key>CFBundleShortVersionString</key>\n\t<string>0.11.0</string>")
                .as_deref(),
            Some("0.11.0")
        );
        assert!(plist_short_version("<key>CFBundleName</key><string>tracon</string>").is_none());
        assert!(plist_short_version(
            "<key>CFBundleShortVersionString</key><key>Other</key><string>0.1.0</string>"
        )
        .is_none());
    }

    #[test]
    fn a_staged_bundle_must_be_runnable_and_the_advertised_version() {
        let dir = scratch("staged");
        let staged = dir.join("tracon.app");
        fabricate_bundle(&staged, "0.9.2");
        assert!(validate_staged_bundle(&staged, &Version::new(0, 9, 2)).is_ok());
        assert_eq!(
            validate_staged_bundle(&staged, &Version::new(0, 9, 3)).unwrap_err(),
            STAGED_MESSAGE
        );
        assert_eq!(
            staged_bundle_in(&dir).unwrap(),
            staged,
            "the one bundle in the staging directory"
        );

        std::fs::remove_file(staged.join("Contents/MacOS/tracon")).unwrap();
        assert_eq!(
            validate_staged_bundle(&staged, &Version::new(0, 9, 2)).unwrap_err(),
            STAGED_MESSAGE,
            "a bundle without the node sidecar is not an install"
        );

        #[cfg(unix)]
        {
            let wrapper = staged.join("Contents/MacOS/tracon-wrapper");
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(
                validate_staged_bundle(&staged, &Version::new(0, 9, 2)).unwrap_err(),
                STAGED_MESSAGE
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_swap_leaves_the_install_exactly_as_it_was() {
        let dir = scratch("swap");
        let installed = dir.join("tracon.app");
        fabricate_bundle(&installed, "0.9.1");
        let old = dir.join(format!("{OLD_PREFIX}1"));

        assert!(swap_bundles(&installed, &dir.join("missing.app"), &old).is_err());
        assert!(!old.exists(), "the old bundle is renamed back");
        assert_eq!(
            plist_short_version(
                &std::fs::read_to_string(installed.join("Contents/Info.plist")).unwrap()
            )
            .as_deref(),
            Some("0.9.1")
        );

        let staged = dir.join("staged.app");
        fabricate_bundle(&staged, "0.9.2");
        swap_bundles(&installed, &staged, &old).unwrap();
        assert_eq!(
            plist_short_version(
                &std::fs::read_to_string(installed.join("Contents/Info.plist")).unwrap()
            )
            .as_deref(),
            Some("0.9.2")
        );
        assert!(old.join("Contents/Info.plist").exists(), "kept until swept");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sweeping_removes_only_what_an_update_left() {
        let dir = scratch("sweep");
        let installed = dir.join("tracon.app");
        fabricate_bundle(&installed, "0.9.2");
        std::fs::create_dir(dir.join(format!("{OLD_PREFIX}1"))).unwrap();
        std::fs::create_dir(dir.join(format!("{STAGE_PREFIX}2"))).unwrap();
        std::fs::write(dir.join("unrelated.txt"), b"keep").unwrap();

        sweep_stale(&Target::MacApp(installed.clone()));
        assert!(!dir.join(format!("{OLD_PREFIX}1")).exists());
        assert!(!dir.join(format!("{STAGE_PREFIX}2")).exists());
        assert!(dir.join("unrelated.txt").exists());
        assert!(installed.exists());

        sweep_stale(&appimage());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn restart_arguments_are_a_pid_and_something_to_start() {
        let args = |items: &[&str]| {
            items
                .iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
                .into_iter()
        };
        assert!(parse_restart_args(args(&["--something-else"])).is_none());
        assert!(parse_restart_args(args(&[])).is_none());

        let parsed = parse_restart_args(args(&[
            RESTART_HELPER_ARGUMENT,
            "42",
            "/Applications/tracon.app",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(parsed.parent, 42);
        assert_eq!(parsed.relaunch, OsString::from("/Applications/tracon.app"));

        assert!(
            parse_restart_args(args(&[RESTART_HELPER_ARGUMENT, "not-a-pid", "/x"]))
                .unwrap()
                .is_err()
        );
        assert!(parse_restart_args(args(&[RESTART_HELPER_ARGUMENT, "42"]))
            .unwrap()
            .is_err());
    }

    /// The whole macOS replacement minus the network: archive a bundle the way
    /// the release does, then unpack, validate, swap, and sweep.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn an_archived_bundle_survives_the_round_trip_into_place() {
        let dir = scratch("bundle");
        let source = dir.join("source");
        std::fs::create_dir(&source).unwrap();
        fabricate_bundle(&source.join("tracon.app"), "0.9.2");
        std::os::unix::fs::symlink("MacOS/tracon", source.join("tracon.app/Contents/node"))
            .unwrap();

        let archive = dir.join("update.tar.gz");
        assert!(std::process::Command::new("/usr/bin/tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&source)
            .arg("tracon.app")
            .status()
            .unwrap()
            .success());

        let installed = dir.join("tracon.app");
        fabricate_bundle(&installed, "0.9.1");
        let staging = dir.join(format!("{STAGE_PREFIX}3"));
        std::fs::create_dir(&staging).unwrap();
        unpack(&archive, &staging).await.unwrap();

        let staged = staged_bundle_in(&staging).unwrap();
        validate_staged_bundle(&staged, &Version::new(0, 9, 2)).unwrap();
        assert!(
            std::fs::symlink_metadata(staged.join("Contents/node"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "bsdtar keeps the symlinks a bundle is built from"
        );
        assert_eq!(
            std::fs::metadata(staged.join("Contents/MacOS/tracon-wrapper"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );

        swap_bundles(&installed, &staged, &dir.join(format!("{OLD_PREFIX}3"))).unwrap();
        assert_eq!(
            plist_short_version(
                &std::fs::read_to_string(installed.join("Contents/Info.plist")).unwrap()
            )
            .as_deref(),
            Some("0.9.2")
        );

        sweep_stale(&Target::MacApp(installed));
        assert!(!dir.join(format!("{OLD_PREFIX}3")).exists());
        assert!(!staging.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn digest_requires_lowercase_sha256() {
        assert!(parse_digest(&format!("sha256:{}", "0".repeat(64))).is_some());
        assert!(parse_digest(&format!("sha256:{}", "A".repeat(64))).is_none());
        assert!(parse_digest("sha512:00").is_none());
    }

    #[tokio::test]
    async fn verified_download_replaces_the_appimage_and_keeps_its_mode() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tracon-updater-{unique}"));
        fs::create_dir(&dir).await.unwrap();
        let destination = dir.join("tracon.AppImage");
        fs::write(&destination, b"old").await.unwrap();
        #[cfg(unix)]
        {
            fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o751))
                .await
                .unwrap();
        }
        let body = b"new AppImage".to_vec();
        let asset = Asset {
            version: Version::new(0, 9, 2),
            url: serve_once(body.clone()).await,
            size: body.len() as u64,
            digest: Sha256::digest(&body).into(),
        };
        download_and_replace(&reqwest::Client::new(), &asset, &destination)
            .await
            .unwrap();
        assert_eq!(fs::read(&destination).await.unwrap(), body);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&destination)
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o751
        );
        fs::remove_dir_all(dir).await.unwrap();
    }

    #[tokio::test]
    async fn invalid_downloads_leave_the_appimage_unchanged() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tracon-updater-{unique}"));
        fs::create_dir(&dir).await.unwrap();
        let destination = dir.join("tracon.AppImage");
        fs::write(&destination, b"old").await.unwrap();
        let body = b"new".to_vec();
        let digest = Sha256::digest(&body).into();
        let mismatch = Asset {
            version: Version::new(0, 9, 2),
            url: serve_once(body.clone()).await,
            size: body.len() as u64,
            digest: [0; 32],
        };
        assert_eq!(
            download_and_replace(&reqwest::Client::new(), &mismatch, &destination)
                .await
                .unwrap_err(),
            DIGEST_MISMATCH_MESSAGE
        );
        assert_eq!(fs::read(&destination).await.unwrap(), b"old");

        let short = Asset {
            version: Version::new(0, 9, 2),
            url: serve_once(body).await,
            size: 4,
            digest,
        };
        assert_eq!(
            download_and_replace(&reqwest::Client::new(), &short, &destination)
                .await
                .unwrap_err(),
            DIGEST_MISMATCH_MESSAGE
        );
        assert_eq!(fs::read(&destination).await.unwrap(), b"old");
        assert!(!dir.join(".tracon.AppImage.tracon-update").exists());
        fs::remove_dir_all(dir).await.unwrap();
    }
}
