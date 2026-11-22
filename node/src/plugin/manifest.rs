//! A plugin's `tracon-plugin.toml`, contract v1: what it is, what it runs,
//! the credentials it may be lent, the tools it offers, and what the
//! operator is shown about it before it is installed.
//!
//! Unknown keys are refused: the contract number versions the shape, so a
//! typo is a mistake to report, not a field to ignore.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The file at the root of a plugin archive.
pub const FILE: &str = "tracon-plugin.toml";

/// The contract this node speaks.
pub const CONTRACT: u32 = 1;

/// Arguments the node appends to `exec` itself.
const NODE_ARGS: &[&str] = &["--socket", "--data", "--node-socket"];

/// The methods a plugin may call on the node, on the socket the node serves
/// for each of its instances (`super::host`).
pub const NODE_METHODS: &[(&str, &str)] = &[
    (
        "approval/request",
        "raises approval cards for the operator, as itself",
    ),
    ("approval/status", "waits on the cards it raised"),
    ("notify", "sends the operator a push notification"),
    (
        "tools/call",
        "calls other plugins' tools, as itself, under policy",
    ),
];

/// Formats the node renders itself, which no plugin may own.
const RESERVED_FORMATS: &[&str] = &["markdown", "source"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub contract: u32,
    /// The program and its arguments, relative to the install directory.
    /// The node appends `--socket <path> --data <dir>`.
    pub exec: Vec<String>,
    #[serde(default, rename = "credential", skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<Credential>,
    #[serde(default, rename = "tool", skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<Ui>,
    /// Hosts it says it reaches: shown on cards, not enforced.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub egress: Vec<String>,
    /// Document formats it owns; each format has one owner on a node.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub formats: Vec<String>,
    /// The node's methods it calls (`NODE_METHODS`). One that names any is
    /// started with `--node-socket <path>` as well, and the node answers
    /// only these there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub node_methods: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    /// The broker credential it is lent by default.
    pub name: String,
    /// What other bindings it may be lent: `*` matches any run of characters.
    pub pattern: String,
    #[serde(default)]
    pub lend: Lend,
    /// The call argument that picks the one binding a call is lent:
    /// `<name>-<value>`, or `name` itself when the call leaves it out. Unset,
    /// a call is lent every binding the credential matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select: Option<String>,
}

/// When a credential is lent to a call.
/// Ordered from most to least freely lent: a later variant is stricter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lend {
    /// To any call the channel's policy lets through.
    #[default]
    Always,
    /// Only to a call dispatched from a card the operator approved.
    Approved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tool {
    pub name: String,
    /// What the plugin says the tool does. Shown on cards; it never decides
    /// anything.
    pub hint: Hint,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub consequential: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Hint {
    Read,
    Write,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ui {
    pub entry: String,
    pub title: String,
}

impl Credential {
    /// Whether a binding named `binding` is this credential: its own name,
    /// or one its pattern matches.
    pub fn matches(&self, binding: &str) -> bool {
        binding == self.name || crate::policy::glob(&self.pattern, binding)
    }

    /// Whether some binding name could match both: a name or the pattern of
    /// each.
    pub fn overlaps(&self, other: &Credential) -> bool {
        self.matches(&other.name)
            || other.matches(&self.name)
            || globs_overlap(&self.pattern, &other.pattern)
    }

    /// Whether a call may be lent more than one binding of it: its pattern
    /// matches more than its name (a pattern that is not the name, with or
    /// without `*`, matches the name and itself), and no `select` picks one.
    pub fn lends_many(&self) -> bool {
        self.select.is_none() && self.pattern != self.name
    }
}

impl Hint {
    pub fn as_str(self) -> &'static str {
        match self {
            Hint::Read => "read",
            Hint::Write => "write",
        }
    }
}

impl Lend {
    pub fn as_str(self) -> &'static str {
        match self {
            Lend::Always => "always",
            Lend::Approved => "approved",
        }
    }
}

/// Whether some name matches both `*` globs (`*` matching any run,
/// including none). A glob without `*` is the one name it matches. Two that
/// each hold a `*` share a name exactly when their literal prefixes (before
/// the first `*`) agree, one starting the other, and so do their suffixes
/// (after the last `*`): `P + M + N + S` matches both, where `P` and `S` are
/// the longer prefix and suffix and `M` and `N` each glob's middle pieces in
/// order, since each glob's first `*` takes what of `P` its own prefix
/// leaves, and its last `*` everything up to its own suffix.
fn globs_overlap(a: &str, b: &str) -> bool {
    let agree = |x: &str, y: &str| x.starts_with(y) || y.starts_with(x);
    let ends_agree = |x: &str, y: &str| x.ends_with(y) || y.ends_with(x);
    match (a.split_once('*'), b.split_once('*')) {
        (None, _) => crate::policy::glob(b, a),
        (_, None) => crate::policy::glob(a, b),
        (Some((a_prefix, _)), Some((b_prefix, _))) => {
            let (_, a_suffix) = a.rsplit_once('*').unwrap_or_default();
            let (_, b_suffix) = b.rsplit_once('*').unwrap_or_default();
            agree(a_prefix, b_prefix) && ends_agree(a_suffix, b_suffix)
        }
    }
}

/// `N.N.N`, optionally `-pre` and `+build` of letters, digits, `.` and `-`.
fn version_ok(version: &str) -> bool {
    let (core, build) = match version.split_once('+') {
        Some((core, build)) => (core, Some(build)),
        None => (version, None),
    };
    let (core, pre) = match core.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (core, None),
    };
    let numbers: Vec<&str> = core.split('.').collect();
    let tag_ok = |tag: &str| {
        !tag.is_empty()
            && tag
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    };
    numbers.len() == 3
        && numbers
            .iter()
            .all(|n| !n.is_empty() && n.len() <= 9 && n.bytes().all(|b| b.is_ascii_digit()))
        && pre.is_none_or(tag_ok)
        && build.is_none_or(tag_ok)
        && version.len() <= 64
}

fn tool_name_ok(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && !name.contains("__")
}

fn credential_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')
}

/// A call argument's name `select` may give: what a tool's input schema
/// names a property by, kept to name characters.
fn argument_ok(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn format_ok(format: &str) -> bool {
    (1..=32).contains(&format.len())
        && format
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// `host` or `host:port`, the host a DNS name or an IPv4 address.
fn egress_ok(entry: &str) -> bool {
    let (host, port) = match entry.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (entry, None),
    };
    let port_ok = port.is_none_or(|p| {
        !p.is_empty()
            && p.bytes().all(|b| b.is_ascii_digit())
            && p.parse::<u32>().is_ok_and(|n| (1..=65535).contains(&n))
    });
    // A leading `*.` names every host under a domain (a Jira Cloud site, say).
    // Egress is shown on cards, never enforced, so a pattern is enough; it
    // must still leave a domain of at least two labels.
    let host = match host.strip_prefix("*.") {
        Some(domain) if domain.contains('.') => domain,
        Some(_) => return false,
        None => host,
    };
    let host_ok = !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
    host_ok && port_ok
}

/// A path inside the archive: relative, plain names only, under `dir/`.
fn under(path: &str, dir: &str) -> bool {
    let path = Path::new(path);
    crate::bundle::plain(path) && path.starts_with(dir) && path != Path::new(dir)
}

impl Manifest {
    /// Parse and validate a manifest's text.
    pub fn parse(text: &str) -> Result<Self, String> {
        let manifest: Manifest = toml::from_str(text).map_err(|e| {
            // A top-level key written after `[ui]` lands inside it; say so
            // rather than leave the author to read TOML's rules.
            let table = text.parse::<toml::Table>().ok();
            let misplaced = table.as_ref().and_then(|t| {
                let ui = t.get("ui")?.as_table()?;
                ["egress", "formats", "node_methods"]
                    .into_iter()
                    .find(|key| ui.contains_key(*key))
            });
            match misplaced {
                Some(key) => format!(
                    "{FILE}: {key} is a top-level key; write it before the first [table], not under [ui]"
                ),
                // Name the line, which names the field: serde's own
                // message often does not.
                None => match e.span().map(|span| {
                    let line = text[..span.start].matches('\n').count() + 1;
                    let source = text.lines().nth(line - 1).unwrap_or("").trim();
                    format!("line {line} (`{source}`)")
                }) {
                    Some(at) => format!("{FILE}: {at}: {}", e.message().trim()),
                    None => format!("{FILE}: {}", e.message().trim()),
                },
            }
        })?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Every rule a manifest is held to that its text alone can show.
    pub fn validate(&self) -> Result<(), String> {
        let bad = |field: &str, why: String| Err(format!("{FILE}: {field}: {why}"));
        if !crate::config::namespace_name_ok(&self.name) {
            return bad(
                "name",
                format!(
                    "{:?} must be 1-24 lowercase letters, digits or '-', not starting with '-'",
                    self.name
                ),
            );
        }
        if !version_ok(&self.version) {
            return bad(
                "version",
                format!("{:?} is not a version (N.N.N[-pre][+build])", self.version),
            );
        }
        if self.contract != CONTRACT {
            return bad(
                "contract",
                format!("contract {}; this node speaks {CONTRACT}", self.contract),
            );
        }
        let Some(program) = self.exec.first() else {
            return bad("exec", "is empty; name the program under bin/".into());
        };
        if Path::new(program).is_absolute() {
            return bad(
                "exec",
                format!("{program:?} is absolute; a plugin runs a program it ships, under bin/"),
            );
        }
        if !under(program, "bin") {
            return bad(
                "exec",
                format!("{program:?} must be a plain path under bin/"),
            );
        }
        for arg in &self.exec {
            if arg.is_empty() || arg.contains('\0') {
                return bad(
                    "exec",
                    "has an empty argument or one with a NUL byte".into(),
                );
            }
            if NODE_ARGS
                .iter()
                .any(|own| arg == own || arg.starts_with(&format!("{own}=")))
            {
                return bad(
                    "exec",
                    format!("{arg:?} is the node's own to pass; leave it out"),
                );
            }
        }
        let mut seen = BTreeSet::new();
        for credential in &self.credentials {
            let field = format!("credential {:?}", credential.name);
            if credential.name.is_empty() || !credential.name.bytes().all(credential_char) {
                return bad(
                    &field,
                    "name must be letters, digits, '-', '_' or '.'".into(),
                );
            }
            if !seen.insert(credential.name.as_str()) {
                return bad(&field, "is declared twice".into());
            }
            if credential.pattern.is_empty()
                || !credential
                    .pattern
                    .bytes()
                    .all(|b| credential_char(b) || b == b'*')
            {
                return bad(
                    &field,
                    format!(
                        "pattern {:?} must be name characters and '*'",
                        credential.pattern
                    ),
                );
            }
            if let Some(select) = &credential.select {
                if !argument_ok(select) {
                    return bad(
                        &field,
                        format!(
                            "select {select:?} must be 1-64 letters, digits, '_' or '-': the call argument that names the binding"
                        ),
                    );
                }
            }
        }
        // A binding belongs to the first declared credential it matches, so
        // an earlier one lent more freely would lend a later, stricter one's
        // bindings on its own terms.
        for (i, later) in self.credentials.iter().enumerate() {
            if let Some(earlier) = self.credentials[..i]
                .iter()
                .find(|earlier| earlier.lend < later.lend && earlier.overlaps(later))
            {
                return Err(format!(
                    "{FILE}: credential `{}` (`{}`) shadows `{}` (`{}`, lend = {}): declare `{}` first or narrow `{}`",
                    earlier.name,
                    earlier.pattern,
                    later.name,
                    later.pattern,
                    later.lend.as_str(),
                    later.name,
                    earlier.name
                ));
            }
        }
        let mut seen = BTreeSet::new();
        for tool in &self.tools {
            let field = format!("tool {:?}", tool.name);
            if !tool_name_ok(&tool.name) {
                return bad(
                    &field,
                    "name must be 1-64 letters, digits, '_' or '-', without '__'".into(),
                );
            }
            if !seen.insert(tool.name.as_str()) {
                return bad(&field, "is declared twice".into());
            }
            // A session calls it `<plugin>__<tool>`, which a model's tool
            // name must hold.
            let offered = self.name.len() + 2 + tool.name.len();
            if offered > crate::mcp::upstream::MAX_TOOL_NAME {
                return bad(
                    &field,
                    format!(
                        "{}__{} is {offered} characters; a tool's offered name holds at most {}",
                        self.name,
                        tool.name,
                        crate::mcp::upstream::MAX_TOOL_NAME
                    ),
                );
            }
        }
        if let Some(ui) = &self.ui {
            if !under(&ui.entry, "ui") {
                return bad(
                    "ui.entry",
                    format!("{:?} must be a plain path under ui/", ui.entry),
                );
            }
            if ui.title.trim().is_empty() {
                return bad("ui.title", "is empty".into());
            }
        }
        for entry in &self.egress {
            if !egress_ok(entry) {
                return bad(
                    "egress",
                    format!("{entry:?} is not host or host:port (port 1-65535)"),
                );
            }
        }
        let mut seen = BTreeSet::new();
        for format in &self.formats {
            if !format_ok(format) {
                return bad(
                    "formats",
                    format!("{format:?} must be 1-32 lowercase letters, digits or '_'"),
                );
            }
            if RESERVED_FORMATS.contains(&format.as_str()) {
                return bad(
                    "formats",
                    format!("{format:?} is rendered by the node itself"),
                );
            }
            if !seen.insert(format.as_str()) {
                return bad("formats", format!("{format:?} is listed twice"));
            }
        }
        let mut seen = BTreeSet::new();
        for method in &self.node_methods {
            if !NODE_METHODS.iter().any(|(known, _)| known == method) {
                let known: Vec<&str> = NODE_METHODS.iter().map(|(m, _)| *m).collect();
                return bad(
                    "node_methods",
                    format!("{method:?} is not a node method ({})", known.join(", ")),
                );
            }
            if !seen.insert(method.as_str()) {
                return bad("node_methods", format!("{method:?} is listed twice"));
            }
        }
        Ok(())
    }

    /// Whether it may call the node's `method`.
    pub fn calls_node(&self, method: &str) -> bool {
        self.node_methods.iter().any(|m| m == method)
    }

    /// What the unpacked tree at `root` must hold for this manifest: the
    /// program, a regular executable file, and the UI's entry.
    pub fn validate_tree(&self, root: &Path) -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt as _;
        let program = &self.exec[0];
        match std::fs::symlink_metadata(root.join(program)) {
            Ok(meta) if meta.is_file() && meta.permissions().mode() & 0o111 != 0 => {}
            Ok(meta) if meta.is_file() => {
                return Err(format!("exec: {program} is not executable"));
            }
            _ => return Err(format!("exec: {program} is not a file in the archive")),
        }
        if let Some(ui) = &self.ui {
            if !std::fs::symlink_metadata(root.join(&ui.entry)).is_ok_and(|m| m.is_file()) {
                return Err(format!(
                    "ui.entry: {} is not a file in the archive",
                    ui.entry
                ));
            }
        }
        Ok(())
    }

    /// The archive a release publishes this build as, for `target`.
    pub fn asset_name(&self, target: &str) -> String {
        format!("{}-{}-{target}.tar.gz", self.name, self.version)
    }

    /// What the install card says about it, in Markdown.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        out.push_str("**Tools**\n\n");
        if self.tools.is_empty() {
            out.push_str("- none\n");
        }
        for tool in &self.tools {
            out.push_str(&format!(
                "- `{}__{}`: {}{}\n",
                self.name,
                tool.name,
                tool.hint.as_str(),
                if tool.consequential {
                    ", consequential"
                } else {
                    ""
                }
            ));
        }
        out.push_str("\n**Credentials**\n\n");
        if self.credentials.is_empty() {
            out.push_str("- none\n");
        }
        for credential in &self.credentials {
            out.push_str(&format!(
                "- `{}` (`{}`){}\n",
                credential.name,
                credential.pattern,
                match credential.lend {
                    Lend::Approved => ": lent only to a call from an approved card",
                    Lend::Always => "",
                }
            ));
            match &credential.select {
                Some(select) => out.push_str(&format!(
                    "  - a call is lent only the binding its `{select}` names: `{name}-<{select}>`, or `{name}` when it names none\n",
                    name = credential.name
                )),
                None if credential.lends_many() => out.push_str(&format!(
                    "  - **Warning:** every binding the channel holds matching {} is lent to each call, and the plugin picks one itself; it declares no `select` to lend only the one a call names\n",
                    // The name is lent too, when the pattern does not cover it.
                    if crate::policy::glob(&credential.pattern, &credential.name) {
                        format!("`{}`", credential.pattern)
                    } else {
                        format!("`{}` or `{}`", credential.name, credential.pattern)
                    }
                )),
                None => {}
            }
        }
        out.push_str("\n**UI**\n\n");
        match &self.ui {
            Some(ui) => out.push_str(&format!("- {} (`{}`)\n", ui.title, ui.entry)),
            None => out.push_str("- none\n"),
        }
        out.push_str("\n**Egress** (shown, not enforced)\n\n");
        if self.egress.is_empty() {
            out.push_str("- none declared\n");
        }
        for host in &self.egress {
            out.push_str(&format!("- `{host}`\n"));
        }
        out.push_str("\n**Formats**\n\n");
        if self.formats.is_empty() {
            out.push_str("- none\n");
        }
        for format in &self.formats {
            out.push_str(&format!("- `{format}`\n"));
        }
        out.push_str("\n**Calls the node**\n\n");
        if self.node_methods.is_empty() {
            out.push_str("- never\n");
        }
        for method in &self.node_methods {
            let what = NODE_METHODS
                .iter()
                .find(|(m, _)| m == method)
                .map_or("", |(_, what)| *what);
            out.push_str(&format!("- `{method}`: {what}\n"));
        }
        out
    }
}

/// What changed from `old` to `new`, a line each, for an update's card.
pub fn diff(old: &Manifest, new: &Manifest) -> Vec<String> {
    let mut lines = Vec::new();
    if old.version != new.version {
        lines.push(format!("version {} → {}", old.version, new.version));
    }
    if old.exec != new.exec {
        lines.push(format!(
            "exec `{}` → `{}`",
            old.exec.join(" "),
            new.exec.join(" ")
        ));
    }
    let shown_tool = |t: &Tool| {
        format!(
            "{}{}",
            t.hint.as_str(),
            if t.consequential {
                ", consequential"
            } else {
                ""
            }
        )
    };
    for tool in &new.tools {
        match old.tools.iter().find(|t| t.name == tool.name) {
            None => lines.push(format!("tool `{}` added ({})", tool.name, shown_tool(tool))),
            Some(was) if was != tool => lines.push(format!(
                "tool `{}`: {} → {}",
                tool.name,
                shown_tool(was),
                shown_tool(tool)
            )),
            Some(_) => {}
        }
    }
    for tool in &old.tools {
        if !new.tools.iter().any(|t| t.name == tool.name) {
            lines.push(format!("tool `{}` removed", tool.name));
        }
    }
    for credential in &new.credentials {
        match old.credentials.iter().find(|c| c.name == credential.name) {
            None => lines.push(format!(
                "credential `{}` added (`{}`, lent {})",
                credential.name,
                credential.pattern,
                credential.lend.as_str()
            )),
            Some(was) => {
                if was.pattern != credential.pattern {
                    lines.push(format!(
                        "credential `{}`: pattern `{}` → `{}`",
                        credential.name, was.pattern, credential.pattern
                    ));
                }
                if was.lend != credential.lend {
                    lines.push(format!(
                        "credential `{}`: lent {} → {}",
                        credential.name,
                        was.lend.as_str(),
                        credential.lend.as_str()
                    ));
                }
                if was.select != credential.select {
                    let shown = |s: &Option<String>| match s {
                        Some(arg) => format!("the binding `{arg}` names"),
                        None => "every matching binding".to_string(),
                    };
                    lines.push(format!(
                        "credential `{}`: a call is lent {} → {}",
                        credential.name,
                        shown(&was.select),
                        shown(&credential.select)
                    ));
                }
            }
        }
    }
    for credential in &old.credentials {
        if !new.credentials.iter().any(|c| c.name == credential.name) {
            lines.push(format!("credential `{}` removed", credential.name));
        }
    }
    match (&old.ui, &new.ui) {
        (None, Some(ui)) => lines.push(format!("UI added: {} (`{}`)", ui.title, ui.entry)),
        (Some(_), None) => lines.push("UI removed".into()),
        (Some(was), Some(ui)) if was != ui => lines.push(format!(
            "UI: {} (`{}`) → {} (`{}`)",
            was.title, was.entry, ui.title, ui.entry
        )),
        _ => {}
    }
    for (what, old, new) in [
        ("egress", &old.egress, &new.egress),
        ("format", &old.formats, &new.formats),
        ("node method", &old.node_methods, &new.node_methods),
    ] {
        for item in new.iter().filter(|i| !old.contains(i)) {
            lines.push(format!("{what} `{item}` added"));
        }
        for item in old.iter().filter(|i| !new.contains(i)) {
            lines.push(format!("{what} `{item}` removed"));
        }
    }
    lines
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn egress_takes_a_host_a_port_or_a_domain_wildcard() {
        for ok in [
            "db.internal",
            "db.internal:1521",
            "*.atlassian.net:443",
            "*.atlassian.net",
        ] {
            assert!(egress_ok(ok), "{ok}");
        }
        for bad in [
            "",
            "*",
            "*.net",
            "*.*.atlassian.net",
            "a*.atlassian.net",
            "jira.*.net",
            "db.internal:0",
            "db.internal:99999",
            "-db.internal",
        ] {
            assert!(!egress_ok(bad), "{bad}");
        }
    }

    /// The contract's own example, with `egress` where TOML keeps it
    /// top-level.
    pub(crate) const EXAMPLE: &str = r#"
name = "sql-review"
version = "0.1.0"
contract = 1
exec = ["bin/sql-review", "serve"]
egress = ["db.internal:1521"]

[[credential]]
name = "sql-read"
pattern = "sql-read-*"

[[credential]]
name = "sql-write"
pattern = "sql-write-*"
lend = "approved"

[[tool]]
name = "query"
hint = "read"

[[tool]]
name = "write"
hint = "write"
consequential = true

[ui]
entry = "ui/index.html"
title = "SQL"
"#;

    #[test]
    fn the_contract_example_parses() {
        let m = Manifest::parse(EXAMPLE).unwrap();
        assert_eq!(m.name, "sql-review");
        assert_eq!(m.exec, ["bin/sql-review", "serve"]);
        assert_eq!(m.credentials[1].lend, Lend::Approved);
        assert_eq!(m.credentials[0].lend, Lend::Always);
        assert_eq!(m.tools[0].hint, Hint::Read);
        assert!(m.tools[1].consequential);
        assert_eq!(m.ui.as_ref().unwrap().title, "SQL");
        assert_eq!(m.egress, ["db.internal:1521"]);
        assert_eq!(
            m.asset_name("x86_64-unknown-linux-gnu"),
            "sql-review-0.1.0-x86_64-unknown-linux-gnu.tar.gz"
        );
        // It survives the round trip the store gives it.
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<Manifest>(&json).unwrap(), m);
    }

    #[test]
    fn a_credential_matches_its_name_and_its_pattern() {
        let m = Manifest::parse(EXAMPLE).unwrap();
        let read = &m.credentials[0];
        assert!(read.matches("sql-read"));
        assert!(read.matches("sql-read-prod"));
        assert!(!read.matches("sql-write-prod"));
        assert!(!read.matches("sql-readx"));
    }

    fn refused(edit: impl Fn(&str) -> String) -> String {
        Manifest::parse(&edit(EXAMPLE)).unwrap_err()
    }

    #[test]
    fn each_rule_is_refused_naming_its_field() {
        let cases: &[(&str, &str, &str)] = &[
            ("name = \"sql-review\"", "name = \"SQL\"", "name:"),
            ("name = \"sql-review\"", "name = \"-sql\"", "name:"),
            ("version = \"0.1.0\"", "version = \"1.0\"", "version:"),
            (
                "contract = 1",
                "contract = 2",
                "contract 2; this node speaks 1",
            ),
            (
                "exec = [\"bin/sql-review\", \"serve\"]",
                "exec = []",
                "exec: is empty",
            ),
            (
                "exec = [\"bin/sql-review\", \"serve\"]",
                "exec = [\"/usr/bin/python3\"]",
                "is absolute",
            ),
            (
                "exec = [\"bin/sql-review\", \"serve\"]",
                "exec = [\"bin/../../x\"]",
                "plain path under bin/",
            ),
            (
                "exec = [\"bin/sql-review\", \"serve\"]",
                "exec = [\"sql-review\"]",
                "plain path under bin/",
            ),
            (
                "exec = [\"bin/sql-review\", \"serve\"]",
                "exec = [\"bin/sql-review\", \"--socket\", \"/tmp/x\"]",
                "the node's own",
            ),
            (
                "exec = [\"bin/sql-review\", \"serve\"]",
                "exec = [\"bin/sql-review\", \"--data=/x\"]",
                "the node's own",
            ),
            (
                "name = \"query\"",
                "name = \"write\"",
                "tool \"write\": is declared twice",
            ),
            ("name = \"query\"", "name = \"q__x\"", "tool \"q__x\""),
            (
                "name = \"query\"",
                &format!("name = \"{}\"", "q".repeat(60)),
                "holds at most 64",
            ),
            (
                "name = \"sql-write\"",
                "name = \"sql-read\"",
                "credential \"sql-read\": is declared twice",
            ),
            ("lend = \"approved\"", "lend = \"never\"", "lend"),
            (
                "lend = \"approved\"",
                "lend = \"approved\"\nselect = \"a.b\"",
                "credential \"sql-write\": select \"a.b\" must be",
            ),
            (
                "lend = \"approved\"",
                "lend = \"approved\"\nselect = \"\"",
                "credential \"sql-write\": select",
            ),
            ("hint = \"read\"", "hint = \"maybe\"", "hint"),
            (
                "entry = \"ui/index.html\"",
                "entry = \"index.html\"",
                "ui.entry:",
            ),
            (
                "entry = \"ui/index.html\"",
                "entry = \"ui/../bin/x\"",
                "ui.entry:",
            ),
            ("title = \"SQL\"", "title = \" \"", "ui.title:"),
            ("db.internal:1521", "db.internal:70000", "egress:"),
            ("db.internal:1521", "db internal", "egress:"),
            (
                "contract = 1",
                "contract = 1\nformats = [\"markdown\"]",
                "rendered by the node itself",
            ),
            (
                "contract = 1",
                "contract = 1\nformats = [\"Wiki\"]",
                "formats:",
            ),
            (
                "contract = 1",
                "contract = 1\ncolour = \"red\"",
                "unknown field",
            ),
            (
                "exec = [\"bin/sql-review\", \"serve\"]",
                "exec = [\"bin/sql-review\", \"--node-socket\", \"/x\"]",
                "the node's own",
            ),
            (
                "contract = 1",
                "contract = 1\nnode_methods = [\"approval/grant\"]",
                "node_methods: \"approval/grant\" is not a node method",
            ),
            (
                "contract = 1",
                "contract = 1\nnode_methods = [\"notify\", \"notify\"]",
                "node_methods: \"notify\" is listed twice",
            ),
        ];
        for (from, to, says) in cases {
            let error = refused(|text| text.replacen(from, to, 1));
            assert!(error.contains(says), "{to}: {error}");
            assert!(error.starts_with(FILE), "{error}");
        }
    }

    /// The example with its credentials replaced by `(name, pattern, lend)`.
    fn with_credentials(credentials: &[(&str, &str, &str)]) -> Result<Manifest, String> {
        let (head, rest) = EXAMPLE.split_once("[[credential]]").unwrap();
        let (_, tail) = rest.split_once("[[tool]]").unwrap();
        let declared: String = credentials
            .iter()
            .map(|(name, pattern, lend)| {
                format!("[[credential]]\nname = \"{name}\"\npattern = \"{pattern}\"\nlend = \"{lend}\"\n\n")
            })
            .collect();
        Manifest::parse(&format!("{head}{declared}[[tool]]{tail}"))
    }

    #[test]
    fn an_earlier_credential_lent_more_freely_may_not_shadow_a_later_one() {
        let refused = with_credentials(&[
            ("sql", "sql-*", "always"),
            ("sql-write", "sql-write-*", "approved"),
        ])
        .unwrap_err();
        assert_eq!(
            refused,
            "tracon-plugin.toml: credential `sql` (`sql-*`) shadows `sql-write` (`sql-write-*`, lend = approved): declare `sql-write` first or narrow `sql`"
        );
        // Declared the other way round, each binding goes where it should.
        let m = with_credentials(&[
            ("sql-write", "sql-write-*", "approved"),
            ("sql", "sql-*", "always"),
        ])
        .unwrap();
        assert_eq!(m.credentials[0].lend, Lend::Approved);
        // Disjoint patterns may come in any order.
        with_credentials(&[
            ("sql-read", "sql-read-*", "always"),
            ("sql-write", "sql-write-*", "approved"),
        ])
        .unwrap();
        // So may overlapping ones lent alike.
        with_credentials(&[
            ("sql", "sql-*", "approved"),
            ("sql-write", "sql-write-*", "approved"),
        ])
        .unwrap();
        // A name the earlier pattern matches is shadowed too, whatever the
        // later pattern.
        assert!(
            with_credentials(&[("any", "admin", "always"), ("admin", "root-*", "approved"),])
                .unwrap_err()
                .contains("credential `any` (`admin`) shadows `admin`")
        );
        // The first earlier credential that shadows is named.
        assert!(with_credentials(&[
            ("read", "read-*", "always"),
            ("everything", "*", "always"),
            ("write", "write-*", "approved"),
        ])
        .unwrap_err()
        .contains("credential `everything` (`*`) shadows `write`"));
    }

    #[test]
    fn globs_overlap_when_some_name_matches_both() {
        for (a, b) in [
            ("sql-*", "sql-write-*"),
            ("*", "anything"),
            ("*-prod", "sql-*"),
            ("sql-*-prod", "sql-write-*"),
            ("a*x*b", "a*y*b"),
            ("ab*", "*ba"),
            ("sql-*-eu-*", "*-prod"),
            ("same", "same"),
            ("sql-*-prod", "sql-write-prod"),
            ("**", "x"),
        ] {
            assert!(globs_overlap(a, b), "{a} {b}");
            assert!(globs_overlap(b, a), "{b} {a}");
        }
        for (a, b) in [
            ("sql-read-*", "sql-write-*"),
            ("*-prod", "*-test"),
            ("sql-*-prod", "sql-*-test"),
            ("a*b*c", "x*"),
            ("exact", "other"),
            ("sql-*-prod", "sql-write-test"),
            ("sql-*", "sq"),
        ] {
            assert!(!globs_overlap(a, b), "{a} {b}");
            assert!(!globs_overlap(b, a), "{b} {a}");
        }
    }

    /// Checks `globs_overlap` against a search of every short name.
    #[test]
    fn globs_overlap_agrees_with_trying_every_short_name() {
        let alphabet = ["", "a", "b"];
        let mut globs = vec![String::new()];
        for _ in 0..4 {
            globs = globs
                .iter()
                .flat_map(|g| ["a", "b", "*"].map(|c| format!("{g}{c}")))
                .chain(globs.iter().cloned())
                .collect();
        }
        globs.retain(|g| !g.is_empty());
        globs.sort();
        globs.dedup();
        // Any common name is at most as long as both globs' literals
        // together, which is under 9 here.
        let mut names = vec![String::new()];
        for _ in 0..8 {
            names = names
                .iter()
                .flat_map(|n| alphabet.map(|c| format!("{n}{c}")))
                .collect();
            names.sort();
            names.dedup();
        }
        for a in &globs {
            for b in &globs {
                let some = names
                    .iter()
                    .any(|n| crate::policy::glob(a, n) && crate::policy::glob(b, n));
                assert_eq!(globs_overlap(a, b), some, "{a} {b}");
            }
        }
    }

    #[test]
    fn a_top_level_key_under_ui_is_pointed_out() {
        let text = EXAMPLE
            .replace("egress = [\"db.internal:1521\"]\n", "")
            .replace("title = \"SQL\"", "title = \"SQL\"\negress = [\"x:1\"]");
        let error = Manifest::parse(&text).unwrap_err();
        assert!(error.contains("egress is a top-level key"), "{error}");
    }

    #[test]
    fn versions_are_semver_shaped() {
        for ok in ["0.1.0", "1.2.3-rc.1", "1.2.3+build.5", "10.0.0-alpha+x"] {
            assert!(version_ok(ok), "{ok}");
        }
        for bad in [
            "1", "1.2", "1.2.3.4", "v1.2.3", "1.2.3-", "1.2.3/x", "", "1..3",
        ] {
            assert!(!version_ok(bad), "{bad}");
        }
    }

    #[test]
    fn the_tree_must_hold_the_program_and_the_ui() {
        use std::os::unix::fs::PermissionsExt as _;
        let m = Manifest::parse(EXAMPLE).unwrap();
        let dir = tempfile::tempdir().unwrap();
        assert!(m.validate_tree(dir.path()).unwrap_err().contains("exec:"));
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        std::fs::create_dir_all(dir.path().join("ui")).unwrap();
        let program = dir.path().join("bin/sql-review");
        std::fs::write(&program, b"x").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(m
            .validate_tree(dir.path())
            .unwrap_err()
            .contains("not executable"));
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(m
            .validate_tree(dir.path())
            .unwrap_err()
            .contains("ui.entry"));
        std::fs::write(dir.path().join("ui/index.html"), b"<p>").unwrap();
        m.validate_tree(dir.path()).unwrap();
    }

    #[test]
    fn a_diff_says_what_an_update_changes() {
        let old = Manifest::parse(EXAMPLE).unwrap();
        let mut new = old.clone();
        new.version = "0.2.0".into();
        new.tools.push(Tool {
            name: "explain".into(),
            hint: Hint::Read,
            consequential: false,
        });
        new.tools.retain(|t| t.name != "write");
        new.credentials[0].lend = Lend::Approved;
        new.egress = vec!["db.internal:1522".into()];
        let lines = diff(&old, &new);
        for expected in [
            "version 0.1.0 → 0.2.0",
            "tool `explain` added (read)",
            "tool `write` removed",
            "credential `sql-read`: lent always → approved",
            "egress `db.internal:1522` added",
            "egress `db.internal:1521` removed",
        ] {
            assert!(lines.iter().any(|l| l == expected), "{expected}: {lines:?}");
        }
        assert!(diff(&old, &old).is_empty());
    }

    #[test]
    fn node_methods_are_shown_and_their_changes_diffed() {
        let old = Manifest::parse(EXAMPLE).unwrap();
        assert!(old.node_methods.is_empty());
        assert!(old.summary().contains("**Calls the node**\n\n- never\n"));
        let text = EXAMPLE.replace(
            "contract = 1",
            "contract = 1\nnode_methods = [\"approval/request\", \"tools/call\"]",
        );
        let new = Manifest::parse(&text).unwrap();
        assert!(new.calls_node("tools/call"));
        assert!(!new.calls_node("notify"));
        let summary = new.summary();
        assert!(
            summary.contains("- `approval/request`: raises approval cards for the operator"),
            "{summary}"
        );
        assert!(summary.contains("- `tools/call`: calls other plugins' tools"));
        let lines = diff(&old, &new);
        assert!(
            lines.iter().any(|l| l == "node method `tools/call` added"),
            "{lines:?}"
        );
        assert!(diff(&new, &old)
            .iter()
            .any(|l| l == "node method `approval/request` removed"));
        // The store's round trip keeps them, and an older row without the
        // key reads as calling nothing.
        let json = serde_json::to_string(&new).unwrap();
        assert_eq!(serde_json::from_str::<Manifest>(&json).unwrap(), new);
        assert!(!serde_json::to_string(&old)
            .unwrap()
            .contains("node_methods"));
    }

    #[test]
    fn the_summary_marks_what_is_lent_only_on_approval() {
        let summary = Manifest::parse(EXAMPLE).unwrap().summary();
        assert!(summary.contains("`sql-review__write`: write, consequential"));
        assert!(summary
            .contains("`sql-write` (`sql-write-*`): lent only to a call from an approved card"));
        assert!(summary.contains("SQL (`ui/index.html`)"));
        assert!(summary.contains("`db.internal:1521`"));
    }

    #[test]
    fn the_summary_warns_of_a_pattern_lent_whole_and_says_what_select_lends() {
        let summary = Manifest::parse(EXAMPLE).unwrap().summary();
        assert!(
            summary.contains(
                "**Warning:** every binding the channel holds matching `sql-write` or `sql-write-*` is lent to each call"
            ),
            "{summary}"
        );
        let text = EXAMPLE.replace(
            "lend = \"approved\"",
            "lend = \"approved\"\nselect = \"profile\"",
        );
        let selecting = Manifest::parse(&text).unwrap();
        assert_eq!(selecting.credentials[1].select.as_deref(), Some("profile"));
        let summary = selecting.summary();
        assert!(summary.contains(
            "a call is lent only the binding its `profile` names: `sql-write-<profile>`, or `sql-write` when it names none"
        ), "{summary}");
        assert!(!summary.contains("`sql-write-*` is lent"), "{summary}");
        // `sql-read` still selects nothing.
        assert!(
            summary.contains("matching `sql-read` or `sql-read-*` is lent"),
            "{summary}"
        );
        // A credential that matches only its name lends one binding.
        let exact = Manifest::parse(&EXAMPLE.replace("sql-read-*", "sql-read")).unwrap();
        assert!(!exact.summary().contains("matching `sql-read"));
        // A pattern without `*` that is not the name still lends two.
        let two = Manifest::parse(&EXAMPLE.replace("sql-read-*", "sql-read-qa")).unwrap();
        assert!(two
            .summary()
            .contains("matching `sql-read` or `sql-read-qa` is lent"));
        // A pattern that covers the name is named alone.
        let covers = Manifest::parse(&EXAMPLE.replace("sql-read-*", "sql-read*")).unwrap();
        assert!(covers.summary().contains("matching `sql-read*` is lent"));
        let old = Manifest::parse(EXAMPLE).unwrap();
        assert!(diff(&old, &selecting).iter().any(|l| l
            == "credential `sql-write`: a call is lent every matching binding → the binding `profile` names"));
        // The store's round trip keeps it, and leaves it out when unset.
        let json = serde_json::to_string(&selecting).unwrap();
        assert_eq!(serde_json::from_str::<Manifest>(&json).unwrap(), selecting);
        assert!(!serde_json::to_string(&old).unwrap().contains("select"));
    }
}
