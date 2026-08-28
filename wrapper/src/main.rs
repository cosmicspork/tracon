//! A tray client for a node that is already running.
//!
//! What is actually wanted from native is tray presence, command-tab, a global
//! hotkey, and system notifications. This provides those four and nothing
//! else. It does not supervise the node — the platform does that
//! (`tracon service install`) — and it holds no session state: the window is
//! the same interface the node serves over HTTP, so a crash here is a
//! reconnect, never lost work.
//!
//! The tray is the queue plus a kill switch. It does not stream output; that
//! is what the window is for.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

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
}

fn main() {
    let state = Arc::new(State::default());

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
        .setup(move |app| {
            let handle = app.handle().clone();
            tray::install(&handle)?;

            // Ctrl+Alt+T: reachable from whatever has focus, and not something
            // an editor or a browser already claims.
            let shortcut = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyT);
            use tauri_plugin_global_shortcut::GlobalShortcutExt;
            if let Err(e) = handle.global_shortcut().register(shortcut) {
                // A taken hotkey is a nuisance, not a reason to refuse to run.
                eprintln!("tracon: could not register the global shortcut: {e}");
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
        .run(tauri::generate_context!())
        .expect("running the wrapper");
}

/// Show and focus the window, or hide it if it already has focus.
pub fn toggle_window(app: &tauri::AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let focused = window.is_focused().unwrap_or(false);
    let visible = window.is_visible().unwrap_or(false);
    if visible && focused {
        let _ = window.hide();
    } else {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Open the window at one of the interface's routes.
pub fn open_at(app: &tauri::AppHandle, path: &str) {
    if let Some(window) = app.get_webview_window("main") {
        let url = format!("{}{path}", node_url());
        if let Ok(parsed) = url.parse() {
            let _ = window.navigate(parsed);
        }
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}
