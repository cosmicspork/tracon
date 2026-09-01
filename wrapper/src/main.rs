//! A tray client for a node, and — on a machine where you want one — the thing
//! that runs it.
//!
//! What is actually wanted from native is tray presence, command-tab, a global
//! hotkey, and system notifications. This provides those four, and will start
//! and stop the node itself so a laptop does not need a unit file for
//! something that is only wanted while you are logged in. It adopts a node
//! that is already running rather than starting a second one, so
//! `tracon service install` remains the right answer for a machine that has to
//! stay reachable.
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
mod tray;

use std::sync::{Arc, Mutex};

use tauri::{Manager, WindowEvent};
use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut, ShortcutState};

/// Where the node is. A wrapper talks to the node on its own machine, which is
/// loopback and needs no credential; `TRACON_URL` points it elsewhere.
pub fn node_url() -> String {
    std::env::var("TRACON_URL").unwrap_or_else(|_| "http://127.0.0.1:7420".into())
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
    /// Why the node is not running, when this app was the one running it.
    pub node_error: Mutex<Option<String>>,
}

/// End the app, whatever ⌘Q is set to do. The tray's Quit and the menu's
/// Quit both land here; only the latter consults the preference first.
pub fn quit(app: &tauri::AppHandle) {
    app.exit(0);
}

fn main() {
    let state = Arc::new(State::default());
    let supervisor = Arc::new(node::Node::default());
    let stopper = supervisor.clone();
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
        .manage(supervisor.clone())
        .manage(preferences.clone())
        .setup(move |app| {
            let handle = app.handle().clone();
            tray::install(&handle)?;

            // Start the node before anything tries to read from it, then point
            // the window at the node's own origin. The bundled assets exist
            // only to satisfy the build: served from `tauri://` their relative
            // `/api` calls would go nowhere, so the interface has to come from
            // the node the way a browser gets it.
            {
                let handle = handle.clone();
                let st = state.clone();
                let node = supervisor.clone();
                let launch_visible = preferences.get().open_window_at_launch;
                tauri::async_runtime::spawn(async move {
                    let http = reqwest::Client::new();
                    let url = node_url();
                    match node.ensure(&http, &url).await {
                        Ok(()) => {
                            if let Some(window) = handle.get_webview_window("main") {
                                if let Ok(parsed) = url.parse() {
                                    let _ = window.navigate(parsed);
                                }
                            }
                            // Only now: showing the window before the node
                            // answers shows the failure page of a node that
                            // is merely still starting.
                            if launch_visible {
                                show_window(&handle);
                            }
                            if let Some(why) = node.supervise(http, url).await {
                                *st.node_error.lock().unwrap() = Some(why);
                                tray::refresh(&handle, &st);
                            }
                        }
                        Err(why) => {
                            eprintln!("tracon: {why}");
                            *st.node_error.lock().unwrap() = Some(why);
                            tray::refresh(&handle, &st);
                        }
                    }
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
            // one, and so does anything that kills the app. Routing a signal
            // through `exit` rather than letting the process die is what stops
            // a node being orphaned by a logout: the same shutdown runs either
            // way, and an orphan would outlive the app that owns it.
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
            // Quitting takes the node with it, but only if this app started
            // it. Exit is the last event, and stopping is synchronous on
            // purpose: the process must not go away while the node is still
            // tearing down its containers.
            match event {
                tauri::RunEvent::Exit => stopper.stop(),
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
    if let Some(window) = app.get_webview_window("main") {
        let url = format!("{}{path}", node_url());
        if let Ok(parsed) = url.parse() {
            let _ = window.navigate(parsed);
        }
    }
    show_window(app);
}
