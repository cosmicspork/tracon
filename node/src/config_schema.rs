//! What each `node.toml` key is, for the one path that edits the file
//! (`config::edit`): whether a change applies to the running node or waits
//! for a restart, whether writing it is an administrative act, whether it
//! names an executable the node runs or a host path it reads or writes, and
//! whether its value is a secret the audit log and any reader must not see.
//!
//! One line per key, written as the file spells it: tables by their dotted
//! path, a map's entries as `*`, an array of tables' fields under the array's
//! own name (`repo.path`, not `repo[0].path`). The test below walks the
//! `Config` structs themselves and fails on a key missing here, or on an
//! entry no struct has, so a new setting cannot arrive without saying what
//! it is.

use serde::Serialize;

/// When a change to a key takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Apply {
    /// The running node picks it up from the next session or check.
    Live,
    /// The running node holds what it loaded; a restart applies it.
    Restart,
}

/// What a path-valued key names, which decides the rules
/// `config::path_guard` holds it to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PathKind {
    /// A program the node runs on its host: an absolute path or a bare name
    /// found on the node's `PATH`. Its file and every directory above it
    /// (and above any link on the way) must be out of an agent's reach and
    /// writable by nobody but root and, inside a package manager's prefix,
    /// the node's own user. Checked again just before every spawn.
    Executable,
    /// Arguments to such a program that decide what it in turn runs
    /// (`consulta.args`: `uv run --project <dir> consulta`). Held to a fixed
    /// shape, and every path among them to the rules for a secret.
    ExecArgs,
    /// A file whose content the node reads as a credential and sends on.
    /// Never inside an agent's reach, never one of the node's own secrets,
    /// never an operator's well-known credential store, and writable by
    /// nobody but root and the node's own user.
    SecretFile,
    /// A directory of credential files the node reads; as `SecretFile`.
    SecretDir,
    /// A directory the node writes files into. Never a system directory, the
    /// configuration or state directory, a worktree root or a package
    /// manager's prefix, and never inside or around any of them.
    WriteDir,
    /// A directory the node replaces whole and serves files from; as
    /// `WriteDir`, except that it may sit inside the state directory.
    ManagedDir,
    /// A directory under which worktrees are made or accepted. Never `/`, a
    /// system directory or a package manager's prefix, and never the state
    /// or configuration directory or a directory holding either.
    Root,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct KeySpec {
    pub key: &'static str,
    pub apply: Apply,
    /// Writing it is an administrative act. Every key is, today: what the
    /// node runs, trusts and reaches is all decided here.
    pub admin: bool,
    /// It names an executable the node runs on its host, or arguments that
    /// decide what that executable runs, so a writer that could choose it
    /// could choose what the node executes.
    pub exec_path: bool,
    /// It names a file or directory on the host that the node reads or
    /// writes (a credential file, a certificate directory, an export
    /// target), so a writer that could choose it could choose what the node
    /// reads, sends on or overwrites. The value is a path, not a secret.
    pub fs_path: bool,
    /// Which rules `config::path_guard` holds the value to, for a key that
    /// is `exec_path` or `fs_path`.
    pub path: Option<PathKind>,
    /// Its value is never shown: the audit log records that it changed, not
    /// what to.
    pub secret: bool,
}

const fn restart(key: &'static str) -> KeySpec {
    KeySpec {
        key,
        apply: Apply::Restart,
        admin: true,
        exec_path: false,
        fs_path: false,
        path: None,
        secret: false,
    }
}

const fn live(key: &'static str) -> KeySpec {
    KeySpec {
        apply: Apply::Live,
        ..restart(key)
    }
}

impl KeySpec {
    const fn exec(self) -> Self {
        KeySpec {
            exec_path: true,
            path: Some(PathKind::Executable),
            ..self
        }
    }

    /// Arguments that decide what an executable runs.
    const fn exec_args(self) -> Self {
        KeySpec {
            exec_path: true,
            path: Some(PathKind::ExecArgs),
            ..self
        }
    }

    const fn path(self, kind: PathKind) -> Self {
        KeySpec {
            fs_path: true,
            path: Some(kind),
            ..self
        }
    }

    #[allow(dead_code)]
    const fn secret(self) -> Self {
        KeySpec {
            secret: true,
            ..self
        }
    }
}

/// Every key `node.toml` may hold.
pub const KEYS: &[KeySpec] = &[
    restart("node_name"),
    // [harness]
    restart("harness.id"),
    restart("harness.tools"),
    restart("harness.version"),
    // [boundary]
    restart("boundary.podman").exec(),
    restart("boundary.network"),
    restart("boundary.subnet"),
    restart("boundary.gateway_ip"),
    restart("boundary.gateway_container"),
    restart("boundary.gateway_image"),
    restart("boundary.harness_image"),
    restart("boundary.selinux_label_disable"),
    restart("boundary.start_machine"),
    restart("boundary.stop_timeout_secs"),
    // [gateway]
    restart("gateway.allow_hosts"),
    restart("gateway.proxy_port"),
    restart("gateway.qa_proxy_port"),
    restart("gateway.forward_port"),
    restart("gateway.egress_port"),
    restart("gateway.harness_listen"),
    // [session]
    restart("session.budget_tokens"),
    restart("session.permission_timeout_secs"),
    restart("session.approval_expiry_secs"),
    restart("session.harness_api_timeout_secs"),
    restart("session.default_channel"),
    restart("session.claim_grace_secs"),
    restart("session.suspend_published_after_secs"),
    restart("session.worktree_root").path(PathKind::Root),
    restart("session.pty_buffer_frames"),
    restart("session.pty_capture_output"),
    // [consulta]
    restart("consulta.command").exec(),
    restart("consulta.args").exec_args(),
    restart("consulta.timeout_secs"),
    // [publish]
    restart("publish.gh").exec(),
    restart("publish.glab").exec(),
    restart("publish.git").exec(),
    restart("publish.forge_timeout_secs"),
    restart("publish.commits"),
    restart("publish.style.conventional"),
    restart("publish.style.types"),
    restart("publish.style.imperative"),
    restart("publish.style.max_subject"),
    restart("publish.style.kebab_branch"),
    restart("publish.style.no_ticket_keys"),
    // [github]
    restart("github.dispatch.*"),
    // [mesh]
    restart("mesh.hub_url"),
    restart("mesh.heartbeat_secs"),
    restart("mesh.poll_secs"),
    restart("mesh.command_timeout_secs"),
    restart("mesh.stream_open_timeout_secs"),
    restart("mesh.stream_idle_secs"),
    restart("mesh.stream_max_body_bytes"),
    restart("mesh.stream_max_response_bytes"),
    restart("mesh.stream_max_concurrent"),
    restart("mesh.renew_credentials"),
    // [runtime]
    restart("runtime.kind"),
    restart("runtime.approved_images"),
    restart("runtime.kubernetes.namespace"),
    restart("runtime.kubernetes.harness_image"),
    restart("runtime.kubernetes.state_claim"),
    restart("runtime.kubernetes.state_mount"),
    restart("runtime.kubernetes.harness_home"),
    restart("runtime.kubernetes.uid"),
    restart("runtime.kubernetes.gateway_host"),
    restart("runtime.kubernetes.start_timeout_secs"),
    restart("runtime.kubernetes.image_pull_secrets"),
    restart("runtime.kubernetes.internal_cidrs"),
    // [runtime.kubernetes.builder]
    restart("runtime.kubernetes.builder.addr"),
    restart("runtime.kubernetes.builder.tls_dir").path(PathKind::SecretDir),
    restart("runtime.kubernetes.builder.registry"),
    restart("runtime.kubernetes.builder.docker_config").path(PathKind::SecretDir),
    restart("runtime.kubernetes.builder.timeout_secs"),
    restart("runtime.kubernetes.builder.do_token_file").path(PathKind::SecretFile),
    restart("runtime.kubernetes.builder.buildctl").exec(),
    restart("runtime.kubernetes.shm_size"),
    restart("runtime.kubernetes.resources.session.cpu_request"),
    restart("runtime.kubernetes.resources.session.cpu_limit"),
    restart("runtime.kubernetes.resources.session.memory_request"),
    restart("runtime.kubernetes.resources.session.memory_limit"),
    restart("runtime.kubernetes.resources.command.cpu_request"),
    restart("runtime.kubernetes.resources.command.cpu_limit"),
    restart("runtime.kubernetes.resources.command.memory_request"),
    restart("runtime.kubernetes.resources.command.memory_limit"),
    // [[repo]]: the one table a running node reads live (`Config::repos`).
    live("repo.path"),
    live("repo.image"),
    live("repo.dockerfile"),
    live("repo.context"),
    live("repo.checks"),
    live("repo.timeout_secs"),
    live("repo.prepare"),
    live("repo.egress"),
    live("repo.session_egress"),
    live("repo.prepare_credentials"),
    live("repo.commits"),
    live("repo.style.conventional"),
    live("repo.style.types"),
    live("repo.style.imperative"),
    live("repo.style.max_subject"),
    live("repo.style.kebab_branch"),
    live("repo.style.no_ticket_keys"),
    // [[service]]
    restart("service.name"),
    restart("service.image"),
    restart("service.command"),
    restart("service.port"),
    restart("service.ready"),
    restart("service.timeout_secs"),
    // [providers.<name>]
    restart("providers.*.credential"),
    restart("providers.*.upstream"),
    restart("providers.*.shape"),
    restart("providers.*.login"),
    restart("providers.*.price.input_per_mtok"),
    restart("providers.*.price.output_per_mtok"),
    restart("providers.*.models.id"),
    restart("providers.*.models.name"),
    restart("providers.*.models.context"),
    restart("providers.*.models.output"),
    restart("providers.*.models.reasoning"),
    restart("providers.*.models.attachment"),
    // [memory]
    restart("memory.promote_at"),
    // [supervision]
    restart("supervision.checks"),
    restart("supervision.timeout_secs"),
    restart("supervision.dependency_inputs.*"),
    restart("supervision.max_snapshot_bytes"),
    // [review]
    restart("review.max_diff_lines"),
    restart("review.max_files"),
    // [notify]
    restart("notify.contact"),
    // [embed]
    restart("embed.enabled"),
    restart("embed.base_url"),
    restart("embed.model"),
    restart("embed.dim"),
    restart("embed.api_key_file").path(PathKind::SecretFile),
    restart("embed.provider"),
    restart("embed.batch"),
    restart("embed.timeout_secs"),
    restart("embed.min_similarity"),
    // [external]
    restart("external.enabled"),
    restart("external.idle_timeout_secs"),
    restart("external.repo_roots").path(PathKind::Root),
    // [docs]
    restart("docs.export_dir").path(PathKind::WriteDir),
    restart("docs.export_channel"),
    restart("docs.export_every_secs"),
    restart("docs.preview_listen"),
    restart("docs.preview_url"),
    // [launch]
    restart("launch.plugins"),
    // [ui]
    restart("ui.opencode_listen"),
    restart("ui.opencode_url"),
    restart("ui.opencode_bundle_dir").path(PathKind::ManagedDir),
    // [auth]
    restart("auth.public_origin"),
    restart("auth.trusted_proxies"),
];

/// What is assumed of a key the schema does not know: one the node ignores
/// (a typo, a retired section). It is held to the strictest answer, so its
/// value stays out of the audit log.
const UNKNOWN: KeySpec = KeySpec {
    key: "",
    apply: Apply::Restart,
    admin: true,
    exec_path: false,
    fs_path: false,
    path: None,
    secret: true,
};

fn matches(pattern: &str, key: &[&str]) -> bool {
    let mut parts = pattern.split('.');
    key.iter()
        .all(|segment| parts.next().is_some_and(|p| p == "*" || p == *segment))
}

/// The entry for one key, given as its table segments with any array index
/// left out (`["repo", "egress"]`).
///
/// A key that is a whole table (`providers.openrouter`, added or removed in
/// one piece) answers for everything under it: live only if all of it is,
/// a secret or an executable if any of it is.
pub fn lookup(key: &[&str]) -> KeySpec {
    let depth = key.len();
    if let Some(spec) = KEYS
        .iter()
        .find(|spec| spec.key.split('.').count() == depth && matches(spec.key, key))
    {
        return *spec;
    }
    let under: Vec<&KeySpec> = KEYS
        .iter()
        .filter(|spec| spec.key.split('.').count() > depth && matches(spec.key, key))
        .collect();
    if under.is_empty() {
        return UNKNOWN;
    }
    KeySpec {
        key: "",
        apply: match under.iter().all(|spec| spec.apply == Apply::Live) {
            true => Apply::Live,
            false => Apply::Restart,
        },
        admin: under.iter().any(|spec| spec.admin),
        exec_path: under.iter().any(|spec| spec.exec_path),
        fs_path: under.iter().any(|spec| spec.fs_path),
        path: None,
        secret: under.iter().any(|spec| spec.secret),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::config::Config;

    /// Every key the `Config` structs can read from the file has exactly one
    /// entry, and every entry is a key they read. The keys come from the
    /// structs' own `Deserialize`, not from a serialized value, so a field
    /// that is skipped when empty, an `Option` that is `None` by default, and
    /// the fields of an array or map entry nobody has written yet all count.
    #[test]
    fn every_config_key_has_a_schema_entry() {
        let traced = trace::keys::<Config>();
        let listed: BTreeSet<String> = KEYS.iter().map(|spec| spec.key.to_string()).collect();
        assert_eq!(
            listed.len(),
            KEYS.len(),
            "a key is listed twice in config_schema::KEYS"
        );
        let missing: Vec<_> = traced.difference(&listed).collect();
        let stale: Vec<_> = listed.difference(&traced).collect();
        assert!(
            missing.is_empty(),
            "node.toml keys with no entry in config_schema::KEYS (add one line each): {missing:?}"
        );
        assert!(
            stale.is_empty(),
            "config_schema::KEYS names keys no Config struct reads: {stale:?}"
        );
    }

    #[test]
    fn a_key_is_found_through_its_table_and_entry() {
        assert_eq!(lookup(&["repo", "egress"]).apply, Apply::Live);
        assert_eq!(lookup(&["harness", "id"]).apply, Apply::Restart);
        assert!(lookup(&["publish", "gh"]).exec_path);
        assert!(!lookup(&["publish", "forge_timeout_secs"]).exec_path);
        let builder = |key: &str| lookup(&["runtime", "kubernetes", "builder", key]);
        assert!(builder("buildctl").exec_path);
        for key in ["tls_dir", "docker_config", "do_token_file"] {
            let spec = builder(key);
            assert!(spec.fs_path && !spec.secret && !spec.exec_path, "{key}");
        }
        assert!(lookup(&["embed", "api_key_file"]).fs_path);
        let args = lookup(&["consulta", "args"]);
        assert!(args.exec_path && args.path == Some(PathKind::ExecArgs));
        assert_eq!(
            lookup(&["providers", "openrouter", "models", "id"]).key,
            "providers.*.models.id"
        );
        assert_eq!(
            lookup(&["github", "dispatch", "owner/name"]).key,
            "github.dispatch.*"
        );
        // A whole table answers for what is under it.
        assert_eq!(lookup(&["repo"]).apply, Apply::Live);
        assert_eq!(lookup(&["providers", "openrouter"]).apply, Apply::Restart);
        assert!(lookup(&["publish"]).exec_path);
        // A key nobody reads is kept out of the log.
        let unknown = lookup(&["qa", "browser"]);
        assert!(unknown.secret && unknown.admin);
    }

    /// A key the path guard checks says so in both ways, and one that names
    /// a path says which rules hold it.
    #[test]
    fn every_path_key_says_which_rules_hold_it() {
        for spec in KEYS {
            match spec.path {
                Some(PathKind::Executable | PathKind::ExecArgs) => {
                    assert!(spec.exec_path && !spec.fs_path, "{}", spec.key)
                }
                Some(_) => assert!(spec.fs_path && !spec.exec_path, "{}", spec.key),
                None => assert!(!spec.exec_path && !spec.fs_path, "{}", spec.key),
            }
        }
    }

    #[test]
    fn every_write_is_an_admin_action() {
        assert!(KEYS.iter().all(|spec| spec.admin));
    }

    /// Records the key paths a type reads by deserializing it from a
    /// deserializer that offers every field it asks for.
    mod trace {
        use std::cell::RefCell;
        use std::collections::BTreeSet;

        use serde::de::{self, DeserializeSeed, IntoDeserializer, MapAccess, SeqAccess, Visitor};

        #[derive(Default)]
        struct Seen {
            keys: BTreeSet<String>,
            tables: BTreeSet<String>,
        }

        /// The leaf keys `T` reads: every field path, less those that are
        /// themselves tables. A map's entries are `*`; an array's elements
        /// share the array's path.
        pub fn keys<T: de::DeserializeOwned>() -> BTreeSet<String> {
            let seen = RefCell::new(Seen::default());
            T::deserialize(Tracer {
                path: String::new(),
                seen: &seen,
            })
            .unwrap_or_else(|e| panic!("tracing Config failed: {e}"));
            let seen = seen.into_inner();
            seen.keys.difference(&seen.tables).cloned().collect()
        }

        #[derive(Debug)]
        struct Error(String);

        impl std::fmt::Display for Error {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl std::error::Error for Error {}
        impl de::Error for Error {
            fn custom<T: std::fmt::Display>(msg: T) -> Self {
                Error(msg.to_string())
            }
        }

        struct Tracer<'a> {
            path: String,
            seen: &'a RefCell<Seen>,
        }

        /// A string every string-shaped field accepts: a socket address
        /// parses from it, and a path or a name takes it as it is.
        const TEXT: &str = "127.0.0.1:1";

        impl<'de> de::Deserializer<'de> for Tracer<'_> {
            type Error = Error;

            fn deserialize_any<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_str(TEXT)
            }
            fn deserialize_bool<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_bool(false)
            }
            fn deserialize_i8<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_i64(0)
            }
            fn deserialize_i16<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_i64(0)
            }
            fn deserialize_i32<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_i64(0)
            }
            fn deserialize_i64<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_i64(0)
            }
            fn deserialize_u8<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_u64(0)
            }
            fn deserialize_u16<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_u64(0)
            }
            fn deserialize_u32<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_u64(0)
            }
            fn deserialize_u64<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_u64(0)
            }
            fn deserialize_f32<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_f64(0.0)
            }
            fn deserialize_f64<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_f64(0.0)
            }
            fn deserialize_char<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_char('x')
            }
            fn deserialize_str<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_str(TEXT)
            }
            fn deserialize_string<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_str(TEXT)
            }
            fn deserialize_bytes<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_bytes(b"")
            }
            fn deserialize_byte_buf<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_bytes(b"")
            }
            fn deserialize_option<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_some(self)
            }
            fn deserialize_unit<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_unit()
            }
            fn deserialize_unit_struct<V: Visitor<'de>>(
                self,
                _: &'static str,
                v: V,
            ) -> Result<V::Value, Error> {
                v.visit_unit()
            }
            fn deserialize_newtype_struct<V: Visitor<'de>>(
                self,
                _: &'static str,
                v: V,
            ) -> Result<V::Value, Error> {
                v.visit_newtype_struct(self)
            }
            fn deserialize_seq<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_seq(One(Some(self)))
            }
            fn deserialize_tuple<V: Visitor<'de>>(self, _: usize, v: V) -> Result<V::Value, Error> {
                v.visit_seq(One(Some(self)))
            }
            fn deserialize_tuple_struct<V: Visitor<'de>>(
                self,
                _: &'static str,
                _: usize,
                v: V,
            ) -> Result<V::Value, Error> {
                v.visit_seq(One(Some(self)))
            }
            fn deserialize_map<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                self.seen.borrow_mut().tables.insert(self.path.clone());
                v.visit_map(Fields::new(self, &["*"]))
            }
            fn deserialize_struct<V: Visitor<'de>>(
                self,
                _: &'static str,
                fields: &'static [&'static str],
                v: V,
            ) -> Result<V::Value, Error> {
                self.seen.borrow_mut().tables.insert(self.path.clone());
                v.visit_map(Fields::new(self, fields))
            }
            fn deserialize_enum<V: Visitor<'de>>(
                self,
                _: &'static str,
                variants: &'static [&'static str],
                v: V,
            ) -> Result<V::Value, Error> {
                v.visit_enum(IntoDeserializer::<Error>::into_deserializer(variants[0]))
            }
            fn deserialize_identifier<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_str(TEXT)
            }
            fn deserialize_ignored_any<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Error> {
                v.visit_unit()
            }
        }

        /// A sequence of one element, at the sequence's own path.
        struct One<'a>(Option<Tracer<'a>>);

        impl<'de> SeqAccess<'de> for One<'_> {
            type Error = Error;
            fn next_element_seed<T: DeserializeSeed<'de>>(
                &mut self,
                seed: T,
            ) -> Result<Option<T::Value>, Error> {
                self.0
                    .take()
                    .map(|tracer| seed.deserialize(tracer))
                    .transpose()
            }
        }

        /// A table offering each of `fields`, and recording each one's path.
        struct Fields<'a> {
            path: String,
            seen: &'a RefCell<Seen>,
            fields: std::slice::Iter<'static, &'static str>,
            current: &'static str,
        }

        impl<'a> Fields<'a> {
            fn new(tracer: Tracer<'a>, fields: &'static [&'static str]) -> Self {
                Fields {
                    path: tracer.path,
                    seen: tracer.seen,
                    fields: fields.iter(),
                    current: "",
                }
            }

            fn child(&self) -> String {
                match self.path.is_empty() {
                    true => self.current.to_string(),
                    false => format!("{}.{}", self.path, self.current),
                }
            }
        }

        impl<'de> MapAccess<'de> for Fields<'_> {
            type Error = Error;
            fn next_key_seed<K: DeserializeSeed<'de>>(
                &mut self,
                seed: K,
            ) -> Result<Option<K::Value>, Error> {
                let Some(field) = self.fields.next() else {
                    return Ok(None);
                };
                self.current = field;
                self.seen.borrow_mut().keys.insert(self.child());
                seed.deserialize(IntoDeserializer::<Error>::into_deserializer(*field))
                    .map(Some)
            }
            fn next_value_seed<V: DeserializeSeed<'de>>(
                &mut self,
                seed: V,
            ) -> Result<V::Value, Error> {
                seed.deserialize(Tracer {
                    path: self.child(),
                    seen: self.seen,
                })
            }
        }
    }
}
