//! Sandbox providers: what makes, runs and tears down the sandbox a session
//! runs in (Podman, Apple Containerization, Kubernetes, DigitalOcean's
//! Harness Runtime). Not to be confused with the model providers in
//! `crate::providers`.
//!
//! Every provider answers the same [`SandboxProvider`] interface and reports
//! the same [`Capabilities`]; [`placement::place`] decides which one a
//! session runs on from those reports and what the session's channel and
//! repository require (`[placement]`, [`requirements`]).
//!
//! Today the boundary backends (`crate::boundary`) still run every session.
//! What this module adds alongside them is the vocabulary: Podman's and
//! Kubernetes's checks read as capabilities ([`capabilities::from_checks`]),
//! shown in the checks interface and acted on by nothing yet.

pub mod capabilities;
pub mod placement;
pub mod requirements;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub use capabilities::{Capabilities, CapabilityReport};
pub use placement::{place, Candidate, Placement};
pub use requirements::{DoMode, Requirements};

use crate::boundary::BoundaryReport;
use crate::runner::Mount;

/// Which provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Podman,
    Apple,
    Kubernetes,
    #[serde(rename = "digitalocean")]
    DigitalOcean,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Podman => "podman",
            Self::Apple => "apple",
            Self::Kubernetes => "kubernetes",
            Self::DigitalOcean => "digitalocean",
        }
    }

    /// Elastic: off the node's own machine, used per `[placement]
    /// digitalocean` rather than whenever it is free.
    pub fn is_elastic(self) -> bool {
        self == Self::DigitalOcean
    }

    /// The provider a boundary backend is, by `Backend::kind`.
    pub fn of_backend(kind: &str) -> Option<Self> {
        match kind {
            "podman" => Some(Self::Podman),
            "kubernetes" => Some(Self::Kubernetes),
            _ => None,
        }
    }
}

/// A provider's own name for one sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Handle(pub String);

/// A provider's own name for one snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SnapshotId(pub String);

/// Where a sandbox is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    Starting,
    Running,
    /// Stopped by the provider or the node and resumable, with its state
    /// kept: not [`Status::Lost`].
    Paused,
    Exited {
        code: i32,
    },
    /// The provider no longer knows it, or cannot say.
    Lost,
}

/// The image a sandbox actually runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageIdentity {
    /// The digest the node named.
    Exact { digest: String },
    /// A digest the provider made from the one the node named.
    Derived { from: String, actual: String },
}

impl ImageIdentity {
    pub fn kind(&self) -> capabilities::ImageIdentityKind {
        match self {
            Self::Exact { .. } => capabilities::ImageIdentityKind::Exact,
            Self::Derived { .. } => capabilities::ImageIdentityKind::Derived,
        }
    }
}

/// How many sandboxes a provider runs, and how many it may.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capacity {
    pub running: u32,
    /// `None` for no limit of the provider's own.
    pub limit: Option<u32>,
}

impl Capacity {
    pub fn is_full(&self) -> bool {
        self.limit.is_some_and(|limit| self.running >= limit)
    }
}

impl std::fmt::Display for Capacity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.limit {
            Some(limit) => write!(f, "{} of {limit}", self.running),
            None => write!(f, "{} running", self.running),
        }
    }
}

/// What one sandbox is given to run with. `None` leaves it to the provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resources {
    pub cpu_millis: Option<u32>,
    pub memory_bytes: Option<u64>,
    pub disk_bytes: Option<u64>,
}

/// How the in-sandbox supervisor finds the node: the one connection out it
/// makes, to `node_addr`, trusting only the certificate `cert_pin` names and
/// presenting `token`, which grants that one run's channels.
#[derive(Clone, PartialEq, Eq)]
pub struct Bootstrap {
    pub node_addr: String,
    /// SHA-256 of the node's certificate, hex.
    pub cert_pin: String,
    pub token: String,
}

impl std::fmt::Debug for Bootstrap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bootstrap")
            .field("node_addr", &self.node_addr)
            .field("cert_pin", &self.cert_pin)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// A sandbox's network, as the provider reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Network {
    /// Every destination the provider lets the sandbox reach. The node's
    /// endpoint should be the only one.
    pub destinations: Vec<String>,
    pub dns: capabilities::Dns,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// This provider cannot do it at all (`pause` on one with no pause).
    #[error("{provider} does not support {operation}")]
    Unsupported {
        provider: &'static str,
        operation: &'static str,
    },
    #[error("no such sandbox: {0}")]
    NotFound(String),
    #[error("{0}")]
    Failed(String),
}

/// One way of making sandboxes. Implemented by each provider; nothing calls
/// it to place a session yet.
///
/// What a provider reports about a sandbox is read back from the provider,
/// never from inside the sandbox: the harness may have changed anything in
/// there, so nothing it says is evidence of isolation.
#[async_trait]
pub trait SandboxProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;

    /// Make and start a sandbox from `image` (by digest), with the harness
    /// bundle and the session's directories among `mounts`.
    async fn create(
        &self,
        image: &str,
        resources: &Resources,
        mounts: &[Mount],
        bootstrap: &Bootstrap,
    ) -> Result<Handle, ProviderError>;

    /// Stop it and remove everything the provider made for it. Idempotent.
    async fn destroy(&self, handle: &Handle) -> Result<(), ProviderError>;

    async fn status(&self, handle: &Handle) -> Result<Status, ProviderError>;

    async fn pause(&self, _handle: &Handle) -> Result<(), ProviderError> {
        Err(self.unsupported("pause"))
    }

    async fn resume(&self, _handle: &Handle) -> Result<(), ProviderError> {
        Err(self.unsupported("resume"))
    }

    async fn snapshot(&self, _handle: &Handle) -> Result<SnapshotId, ProviderError> {
        Err(self.unsupported("snapshot"))
    }

    /// A new sandbox from a snapshot, with its own bootstrap: a fork is a
    /// different run and never holds its parent's token.
    async fn fork(
        &self,
        _snapshot: &SnapshotId,
        _resources: &Resources,
        _bootstrap: &Bootstrap,
    ) -> Result<Handle, ProviderError> {
        Err(self.unsupported("fork"))
    }

    async fn image_identity(&self, handle: &Handle) -> Result<ImageIdentity, ProviderError>;

    async fn network(&self, handle: &Handle) -> Result<Network, ProviderError>;

    /// The capabilities one sandbox actually has, as the provider reports
    /// them: what placement promised is checked against this.
    async fn verify_isolation(&self, handle: &Handle) -> Result<Capabilities, ProviderError>;

    /// What this provider promises of every sandbox it makes.
    fn capabilities(&self) -> Capabilities;

    async fn capacity(&self) -> Result<Capacity, ProviderError>;

    fn unsupported(&self, operation: &'static str) -> ProviderError {
        ProviderError::Unsupported {
            provider: self.kind().as_str(),
            operation,
        }
    }
}

/// The capabilities a boundary backend's report shows, for a backend that
/// is a provider.
pub fn backend_report(backend_kind: &str, report: &BoundaryReport) -> Option<CapabilityReport> {
    ProviderKind::of_backend(backend_kind).map(|kind| capabilities::from_checks(kind, report))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provider that implements only what it must, to show the rest
    /// refuses rather than pretends.
    struct Minimal;

    #[async_trait]
    impl SandboxProvider for Minimal {
        fn kind(&self) -> ProviderKind {
            ProviderKind::Podman
        }
        async fn create(
            &self,
            _: &str,
            _: &Resources,
            _: &[Mount],
            _: &Bootstrap,
        ) -> Result<Handle, ProviderError> {
            Ok(Handle("one".into()))
        }
        async fn destroy(&self, _: &Handle) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn status(&self, h: &Handle) -> Result<Status, ProviderError> {
            match h.0.as_str() {
                "one" => Ok(Status::Paused),
                other => Err(ProviderError::NotFound(other.into())),
            }
        }
        async fn image_identity(&self, _: &Handle) -> Result<ImageIdentity, ProviderError> {
            Ok(ImageIdentity::Exact {
                digest: "sha256:aa".into(),
            })
        }
        async fn network(&self, _: &Handle) -> Result<Network, ProviderError> {
            Ok(Network {
                destinations: vec!["node".into()],
                dns: capabilities::Dns::None,
            })
        }
        async fn verify_isolation(&self, _: &Handle) -> Result<Capabilities, ProviderError> {
            Ok(self.capabilities())
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                tier: capabilities::Tier::Container,
                route: capabilities::Route::NodeOnly,
                dns: capabilities::Dns::None,
                privilege: capabilities::Privilege::Unprivileged,
                runtime_socket: capabilities::RuntimeSocket::Absent,
                image_identity: capabilities::ImageIdentityKind::Exact,
            }
        }
        async fn capacity(&self) -> Result<Capacity, ProviderError> {
            Ok(Capacity {
                running: 0,
                limit: None,
            })
        }
    }

    fn bootstrap() -> Bootstrap {
        Bootstrap {
            node_addr: "node.example:443".into(),
            cert_pin: "ab".repeat(32),
            token: "run-secret".into(),
        }
    }

    #[tokio::test]
    async fn what_a_provider_does_not_implement_is_refused_by_name() {
        let p = Minimal;
        let h = p
            .create("img@sha256:aa", &Resources::default(), &[], &bootstrap())
            .await
            .unwrap();
        assert_eq!(p.status(&h).await.unwrap(), Status::Paused);
        let refused = p.pause(&h).await.unwrap_err();
        assert_eq!(refused.to_string(), "podman does not support pause");
        assert!(matches!(
            p.resume(&h).await,
            Err(ProviderError::Unsupported {
                operation: "resume",
                ..
            })
        ));
        assert!(p.snapshot(&h).await.is_err());
        assert!(p
            .fork(&SnapshotId("s".into()), &Resources::default(), &bootstrap())
            .await
            .is_err());
        assert!(matches!(
            p.status(&Handle("gone".into())).await,
            Err(ProviderError::NotFound(_))
        ));
        assert_eq!(p.verify_isolation(&h).await.unwrap(), p.capabilities());
        assert_eq!(
            p.image_identity(&h).await.unwrap().kind(),
            capabilities::ImageIdentityKind::Exact
        );
        p.destroy(&h).await.unwrap();
    }

    #[test]
    fn paused_is_not_lost() {
        assert_ne!(Status::Paused, Status::Lost);
        assert_eq!(
            serde_json::to_value(Status::Paused).unwrap(),
            serde_json::json!({"state": "paused"})
        );
        assert_eq!(
            serde_json::to_value(Status::Exited { code: 3 }).unwrap(),
            serde_json::json!({"state": "exited", "code": 3})
        );
    }

    #[test]
    fn a_derived_image_names_both_digests() {
        let id = ImageIdentity::Derived {
            from: "sha256:aa".into(),
            actual: "sha256:bb".into(),
        };
        assert_eq!(id.kind(), capabilities::ImageIdentityKind::Derived);
        assert_eq!(
            serde_json::to_value(&id).unwrap(),
            serde_json::json!({"kind": "derived", "from": "sha256:aa", "actual": "sha256:bb"})
        );
    }

    #[test]
    fn the_token_never_reaches_a_log() {
        let shown = format!("{:?}", bootstrap());
        assert!(!shown.contains("run-secret"), "{shown}");
        assert!(shown.contains("node.example:443"));
    }

    #[test]
    fn only_the_boundary_backends_report_today() {
        let report = BoundaryReport { checks: vec![] };
        assert_eq!(
            backend_report("podman", &report).unwrap().provider,
            ProviderKind::Podman
        );
        assert_eq!(
            backend_report("kubernetes", &report).unwrap().provider,
            ProviderKind::Kubernetes
        );
        assert!(backend_report("test", &report).is_none());
        assert_eq!(
            serde_json::to_value(ProviderKind::DigitalOcean).unwrap(),
            serde_json::json!("digitalocean")
        );
    }
}
