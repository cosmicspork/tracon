//! Where snapshots go: three verbs behind a trait, a directory for tests and
//! laptops, S3-compatible storage (DigitalOcean Spaces) for the cluster.

use std::path::{Path, PathBuf};

pub trait ObjectStore: Send + Sync {
    fn put(&self, key: &str, bytes: &[u8]) -> std::io::Result<()>;
    fn get(&self, key: &str) -> std::io::Result<Vec<u8>>;
    fn list(&self, prefix: &str) -> std::io::Result<Vec<String>>;
    fn delete(&self, key: &str) -> std::io::Result<()>;
}

/// A directory as a bucket. Keys are relative paths.
pub struct FsObjects {
    root: PathBuf,
}

impl FsObjects {
    pub fn new(root: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(root)?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }
}

impl ObjectStore for FsObjects {
    fn put(&self, key: &str, bytes: &[u8]) -> std::io::Result<()> {
        let p = self.root.join(key);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(tmp, p)
    }
    fn get(&self, key: &str) -> std::io::Result<Vec<u8>> {
        std::fs::read(self.root.join(key))
    }
    fn list(&self, prefix: &str) -> std::io::Result<Vec<String>> {
        let mut out = Vec::new();
        walk(&self.root, &self.root, &mut out)?;
        out.retain(|k| k.starts_with(prefix));
        out.sort();
        Ok(out)
    }
    fn delete(&self, key: &str) -> std::io::Result<()> {
        std::fs::remove_file(self.root.join(key))
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if p.is_dir() {
            walk(root, &p, out)?;
        } else if let Ok(rel) = p.strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}
