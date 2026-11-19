//! What each sandbox here costs: its lifetime, noted in the store
//! (`store::infra`) as it is made and removed, at the hourly price its size
//! had in the runtime's catalog when it was made.
//!
//! A sandbox is kept warm, so DigitalOcean bills it from create to destroy,
//! not only while its agent works: that span is what is noted. A sandbox the
//! node never saw removed (it was lost, or the node stopped and a later
//! process swept it) is ended when the upkeep next finds it gone.
//!
//! Metering never fails a run: a store error is logged and the sandbox goes
//! on, unnoted. The catalog is another matter: a sandbox's size is chosen
//! from it ([`choose`]), and one that cannot be chosen is not made.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::do_harness::{Client, Size};
use crate::store::{now_ms, SandboxUsage, Store};

/// The provider name rows carry.
pub const PROVIDER: &str = "digitalocean";
/// How long a read of the size catalog is trusted.
const PRICES_FOR: Duration = Duration::from_secs(6 * 60 * 60);

/// What a run is, for its row: the label's kind, or `fork`.
pub const FORK: &str = "fork";

#[derive(Default)]
pub struct Meter {
    store: OnceLock<Arc<Store>>,
    prices: Mutex<Option<(Instant, Vec<Size>)>>,
}

/// The least memory a sandbox is made with when nothing says more: the
/// runtime's own default (1 GB) is too small for a build.
pub const DEFAULT_MIN_MB: u64 = 2048;

/// The size a sandbox is made at, decided once: its manifest, its usage row
/// and a fork of it all carry this.
#[derive(Debug, Clone, PartialEq)]
pub struct Chosen {
    pub slug: String,
    /// `None` when the catalog was not read, so the size is the configured
    /// slug taken on trust.
    pub memory_mb: Option<u64>,
    /// `None` likewise: the sandbox is unpriced.
    pub price_per_hour_usd: Option<f64>,
}

impl Chosen {
    fn of(size: &Size) -> Self {
        Chosen {
            slug: size.slug.clone(),
            memory_mb: Some(size.memory_mb),
            price_per_hour_usd: Some(size.price_per_hour_usd),
        }
    }
}

/// Megabytes as a quantity reads: `4Gi` when whole gibibytes, else `MB`.
pub fn spell_mb(mb: u64) -> String {
    match mb % 1024 {
        0 => format!("{}Gi", mb / 1024),
        _ => format!("{mb} MB"),
    }
}

/// A memory floor in bytes as whole megabytes (MiB, as the catalog counts
/// them), rounded up.
pub fn floor_mb(bytes: u64) -> u64 {
    bytes.div_ceil(1 << 20)
}

/// The cheapest size with at least `mb`, by price and then slug, whatever
/// order the catalog lists them in.
fn cheapest(sizes: &[Size], mb: u64) -> Option<&Size> {
    sizes.iter().filter(|s| s.memory_mb >= mb).min_by(|a, b| {
        a.price_per_hour_usd
            .total_cmp(&b.price_per_hour_usd)
            .then_with(|| a.slug.cmp(&b.slug))
    })
}

/// Why no size has `mb`: the largest there is, named.
fn too_big(sizes: &[Size], mb: u64) -> String {
    let largest = sizes.iter().max_by(|a, b| {
        a.memory_mb
            .cmp(&b.memory_mb)
            .then_with(|| b.slug.cmp(&a.slug))
    });
    match largest {
        Some(l) => format!(
            "no DigitalOcean size has {}; the largest is {} ({} MB)",
            spell_mb(mb),
            l.slug,
            l.memory_mb
        ),
        None => format!("no DigitalOcean size has {}", spell_mb(mb)),
    }
}

/// The size a sandbox is made at: the configured slug (`[digitalocean]
/// size`), or else the cheapest with at least [`DEFAULT_MIN_MB`]; then, when
/// that has less memory than `floor_mb` (a `[placement]` `require.memory`),
/// the cheapest that has as much.
///
/// An empty `sizes` is a catalog not read. Then only a configured slug with
/// no floor is made, unpriced: a floor cannot be checked against an unknown
/// catalog, and with no slug the runtime would pick its own 1 GB default.
pub fn choose(sizes: &[Size], configured: &str, floor_mb: Option<u64>) -> Result<Chosen, String> {
    if sizes.is_empty() {
        return match (configured, floor_mb) {
            ("", _) => Err(
                "DigitalOcean's size catalog could not be read, so no size could be chosen for \
                 this sandbox; setting `[digitalocean] size` lets sandboxes start without the \
                 catalog"
                    .into(),
            ),
            (_, Some(mb)) => Err(format!(
                "DigitalOcean's size catalog could not be read, so a `require.memory` floor of \
                 {} could not be checked against it",
                spell_mb(mb)
            )),
            (slug, None) => Ok(Chosen {
                slug: slug.to_string(),
                memory_mb: None,
                price_per_hour_usd: None,
            }),
        };
    }
    let base = match configured {
        "" => cheapest(sizes, DEFAULT_MIN_MB).ok_or_else(|| too_big(sizes, DEFAULT_MIN_MB))?,
        slug => sizes.iter().find(|s| s.slug == slug).ok_or_else(|| {
            let mut known: Vec<&str> = sizes.iter().map(|s| s.slug.as_str()).collect();
            known.sort_unstable();
            format!(
                "`[digitalocean] size` `{slug}` is not in DigitalOcean's catalog ({})",
                known.join(", ")
            )
        })?,
    };
    match floor_mb {
        Some(mb) if base.memory_mb < mb => cheapest(sizes, mb)
            .map(Chosen::of)
            .ok_or_else(|| too_big(sizes, mb)),
        _ => Ok(Chosen::of(base)),
    }
}

impl Meter {
    /// Note into `store` from now on; before it is set nothing is noted.
    pub fn set_store(&self, store: Arc<Store>) {
        let _ = self.store.set(store);
    }

    pub fn store(&self) -> Option<&Arc<Store>> {
        self.store.get()
    }

    /// The size catalog, read again when it is older than [`PRICES_FOR`].
    /// A read that fails falls back on the last good one; with none, why it
    /// failed.
    pub async fn catalog(&self, client: &Client) -> Result<Vec<Size>, String> {
        if let Some((at, sizes)) = self.prices.lock().unwrap().as_ref() {
            if at.elapsed() < PRICES_FOR {
                return Ok(sizes.clone());
            }
        }
        match client.sizes().await {
            Ok(sizes) => {
                *self.prices.lock().unwrap() = Some((Instant::now(), sizes.clone()));
                Ok(sizes)
            }
            Err(error) => {
                let last = self.prices.lock().unwrap().as_ref().map(|(_, s)| s.clone());
                match last {
                    Some(sizes) => {
                        tracing::warn!(%error, "could not read DigitalOcean's sandbox sizes; using the last list read");
                        Ok(sizes)
                    }
                    None => Err(format!(
                        "could not read DigitalOcean's sandbox sizes: {error}"
                    )),
                }
            }
        }
    }

    /// The size a sandbox made now is made at (see [`choose`]), with the
    /// catalog read for it.
    pub async fn choose(
        &self,
        client: &Client,
        configured: &str,
        floor_mb: Option<u64>,
    ) -> Result<Chosen, String> {
        match self.catalog(client).await {
            Ok(sizes) => choose(&sizes, configured, floor_mb),
            Err(unread) => {
                choose(&[], configured, floor_mb).map_err(|why| format!("{why} ({unread})"))
            }
        }
    }

    /// Note a sandbox made at `started_ms`.
    pub fn started(&self, usage: SandboxUsage) {
        let Some(store) = self.store.get() else {
            return;
        };
        if let Err(error) = store.sandbox_started(&usage) {
            tracing::warn!(%error, sandbox = %usage.sandbox_id, "sandbox lifetime not recorded");
        }
    }

    /// Note a sandbox removed (or found gone) now.
    pub fn ended(&self, sandbox_id: &str, reason: &str) {
        let Some(store) = self.store.get() else {
            return;
        };
        if let Err(error) = store.sandbox_ended(sandbox_id, now_ms(), reason) {
            tracing::warn!(%error, sandbox = %sandbox_id, "sandbox end not recorded");
        }
    }

    /// End every sandbox noted as running that is not in `live` (the
    /// runtime's own list of the sessions still there, asked for at
    /// `listed_ms`). One noted after the list was asked for may be missing
    /// from it only because it is new, and is left. Returns how many ended.
    pub fn reconcile(&self, live: &HashSet<String>, listed_ms: i64) -> usize {
        let Some(store) = self.store.get() else {
            return 0;
        };
        let running = match store.running_sandboxes(PROVIDER) {
            Ok(running) => running,
            Err(error) => {
                tracing::warn!(%error, "could not read running sandboxes");
                return 0;
            }
        };
        let mut ended = 0;
        for row in running
            .iter()
            .filter(|r| r.started_ms < listed_ms && !live.contains(&r.sandbox_id))
        {
            if let Ok(true) = store.sandbox_ended(&row.sandbox_id, now_ms(), "gone") {
                ended += 1;
            }
        }
        ended
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sized(slug: &str, memory_mb: u64, price: f64) -> Size {
        Size {
            slug: slug.into(),
            memory_mb,
            price_per_hour_usd: price,
            ..Default::default()
        }
    }

    /// The catalog of 2026-11-19, listed out of order: nothing may rely on
    /// the catalog listing its sizes smallest first.
    fn catalog() -> Vec<Size> {
        vec![
            sized("mars-4vcpu-8gb", 8192, 0.252),
            sized("mars-2vcpu-2gb", 2048, 0.107),
            sized("mars-16vcpu-32gb", 32768, 1.008),
            sized("mars-1vcpu-1gb", 1024, 0.0535),
            sized("mars-2vcpu-4gb", 4096, 0.126),
        ]
    }

    const GI4: Option<u64> = Some(4096);

    fn chose(slug: &str, mb: u64, price: f64) -> Result<Chosen, String> {
        Ok(Chosen {
            slug: slug.into(),
            memory_mb: Some(mb),
            price_per_hour_usd: Some(price),
        })
    }

    #[test]
    fn unset_the_size_is_the_cheapest_with_two_gibibytes() {
        let sizes = catalog();
        assert_eq!(
            choose(&sizes, "", None),
            chose("mars-2vcpu-2gb", 2048, 0.107)
        );
        let mut reversed = sizes.clone();
        reversed.reverse();
        assert_eq!(
            choose(&reversed, "", None),
            chose("mars-2vcpu-2gb", 2048, 0.107)
        );
        // A floor the default already meets changes nothing.
        assert_eq!(
            choose(&sizes, "", Some(1024)),
            chose("mars-2vcpu-2gb", 2048, 0.107)
        );
        assert_eq!(
            choose(&sizes, "", GI4),
            chose("mars-2vcpu-4gb", 4096, 0.126)
        );
    }

    #[test]
    fn a_configured_size_is_kept_even_a_smaller_one() {
        assert_eq!(
            choose(&catalog(), "mars-1vcpu-1gb", None),
            chose("mars-1vcpu-1gb", 1024, 0.0535)
        );
    }

    #[test]
    fn a_floor_raises_a_smaller_size_and_leaves_a_larger_one() {
        let sizes = catalog();
        assert_eq!(
            choose(&sizes, "mars-2vcpu-2gb", GI4),
            chose("mars-2vcpu-4gb", 4096, 0.126)
        );
        assert_eq!(
            choose(&sizes, "mars-4vcpu-8gb", GI4),
            chose("mars-4vcpu-8gb", 8192, 0.252)
        );
    }

    #[test]
    fn ties_go_to_the_lower_price_then_the_slug() {
        let sizes = vec![
            sized("b-4gb", 4096, 0.2),
            sized("a-4gb", 4096, 0.2),
            sized("c-8gb", 8192, 0.1),
        ];
        assert_eq!(choose(&sizes, "", GI4).unwrap().slug, "c-8gb");
        let sizes = &sizes[..2];
        assert_eq!(choose(sizes, "", GI4).unwrap().slug, "a-4gb");
    }

    #[test]
    fn a_floor_no_size_meets_is_refused_naming_the_largest() {
        let why = choose(&catalog(), "", Some(64 * 1024)).unwrap_err();
        assert_eq!(
            why,
            "no DigitalOcean size has 64Gi; the largest is mars-16vcpu-32gb (32768 MB)"
        );
    }

    #[test]
    fn a_slug_the_catalog_does_not_list_is_refused() {
        let why = choose(&catalog(), "mars-1vcpu-512mb", None).unwrap_err();
        assert!(
            why.starts_with("`[digitalocean] size` `mars-1vcpu-512mb` is not in DigitalOcean's catalog (mars-16vcpu-32gb, mars-1vcpu-1gb"),
            "{why}"
        );
    }

    #[test]
    fn with_no_catalog_only_a_configured_size_with_no_floor_is_made() {
        assert_eq!(
            choose(&[], "mars-2vcpu-4gb", None),
            Ok(Chosen {
                slug: "mars-2vcpu-4gb".into(),
                memory_mb: None,
                price_per_hour_usd: None,
            })
        );
        let unset = choose(&[], "", None).unwrap_err();
        assert!(unset.contains("catalog could not be read"), "{unset}");
        assert!(unset.contains("setting `[digitalocean] size`"), "{unset}");
        let floor = choose(&[], "mars-2vcpu-4gb", GI4).unwrap_err();
        assert!(floor.contains("4Gi could not be checked"), "{floor}");
    }

    #[test]
    fn a_floor_in_bytes_is_whole_megabytes() {
        assert_eq!(floor_mb(4 << 30), 4096);
        assert_eq!(floor_mb((4 << 30) + 1), 4097);
        assert_eq!(spell_mb(65536), "64Gi");
        assert_eq!(spell_mb(1500), "1500 MB");
    }

    #[test]
    fn a_sandbox_the_runtime_no_longer_lists_is_ended() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let meter = Meter::default();
        meter.set_store(store.clone());
        for (id, started_ms) in [("a", 1), ("b", 1), ("new", 10)] {
            meter.started(SandboxUsage {
                sandbox_id: id.into(),
                provider: PROVIDER.into(),
                kind: "session".into(),
                started_ms,
                ..Default::default()
            });
        }
        // Listed at 5: "b" is gone; "new" was made after the list was asked.
        let live = HashSet::from(["a".to_string()]);
        assert_eq!(meter.reconcile(&live, 5), 1);
        let running: Vec<_> = store
            .running_sandboxes(PROVIDER)
            .unwrap()
            .into_iter()
            .map(|r| r.sandbox_id)
            .collect();
        assert_eq!(running, ["a", "new"]);
    }
}
