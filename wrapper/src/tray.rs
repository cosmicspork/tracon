//! The tray: what is waiting, and a way to stop a session.
//!
//! It does not stream output and does not show a diff. Anything that needs
//! reading opens the window at the right route.

use std::sync::Arc;

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::{TrayIcon, TrayIconBuilder},
    AppHandle, Manager,
};

use crate::{open_at, queue, updater, State};

const TRAY_ID: &str = "tracon";
/// The menu bar glyph, rendered from `icons/tray.svg`. A template icon is
/// drawn from its alpha channel alone, so this is black on transparent —
/// handing macOS the full-colour app icon paints a filled square instead.
const TRAY_ICON: &[u8] = include_bytes!("../icons/tray-44.png");
/// A Linux panel shows the template as the black it is, which vanishes on a
/// dark panel; this copy is swapped in when the desktop prefers dark.
#[cfg(target_os = "linux")]
const TRAY_ICON_WHITE: &[u8] = include_bytes!("../icons/tray-44-white.png");
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
    #[cfg(target_os = "linux")]
    follow_color_scheme(app);
    Ok(())
}

/// Keep the Linux glyph white on a dark desktop and black on a light one,
/// following the desktop portal's colour scheme as it changes.
///
/// A themed icon name would let the panel recolour the glyph itself, but
/// Plasma does not find a name under an IconThemePath made at runtime and
/// shows the app icon instead. Without a portal the glyph stays black.
#[cfg(target_os = "linux")]
fn follow_color_scheme(app: &AppHandle) {
    let app = app.clone();
    // Its own thread: a portal that never answers must not hold up the tray.
    std::thread::spawn(move || {
        if let Err(e) = watch_color_scheme(&app) {
            eprintln!("tracon: tray icon will not follow the colour scheme: {e}");
        }
    });
}

#[cfg(target_os = "linux")]
fn watch_color_scheme(app: &AppHandle) -> zbus::Result<()> {
    use zbus::zvariant::OwnedValue;

    const NAMESPACE: &str = "org.freedesktop.appearance";
    const KEY: &str = "color-scheme";

    let conn = zbus::blocking::Connection::session()?;
    let portal = zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Settings",
    )?;
    // Subscribed before the first read, so a switch between the two is not
    // missed.
    let changes = portal.receive_signal("SettingChanged")?;
    let scheme: OwnedValue = portal.call("ReadOne", &(NAMESPACE, KEY))?;
    set_glyph(app, prefers_dark(&scheme));
    for change in changes {
        let (namespace, key, scheme): (String, String, OwnedValue) = change.body().deserialize()?;
        if namespace == NAMESPACE && key == KEY {
            set_glyph(app, prefers_dark(&scheme));
        }
    }
    Ok(())
}

/// The portal's scheme is 1 for dark, 2 for light, 0 for no preference.
#[cfg(target_os = "linux")]
fn prefers_dark(scheme: &zbus::zvariant::OwnedValue) -> bool {
    matches!(u32::try_from(scheme), Ok(1))
}

#[cfg(target_os = "linux")]
fn set_glyph(app: &AppHandle, dark: bool) {
    let bytes = if dark { TRAY_ICON_WHITE } else { TRAY_ICON };
    let (Some(tray), Ok(image)) = (tray_icon(app), tauri::image::Image::from_bytes(bytes)) else {
        return;
    };
    let _ = tray.set_icon(Some(image));
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

    let pending_update = state.node_update.lock().unwrap().clone();
    let Ok(menu) = build_menu(
        app,
        &queue,
        connected,
        failed.as_deref(),
        pending_update.as_deref(),
    ) else {
        return;
    };
    let _ = tray.set_menu(Some(menu));
}

fn build_menu(
    app: &AppHandle,
    q: &queue::Queue,
    connected: bool,
    failed: Option<&str>,
    pending_update: Option<&str>,
) -> tauri::Result<Menu<tauri::Wry>> {
    let menu = Menu::new(app)?;

    // A node restart the app is holding back until sessions end, said where
    // the operator will look when the tray icon changes.
    if let Some(line) = pending_update {
        menu.append(&MenuItem::with_id(
            app,
            "noop-update",
            truncate(line),
            false,
            None::<&str>,
        )?)?;
    }

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

    menu.append(&MenuItem::with_id(
        app,
        "open:/settings#general",
        "Settings…",
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
        "open" => crate::show_window(app),
        "quit" => crate::quit(app),
        "noop" => {}
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

    #[cfg(target_os = "linux")]
    #[test]
    fn both_glyphs_load_at_the_same_size() {
        use tauri::image::Image;
        let black = Image::from_bytes(super::TRAY_ICON).unwrap();
        let white = Image::from_bytes(super::TRAY_ICON_WHITE).unwrap();
        assert_eq!(
            (black.width(), black.height()),
            (white.width(), white.height())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn only_a_dark_preference_turns_the_glyph_white() {
        use zbus::zvariant::{OwnedValue, Value};
        let scheme = |v: u32| OwnedValue::try_from(Value::from(v)).unwrap();
        assert!(super::prefers_dark(&scheme(1)));
        assert!(!super::prefers_dark(&scheme(2)));
        assert!(!super::prefers_dark(&scheme(0)));
        assert!(!super::prefers_dark(&OwnedValue::from(
            zbus::zvariant::Str::from("dark")
        )));
    }

    #[test]
    fn cutting_does_not_split_a_character() {
        // Counting bytes rather than characters would panic here.
        let s = "é".repeat(120);
        assert_eq!(truncate(&s).chars().count(), 60);
    }
}
