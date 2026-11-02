//! What the Harness Runtime API sends and takes. Every response type ignores
//! fields it does not name, and every status is an open set: the API is in
//! public preview and adds values without notice, so an unrecognised one is
//! carried as `Other` rather than failing the whole response.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Declares an enum that reads from and writes to the API's string spelling,
/// keeping any value it does not know in `Other`.
macro_rules! open_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub enum $name {
            $($variant,)+
            Other(String),
        }

        impl $name {
            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $wire,)+
                    Self::Other(other) => other,
                }
            }
        }

        impl From<&str> for $name {
            fn from(text: &str) -> Self {
                match text {
                    $($wire => Self::$variant,)+
                    other => Self::Other(other.to_string()),
                }
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::Other(String::new())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let text = Option::<String>::deserialize(d)?.unwrap_or_default();
                Ok(Self::from(text.as_str()))
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }
    };
}

open_enum!(
    /// A session's lifecycle state.
    SessionStatus {
        Provisioning => "SESSION_STATUS_PROVISIONING",
        Ready => "SESSION_STATUS_READY",
        Detached => "SESSION_STATUS_DETACHED",
        Paused => "SESSION_STATUS_PAUSED",
        Destroying => "SESSION_STATUS_DESTROYING",
        Destroyed => "SESSION_STATUS_DESTROYED",
        Failed => "SESSION_STATUS_FAILED",
    }
);

impl SessionStatus {
    /// Whether the session is gone or going: nothing it holds is in use any
    /// more. An unrecognised status counts as live, so cleanup never removes
    /// what it does not understand.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Destroying | Self::Destroyed | Self::Failed)
    }
}

open_enum!(
    /// A checkpoint's lifecycle state.
    CheckpointStatus {
        Pending => "PENDING",
        Ready => "READY",
        Failed => "FAILED",
    }
);

open_enum!(
    /// Whether a checkpoint was asked for or taken by the runtime itself.
    CheckpointKind {
        Explicit => "explicit",
        Implicit => "implicit",
    }
);

open_enum!(
    /// A custom template's state; a rebase spends several minutes in
    /// `Building`.
    TemplateStatus {
        Pending => "TEMPLATE_STATUS_PENDING",
        Building => "TEMPLATE_STATUS_BUILDING",
        Ready => "TEMPLATE_STATUS_READY",
        Failed => "TEMPLATE_STATUS_FAILED",
    }
);

open_enum!(
    /// One template build's state.
    BuildStatus {
        Pending => "PENDING",
        Building => "BUILDING",
        Succeeded => "SUCCEEDED",
        Failed => "FAILED",
    }
);

/// One sandbox session.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Session {
    pub session_id: String,
    #[serde(default)]
    pub name: String,
    /// `AGENT_KIND_NONE` for a bare sandbox (`harness: none`).
    #[serde(default)]
    pub agent_kind: String,
    #[serde(default)]
    pub status: SessionStatus,
    /// `manual`, `idle` or `zero_balance` while paused; open-ended.
    pub pause_reason: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub last_event_at: String,
    #[serde(default)]
    pub keep_warm: bool,
    pub sandbox_id: Option<String>,
    /// The agent config the session was created from. Creating from a
    /// manifest stores one too, which removing the session leaves behind.
    pub config_id: Option<String>,
    pub parent_session_id: Option<String>,
    pub fork_id: Option<String>,
    /// Only for teams on regional routing; absent means the default region.
    pub region: Option<String>,
    /// Create-time advisories, such as an egress allowlist with no hosts.
    #[serde(default)]
    pub warnings: Vec<String>,
    /// The create response's `Location` header: the session's URL on the
    /// regional host that owns it, when the API sent one.
    #[serde(skip)]
    pub location: Option<String>,
}

/// A bare-sandbox manifest in the `flat.v1` shape. The API rejects any field
/// it does not know (there is no `region`: placement follows the API host),
/// so this names only what is sent and omits what is unset.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Manifest {
    pub name: String,
    /// `none` for a sandbox with no managed agent in it.
    pub harness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// Without it the runtime pauses a session 15 minutes after its last exec.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub keep_warm: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub egress: Option<Egress>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

impl Manifest {
    pub fn bare(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            harness: "none".into(),
            ..Self::default()
        }
    }
}

/// Where a sandbox may connect. Omitting egress leaves it unrestricted, so
/// `Manifest::egress` of `None` and `Egress::Unrestricted` mean the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Egress {
    Unrestricted,
    Hosts(Vec<String>),
    /// With `allow_ips` and no hosts, the runtime's proxy permits only those
    /// literal addresses and refuses every hostname; DNS itself stays open.
    Rules {
        allow_hosts: Vec<String>,
        allow_ips: Vec<String>,
        vpc_uuid: Option<String>,
        subnet_uuid: Option<String>,
    },
}

impl Serialize for Egress {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Self::Unrestricted => s.serialize_str("unrestricted"),
            Self::Hosts(hosts) => hosts.serialize(s),
            Self::Rules {
                allow_hosts,
                allow_ips,
                vpc_uuid,
                subnet_uuid,
            } => {
                let mut map = s.serialize_map(None)?;
                if !allow_hosts.is_empty() {
                    map.serialize_entry("allow_hosts", allow_hosts)?;
                }
                if !allow_ips.is_empty() {
                    map.serialize_entry("allow_ips", allow_ips)?;
                }
                if let Some(vpc) = vpc_uuid {
                    map.serialize_entry("vpc_uuid", vpc)?;
                }
                if let Some(subnet) = subnet_uuid {
                    map.serialize_entry("subnet_uuid", subnet)?;
                }
                map.end()
            }
        }
    }
}

/// An agent config as listed: no manifest.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct ConfigSummary {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub agentspec_schema_version: String,
    #[serde(default)]
    pub content_hash: String,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    pub region: Option<String>,
}

/// One agent config, with the manifest it stores.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Config {
    #[serde(flatten)]
    pub summary: ConfigSummary,
    #[serde(default)]
    pub manifest: serde_json::Value,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// A save point of a session's microVM: files and process memory.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Checkpoint {
    pub checkpoint_id: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub status: CheckpointStatus,
    #[serde(default)]
    pub kind: CheckpointKind,
    pub label: Option<String>,
    /// Implicit checkpoints have been recorded at 51 GB, past `u32`, and a
    /// 64-bit integer may arrive as a JSON string; both are read.
    #[serde(default, deserialize_with = "u64_number_or_string")]
    pub size_bytes: u64,
    #[serde(default)]
    pub created_at: String,
    pub error_message: Option<String>,
}

/// The customer image a template is built from. `digest` is what was pushed;
/// the template the runtime builds has a digest of its own.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct ImageSource {
    pub registry: Option<String>,
    pub repository: Option<String>,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct TemplateSpec {
    pub base_template: Option<String>,
    pub image: Option<ImageSource>,
}

/// A team's custom sandbox template: a customer image rebased onto one of
/// the platform's bases.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Template {
    pub template_id: String,
    #[serde(default)]
    pub name: String,
    pub spec: Option<TemplateSpec>,
    #[serde(default)]
    pub status: TemplateStatus,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct TemplateBuild {
    pub build_id: String,
    pub template_id: Option<String>,
    pub spec: Option<TemplateSpec>,
    #[serde(default)]
    pub status: BuildStatus,
    pub error: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

/// One selectable sandbox size, smallest first in the catalog.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Size {
    pub slug: String,
    #[serde(default)]
    pub vcpus: u32,
    #[serde(default)]
    pub memory_mb: u64,
    #[serde(default)]
    pub price_per_hour_usd: f64,
    #[serde(default)]
    pub price_per_month_usd: f64,
}

/// One command run in a sandbox, outside any agent. Each call starts fresh:
/// nothing carries between calls but the filesystem.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ExecRequest {
    pub argv: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    /// Zero or `None` takes the server's default (four minutes buffered).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
}

/// A buffered exec's result; each stream is capped at 1 MiB by the API.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct ExecOutput {
    pub exit_code: i32,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
}

/// The team's prepayment state: when the balance runs out the runtime
/// pauses running sessions.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Balance {
    /// A decimal string in US dollars, e.g. `"10.00"`.
    pub balance: Option<String>,
    pub month_to_date_balance: Option<String>,
    #[serde(default)]
    pub eligible: bool,
    #[serde(default)]
    pub blocked: bool,
    #[serde(default)]
    pub is_auto_prepay_enabled: bool,
}

fn u64_number_or_string<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Wire {
        Number(u64),
        Text(String),
        Null(()),
    }
    match Wire::deserialize(d)? {
        Wire::Number(n) => Ok(n),
        Wire::Text(text) => text
            .trim()
            .parse()
            .map_err(|_| serde::de::Error::custom(format!("not a byte count: {text:?}"))),
        Wire::Null(()) => Ok(0),
    }
}
