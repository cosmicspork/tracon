//! The hub's stores. Frames are opaque strings (the serialized envelope the
//! sender signed) appended per channel with a monotonic sequence. Members are
//! routing metadata: which keys may connect and which channels they may read.
//! Enrollment slots are short-lived and in memory only.
//!
//! Each durable store has a memory implementation (tests, ephemeral runs) and
//! a filesystem one (the PVC). Writes are staged and renamed so a crash never
//! leaves a partial file.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

// ---------------------------------------------------------------- frames

/// One page of a channel's frames.
#[derive(Debug, Default)]
pub struct Page {
    /// `(seq, envelope json)`, ascending.
    pub frames: Vec<(u64, String)>,
    /// Lowest seq still retained. 0 when the channel never had a frame;
    /// `latest + 1` when it had frames and every one has been pruned, so a
    /// cursor behind it is still detected as stale.
    pub oldest: u64,
    /// Highest seq assigned so far; 0 when empty.
    pub latest: u64,
}

pub trait FrameStore: Send + Sync {
    /// Append and return the assigned seq (1-based, per channel).
    fn append(&self, channel: &str, envelope: &str, now_ms: i64) -> io::Result<u64>;
    /// Frames with `seq > after`, at most `limit`.
    fn read(&self, channel: &str, after: u64, limit: usize) -> io::Result<Page>;
    /// Drop frames older than `older_than_ms` and, oldest first, until the
    /// channel is within `max_bytes`. Returns how many were dropped.
    fn prune(&self, channel: &str, older_than_ms: i64, max_bytes: u64) -> io::Result<usize>;
    fn channels(&self) -> io::Result<Vec<String>>;
}

struct MemChannel {
    frames: VecDeque<(u64, String, i64)>,
    next: u64,
}

#[derive(Default)]
pub struct MemoryFrames {
    channels: Mutex<HashMap<String, MemChannel>>,
}

impl MemoryFrames {
    pub fn new() -> Self {
        Self::default()
    }
}

impl FrameStore for MemoryFrames {
    fn append(&self, channel: &str, envelope: &str, now_ms: i64) -> io::Result<u64> {
        let mut m = self.channels.lock().unwrap();
        let c = m.entry(channel.to_string()).or_insert_with(|| MemChannel {
            frames: VecDeque::new(),
            next: 1,
        });
        let seq = c.next;
        c.next += 1;
        c.frames.push_back((seq, envelope.to_string(), now_ms));
        Ok(seq)
    }

    fn read(&self, channel: &str, after: u64, limit: usize) -> io::Result<Page> {
        let m = self.channels.lock().unwrap();
        let Some(c) = m.get(channel) else {
            return Ok(Page::default());
        };
        Ok(Page {
            frames: c
                .frames
                .iter()
                .filter(|(s, _, _)| *s > after)
                .take(limit)
                .map(|(s, e, _)| (*s, e.clone()))
                .collect(),
            oldest: c
                .frames
                .front()
                .map(|f| f.0)
                .unwrap_or(c.next - 1 + u64::from(c.next > 1)),
            latest: c.next - 1,
        })
    }

    fn prune(&self, channel: &str, older_than_ms: i64, max_bytes: u64) -> io::Result<usize> {
        let mut m = self.channels.lock().unwrap();
        let Some(c) = m.get_mut(channel) else {
            return Ok(0);
        };
        let mut dropped = 0;
        while c.frames.front().is_some_and(|f| f.2 < older_than_ms) {
            c.frames.pop_front();
            dropped += 1;
        }
        let mut total: u64 = c.frames.iter().map(|f| f.1.len() as u64).sum();
        while total > max_bytes {
            let Some(f) = c.frames.pop_front() else { break };
            total -= f.1.len() as u64;
            dropped += 1;
        }
        Ok(dropped)
    }

    fn channels(&self) -> io::Result<Vec<String>> {
        Ok(self.channels.lock().unwrap().keys().cloned().collect())
    }
}

/// `{root}/frames/{channel}/{seq:020}` holds the envelope; `.seq` caches the
/// next sequence and is rebuilt from a directory scan if absent. One lock for
/// the store: a single operator's hub does not need finer.
pub struct FsFrames {
    root: PathBuf,
    lock: Mutex<()>,
}

impl FsFrames {
    pub fn new(data_dir: &Path) -> io::Result<Self> {
        let root = data_dir.join("frames");
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            lock: Mutex::new(()),
        })
    }

    fn dir(&self, channel: &str) -> PathBuf {
        self.root.join(channel)
    }

    fn scan(&self, channel: &str) -> io::Result<BTreeMap<u64, PathBuf>> {
        let mut out = BTreeMap::new();
        let dir = self.dir(channel);
        if !dir.exists() {
            return Ok(out);
        }
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Ok(seq) = name.parse::<u64>() {
                out.insert(seq, entry.path());
            }
        }
        Ok(out)
    }

    fn next_seq(&self, channel: &str) -> io::Result<u64> {
        let seq_file = self.dir(channel).join(".seq");
        if let Ok(s) = fs::read_to_string(&seq_file) {
            if let Ok(n) = s.trim().parse::<u64>() {
                return Ok(n);
            }
        }
        Ok(self
            .scan(channel)?
            .keys()
            .next_back()
            .map(|s| s + 1)
            .unwrap_or(1))
    }
}

fn atomic_write(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let mut tmp = NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.flush()?;
    tmp.persist(dir.join(name)).map_err(|e| e.error)?;
    Ok(())
}

impl FrameStore for FsFrames {
    fn append(&self, channel: &str, envelope: &str, _now_ms: i64) -> io::Result<u64> {
        let _g = self.lock.lock().unwrap();
        let seq = self.next_seq(channel)?;
        let dir = self.dir(channel);
        atomic_write(&dir, &format!("{seq:020}"), envelope.as_bytes())?;
        atomic_write(&dir, ".seq", (seq + 1).to_string().as_bytes())?;
        Ok(seq)
    }

    fn read(&self, channel: &str, after: u64, limit: usize) -> io::Result<Page> {
        let _g = self.lock.lock().unwrap();
        let files = self.scan(channel)?;
        let latest = self.next_seq(channel)?.saturating_sub(1);
        let mut frames = Vec::new();
        for (seq, path) in files.range((after + 1)..) {
            if frames.len() >= limit {
                break;
            }
            frames.push((*seq, fs::read_to_string(path)?));
        }
        Ok(Page {
            frames,
            oldest: files
                .keys()
                .next()
                .copied()
                .unwrap_or(latest + u64::from(latest > 0)),
            latest,
        })
    }

    fn prune(&self, channel: &str, older_than_ms: i64, max_bytes: u64) -> io::Result<usize> {
        let _g = self.lock.lock().unwrap();
        let files = self.scan(channel)?;
        let mut sized: Vec<(u64, PathBuf, u64, i64)> = Vec::with_capacity(files.len());
        for (seq, path) in files {
            let md = fs::metadata(&path)?;
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            sized.push((seq, path, md.len(), mtime));
        }
        let mut total: u64 = sized.iter().map(|f| f.2).sum();
        let mut dropped = 0;
        for (_, path, len, mtime) in sized {
            if mtime < older_than_ms || total > max_bytes {
                fs::remove_file(&path)?;
                total -= len;
                dropped += 1;
            } else {
                break;
            }
        }
        Ok(dropped)
    }

    fn channels(&self) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if let Some(n) = entry.file_name().to_str() {
                    out.push(n.to_string());
                }
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------- members

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Member {
    pub node_id: String,
    pub x25519_pub: String,
    pub name: String,
    pub channels: Vec<String>,
    pub admitted_ms: i64,
    pub admitted_by: String,
}

pub trait MemberStore: Send + Sync {
    fn get(&self, node_id: &str) -> io::Result<Option<Member>>;
    fn put(&self, member: &Member) -> io::Result<()>;
    fn remove(&self, node_id: &str) -> io::Result<bool>;
    fn list(&self) -> io::Result<Vec<Member>>;
    fn members_of(&self, channel: &str) -> io::Result<Vec<Member>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|m| m.channels.iter().any(|c| c == channel))
            .collect())
    }
}

#[derive(Default)]
pub struct MemoryMembers {
    members: Mutex<BTreeMap<String, Member>>,
}

impl MemoryMembers {
    pub fn new() -> Self {
        Self::default()
    }
}

impl MemberStore for MemoryMembers {
    fn get(&self, node_id: &str) -> io::Result<Option<Member>> {
        Ok(self.members.lock().unwrap().get(node_id).cloned())
    }
    fn put(&self, member: &Member) -> io::Result<()> {
        self.members
            .lock()
            .unwrap()
            .insert(member.node_id.clone(), member.clone());
        Ok(())
    }
    fn remove(&self, node_id: &str) -> io::Result<bool> {
        Ok(self.members.lock().unwrap().remove(node_id).is_some())
    }
    fn list(&self) -> io::Result<Vec<Member>> {
        Ok(self.members.lock().unwrap().values().cloned().collect())
    }
}

/// `{root}/members/{node_id}.json`. `node_id` is validated hex at the route.
pub struct FsMembers {
    root: PathBuf,
}

impl FsMembers {
    pub fn new(data_dir: &Path) -> io::Result<Self> {
        let root = data_dir.join("members");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }
    fn path(&self, node_id: &str) -> PathBuf {
        self.root.join(format!("{node_id}.json"))
    }
}

impl MemberStore for FsMembers {
    fn get(&self, node_id: &str) -> io::Result<Option<Member>> {
        match fs::read(self.path(node_id)) {
            Ok(b) => Ok(Some(serde_json::from_slice(&b).map_err(io::Error::other)?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn put(&self, member: &Member) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(member).map_err(io::Error::other)?;
        atomic_write(&self.root, &format!("{}.json", member.node_id), &bytes)
    }
    fn remove(&self, node_id: &str) -> io::Result<bool> {
        match fs::remove_file(self.path(node_id)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
    fn list(&self) -> io::Result<Vec<Member>> {
        let mut out: Vec<Member> = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.path().extension().is_some_and(|e| e == "json") {
                let b = fs::read(entry.path())?;
                out.push(serde_json::from_slice(&b).map_err(io::Error::other)?);
            }
        }
        out.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        Ok(out)
    }
}

// ---------------------------------------------------------------- enrollment

struct Slot {
    inviter: [u8; 32],
    expires_at: u64,
    filled: Option<String>,
}

pub enum Fill {
    Filled,
    Unknown,
    AlreadyFilled,
}

pub enum Take {
    NotYet,
    Ready(String),
    Unknown,
    NotYours,
}

/// Short-lived enrollment slots. Memory only: a slot outlives nothing worth
/// persisting, and a hub restart during a ten-minute enrollment is answered by
/// running `tracon mesh invite` again.
#[derive(Default)]
pub struct EnrollSlots {
    slots: Mutex<HashMap<String, Slot>>,
}

impl EnrollSlots {
    pub fn new() -> Self {
        Self::default()
    }

    fn sweep(slots: &mut HashMap<String, Slot>, now: u64) {
        slots.retain(|_, s| s.expires_at > now);
    }

    /// `false` if a live slot already exists under `code`.
    pub fn open(&self, code: &str, inviter: [u8; 32], expires_at: u64, now: u64) -> bool {
        let mut slots = self.slots.lock().unwrap();
        Self::sweep(&mut slots, now);
        if slots.contains_key(code) {
            return false;
        }
        slots.insert(
            code.to_string(),
            Slot {
                inviter,
                expires_at,
                filled: None,
            },
        );
        true
    }

    pub fn fill(&self, code: &str, body: String, now: u64) -> Fill {
        let mut slots = self.slots.lock().unwrap();
        Self::sweep(&mut slots, now);
        match slots.get_mut(code) {
            None => Fill::Unknown,
            Some(s) if s.filled.is_some() => Fill::AlreadyFilled,
            Some(s) => {
                s.filled = Some(body);
                Fill::Filled
            }
        }
    }

    /// Fetch-and-delete once filled.
    pub fn take(&self, code: &str, inviter: &[u8; 32], now: u64) -> Take {
        let mut slots = self.slots.lock().unwrap();
        Self::sweep(&mut slots, now);
        match slots.get(code) {
            None => Take::Unknown,
            Some(s) if &s.inviter != inviter => Take::NotYours,
            Some(s) if s.filled.is_none() => Take::NotYet,
            Some(_) => {
                let s = slots.remove(code).unwrap();
                Take::Ready(s.filled.unwrap())
            }
        }
    }

    pub fn cancel(&self, code: &str, inviter: &[u8; 32]) -> bool {
        let mut slots = self.slots.lock().unwrap();
        match slots.get(code) {
            Some(s) if &s.inviter == inviter => {
                slots.remove(code);
                true
            }
            _ => false,
        }
    }
}

/// Per-key fixed-window rate limit for the one public write.
#[derive(Default)]
pub struct RateLimit {
    windows: Mutex<HashMap<String, (u64, u32)>>,
}

impl RateLimit {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` if the call is within `max` per `window_secs` for `key`.
    pub fn allow(&self, key: &str, max: u32, window_secs: u64, now: u64) -> bool {
        let mut w = self.windows.lock().unwrap();
        w.retain(|_, (start, _)| now.saturating_sub(*start) < window_secs * 2);
        let e = w.entry(key.to_string()).or_insert((now, 0));
        if now.saturating_sub(e.0) >= window_secs {
            *e = (now, 0);
        }
        e.1 += 1;
        e.1 <= max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames_contract(s: &dyn FrameStore) {
        assert_eq!(s.append("personal", "a", 10).unwrap(), 1);
        assert_eq!(s.append("personal", "bb", 20).unwrap(), 2);
        assert_eq!(s.append("work", "c", 30).unwrap(), 1);
        let p = s.read("personal", 0, 10).unwrap();
        assert_eq!(p.frames, vec![(1, "a".into()), (2, "bb".into())]);
        assert_eq!((p.oldest, p.latest), (1, 2));
        let p = s.read("personal", 1, 1).unwrap();
        assert_eq!(p.frames, vec![(2, "bb".into())]);
        assert!(s.read("nope", 0, 10).unwrap().frames.is_empty());
        let mut ch = s.channels().unwrap();
        ch.sort();
        assert_eq!(ch, vec!["personal", "work"]);
        // Byte cap drops oldest first.
        assert_eq!(s.prune("personal", 0, 2).unwrap(), 1);
        let p = s.read("personal", 0, 10).unwrap();
        assert_eq!(p.oldest, 2);
        assert_eq!(p.frames.len(), 1);
        // Seq keeps climbing after a prune.
        assert_eq!(s.append("personal", "d", 40).unwrap(), 3);
    }

    #[test]
    fn memory_frames() {
        let s = MemoryFrames::new();
        frames_contract(&s);
        assert_eq!(s.prune("personal", 100, u64::MAX).unwrap(), 2);
    }

    #[test]
    fn fs_frames_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = FsFrames::new(dir.path()).unwrap();
            frames_contract(&s);
        }
        let s = FsFrames::new(dir.path()).unwrap();
        assert_eq!(s.append("personal", "e", 50).unwrap(), 4);
        // Without the cache, the scan still finds the right next seq.
        fs::remove_file(dir.path().join("frames/personal/.seq")).unwrap();
        assert_eq!(s.append("personal", "f", 60).unwrap(), 5);
        let p = s.read("personal", 3, 10).unwrap();
        assert_eq!(p.frames.iter().map(|f| f.0).collect::<Vec<_>>(), vec![4, 5]);
    }

    #[test]
    fn members_memory_and_fs() {
        let m = Member {
            node_id: "aa".into(),
            x25519_pub: "bb".into(),
            name: "n".into(),
            channels: vec!["@mesh".into(), "personal".into()],
            admitted_ms: 1,
            admitted_by: "env".into(),
        };
        for s in [
            Box::new(MemoryMembers::new()) as Box<dyn MemberStore>,
            Box::new(FsMembers::new(tempfile::tempdir().unwrap().path()).unwrap()),
        ] {
            assert!(s.get("aa").unwrap().is_none());
            s.put(&m).unwrap();
            assert_eq!(s.get("aa").unwrap().unwrap(), m);
            assert_eq!(s.members_of("personal").unwrap().len(), 1);
            assert_eq!(s.members_of("work").unwrap().len(), 0);
            assert!(s.remove("aa").unwrap());
            assert!(!s.remove("aa").unwrap());
        }
    }

    #[test]
    fn enroll_slots_lifecycle() {
        let e = EnrollSlots::new();
        let a = [1u8; 32];
        let b = [2u8; 32];
        assert!(e.open("CODE", a, 100, 0));
        assert!(!e.open("CODE", a, 100, 0));
        assert!(matches!(e.take("CODE", &a, 1), Take::NotYet));
        assert!(matches!(e.take("CODE", &b, 1), Take::NotYours));
        assert!(matches!(e.fill("NOPE", "x".into(), 1), Fill::Unknown));
        assert!(matches!(e.fill("CODE", "x".into(), 1), Fill::Filled));
        assert!(matches!(e.fill("CODE", "y".into(), 1), Fill::AlreadyFilled));
        assert!(matches!(e.take("CODE", &a, 1), Take::Ready(s) if s == "x"));
        assert!(matches!(e.take("CODE", &a, 1), Take::Unknown));
        // Expiry.
        assert!(e.open("LATE", a, 100, 0));
        assert!(matches!(e.fill("LATE", "x".into(), 200), Fill::Unknown));
        assert!(e.open("LATE", a, 400, 300));
        assert!(e.cancel("LATE", &a));
        assert!(!e.cancel("LATE", &a));
    }

    #[test]
    fn rate_limit_windows() {
        let r = RateLimit::new();
        for _ in 0..3 {
            assert!(r.allow("ip", 3, 60, 0));
        }
        assert!(!r.allow("ip", 3, 60, 10));
        assert!(r.allow("other", 3, 60, 10));
        assert!(r.allow("ip", 3, 60, 61));
    }
}
