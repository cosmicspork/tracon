//! What a repository or a channel requires of the provider its sessions run
//! on, and the `[placement]` table that says so.
//!
//! ```toml
//! [placement]
//! digitalocean = "overflow"          # off | overflow | prefer
//!
//! [placement.channel.acme]
//! client = true                      # client work: the stricter defaults
//!
//! [[placement.repo]]
//! path = "github.com/acme/site"
//! forbid_digitalocean = true
//! [placement.repo.require]
//! tier = ["microvm"]
//! ```
//!
//! Each requirement is the set of values it accepts; a capability left out
//! accepts any. Where a channel, a repository and the client defaults all
//! speak, a provider must satisfy every one of them: the stricter wins, and
//! nothing a repository says loosens what its channel or its being client
//! work requires.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::capabilities::{
    Capabilities, Dns, ImageIdentityKind, Privilege, Route, RuntimeSocket, Tier,
};

/// Whether this node may run sessions on DigitalOcean's Harness Runtime.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DoMode {
    /// Never.
    #[default]
    Off,
    /// When every local provider that would take the session is full.
    Overflow,
    /// First, falling back to a local provider when it cannot take one.
    Prefer,
}

/// The values each capability may take for a session to run there. `None`
/// accepts any value; a list accepts only what it names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Requirements {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<Vec<Tier>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<Vec<Route>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dns: Option<Vec<Dns>>,
    /// Asked only of a container: in a VM of its own, root is the VM's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub privilege: Option<Vec<Privilege>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_socket: Option<Vec<RuntimeSocket>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_identity: Option<Vec<ImageIdentityKind>>,
}

/// One `[placement.channel.<name>]` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChannelPlacement {
    /// Client work: held to [`Requirements::client_work`] as well.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub client: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub forbid_digitalocean: bool,
    pub require: Requirements,
}

/// One `[[placement.repo]]` entry, matched as a `[[repo]]` entry's `path` is
/// (`config::repo_path_matches`); the first match wins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepoPlacement {
    pub path: PathBuf,
    /// Client work: held to [`Requirements::client_work`] as well.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub client: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub forbid_digitalocean: bool,
    pub require: Requirements,
}

/// `[placement]`: where sessions may run. Read by nothing that places a
/// session yet; the checks interface shows what providers report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Placement {
    pub digitalocean: DoMode,
    pub channel: std::collections::BTreeMap<String, ChannelPlacement>,
    pub repo: Vec<RepoPlacement>,
}

/// What one session is held to, resolved from its channel and repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    pub requirements: Requirements,
    pub client: bool,
    pub forbid_digitalocean: bool,
}

/// Every accept list, by capability name, for one generic walk.
macro_rules! each_capability {
    ($m:ident) => {
        $m!(tier);
        $m!(route);
        $m!(dns);
        $m!(privilege);
        $m!(runtime_socket);
        $m!(image_identity);
    };
}

fn meet<T: Copy + PartialEq>(a: &Option<Vec<T>>, b: &Option<Vec<T>>) -> Option<Vec<T>> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x.clone()),
        (Some(x), Some(y)) => Some(x.iter().copied().filter(|v| y.contains(v)).collect()),
    }
}

fn admits<T: Copy + PartialEq>(accepted: &Option<Vec<T>>, value: T) -> bool {
    accepted
        .as_ref()
        .is_none_or(|values| values.contains(&value))
}

fn spelled<T: Copy>(values: &[T], as_str: fn(T) -> &'static str) -> String {
    if values.is_empty() {
        return "nothing".into();
    }
    values
        .iter()
        .map(|v| as_str(*v))
        .collect::<Vec<_>>()
        .join(" or ")
}

impl Requirements {
    /// Any verified provider: what personal work is held to.
    pub fn any() -> Self {
        Self::default()
    }

    /// Client work: the sandbox may reach nothing but the node, and no
    /// resolver may carry names out.
    pub fn client_work() -> Self {
        Self {
            route: Some(vec![Route::NodeOnly]),
            dns: Some(vec![Dns::None, Dns::Node]),
            ..Self::default()
        }
    }

    /// What both `self` and `other` accept. A capability both constrain
    /// may come out accepting nothing, which no provider satisfies.
    pub fn and(&self, other: &Self) -> Self {
        let mut out = Self::default();
        macro_rules! m {
            ($f:ident) => {
                out.$f = meet(&self.$f, &other.$f);
            };
        }
        each_capability!(m);
        out
    }

    /// Why a provider with `caps` does not satisfy these, or `Ok`.
    pub fn check(&self, caps: &Capabilities) -> Result<(), String> {
        let mut misses = Vec::new();
        macro_rules! m {
            ($f:ident) => {
                let skip = stringify!($f) == "privilege" && caps.tier != Tier::Container;
                if !skip && !admits(&self.$f, caps.$f) {
                    misses.push(format!(
                        "{} is {}, and {} is required",
                        stringify!($f),
                        caps.$f.as_str(),
                        spelled(self.$f.as_deref().unwrap_or_default(), |v| v.as_str())
                    ));
                }
            };
        }
        each_capability!(m);
        match misses.is_empty() {
            true => Ok(()),
            false => Err(misses.join("; ")),
        }
    }

    /// A capability that accepts nothing, named: an empty list is never
    /// what an operator means.
    fn empty_list(&self) -> Option<&'static str> {
        macro_rules! m {
            ($f:ident) => {
                if self.$f.as_ref().is_some_and(Vec::is_empty) {
                    return Some(stringify!($f));
                }
            };
        }
        each_capability!(m);
        None
    }
}

impl Placement {
    /// What a session on `channel`, in `repo` when it has one, is held to.
    pub fn resolve(&self, channel: &str, repo: Option<&Path>) -> Resolved {
        let on_channel = self.channel.get(channel);
        let on_repo = repo.and_then(|repo| {
            self.repo
                .iter()
                .find(|entry| crate::config::repo_path_matches(&entry.path, repo))
        });
        let client = on_channel.is_some_and(|c| c.client) || on_repo.is_some_and(|r| r.client);
        let mut requirements = if client {
            Requirements::client_work()
        } else {
            Requirements::any()
        };
        if let Some(c) = on_channel {
            requirements = requirements.and(&c.require);
        }
        if let Some(r) = on_repo {
            requirements = requirements.and(&r.require);
        }
        Resolved {
            requirements,
            client,
            forbid_digitalocean: on_channel.is_some_and(|c| c.forbid_digitalocean)
                || on_repo.is_some_and(|r| r.forbid_digitalocean),
        }
    }
}

/// Refuse a `[placement]` table the node could not honour: an accept list
/// that accepts nothing, a repository entry with no path or named twice, or
/// requirements that leave a client entry nothing to run on.
pub fn validate(placement: &Placement) -> Result<(), String> {
    let held = |what: String, client: bool, require: &Requirements| {
        if let Some(field) = require.empty_list() {
            return Err(format!(
                "placement {what}: require.{field} accepts nothing; leave it out to accept any"
            ));
        }
        let effective = if client {
            Requirements::client_work().and(require)
        } else {
            require.clone()
        };
        if let Some(field) = effective.empty_list() {
            return Err(format!(
                "placement {what}: require.{field} accepts nothing client work may run on"
            ));
        }
        Ok(())
    };
    for (name, entry) in &placement.channel {
        held(format!("channel {name}"), entry.client, &entry.require)?;
    }
    let mut seen: Vec<PathBuf> = Vec::new();
    for entry in &placement.repo {
        if entry.path.as_os_str().is_empty() {
            return Err("placement repo entry has no path: name the repository it is for".into());
        }
        held(
            format!("repo {}", entry.path.display()),
            entry.client,
            &entry.require,
        )?;
        let path = crate::config::expand_home(&entry.path);
        if seen.contains(&path) {
            return Err(format!(
                "placement repo {} is named twice: the first match wins, so the second entry \
                 could never apply",
                entry.path.display()
            ));
        }
        seen.push(path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> Capabilities {
        Capabilities {
            tier: Tier::Container,
            route: Route::NodeOnly,
            dns: Dns::None,
            privilege: Privilege::Unprivileged,
            runtime_socket: RuntimeSocket::Absent,
            image_identity: ImageIdentityKind::Exact,
        }
    }

    /// DigitalOcean as the spikes found it: a VM, route limited to the
    /// node, the provider's resolver open, the image rebuilt.
    fn digitalocean() -> Capabilities {
        Capabilities {
            tier: Tier::Microvm,
            route: Route::NodeOnly,
            dns: Dns::ProviderOpen,
            privilege: Privilege::RootInVm,
            runtime_socket: RuntimeSocket::Absent,
            image_identity: ImageIdentityKind::Derived,
        }
    }

    fn parse(text: &str) -> Placement {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn personal_work_accepts_any_provider() {
        let any = Placement::default().resolve("personal", Some(Path::new("/src/blog")));
        assert_eq!(any, Resolved::default());
        assert!(any.requirements.check(&caps()).is_ok());
        assert!(any.requirements.check(&digitalocean()).is_ok());
        let worst = Capabilities {
            route: Route::Unverified,
            dns: Dns::ProviderOpen,
            runtime_socket: RuntimeSocket::Present,
            ..digitalocean()
        };
        assert!(any.requirements.check(&worst).is_ok());
    }

    #[test]
    fn client_work_requires_node_only_and_refuses_an_open_resolver() {
        let client = Requirements::client_work();
        assert!(client.check(&caps()).is_ok());
        assert!(client
            .check(&Capabilities {
                dns: Dns::Node,
                ..caps()
            })
            .is_ok());
        let open = client.check(&digitalocean()).unwrap_err();
        assert!(open.contains("dns is provider-open"), "{open}");
        let unverified = client
            .check(&Capabilities {
                route: Route::Unverified,
                ..caps()
            })
            .unwrap_err();
        assert!(unverified.contains("route is unverified"), "{unverified}");
        assert!(unverified.contains("node-only is required"), "{unverified}");
    }

    #[test]
    fn a_repository_marked_client_work_gets_the_client_defaults() {
        let p = parse(
            r#"
            [[repo]]
            path = "github.com/hounddogreading/hounddogreading"
            client = true
            "#,
        );
        let r = p.resolve(
            "personal",
            Some(Path::new(
                "/home/me/src/github.com/hounddogreading/hounddogreading",
            )),
        );
        assert!(r.client);
        assert_eq!(r.requirements, Requirements::client_work());
        // Another repository on the same channel keeps the personal default.
        let other = p.resolve("personal", Some(Path::new("/home/me/src/blog")));
        assert!(!other.client);
        assert_eq!(other.requirements, Requirements::any());
        // A session with no repository is held to the channel alone.
        assert_eq!(p.resolve("personal", None), Resolved::default());
    }

    #[test]
    fn a_channel_marked_client_work_holds_every_repository_on_it() {
        let p = parse(
            r#"
            [channel.acme]
            client = true
            forbid_digitalocean = true
            "#,
        );
        let r = p.resolve("acme", Some(Path::new("/src/anything")));
        assert!(r.client && r.forbid_digitalocean);
        assert_eq!(r.requirements, Requirements::client_work());
        assert_eq!(p.resolve("personal", None), Resolved::default());
    }

    #[test]
    fn the_stricter_requirement_wins_and_a_repository_cannot_loosen_its_channel() {
        let p = parse(
            r#"
            [channel.acme]
            client = true
            [channel.acme.require]
            tier = ["container", "microvm"]

            [[repo]]
            path = "acme/site"
            [repo.require]
            tier = ["microvm"]
            dns = ["none", "node", "provider-open"]
            "#,
        );
        let r = p.resolve("acme", Some(Path::new("/src/acme/site")));
        assert_eq!(r.requirements.tier, Some(vec![Tier::Microvm]));
        // The repository named provider-open; the client default did not.
        assert_eq!(r.requirements.dns, Some(vec![Dns::None, Dns::Node]));
        assert_eq!(r.requirements.route, Some(vec![Route::NodeOnly]));
    }

    #[test]
    fn requirements_that_meet_on_nothing_admit_nothing() {
        let a = Requirements {
            tier: Some(vec![Tier::Container]),
            ..Requirements::default()
        };
        let b = Requirements {
            tier: Some(vec![Tier::Microvm]),
            ..Requirements::default()
        };
        let both = a.and(&b);
        assert_eq!(both.tier, Some(vec![]));
        let why = both.check(&caps()).unwrap_err();
        assert!(why.contains("nothing is required"), "{why}");
        assert!(both
            .check(&Capabilities {
                tier: Tier::Microvm,
                ..caps()
            })
            .is_err());
    }

    #[test]
    fn privilege_is_asked_only_of_a_container() {
        let unprivileged = Requirements {
            privilege: Some(vec![Privilege::Unprivileged]),
            ..Requirements::default()
        };
        assert!(unprivileged.check(&caps()).is_ok());
        assert!(unprivileged.check(&digitalocean()).is_ok());
        let root_container = Capabilities {
            privilege: Privilege::RootInVm,
            ..caps()
        };
        assert!(unprivileged.check(&root_container).is_err());
    }

    #[test]
    fn every_capability_is_checked() {
        let strict = Requirements {
            tier: Some(vec![Tier::Microvm]),
            route: Some(vec![Route::NodeOnly]),
            dns: Some(vec![Dns::None]),
            privilege: Some(vec![Privilege::Unprivileged]),
            runtime_socket: Some(vec![RuntimeSocket::Absent]),
            image_identity: Some(vec![ImageIdentityKind::Exact]),
        };
        let worst = Capabilities {
            tier: Tier::Container,
            route: Route::Unverified,
            dns: Dns::ProviderOpen,
            privilege: Privilege::RootInVm,
            runtime_socket: RuntimeSocket::Present,
            image_identity: ImageIdentityKind::Derived,
        };
        let why = strict.check(&worst).unwrap_err();
        for name in [
            "tier",
            "route",
            "dns",
            "privilege",
            "runtime_socket",
            "image_identity",
        ] {
            assert!(why.contains(&format!("{name} is ")), "{name}: {why}");
        }
    }

    #[test]
    fn the_first_matching_repository_entry_wins() {
        let p = parse(
            r#"
            [[repo]]
            path = "acme/site"
            client = true
            [[repo]]
            path = "site"
            forbid_digitalocean = true
            "#,
        );
        let r = p.resolve("personal", Some(Path::new("/src/acme/site")));
        assert!(r.client && !r.forbid_digitalocean);
        let r = p.resolve("personal", Some(Path::new("/src/other/site")));
        assert!(!r.client && r.forbid_digitalocean);
        // Component-wise, as `[[repo]]` matches.
        assert_eq!(
            p.resolve("personal", Some(Path::new("/src/acme/website"))),
            Resolved::default()
        );
    }

    #[test]
    fn digitalocean_is_off_unless_turned_on() {
        assert_eq!(Placement::default().digitalocean, DoMode::Off);
        for (text, mode) in [
            ("off", DoMode::Off),
            ("overflow", DoMode::Overflow),
            ("prefer", DoMode::Prefer),
        ] {
            assert_eq!(
                parse(&format!("digitalocean = \"{text}\"")).digitalocean,
                mode
            );
        }
        assert!(toml::from_str::<Placement>("digitalocean = \"always\"").is_err());
    }

    #[test]
    fn values_parse_as_the_table_spells_them() {
        let p = parse(
            r#"
            [channel.x.require]
            tier = ["container", "microvm"]
            route = ["node-only", "unverified"]
            dns = ["none", "node", "provider-open"]
            privilege = ["unprivileged", "root-in-vm"]
            runtime_socket = ["absent", "present"]
            image_identity = ["exact", "derived"]
            "#,
        );
        let r = &p.channel["x"].require;
        assert_eq!(r.tier.as_ref().unwrap().len(), 2);
        assert_eq!(r.dns.as_ref().unwrap().len(), 3);
        assert!(toml::from_str::<Placement>("[channel.x.require]\nroute = [\"open\"]").is_err());
        assert!(toml::from_str::<Placement>("[channel.x.require]\nsandbox = [\"x\"]").is_err());
        assert!(toml::from_str::<Placement>("[channel.x]\nclinet = true").is_err());
    }

    #[test]
    fn validation_refuses_what_could_never_apply() {
        validate(&Placement::default()).unwrap();
        validate(&parse(
            "[[repo]]\npath = \"acme/site\"\nclient = true\n[[repo]]\npath = \"blog\"",
        ))
        .unwrap();

        let empty = validate(&parse("[channel.x.require]\ndns = []")).unwrap_err();
        assert!(empty.contains("require.dns accepts nothing"), "{empty}");

        let starved = validate(&parse(
            "[[repo]]\npath = \"acme/site\"\nclient = true\n[repo.require]\ndns = [\"provider-open\"]",
        ))
        .unwrap_err();
        assert!(starved.contains("client work"), "{starved}");

        let unnamed = validate(&parse("[[repo]]\nclient = true")).unwrap_err();
        assert!(unnamed.contains("no path"), "{unnamed}");

        let twice =
            validate(&parse("[[repo]]\npath = \"a/b\"\n[[repo]]\npath = \"a/b\"")).unwrap_err();
        assert!(twice.contains("named twice"), "{twice}");
    }
}
