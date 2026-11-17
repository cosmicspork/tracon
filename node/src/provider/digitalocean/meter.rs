//! What each sandbox here costs: its lifetime, noted in the store
//! (`store::infra`) as it is made and removed, at the hourly price its size
//! had in the runtime's catalog when it was made.
//!
//! A sandbox is kept warm, so DigitalOcean bills it from create to destroy,
//! not only while its agent works: that span is what is noted. A sandbox the
//! node never saw removed (it was lost, or the node stopped and a later
//! process swept it) is ended when the upkeep next finds it gone.
//!
//! Metering never fails a run: a store or catalog error is logged and the
//! sandbox goes on, unpriced or unnoted.

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

/// The slug a sandbox is made at and its hourly price: the configured size,
/// or the catalog's first (smallest) when none is, which is the runtime's
/// default. `None` for the price when the catalog does not list the size.
pub fn priced(sizes: &[Size], configured: &str) -> (Option<String>, Option<f64>) {
    let size = match configured {
        "" => sizes.first(),
        slug => sizes.iter().find(|s| s.slug == slug),
    };
    match size {
        Some(s) => (Some(s.slug.clone()), Some(s.price_per_hour_usd)),
        None => (
            (!configured.is_empty()).then(|| configured.to_string()),
            None,
        ),
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
    pub async fn sizes(&self, client: &Client) -> Vec<Size> {
        if let Some((at, sizes)) = self.prices.lock().unwrap().as_ref() {
            if at.elapsed() < PRICES_FOR {
                return sizes.clone();
            }
        }
        match client.sizes().await {
            Ok(sizes) => {
                *self.prices.lock().unwrap() = Some((Instant::now(), sizes.clone()));
                sizes
            }
            Err(error) => {
                tracing::warn!(%error, "could not read DigitalOcean's sandbox sizes; this sandbox is unpriced");
                self.prices
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|(_, s)| s.clone())
                    .unwrap_or_default()
            }
        }
    }

    /// The size a sandbox made now is made at, and its price.
    pub async fn price(&self, client: &Client, configured: &str) -> (Option<String>, Option<f64>) {
        if self.store.get().is_none() {
            return (None, None);
        }
        priced(&self.sizes(client).await, configured)
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

    fn size(slug: &str, price: f64) -> Size {
        Size {
            slug: slug.into(),
            price_per_hour_usd: price,
            ..Default::default()
        }
    }

    #[test]
    fn a_sandbox_is_priced_at_its_configured_size_or_the_default_one() {
        let sizes = [size("small", 0.05), size("large", 0.4)];
        assert_eq!(priced(&sizes, "large"), (Some("large".into()), Some(0.4)));
        // Unset: the runtime's default, the catalog's smallest.
        assert_eq!(priced(&sizes, ""), (Some("small".into()), Some(0.05)));
        // A size the catalog does not list is named but unpriced.
        assert_eq!(priced(&sizes, "huge"), (Some("huge".into()), None));
        assert_eq!(priced(&[], ""), (None, None));
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
