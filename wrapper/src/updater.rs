//! Unsigned AppImage updates verified against GitHub's release digest.
//!
//! The updater deliberately owns its selected asset. The webview can ask it to
//! check or install, but never chooses a URL, path, or checksum.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::{self, Command},
    thread,
    time::Duration,
};

use futures_util::StreamExt;
use parking_lot::Mutex;
use semver::Version;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};
use tokio::{fs, io::AsyncWriteExt};

use crate::{tray, State};

const RELEASE_URL: &str = "https://api.github.com/repos/cosmicspork/tracon/releases/latest";
const RELEASE_ACCEPT: &str = "application/vnd.github+json";
const RELEASE_API_VERSION: &str = "2022-11-28";
const UNSUPPORTED_MESSAGE: &str =
    "This install is managed by its package; self-update is available in the AppImage.";
const DIGEST_MISMATCH_MESSAGE: &str =
    "Downloaded AppImage did not match GitHub; the current version was not changed.";
const REPLACE_MESSAGE: &str =
    "Cannot replace this AppImage; its containing directory must be writable.";

/// Tauri restarts before the single-instance plugin releases its primary
/// socket, so a direct `request_restart` starts a second instance that exits
/// immediately. This minimal helper waits outside Tauri, then starts the
/// replaced AppImage after the parent is gone.
const RESTART_HELPER_ARGUMENT: &str = "--tracon-restart-after";

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

pub fn run_restart_helper() -> bool {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    if arguments.next().as_deref() != Some(OsStr::new(RESTART_HELPER_ARGUMENT)) {
        return false;
    }
    let Some(parent) = arguments
        .next()
        .and_then(|parent| parent.into_string().ok())
        .and_then(|parent| parent.parse::<u32>().ok())
    else {
        eprintln!("tracon: invalid update restart parent");
        return true;
    };
    let Some(appimage) = arguments.next() else {
        eprintln!("tracon: missing AppImage for update restart");
        return true;
    };
    if let Err(error) = detach_restart_helper() {
        eprintln!("tracon: could not detach update restart helper: {error}");
        return true;
    }
    while Path::new(&format!("/proc/{parent}")).exists() {
        thread::sleep(Duration::from_millis(50));
    }
    let mut restart = Command::new(appimage);
    // The AppImage runtime puts these extraction paths in its child
    // environment. Carrying them into a second AppImage makes its runtime
    // treat the already-deleted extraction directory as current.
    restart
        .env_remove("APPIMAGE")
        .env_remove("APPDIR")
        .env_remove("OWD");
    if let Err(error) = restart.spawn() {
        eprintln!("tracon: could not restart updated AppImage: {error}");
    }
    true
}

fn schedule_restart(appimage: &Path) -> Result<(), String> {
    let helper = std::env::current_exe()
        .map_err(|error| format!("Could not restart the updated AppImage: {error}"))?;
    Command::new(helper)
        .arg(RESTART_HELPER_ARGUMENT)
        .arg(process::id().to_string())
        .arg(appimage)
        .spawn()
        .map_err(|error| format!("Could not restart the updated AppImage: {error}"))?;
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
    appimage: Option<PathBuf>,
    client: reqwest::Client,
    inner: Mutex<Inner>,
}

impl Updater {
    pub fn new(current_version: &str, appimage: Option<PathBuf>) -> Self {
        let current_version =
            Version::parse(current_version).unwrap_or_else(|_| Version::new(0, 0, 0));
        let supported = cfg!(all(target_os = "linux", target_arch = "x86_64"))
            && appimage
                .as_ref()
                .is_some_and(|path| !path.as_os_str().is_empty());
        let status = UpdateStatus {
            state: if supported { "idle" } else { "unsupported" },
            current_version: current_version.to_string(),
            available_version: None,
            message: (!supported).then(|| UNSUPPORTED_MESSAGE.to_string()),
        };
        Self {
            current_version,
            appimage: supported.then_some(appimage).flatten(),
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
            if self.appimage.is_none() {
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
        let (asset, appimage) = {
            let mut inner = self.inner.lock();
            if inner.status.state == "downloading" || inner.status.state == "checking" {
                return inner.status.clone();
            }
            let Some(asset) = inner.asset.clone() else {
                return inner.status.clone();
            };
            let Some(appimage) = self.appimage.clone() else {
                return inner.status.clone();
            };
            inner.status = self.status_with("downloading", Some(asset.version.to_string()), None);
            (asset, appimage)
        };
        self.refresh(app);

        let result = download_and_replace(&self.client, &asset, &appimage).await;
        if let Err(message) = result {
            let mut inner = self.inner.lock();
            inner.status = self.status_with("failed", None, Some(message));
            inner.asset = None;
            drop(inner);
            self.refresh(app);
            return self.status();
        }

        if let Err(message) = schedule_restart(&appimage) {
            let mut inner = self.inner.lock();
            inner.status = self.status_with("failed", None, Some(message));
            inner.asset = None;
            drop(inner);
            self.refresh(app);
            return self.status();
        }
        app.exit(0);
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

fn select_asset(current: &Version, release: Release) -> Result<Option<Asset>, String> {
    let tag = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);
    let version = Version::parse(tag)
        .map_err(|_| format!("GitHub release tag '{}' is not a version", release.tag_name))?;
    if version <= *current {
        return Ok(None);
    }

    let expected_name = format!("tracon_{version}_amd64.AppImage");
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

    // Removing a link removes the link itself; `create_new` below never opens
    // a stale target or overwrites any file in place.
    match fs::remove_file(&temporary).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(REPLACE_MESSAGE.to_string()),
    }
    let original_permissions = fs::metadata(destination)
        .await
        .map_err(|_| REPLACE_MESSAGE.to_string())?
        .permissions();
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .await
        .map_err(|_| REPLACE_MESSAGE.to_string())?;

    let transfer = async {
        let response = client
            .get(&asset.url)
            .send()
            .await
            .map_err(|error| format!("Could not download the AppImage: {error}"))?
            .error_for_status()
            .map_err(|error| format!("Could not download the AppImage: {error}"))?;
        let mut stream = response.bytes_stream();
        let mut hasher = Sha256::new();
        let mut written = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|error| format!("Could not download the AppImage: {error}"))?;
            written = written
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| DIGEST_MISMATCH_MESSAGE.to_string())?;
            hasher.update(&chunk);
            output
                .write_all(&chunk)
                .await
                .map_err(|error| format!("Could not write the AppImage: {error}"))?;
        }
        if written != asset.size || hasher.finalize().as_slice() != asset.digest {
            return Err(DIGEST_MISMATCH_MESSAGE.to_string());
        }
        output
            .set_permissions(original_permissions)
            .await
            .map_err(|_| REPLACE_MESSAGE.to_string())?;
        output
            .sync_all()
            .await
            .map_err(|_| REPLACE_MESSAGE.to_string())?;
        drop(output);
        fs::rename(&temporary, destination)
            .await
            .map_err(|_| REPLACE_MESSAGE.to_string())?;
        Ok(())
    }
    .await;

    if transfer.is_err() {
        let _ = fs::remove_file(&temporary).await;
    }
    transfer
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
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

    #[test]
    fn selects_only_the_newer_exact_appimage() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let selected = select_asset(
            &Version::parse("0.9.1").unwrap(),
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
        assert!(select_asset(&current, release("v0.9.1", vec![]))
            .unwrap()
            .is_none());
        assert!(select_asset(&current, release("0.9.0", vec![]))
            .unwrap()
            .is_none());
    }

    #[test]
    fn rejects_incomplete_or_untrusted_new_release_assets() {
        let current = Version::parse("0.9.1").unwrap();
        let wrong = asset("tracon_0.9.2_aarch64.AppImage", None);
        assert_eq!(
            select_asset(&current, release("v0.9.2", vec![wrong])).unwrap_err(),
            "release v0.9.2 is not ready; try again"
        );
        let malformed = asset("tracon_0.9.2_amd64.AppImage", Some("sha256:ABC"));
        assert_eq!(
            select_asset(&current, release("v0.9.2", vec![malformed])).unwrap_err(),
            "release v0.9.2 is not ready; try again"
        );
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
