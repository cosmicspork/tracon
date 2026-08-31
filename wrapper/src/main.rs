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

fn main() {
    let state = Arc::new(State::default());
    let supervisor = Arc::new(node::Node::default());
    let stopper = supervisor.clone();

    tauri::Builder::default()
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

/// Raise the window: the one thing every path back to it wants.
pub fn show_window(app: &tauri::AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
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

/// The tray's toggle, on visibility alone. Clicking the menu bar deactivates
/// the app first, so a focus test there reports "not focused" for a window
/// that is plainly on screen and the window flashes instead of hiding.
pub fn tray_toggle_window(app: &tauri::AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    if window.is_visible().unwrap_or(false) {
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
