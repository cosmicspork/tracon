//! A tray client for a node, and the thing that installs it.
//!
//! What is actually wanted from native is tray presence, command-tab, a global
//! hotkey, and system notifications. The node itself runs under the user's
//! service manager, so it is there whether or not this app is open; the app
//! installs that service and the CLI it runs, opens on a setup page until
//! they exist, and after that is a window onto the node.
//!
//! It holds no session state: the window is the same interface the node serves
//! over HTTP, so a crash here is a reconnect, never lost work.
//!
//! The tray is the queue plus a kill switch. It does not stream output; that
//! is what the window is for.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod node;
mod prefs;
mod queue;
mod service;
mod setup;
mod tray;
mod updater;

use std::sync::{Arc, Mutex};

use tauri::{Manager, WindowEvent};
use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut, ShortcutState};

/// Where the node is. A wrapper talks to the node on its own machine, which is
/// loopback and needs no credential; `TRACON_URL` points it elsewhere.
pub fn node_url() -> String {
    std::env::var("TRACON_URL").unwrap_or_else(|_| "http://127.0.0.1:7420".into())
}

#[tauri::command]
fn desktop_managed_local() -> bool {
    std::env::var_os("TRACON_URL").is_none()
}

/// Everything the tray needs to render, kept in one place so the menu and the
/// notifier agree about what is waiting.
#[derive(Default)]
pub struct State {
    pub queue: Mutex<queue::Queue>,
    /// What has already been announced, so a reconnect does not re-announce
    /// the same approvals.
    pub announced: Mutex<std::collections::HashSet<String>>,
    pub connected: Mutex<bool>,
    /// Why the node is not running, when the service should be running it.
    pub node_error: Mutex<Option<String>>,
    /// A node restart this app owes and is waiting to make: the node binary
    /// changed under an update, and the running one is left to finish its
    /// sessions first.
    pub node_update: Mutex<Option<String>>,
}

/// End the app, whatever ⌘Q is set to do. The tray's Quit and the menu's
/// Quit both land here; only the latter consults the preference first.
pub fn quit(app: &tauri::AppHandle) {
    app.exit(0);
}

/// After an app update the app carries a new node, while the terminal and
/// the service still run the old one. Move the CLI, then the service once no
/// session is running (an update never ends a session mid-flight), then
/// rebuild the boundary images if the new node finds them stale.
async fn move_to_carried_node(
    app: tauri::AppHandle,
    state: Arc<State>,
    http: reqwest::Client,
    url: String,
) {
    use tauri_plugin_notification::NotificationExt;
    let Some(carried) = node::sidecar_path().and_then(|p| node::version_of(&p)) else {
        return;
    };
    let installed = node::installed_path().and_then(|p| node::version_of(&p));
    let running = node::running_version(&http, &url).await;
    let steps = node::plan(
        running.as_deref(),
        &carried,
        installed.as_deref(),
        service::installed(),
    );
    let say = |line: Option<String>| {
        *state.node_update.lock().unwrap() = line;
        tray::refresh(&app, &state);
    };
    let fail = |why: String| {
        eprintln!("tracon: {why}");
        *state.node_update.lock().unwrap() = None;
        *state.node_error.lock().unwrap() = Some(why);
        tray::refresh(&app, &state);
    };

    if steps.contains(&node::Step::InstallCli) {
        match tauri::async_runtime::spawn_blocking(node::install_cli).await {
            Ok(Ok(_)) => {}
            Ok(Err(why)) => return fail(why),
            Err(e) => return fail(e.to_string()),
        }
    }
    if steps.contains(&node::Step::RestartService) {
        let mut announced = false;
        loop {
            let busy = node::running_sessions(&http, &url).await.unwrap_or(1);
            if busy == 0 {
                break;
            }
            let line = format!(
                "Node restarts to v{carried} when {busy} running session{} end{}",
                if busy == 1 { "" } else { "s" },
                if busy == 1 { "s" } else { "" }
            );
            say(Some(line.clone()));
            if !announced {
                announced = true;
                let _ = app
                    .notification()
                    .builder()
                    .title("tracon updated")
                    .body(line)
                    .show();
            }
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }
        say(Some(format!("Restarting the node to v{carried}…")));
        let Some(cli) = node::installed_path() else {
            return fail("HOME is not set".into());
        };
        match tauri::async_runtime::spawn_blocking(move || service::restart(&cli)).await {
            Ok(Ok(())) => {}
            Ok(Err(why)) => return fail(why),
            Err(e) => return fail(e.to_string()),
        }
        if !node::wait_ready(&http, &url).await {
            return fail(format!(
                "The node did not answer after restarting to v{carried}"
            ));
        }
        let _ = app
            .notification()
            .builder()
            .title("tracon updated")
            .body(format!("The node is running v{carried}."))
            .show();
    }
    if node::images_stale(&http, &url).await {
        say(Some("Rebuilding the boundary images…".into()));
        if let Err(why) = node::run_setup(&http, &url).await {
            return fail(why);
        }
    }
    say(None);
}

#[tauri::command]
fn desktop_update_status(
    updater: tauri::State<'_, Arc<updater::Updater>>,
) -> updater::UpdateStatus {
    updater.status()
}

#[tauri::command]
async fn desktop_check_for_update(
    app: tauri::AppHandle,
    updater: tauri::State<'_, Arc<updater::Updater>>,
) -> Result<updater::UpdateStatus, String> {
    Ok(updater.check(&app).await)
}

#[tauri::command]
async fn desktop_install_update(
    app: tauri::AppHandle,
    updater: tauri::State<'_, Arc<updater::Updater>>,
) -> Result<updater::UpdateStatus, String> {
    Ok(updater.install(&app).await)
}

#[tauri::command]
async fn desktop_setup_status() -> setup::SetupStatus {
    setup::status(&reqwest::Client::new(), &node_url()).await
}

#[tauri::command]
async fn desktop_install_service() -> Result<setup::SetupStatus, String> {
    setup::install_service(&reqwest::Client::new(), &node_url()).await
}

#[tauri::command]
async fn desktop_install_cli() -> Result<setup::SetupStatus, String> {
    setup::install_cli(&reqwest::Client::new(), &node_url()).await
}

#[tauri::command]
async fn desktop_restart_node() -> Result<setup::SetupStatus, String> {
    setup::restart_node(&reqwest::Client::new(), &node_url()).await
}

#[tauri::command]
fn desktop_open_node(app: tauri::AppHandle) {
    open_at(&app, "/");
}

fn main() {
    if updater::run_restart_helper() {
        return;
    }
    let state = Arc::new(State::default());
    let preferences = Arc::new(prefs::Store::new());

    tauri::Builder::default()
        // First, before anything else can take a lock or a port: a second
        // launch hands its argv to the first and exits. Without it, opening
        // the app while it sits in the menu bar with no window starts a
        // second one — two tray icons, two supervisors, one node, because
        // the node at least knows to adopt the one already running.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // What the second launch meant: show me the window.
            show_window(app);
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    // Fire on press, not release: a hotkey that acts twice per
                    // tap feels broken.
                    if event.state() == ShortcutState::Pressed {
                        toggle_window(app);
                    }
                })
                .build(),
        )
        .manage(state.clone())
        .manage(preferences.clone())
        .invoke_handler(tauri::generate_handler![
            desktop_managed_local,
            desktop_update_status,
            desktop_check_for_update,
            desktop_install_update,
            desktop_setup_status,
            desktop_install_service,
            desktop_install_cli,
            desktop_restart_node,
            desktop_open_node,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            let target = updater::Target::detect(&app.env());
            if let Some(target) = target.clone() {
                // Off the runtime: an update leaves a whole bundle behind, and
                // removing it is the one thing the update itself cannot do.
                std::thread::spawn(move || updater::sweep_stale(&target));
            }
            let updater = Arc::new(updater::Updater::new(
                &app.package_info().version.to_string(),
                target,
            ));
            app.manage(updater.clone());

            tray::install(&handle)?;

            // The window opens on the bundled setup page. Once a node answers
            // — the service's, or anyone's — it is pointed at the node's own
            // origin instead: the interface's relative `/api` calls only work
            // served from there. A node an earlier version of this app still
            // runs as its child stays on the setup page, which moves it under
            // the service.
            {
                let handle = handle.clone();
                let st = state.clone();
                let launch_visible = preferences.get().open_window_at_launch;
                tauri::async_runtime::spawn(async move {
                    let http = reqwest::Client::new();
                    let url = node_url();
                    let local = desktop_managed_local();
                    let migrated = local && node::migrated_node(&node::state_dir()).is_some();
                    let ready = node::answering(&http, &url).await
                        || (local && service::installed() && node::wait_ready(&http, &url).await);
                    if ready && !migrated {
                        open_at_if(&handle, "/", launch_visible);
                        if local {
                            move_to_carried_node(handle.clone(), st.clone(), http, url).await;
                        }
                        return;
                    }
                    if !ready && service::installed() {
                        let why =
                            "The service is installed but the node is not answering".to_string();
                        eprintln!("tracon: {why}");
                        *st.node_error.lock().unwrap() = Some(why);
                        tray::refresh(&handle, &st);
                    }
                    show_window(&handle);
                });
            }

            // Ctrl+Alt+T: reachable from whatever has focus, and not something
            // an editor or a browser already claims.
            let shortcut = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyT);
            use tauri_plugin_global_shortcut::GlobalShortcutExt;
            if let Err(e) = handle.global_shortcut().register(shortcut) {
                // A taken hotkey is a nuisance, not a reason to refuse to run.
                eprintln!("tracon: could not register the global shortcut: {e}");
            }

            // A window closed to the tray means the app keeps running, so the
            // only ways out are the tray's Quit and a signal — logout sends
            // one. Routing a signal through `exit` runs the same shutdown
            // either way, which matters for the one node the app still owns:
            // an earlier version's child, not yet moved under the service.
            #[cfg(unix)]
            {
                let handle = handle.clone();
                tauri::async_runtime::spawn(async move {
                    use tokio::signal::unix::{signal, SignalKind};
                    let (Ok(mut term), Ok(mut int)) = (
                        signal(SignalKind::terminate()),
                        signal(SignalKind::interrupt()),
                    ) else {
                        return;
                    };
                    tokio::select! {
                        _ = term.recv() => {}
                        _ = int.recv() => {}
                    }
                    handle.exit(0);
                });
            }

            set_dock_policy(&handle, false, preferences.get().hide_dock_when_closed);

            #[cfg(target_os = "macos")]
            if let Err(e) = install_app_menu(&handle) {
                // Without it ⌘Q is the system's and always quits; that is the
                // old behaviour, not a reason to refuse to start.
                eprintln!("tracon: could not install the application menu: {e}");
            }

            let watcher = handle.clone();
            let st = state.clone();
            tauri::async_runtime::spawn(async move {
                queue::watch(watcher, st).await;
            });
            let update_handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                let status = updater.check(&update_handle).await;
                if status.state == "failed" {
                    if let Some(message) = status.message {
                        eprintln!("tracon: update check failed: {message}");
                    }
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing collapses to the tray rather than quitting: the point of
            // this is to sit alongside Teams and Outlook and be command-tabbed
            // back to.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                let app = window.app_handle();
                let hide_dock = app.state::<Arc<prefs::Store>>().get().hide_dock_when_closed;
                set_dock_policy(app, false, hide_dock);
            }
        })
        .build(tauri::generate_context!())
        .expect("building the wrapper")
        .run(move |_app, event| {
            // The service's node outlives the app. A child an earlier version
            // spawned does not, as it never did: quitting stops it, and waits,
            // so it is not left tearing down its containers alone.
            match event {
                tauri::RunEvent::Exit if !service::installed() => {
                    node::stop_migrated_node(&node::state_dir())
                }
                // The dock icon and the app switcher both reactivate rather
                // than launch, and the window they would raise was hidden by
                // the close button. Without this the icon is inert. macOS
                // only: nothing else has a dock to click.
                #[cfg(target_os = "macos")]
                tauri::RunEvent::Reopen { .. } => show_window(_app),
                _ => {}
            }
        });
}

/// The macOS application menu, with our own Quit in it.
///
/// The predefined Quit sends `terminate:`, which ends the process before any
/// Tauri event fires — `RunEvent::ExitRequested` is documented as not
/// arriving on macOS at all (tauri-apps/tauri#9198), which is why preventing
/// the exit there did nothing. A custom item carrying the same ⌘Q shortcut is
/// an ordinary menu event, and an ordinary menu event can decide.
///
/// The rest of the menu is rebuilt as macOS expects it, because replacing the
/// application submenu replaces all of it.
#[cfg(target_os = "macos")]
fn install_app_menu(app: &tauri::AppHandle) -> tauri::Result<()> {
    use tauri::menu::{AboutMetadata, Menu, MenuItem, PredefinedMenuItem, Submenu};

    let quit = MenuItem::with_id(app, "app:quit", "Quit tracon", true, Some("CmdOrCtrl+Q"))?;
    let app_menu = Submenu::with_items(
        app,
        "tracon",
        true,
        &[
            &PredefinedMenuItem::about(app, None, Some(AboutMetadata::default()))?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::services(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::hide(app, None)?,
            &PredefinedMenuItem::hide_others(app, None)?,
            &PredefinedMenuItem::show_all(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;
    // Without these two, copy and paste stop working in the webview: the
    // shortcuts are menu items on macOS, not window behaviour.
    let edit = Submenu::with_items(
        app,
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(app, None)?,
            &PredefinedMenuItem::redo(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, None)?,
            &PredefinedMenuItem::copy(app, None)?,
            &PredefinedMenuItem::paste(app, None)?,
            &PredefinedMenuItem::select_all(app, None)?,
        ],
    )?;
    let window = Submenu::with_items(
        app,
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(app, None)?,
            &PredefinedMenuItem::fullscreen(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::close_window(app, Some("Close Window"))?,
        ],
    )?;
    let menu = Menu::with_items(app, &[&app_menu, &edit, &window])?;
    app.set_menu(menu)?;
    app.on_menu_event(|app, event| {
        if event.id().as_ref() == "app:quit" {
            on_cmd_q(app);
        }
    });
    Ok(())
}

/// ⌘Q, or Quit from the application menu.
#[cfg(target_os = "macos")]
fn on_cmd_q(app: &tauri::AppHandle) {
    use tauri::Manager;
    if app.state::<Arc<prefs::Store>>().get().cmd_q_quits {
        quit(app);
        return;
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
    let hide_dock = app.state::<Arc<prefs::Store>>().get().hide_dock_when_closed;
    set_dock_policy(app, false, hide_dock);
}

/// Raise the window: the one thing every path back to it wants.
pub fn show_window(app: &tauri::AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    // Before showing: a window raised while the app is an accessory cannot
    // take focus, and arrives behind whatever the operator was looking at.
    set_dock_policy(app, true, true);
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
}

/// Whether the app appears in the dock and the app switcher.
///
/// A tray app with no window has nothing to switch to, so it leaves both
/// while the window is closed and comes back when there is something to
/// show. The way back in is the menu bar icon or the hotkey, which is what
/// the menu bar icon is for.
#[allow(unused_variables)]
pub fn set_dock_policy(app: &tauri::AppHandle, window_visible: bool, hide_when_closed: bool) {
    #[cfg(target_os = "macos")]
    {
        use tauri::ActivationPolicy;
        let policy = if window_visible || !hide_when_closed {
            ActivationPolicy::Regular
        } else {
            ActivationPolicy::Accessory
        };
        if let Err(e) = app.set_activation_policy(policy) {
            eprintln!("tracon: could not set the activation policy: {e}");
        }
    }
}

/// Show and focus the window, or hide it if it already has focus. For the
/// hotkey, where the window may be visible but buried under what has focus:
/// summoning it is what was asked for, and a second press dismisses it.
pub fn toggle_window(app: &tauri::AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let focused = window.is_focused().unwrap_or(false);
    let visible = window.is_visible().unwrap_or(false);
    if visible && focused {
        let _ = window.hide();
    } else {
        show_window(app);
    }
}

/// Open the window at one of the interface's routes.
pub fn open_at(app: &tauri::AppHandle, path: &str) {
    open_at_if(app, path, true);
}

/// Point the window at one of the interface's routes, raising it only when
/// asked: at launch the operator may want the app in the tray and no more.
fn open_at_if(app: &tauri::AppHandle, path: &str, show: bool) {
    if let Some(window) = app.get_webview_window("main") {
        let url = format!("{}{path}", node_url());
        if let Ok(parsed) = url.parse() {
            let _ = window.navigate(parsed);
        }
    }
    if show {
        show_window(app);
    }
}
