//! Bringing a notebook directory in: flat `<kind>-<slug>.md` files, no
//! frontmatter, `archive/` skipped. The filename is the slug; the kind is its
//! prefix; the title is the first heading.

use std::path::Path;

use crate::mcp::docs::{kind_of, title_of, valid_slug};

#[derive(Debug, Clone, PartialEq)]
pub struct Imported {
    pub slug: String,
    pub kind: String,
    pub title: String,
    pub body: String,
}

/// Every importable markdown file directly in `dir`, sorted by slug. Returns
/// what was skipped alongside, so the operator sees what did not come over.
pub fn read_dir(dir: &Path) -> std::io::Result<(Vec<Imported>, Vec<String>)> {
    let mut docs = Vec::new();
    let mut skipped = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            continue;
        }
        let Some(stem) = name.strip_suffix(".md") else {
            skipped.push(format!("{name}: not markdown"));
            continue;
        };
        let slug = stem.to_ascii_lowercase();
        if !valid_slug(&slug) {
            skipped.push(format!("{name}: not a usable slug"));
            continue;
        }
        let body = std::fs::read_to_string(&path)?;
        docs.push(Imported {
            kind: kind_of(&slug).to_string(),
            title: title_of(&slug, &body),
            slug,
            body,
        });
    }
    docs.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok((docs, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notebook_directory_imports_by_filename() {
        let dir = std::env::temp_dir().join(format!("tracon-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("archive")).unwrap();
        std::fs::write(dir.join("ref-deploy.md"), "# Deploying\n\nflux").unwrap();
        std::fs::write(dir.join("note-Odd.md"), "no heading").unwrap();
        std::fs::write(dir.join("archive/plan-old.md"), "# old").unwrap();
        std::fs::write(dir.join("todo.txt"), "x").unwrap();
        std::fs::write(dir.join("bad slug.md"), "x").unwrap();
        let (docs, skipped) = read_dir(&dir).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].slug, "note-odd");
        assert_eq!(docs[0].title, "odd");
        assert_eq!(docs[1].slug, "ref-deploy");
        assert_eq!(docs[1].kind, "ref");
        assert_eq!(docs[1].title, "Deploying");
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
