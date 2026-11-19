//! Generated display names: `<kind>-<adjective>-<noun>-<NN>`, for example
//! `session-loving-bubble-21`.
//!
//! A name is derived from a record's kind and full id alone, so every node, the
//! CLI and the interface (`spa/src/lib/names.ts`) derive the same one with
//! nothing stored or synced. It is a label, never a key: two records of a kind
//! may share one, and anything that resolves a name reports ties.
//!
//! - `h` = FNV-1a-64 over the UTF-8 of `"tracon/name/v1" 0x1f kind 0x1f id`, then
//!   the splitmix64 finalizer.
//! - adjective = `h mod A`, noun = `(h / A) mod N`, `NN` = `(h / (A·N)) mod 100`,
//!   zero-padded, with `A` and `N` the lengths of the lists in `names-v1.json`.
//!
//! The full id is hashed, never a prefix: a UUIDv7's first 12 hex characters are
//! its timestamp. It is not SHA-256 because the interface names ids
//! synchronously while rendering and `crypto.subtle` is async-only; nothing
//! about a name is a security property. Both lists are frozen: adding a word
//! moves every name, so a new list is a new version and a new hash domain.
//! `spec/vectors/names.json` pins the output in both languages.

use std::sync::OnceLock;

use serde::Deserialize;

/// The word lists, embedded by the node and the interface alike.
pub const WORDS_JSON: &str = include_str!("names-v1.json");

/// The hash domain. Bumps with the word lists.
pub const DOMAIN: &str = "tracon/name/v1";

/// The kinds that carry a generated name. A kind is part of the hash, so the
/// same id named as two kinds gets two unrelated names.
pub mod kind {
    pub const SESSION: &str = "session";
    pub const REVIEW: &str = "review";
    pub const REPORT: &str = "report";
    pub const APPROVAL: &str = "approval";
    pub const MANIFEST: &str = "manifest";
    pub const AGENT: &str = "agent";
    pub const TASK: &str = "task";
    pub const TRANSFER: &str = "transfer";
    pub const CHECK: &str = "check";
    pub const NODE: &str = "node";
}

#[derive(Deserialize)]
struct Words {
    version: u32,
    #[serde(borrow)]
    adjectives: Vec<&'static str>,
    #[serde(borrow)]
    nouns: Vec<&'static str>,
}

fn words() -> &'static Words {
    static WORDS: OnceLock<Words> = OnceLock::new();
    WORDS.get_or_init(|| {
        let words: Words = serde_json::from_str(WORDS_JSON).expect("names-v1.json parses");
        assert_eq!(words.version, 1, "names-v1.json is version 1");
        words
    })
}

/// The adjectives, in list order.
pub fn adjectives() -> &'static [&'static str] {
    &words().adjectives
}

/// The nouns, in list order.
pub fn nouns() -> &'static [&'static str] {
    &words().nouns
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(h: u64, bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(h, |h, b| (h ^ u64::from(*b)).wrapping_mul(FNV_PRIME))
}

/// The splitmix64 finalizer (the mix step without the golden-ratio increment).
fn splitmix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The 64-bit hash a name is picked from.
pub fn hash(kind: &str, id: &str) -> u64 {
    let mut h = fnv1a(FNV_OFFSET, DOMAIN.as_bytes());
    h = fnv1a(h, &[0x1f]);
    h = fnv1a(h, kind.as_bytes());
    h = fnv1a(h, &[0x1f]);
    h = fnv1a(h, id.as_bytes());
    splitmix64(h)
}

/// The generated name of the record of `kind` whose full id is `id`.
pub fn generated(kind: &str, id: &str) -> String {
    let h = hash(kind, id);
    let (adjectives, nouns) = (adjectives(), nouns());
    let (a, n) = (adjectives.len() as u64, nouns.len() as u64);
    format!(
        "{kind}-{}-{}-{:02}",
        adjectives[(h % a) as usize],
        nouns[((h / a) % n) as usize],
        (h / (a * n)) % 100
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const VECTORS: &str = include_str!("../../spec/vectors/names.json");

    #[derive(Deserialize)]
    struct VectorFile {
        domain: String,
        adjectives: usize,
        nouns: usize,
        vectors: Vec<Vector>,
    }

    #[derive(Deserialize)]
    struct Vector {
        kind: String,
        id: String,
        hash_hex: String,
        name: String,
    }

    #[test]
    fn matches_spec_vectors() {
        let f: VectorFile = serde_json::from_str(VECTORS).unwrap();
        assert_eq!(f.domain, DOMAIN);
        assert_eq!(f.adjectives, adjectives().len());
        assert_eq!(f.nouns, nouns().len());
        for v in &f.vectors {
            assert_eq!(
                format!("{:016x}", hash(&v.kind, &v.id)),
                v.hash_hex,
                "{}",
                v.id
            );
            assert_eq!(generated(&v.kind, &v.id), v.name, "{}", v.id);
        }
    }

    #[test]
    fn uuidv7s_minted_in_the_same_millisecond_get_different_names() {
        let a = "019a9e5c-7f20-7a3b-8c1d-2e3f4a5b6c7d";
        let b = "019a9e5c-7f20-7f00-b123-456789abcdef";
        assert_eq!(a[..13], b[..13]);
        assert_ne!(generated(kind::SESSION, a), generated(kind::SESSION, b));
        // The kind is part of the hash.
        assert_ne!(
            generated(kind::SESSION, a)["session-".len()..],
            generated(kind::REVIEW, a)["review-".len()..]
        );
    }

    /// The mechanical curation rules from the list's design. What a word means
    /// (no bodies, sex, death, violence, illness, intoxicants, religion,
    /// politics, ethnicity, insults or slang with those meanings) is reviewed
    /// by hand; these are the rules a machine can hold.
    #[test]
    fn word_lists_follow_the_curation_rules() {
        const UI_STATES: &[&str] = &[
            "failed", "killed", "blocked", "ready", "approved", "rejected", "stale", "closed",
            "expired", "paused", "pending", "running", "waiting",
        ];
        for (list, min) in [(adjectives(), 600), (nouns(), 600)] {
            assert!(
                list.len() >= min,
                "{} words, want at least {min}",
                list.len()
            );
            for w in list {
                assert!(
                    (3..=8).contains(&w.len()) && w.bytes().all(|b| b.is_ascii_lowercase()),
                    "{w:?} is not 3 to 8 lowercase ASCII letters"
                );
                assert!(!UI_STATES.contains(w), "{w:?} is a UI state word");
            }
            assert!(
                list.windows(2).all(|p| p[0] < p[1]),
                "a list is sorted with no repeats"
            );
        }
        let adjectives: std::collections::HashSet<_> = adjectives().iter().collect();
        let shared: Vec<_> = nouns().iter().filter(|n| adjectives.contains(n)).collect();
        assert!(shared.is_empty(), "words on both lists: {shared:?}");
    }
}
