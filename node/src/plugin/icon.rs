//! A plugin page's icon, for the interface's navigation: `[ui] icon` when the
//! manifest names one, otherwise the best `<link rel="icon">` of the `[ui]
//! entry` HTML (an SVG first, then the largest PNG or ICO). Only a file under
//! `ui/` of the build, of at most [`MAX_ICON_BYTES`], SVG, PNG or ICO, is
//! ever taken; anything else is no icon, and the interface shows its generic
//! glyph.
//!
//! The node serves it from the interface's own origin (`GET
//! /api/plugins/{name}/icon`), sandboxed, so the rail can show it with
//! `<img>` without framing the plugin.

use std::path::{Component, Path, PathBuf};

use super::manifest::Ui;

/// The most an icon may weigh.
pub const MAX_ICON_BYTES: u64 = 256 * 1024;

/// A build's icon: its path in the build's tree and its media type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Icon {
    /// Relative to the build's root, always under `ui/`.
    pub path: String,
    pub media: &'static str,
}

/// The media type an icon's file name says, when it is one of the three.
pub fn media_of(path: &str) -> Option<&'static str> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "svg" => Some("image/svg+xml"),
        "png" => Some("image/png"),
        "ico" => Some("image/x-icon"),
        _ => None,
    }
}

/// The icon of the build unpacked at `root` with page `ui`, if it has one
/// the node will serve.
pub fn locate(root: &Path, ui: &Ui) -> Option<Icon> {
    if let Some(named) = &ui.icon {
        // The manifest's choice wins, and is the only candidate: a named
        // icon that is unusable is no icon, not a fallback to the favicon.
        return usable(root, named);
    }
    let html = read_capped(&root.join(&ui.entry), 1024 * 1024)?;
    let html = String::from_utf8_lossy(&html);
    let base = Path::new(&ui.entry).parent().unwrap_or(Path::new(""));
    let mut found: Vec<(u8, u32, String)> = links(&html)
        .into_iter()
        .filter_map(|link| {
            let path = resolve(base, &link.href)?;
            let media = media_of(&path)?;
            // SVG first; then PNG and ICO alike, by their declared size.
            let rank = if media == "image/svg+xml" { 0 } else { 1 };
            Some((rank, link.size, path))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    found
        .into_iter()
        .find_map(|(_, _, path)| usable(root, &path))
}

/// `path` (relative to the build root) as an icon, when it is a plain path
/// under `ui/`, a regular file inside the tree, small enough, and of one of
/// the three types.
fn usable(root: &Path, path: &str) -> Option<Icon> {
    let media = media_of(path)?;
    let relative = Path::new(path);
    if !crate::bundle::plain(relative) || !relative.starts_with("ui") || relative == Path::new("ui")
    {
        return None;
    }
    let ui_root = std::fs::canonicalize(root.join("ui")).ok()?;
    let file = root.join(relative);
    let meta = std::fs::symlink_metadata(&file).ok()?;
    if !meta.is_file() || meta.len() > MAX_ICON_BYTES {
        return None;
    }
    if !std::fs::canonicalize(&file).ok()?.starts_with(ui_root) {
        return None;
    }
    Some(Icon {
        path: path.to_string(),
        media,
    })
}

fn read_capped(path: &Path, max: u64) -> Option<Vec<u8>> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > max {
        return None;
    }
    std::fs::read(path).ok()
}

/// A `href` of the entry page, relative to the entry's directory `base`, as
/// a path from the build root. A URL with a scheme or host, an absolute
/// path, or one that climbs out is refused.
fn resolve(base: &Path, href: &str) -> Option<String> {
    let href = href.trim();
    let href = href.split(['?', '#']).next()?;
    if href.is_empty() || href.starts_with('/') || href.contains(':') || href.contains('\\') {
        return None;
    }
    let decoded = percent_decode(href)?;
    let mut out = PathBuf::new();
    for part in base.join(decoded).components() {
        match part {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    out.to_str().map(str::to_string)
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

struct Link {
    href: String,
    /// The largest dimension its `sizes` declares; `any` (a scalable icon)
    /// counts as largest, none as 0.
    size: u32,
}

/// The `<link>` elements of `html` whose `rel` has the token `icon`.
fn links(html: &str) -> Vec<Link> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(start) = lower[at..].find("<link") {
        let open = at + start + "<link".len();
        // `<linkfoo` is another element.
        if !lower[open..].starts_with(|c: char| c.is_ascii_whitespace() || c == '/' || c == '>') {
            at = open;
            continue;
        }
        let Some(end) = tag_end(&html[open..]) else {
            break;
        };
        let attrs = attributes(&html[open..open + end]);
        at = open + end;
        let get = |name: &str| {
            attrs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        let is_icon = get("rel").is_some_and(|rel| {
            rel.split_ascii_whitespace()
                .any(|t| t.eq_ignore_ascii_case("icon"))
        });
        let Some(href) = get("href").filter(|_| is_icon) else {
            continue;
        };
        let size = get("sizes")
            .map(|sizes| {
                sizes
                    .split_ascii_whitespace()
                    .map(|s| {
                        if s.eq_ignore_ascii_case("any") {
                            return u32::MAX;
                        }
                        let s = s.to_ascii_lowercase();
                        let mut wh = s.split('x').map(|n| n.parse::<u32>().unwrap_or(0));
                        let w = wh.next().unwrap_or(0);
                        let h = wh.next().unwrap_or(0);
                        w.max(h)
                    })
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        out.push(Link {
            href: href.to_string(),
            size,
        });
    }
    out
}

/// Where the tag that starts at `s` closes: the first `>` outside quotes.
fn tag_end(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}

/// A tag's attributes, names lowercased, values unquoted.
fn attributes(s: &str) -> Vec<(String, String)> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let skip_ws = |i: &mut usize| {
        while *i < chars.len() && (chars[*i].is_whitespace() || chars[*i] == '/') {
            *i += 1;
        }
    };
    loop {
        skip_ws(&mut i);
        if i >= chars.len() {
            break;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '=' && chars[i] != '/' {
            i += 1;
        }
        let name: String = chars[start..i]
            .iter()
            .collect::<String>()
            .to_ascii_lowercase();
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < chars.len() && chars[i] == '=' {
            i += 1;
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            if i < chars.len() && (chars[i] == '"' || chars[i] == '\'') {
                let q = chars[i];
                i += 1;
                let vstart = i;
                while i < chars.len() && chars[i] != q {
                    i += 1;
                }
                value = chars[vstart..i].iter().collect();
                i += 1;
            } else {
                let vstart = i;
                while i < chars.len() && !chars[i].is_whitespace() {
                    i += 1;
                }
                value = chars[vstart..i].iter().collect();
            }
        }
        if !name.is_empty() {
            out.push((name, value));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SVG: &[u8] = b"<svg xmlns='http://www.w3.org/2000/svg'/>";

    fn tree(entry_html: &str, files: &[(&str, &[u8])]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("ui")).unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        std::fs::write(dir.path().join("ui/index.html"), entry_html).unwrap();
        for (path, bytes) in files {
            let p = dir.path().join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        }
        dir
    }

    fn ui(icon: Option<&str>) -> Ui {
        Ui {
            entry: "ui/index.html".into(),
            title: "Pageant".into(),
            icon: icon.map(str::to_string),
        }
    }

    #[test]
    fn an_svg_favicon_is_preferred_over_a_larger_png() {
        let t = tree(
            r#"<!doctype html><head>
              <link rel="icon" type="image/png" sizes="512x512" href="./big.png">
              <LINK REL='shortcut icon' href=assets/fav.svg>
              <link rel="stylesheet" href="app.css">
            </head>"#,
            &[
                ("ui/big.png", b"png"),
                ("ui/assets/fav.svg", SVG),
                ("ui/app.css", b""),
            ],
        );
        assert_eq!(
            locate(t.path(), &ui(None)),
            Some(Icon {
                path: "ui/assets/fav.svg".into(),
                media: "image/svg+xml"
            })
        );
    }

    #[test]
    fn without_an_svg_the_largest_png_or_ico_is_taken() {
        let t = tree(
            r#"<link rel="icon" sizes="16x16" href="small.png">
               <link rel="icon" sizes="32x32 64x64" href="fav.ico">
               <link rel="icon" sizes="48x48" href="mid.png">
               <link rel="apple-touch-icon" sizes="180x180" href="touch.png">"#,
            &[
                ("ui/small.png", b"p"),
                ("ui/fav.ico", b"i"),
                ("ui/mid.png", b"p"),
                ("ui/touch.png", b"p"),
            ],
        );
        assert_eq!(
            locate(t.path(), &ui(None)).map(|i| (i.path, i.media)),
            Some(("ui/fav.ico".to_string(), "image/x-icon"))
        );
    }

    #[test]
    fn a_path_outside_ui_or_a_url_elsewhere_is_refused() {
        for href in [
            "../bin/pageant.svg",
            "../tracon-plugin.toml",
            "/fav.svg",
            "https://example.com/fav.svg",
            "//example.com/fav.svg",
            "data:image/svg+xml,<svg/>",
            "%2e%2e/bin/pageant.svg",
        ] {
            let t = tree(
                &format!(r#"<link rel="icon" href="{href}">"#),
                &[("bin/pageant.svg", SVG), ("fav.svg", SVG)],
            );
            assert_eq!(locate(t.path(), &ui(None)), None, "{href}");
        }
    }

    #[test]
    fn a_symlink_out_of_ui_is_refused() {
        let t = tree(r#"<link rel="icon" href="fav.svg">"#, &[("bin/x.svg", SVG)]);
        std::os::unix::fs::symlink(t.path().join("bin/x.svg"), t.path().join("ui/fav.svg"))
            .unwrap();
        assert_eq!(locate(t.path(), &ui(None)), None);
    }

    #[test]
    fn the_manifests_icon_overrides_the_favicon() {
        let t = tree(
            r#"<link rel="icon" href="fav.svg">"#,
            &[("ui/fav.svg", SVG), ("ui/brand/mark.png", b"png")],
        );
        assert_eq!(
            locate(t.path(), &ui(Some("ui/brand/mark.png"))),
            Some(Icon {
                path: "ui/brand/mark.png".into(),
                media: "image/png"
            })
        );
        // A named icon that is not there is no icon, not the favicon.
        assert_eq!(locate(t.path(), &ui(Some("ui/missing.svg"))), None);
    }

    #[test]
    fn size_and_type_are_limited() {
        let big = vec![b' '; MAX_ICON_BYTES as usize + 1];
        let t = tree(
            r#"<link rel="icon" href="huge.svg"><link rel="icon" href="fav.gif"><link rel="icon" href="ok.png">"#,
            &[
                ("ui/huge.svg", &big),
                ("ui/fav.gif", b"gif"),
                ("ui/ok.png", b"png"),
            ],
        );
        assert_eq!(
            locate(t.path(), &ui(None)).map(|i| i.path),
            Some("ui/ok.png".to_string())
        );
        let exact = vec![b' '; MAX_ICON_BYTES as usize];
        let t = tree(
            r#"<link rel="icon" href="fav.svg">"#,
            &[("ui/fav.svg", &exact)],
        );
        assert!(locate(t.path(), &ui(None)).is_some());
        let t = tree(
            r#"<link rel="icon" href="fav.webp">"#,
            &[("ui/fav.webp", b"w")],
        );
        assert_eq!(locate(t.path(), &ui(None)), None);
    }

    #[test]
    fn no_link_is_no_icon() {
        let t = tree(
            "<!doctype html><title>x</title>",
            &[("ui/favicon.ico", b"i")],
        );
        assert_eq!(locate(t.path(), &ui(None)), None);
    }
}
