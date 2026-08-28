//! `snapshot-<ms>.tracon-snap`:
//!
//! ```text
//! "TRSNAP\0" u8 version(1) u32 len ‖ WrappedKey
//! repeated: u32 len ‖ Sealed(chunk_i, aad = "tracon/snapshot\0" ‖ u64_be(i))
//! u32 0                                  terminator, so truncation is detected
//! ```
//!
//! The plaintext is an archive of `u32 len ‖ path ‖ u64 len ‖ bytes` entries:
//! `hub.db` (an online backup, so a live WAL is folded in), `hub-identity.seed`,
//! `members/*.json`, and `frames/**`. No tar: three fields per file is the
//! whole format, and it has no dependency to rot.

use std::io::{Read, Write};
use std::path::{Component, Path};

use proto::envelope::{wrap_key, DataKey, Sealed, WrappedKey};
use proto::keys::Identity;

const MAGIC: &[u8; 7] = b"TRSNAP\0";
const VERSION: u8 = 1;
const CHUNK: usize = 1 << 20;
const AAD: &[u8] = b"tracon/snapshot\0";

fn aad(i: u64) -> Vec<u8> {
    let mut v = AAD.to_vec();
    v.extend_from_slice(&i.to_be_bytes());
    v
}

fn bad(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.to_string())
}

/// Archive the data directory and seal it to `recipient`.
pub fn pack(data_dir: &Path, recipient: &x25519_dalek::PublicKey) -> std::io::Result<Vec<u8>> {
    let archive = archive(data_dir)?;
    let key = DataKey::generate();
    let wrapped: WrappedKey = wrap_key(recipient, &key);
    let mut out = Vec::with_capacity(archive.len() + 4096);
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    let w = wrapped.to_bytes();
    out.extend_from_slice(&(w.len() as u32).to_be_bytes());
    out.extend_from_slice(&w);
    for (i, chunk) in archive.chunks(CHUNK).enumerate() {
        let sealed = key.seal(chunk, &aad(i as u64)).to_bytes();
        out.extend_from_slice(&(sealed.len() as u32).to_be_bytes());
        out.extend_from_slice(&sealed);
    }
    out.extend_from_slice(&0u32.to_be_bytes());
    Ok(out)
}

/// Open a snapshot with the restore identity and write its files under
/// `into`. Returns the paths written.
pub fn unpack(bytes: &[u8], identity: &Identity, into: &Path) -> std::io::Result<Vec<String>> {
    let mut r = bytes;
    let mut magic = [0u8; 7];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(bad("not a tracon snapshot"));
    }
    let mut v = [0u8; 1];
    r.read_exact(&mut v)?;
    if v[0] != VERSION {
        return Err(bad("unknown snapshot version"));
    }
    let wlen = read_u32(&mut r)? as usize;
    let mut w = vec![0u8; wlen];
    r.read_exact(&mut w)?;
    let wrapped = WrappedKey::from_bytes(&w).map_err(|_| bad("malformed wrapped key"))?;
    let key = identity
        .unwrap_key(&wrapped)
        .map_err(|_| bad("this restore key does not open the snapshot"))?;
    let mut plain = Vec::new();
    let mut i = 0u64;
    loop {
        let len = read_u32(&mut r)? as usize;
        if len == 0 {
            break;
        }
        let mut c = vec![0u8; len];
        r.read_exact(&mut c)?;
        let sealed = Sealed::from_bytes(&c).map_err(|_| bad("malformed chunk"))?;
        let chunk = key
            .open(&sealed, &aad(i))
            .map_err(|_| bad("a chunk failed to authenticate"))?;
        plain.extend_from_slice(&chunk);
        i += 1;
    }
    unarchive(&plain, into)
}

fn read_u32(r: &mut &[u8]) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn read_u64(r: &mut &[u8]) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_be_bytes(b))
}

/// Everything under the data directory, as one archive. `hub.db` is copied
/// through SQLite's online backup so a live WAL is folded into one file.
fn archive(data_dir: &Path) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let db = data_dir.join(crate::replica::DB_FILE);
    if db.exists() {
        let tmp = tempfile::NamedTempFile::new_in(data_dir)?;
        backup_db(&db, tmp.path()).map_err(|e| bad(&format!("backup hub.db: {e}")))?;
        entry(
            &mut out,
            crate::replica::DB_FILE,
            &std::fs::read(tmp.path())?,
        );
    }
    let seed = data_dir.join(crate::identity::SEED_FILE);
    if seed.exists() {
        entry(&mut out, crate::identity::SEED_FILE, &std::fs::read(&seed)?);
    }
    for dir in ["members", "frames"] {
        let root = data_dir.join(dir);
        if root.is_dir() {
            walk(&root, &root, dir, &mut out)?;
        }
    }
    Ok(out)
}

fn backup_db(src: &Path, dst: &Path) -> rusqlite::Result<()> {
    let src =
        rusqlite::Connection::open_with_flags(src, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut dst = rusqlite::Connection::open(dst)?;
    let backup = rusqlite::backup::Backup::new(&src, &mut dst)?;
    backup.run_to_completion(64, std::time::Duration::from_millis(5), None)
}

fn walk(root: &Path, dir: &Path, prefix: &str, out: &mut Vec<u8>) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let rel = p
            .strip_prefix(root)
            .unwrap_or(&p)
            .to_string_lossy()
            .replace('\\', "/");
        if p.is_dir() {
            walk(root, &p, prefix, out)?;
        } else if p.is_file() {
            entry(out, &format!("{prefix}/{rel}"), &std::fs::read(&p)?);
        }
    }
    Ok(())
}

fn entry(out: &mut Vec<u8>, path: &str, bytes: &[u8]) {
    out.extend_from_slice(&(path.len() as u32).to_be_bytes());
    out.extend_from_slice(path.as_bytes());
    out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn unarchive(mut r: &[u8], into: &Path) -> std::io::Result<Vec<String>> {
    let mut written = Vec::new();
    while !r.is_empty() {
        let plen = read_u32(&mut r)? as usize;
        let mut p = vec![0u8; plen];
        r.read_exact(&mut p)?;
        let path = String::from_utf8(p).map_err(|_| bad("path is not utf-8"))?;
        let blen = read_u64(&mut r)? as usize;
        let mut b = vec![0u8; blen];
        r.read_exact(&mut b)?;
        // A snapshot names only relative paths inside the data directory.
        let rel = Path::new(&path);
        if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
            return Err(bad(&format!("refusing path {path:?}")));
        }
        let dest = into.join(rel);
        if let Some(d) = dest.parent() {
            std::fs::create_dir_all(d)?;
        }
        let mut f = std::fs::File::create(&dest)?;
        f.write_all(&b)?;
        #[cfg(unix)]
        if path == crate::identity::SEED_FILE {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600));
        }
        written.push(path);
    }
    Ok(written)
}
