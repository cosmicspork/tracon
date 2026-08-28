//! The tray: what is waiting, and a way to stop a session.
//!
//! It does not stream output and does not show a diff. Anything that needs
//! reading opens the window at the right route.

use std::sync::Arc;

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::{TrayIcon, TrayIconBuilder, TrayIconEvent},
    AppHandle,
};

use crate::{open_at, queue, toggle_window, State};

const TRAY_ID: &str = "tracon";
/// Beyond this the menu is a wall of text; the window is better for that.
const MAX_LISTED: usize = 8;

pub fn install(app: &AppHandle) -> tauri::Result<()> {
    let menu = Menu::with_items(
        app,
        &[&MenuItem::with_id(app, "open", "Open", true, None::<&str>)?],
    )?;
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(app.default_window_icon().cloned().unwrap())
        .icon_as_template(true)
        .tooltip("tracon")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            // A left click is the fast path back to the window; the menu is on
            // the right click, where a menu belongs.
            if let TrayIconEvent::Click { button, .. } = event {
                if button == tauri::tray::MouseButton::Left {
                    toggle_window(tray.app_handle());
                }
            }
        })
        .on_menu_event(|app, event| on_menu(app, event.id().as_ref()))
        .build(app)?;
    Ok(())
}

fn tray_icon(app: &AppHandle) -> Option<TrayIcon> {
    app.tray_by_id(TRAY_ID)
}

/// Rebuild the menu from what the node says is waiting.
pub fn refresh(app: &AppHandle, state: &Arc<State>) {
    let Some(tray) = tray_icon(app) else { return };
    let queue = state.queue.lock().unwrap().clone();
    let connected = *state.connected.lock().unwrap();

    let waiting = queue.waiting.len();
    let _ = tray.set_tooltip(Some(&if !connected {
        "tracon · not connected".to_string()
    } else if waiting == 0 {
        "tracon · nothing waiting".to_string()
    } else {
        format!("tracon · {waiting} waiting on you")
    }));

    let Ok(menu) = build_menu(app, &queue, connected) else {
        return;
    };
    let _ = tray.set_menu(Some(menu));
}

fn build_menu(
    app: &AppHandle,
    q: &queue::Queue,
    connected: bool,
) -> tauri::Result<Menu<tauri::Wry>> {
    let menu = Menu::new(app)?;

    if !connected {
        menu.append(&MenuItem::with_id(
            app,
            "noop",
            "Node not reachable",
            false,
            None::<&str>,
        )?)?;
    } else if q.waiting.is_empty() {
        menu.append(&MenuItem::with_id(
            app,
            "noop",
            "Nothing waiting on you",
            false,
            None::<&str>,
        )?)?;
    } else {
        for item in q.waiting.iter().take(MAX_LISTED) {
            menu.append(&MenuItem::with_id(
                app,
                format!("open:{}", item.path),
                truncate(&item.label),
                true,
                None::<&str>,
            )?)?;
        }
        if q.waiting.len() > MAX_LISTED {
            menu.append(&MenuItem::with_id(
                app,
                "open:/",
                format!("… and {} more", q.waiting.len() - MAX_LISTED),
                true,
                None::<&str>,
            )?)?;
        }
    }

    // Killing is destructive and easy to hit by accident, so it lives one
    // level down rather than beside the item it would end.
    if !q.running.is_empty() {
        menu.append(&PredefinedMenuItem::separator(app)?)?;
        let kill = Submenu::new(app, "Kill a session", true)?;
        for s in q.running.iter().take(MAX_LISTED) {
            if let Some(id) = &s.session_id {
                kill.append(&MenuItem::with_id(
                    app,
                    format!("kill:{id}"),
                    truncate(&s.label),
                    true,
                    None::<&str>,
                )?)?;
            }
        }
        menu.append(&kill)?;
    }

    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        "open",
        "Open tracon",
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?)?;
    Ok(menu)
}

/// A menu is not a place for a paragraph.
fn truncate(s: &str) -> String {
    const MAX: usize = 60;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let cut: String = s.chars().take(MAX - 1).collect();
    format!("{cut}…")
}

fn on_menu(app: &AppHandle, id: &str) {
    match id {
        "open" => toggle_window(app),
        "quit" => app.exit(0),
        "noop" => {}
        other => {
            if let Some(path) = other.strip_prefix("open:") {
                open_at(app, path);
            } else if let Some(session) = other.strip_prefix("kill:") {
                let session = session.to_string();
                tauri::async_runtime::spawn(async move {
                    queue::kill(session).await;
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::truncate;

    #[test]
    fn a_long_label_is_cut_to_something_a_menu_can_show() {
        let long = "a".repeat(200);
        let out = truncate(&long);
        assert_eq!(out.chars().count(), 60);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn a_short_label_is_left_alone() {
        assert_eq!(truncate("feat/thing"), "feat/thing");
    }

    #[test]
    fn cutting_does_not_split_a_character() {
        // Counting bytes rather than characters would panic here.
        let s = "é".repeat(120);
        assert_eq!(truncate(&s).chars().count(), 60);
    }
}
