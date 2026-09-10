//! Bringing a docs directory in: flat `<kind>-<slug>.md` files, no
//! frontmatter, and those under `archive/` as archived documents. The
//! filename is the slug; the kind is its prefix; the title is the first
//! heading.

use std::path::Path;

use crate::mcp::docs::{kind_of, title_of, valid_slug};

#[derive(Debug, Clone, PartialEq)]
pub struct Imported {
    pub slug: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub archived: bool,
}

/// Every importable markdown file directly in `dir`, then those in
/// `dir/archive` as archived, sorted by slug. A slug in both is taken from
/// `dir`: the archive is where a document went, not a second copy of it.
/// Returns what was skipped alongside, so the operator sees what did not
/// come over.
pub fn read_dir(dir: &Path) -> std::io::Result<(Vec<Imported>, Vec<String>)> {
    let mut docs = Vec::new();
    let mut skipped = Vec::new();
    read_flat(dir, "", false, &mut docs, &mut skipped)?;
    let archive = dir.join("archive");
    if archive.is_dir() {
        let mut archived = Vec::new();
        read_flat(&archive, "archive/", true, &mut archived, &mut skipped)?;
        for d in archived {
            if docs.iter().any(|x| x.slug == d.slug) {
                skipped.push(format!("archive/{}.md: also outside the archive", d.slug));
            } else {
                docs.push(d);
            }
        }
    }
    docs.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok((docs, skipped))
}

fn read_flat(
    dir: &Path,
    shown_as: &str,
    archived: bool,
    docs: &mut Vec<Imported>,
    skipped: &mut Vec<String>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            continue;
        }
        let Some(stem) = name.strip_suffix(".md") else {
            skipped.push(format!("{shown_as}{name}: not markdown"));
            continue;
        };
        let slug = stem.to_ascii_lowercase();
        if !valid_slug(&slug) {
            skipped.push(format!("{shown_as}{name}: not a usable slug"));
            continue;
        }
        let body = std::fs::read_to_string(&path)?;
        docs.push(Imported {
            kind: kind_of(&slug).to_string(),
            title: title_of(&slug, &body),
            slug,
            body,
            archived,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_docs_directory_imports_by_filename_and_its_archive_as_archived() {
        let dir = std::env::temp_dir().join(format!("tracon-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("archive")).unwrap();
        std::fs::write(dir.join("ref-deploy.md"), "# Deploying\n\nflux").unwrap();
        std::fs::write(dir.join("note-Odd.md"), "no heading").unwrap();
        std::fs::write(dir.join("archive/plan-old.md"), "# old").unwrap();
        std::fs::write(dir.join("archive/ref-deploy.md"), "# stale copy").unwrap();
        std::fs::write(dir.join("todo.txt"), "x").unwrap();
        std::fs::write(dir.join("bad slug.md"), "x").unwrap();
        let (docs, skipped) = read_dir(&dir).unwrap();
        let slugs: Vec<_> = docs.iter().map(|d| d.slug.as_str()).collect();
        assert_eq!(slugs, ["note-odd", "plan-old", "ref-deploy"]);
        assert_eq!(docs[0].title, "odd");
        assert!(docs[1].archived);
        assert_eq!(docs[2].kind, "ref");
        assert_eq!(docs[2].title, "Deploying");
        assert!(!docs[2].archived, "the live copy wins over the archive's");
        assert_eq!(skipped.len(), 3, "{skipped:?}");
        assert!(
            skipped.iter().any(|s| s.contains("also outside")),
            "{skipped:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
