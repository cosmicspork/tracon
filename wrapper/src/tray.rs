//! The tray: what is waiting, and a way to stop a session.
//!
//! It does not stream output and does not show a diff. Anything that needs
//! reading opens the window at the right route.

use std::sync::Arc;

use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::{TrayIcon, TrayIconBuilder},
    AppHandle, Manager,
};

use crate::{open_at, prefs, queue, updater, State};

const TRAY_ID: &str = "tracon";
/// The menu bar glyph, rendered from `icons/tray.svg`. A template icon is
/// drawn from its alpha channel alone, so this is black on transparent —
/// handing macOS the full-colour app icon paints a filled square instead.
const TRAY_ICON: &[u8] = include_bytes!("../icons/tray-44.png");
/// Beyond this the menu is a wall of text; the window is better for that.
const MAX_LISTED: usize = 8;

pub fn install(app: &AppHandle) -> tauri::Result<()> {
    let menu = Menu::with_items(
        app,
        &[&MenuItem::with_id(app, "open", "Open", true, None::<&str>)?],
    )?;
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(tauri::image::Image::from_bytes(TRAY_ICON)?)
        .icon_as_template(true)
        .tooltip("tracon")
        .menu(&menu)
        // Either click opens the menu, and nothing else. A left click that
        // toggled the window fought the app activation the click itself
        // causes, which is what the flashing was; and a menu bar icon whose
        // two buttons do different things is a menu bar icon you have to
        // remember. "Open tracon" is in the menu.
        .show_menu_on_left_click(true)
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

    let failed = state.node_error.lock().unwrap().clone();
    let waiting = queue.waiting.len();
    let _ = tray.set_tooltip(Some(&if let Some(why) = &failed {
        format!("tracon · {why}")
    } else if !connected {
        "tracon · not connected".to_string()
    } else if waiting == 0 {
        "tracon · nothing waiting".to_string()
    } else {
        format!("tracon · {waiting} waiting on you")
    }));

    let Ok(menu) = build_menu(app, &queue, connected, failed.as_deref()) else {
        return;
    };
    let _ = tray.set_menu(Some(menu));
}

fn build_menu(
    app: &AppHandle,
    q: &queue::Queue,
    connected: bool,
    failed: Option<&str>,
) -> tauri::Result<Menu<tauri::Wry>> {
    let menu = Menu::new(app)?;

    // A node this app started and could not keep running is a different
    // situation from one it cannot reach, and saying which saves a hunt
    // through logs for a node that is simply not there.
    if let Some(why) = failed {
        menu.append(&MenuItem::with_id(
            app,
            "noop",
            truncate(why),
            false,
            None::<&str>,
        )?)?;
    } else if !connected {
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

    if let Some(status) = app
        .try_state::<Arc<updater::Updater>>()
        .map(|updater| updater.status())
    {
        match (status.state, status.available_version.as_deref()) {
            ("available", Some(version)) => menu.append(&MenuItem::with_id(
                app,
                "update:install",
                format!("Update to v{version} and restart"),
                true,
                None::<&str>,
            )?)?,
            ("downloading", Some(version)) => menu.append(&MenuItem::with_id(
                app,
                "update:install",
                format!("Installing v{version}…"),
                false,
                None::<&str>,
            )?)?,
            _ => {}
        }
    }

    // Three checkboxes, in the tray, because this is the whole of the app's
    // own interface and they do not earn a window.
    let p = app.state::<Arc<prefs::Store>>().get();
    let settings = Submenu::new(app, "Preferences", true)?;
    settings.append(&CheckMenuItem::with_id(
        app,
        "pref:launch",
        "Open the window at launch",
        true,
        p.open_window_at_launch,
        None::<&str>,
    )?)?;
    settings.append(&CheckMenuItem::with_id(
        app,
        "pref:cmdq",
        "⌘Q quits",
        true,
        p.cmd_q_quits,
        None::<&str>,
    )?)?;
    settings.append(&CheckMenuItem::with_id(
        app,
        "pref:dock",
        "Hide the dock icon while closed",
        true,
        p.hide_dock_when_closed,
        None::<&str>,
    )?)?;
    menu.append(&settings)?;

    menu.append(&MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?)?;
    Ok(menu)
}

/// Flip one preference and rebuild the menu, so the tick matches the file.
fn set_pref(app: &AppHandle, f: impl FnOnce(&mut prefs::Prefs)) {
    let p = app.state::<Arc<prefs::Store>>().update(f);
    // The dock follows immediately: a preference that waits for a restart
    // reads as one that did nothing.
    let visible = app
        .get_webview_window("main")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false);
    crate::set_dock_policy(app, visible, p.hide_dock_when_closed);
    if let Some(st) = app.try_state::<Arc<State>>() {
        refresh(app, &st);
    }
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
        "open" => crate::show_window(app),
        "quit" => crate::quit(app),
        "noop" => {}
        "pref:launch" => set_pref(app, |p| p.open_window_at_launch = !p.open_window_at_launch),
        "pref:cmdq" => set_pref(app, |p| p.cmd_q_quits = !p.cmd_q_quits),
        "pref:dock" => set_pref(app, |p| p.hide_dock_when_closed = !p.hide_dock_when_closed),
        "update:install" => {
            let updater = app.state::<Arc<updater::Updater>>().inner().clone();
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                updater.install(&handle).await;
            });
        }
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
