//! The product brief: who the work is for, what their problem is, what was
//! read to reach that belief, what bounds the answer, what would make it good,
//! and what is still open.
//!
//! Three properties are the point, and each one is a property of the record
//! rather than of the prose written into it.
//!
//! *It is optional.* A brief is a document a work item may point at. An item
//! without one is not incomplete, no phase requires one, and nothing here is
//! reachable from a session that never asks for it.
//!
//! *It says who said so.* Every line carries one of `observed` (the customer
//! said or did this), `inferred` (someone reasoned to it) or `decided` (the
//! operator chose it); a line written without a marker reads back as
//! `unattributed` and is never quietly promoted to one of the three. The node
//! enforces the two rules an agent cannot be trusted to keep about itself: a
//! session may not record a decision, because deciding is the operator's, and
//! a session may not record an observation with nothing to point at, because
//! an observation no one can check is an inference.
//!
//! *It points at evidence rather than holding it.* A line ends in `[doc:slug]`,
//! `[session:id]`, `[evidence:id]`, `[work:id]`, `[url:…]` or `[file:path]`.
//! Reading a brief resolves those against what this node holds and says which
//! ones it has never seen, the way the ledger says so about an unknown
//! dependency. Nothing is copied into the brief and no second store is
//! introduced to hold it.
//!
//! *A line may say what points at it.* Under a line, indented, are the checks,
//! scenarios and observations that would settle it — the same provenance
//! vocabulary one level down, so the standing of a link is carried by the
//! machinery that already enforces it: an agent's link is `inferred`, the
//! operator's is `decided`, and a session writing either a decision or an
//! uncited observation is refused here as it is one level up. What is made of
//! those links — which criterion is covered, which is only a proposal — is
//! `corpus::criteria`'s reading, not this module's.
//!
//! The document is the record. It is Markdown, it round-trips through this
//! module, and it is readable — and editable — with no node running: the
//! structure here is a reading of the file, not a schema the file depends on.

use serde::{Deserialize, Serialize};

use crate::mcp::docs;
use crate::store::{Store, StoreError};
use crate::stream::Bus;

/// The six sections, in the order a brief is read and written.
pub const FIELDS: &[Field] = &[
    Field::IntendedUser,
    Field::Problem,
    Field::SourceReferences,
    Field::Constraints,
    Field::SuccessCriteria,
    Field::UnresolvedQuestions,
];

/// A brief this long is a document that lost its shape; the operator is told
/// rather than having it silently truncated into one.
pub const MAX_BYTES: usize = 64 * 1024;
/// One line's text. Generous: a criterion is a sentence, not an essay.
const MAX_TEXT: usize = 2000;
const MAX_ENTRIES: usize = 400;
const MAX_REFS: usize = 12;
/// How many things one line may say point at it. A criterion that needs a
/// dozen checks is a criterion that is really several.
pub const MAX_LINKS: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    IntendedUser,
    Problem,
    SourceReferences,
    Constraints,
    SuccessCriteria,
    UnresolvedQuestions,
}

impl Field {
    /// The heading this section is written under.
    pub fn heading(self) -> &'static str {
        match self {
            Field::IntendedUser => "Intended user",
            Field::Problem => "Problem",
            Field::SourceReferences => "Source references",
            Field::Constraints => "Constraints",
            Field::SuccessCriteria => "Success criteria",
            Field::UnresolvedQuestions => "Unresolved questions",
        }
    }

    /// The name the API and the tools use.
    pub fn key(self) -> &'static str {
        match self {
            Field::IntendedUser => "intended_user",
            Field::Problem => "problem",
            Field::SourceReferences => "source_references",
            Field::Constraints => "constraints",
            Field::SuccessCriteria => "success_criteria",
            Field::UnresolvedQuestions => "unresolved_questions",
        }
    }

    /// What an empty section means, said in the operator's terms rather than
    /// as a missing field.
    pub fn absent(self) -> &'static str {
        match self {
            Field::IntendedUser => "no intended user named",
            Field::Problem => "no problem stated",
            Field::SourceReferences => "nothing cited",
            Field::Constraints => "no constraints stated",
            Field::SuccessCriteria => "no success criteria stated",
            Field::UnresolvedQuestions => "no open questions recorded",
        }
    }

    /// Match a heading or an API key, however it was cased or punctuated.
    pub fn parse(text: &str) -> Option<Field> {
        let want = normalize(text);
        FIELDS
            .iter()
            .copied()
            .find(|f| normalize(f.heading()) == want || normalize(f.key()) == want)
    }
}

/// Lowercase, alphanumerics and single spaces: `Success criteria`,
/// `success_criteria` and `SUCCESS  CRITERIA` are one heading. Also how a
/// criterion is named from its own text, in `corpus::criteria`: two lines that
/// differ only in punctuation or case are the same line.
pub fn normalize(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
        } else if !out.ends_with(' ') {
            out.push(' ');
        }
    }
    out.trim().to_string()
}

/// Who is behind a line, which is not the same question as who typed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// The customer said or did this.
    Observed,
    /// Someone reasoned to it from something else.
    Inferred,
    /// The operator chose it.
    Decided,
    /// Written with no marker. Kept as it is: an unmarked line is not
    /// evidence, and it is not a decision either.
    Unattributed,
}

impl Provenance {
    pub fn marker(self) -> Option<&'static str> {
        match self {
            Provenance::Observed => Some("observed"),
            Provenance::Inferred => Some("inferred"),
            Provenance::Decided => Some("decided"),
            Provenance::Unattributed => None,
        }
    }

    pub fn parse(word: &str) -> Option<Provenance> {
        match word.trim().to_ascii_lowercase().as_str() {
            "observed" | "observation" => Some(Provenance::Observed),
            "inferred" | "inference" => Some(Provenance::Inferred),
            "decided" | "decision" => Some(Provenance::Decided),
            _ => None,
        }
    }
}

/// The kinds of thing a line may point at. Unknown kinds are left in the
/// text: a bracket the node cannot resolve is prose, not a broken link.
pub const REF_KINDS: &[&str] = &["doc", "session", "evidence", "work", "url", "file"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ref {
    pub kind: String,
    pub value: String,
    /// Filled in on read: the document's or item's title, when this node
    /// holds it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub label: Option<String>,
    /// `Some(false)` is "this node has never seen it", which is said rather
    /// than hidden. `None` is "not this node's to know" — a URL or a path.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub known: Option<bool>,
}

impl Ref {
    pub fn new(kind: &str, value: &str) -> Ref {
        Ref {
            kind: kind.to_string(),
            value: value.to_string(),
            label: None,
            known: None,
        }
    }

    fn render(&self) -> String {
        format!("[{}:{}]", self.kind, self.value)
    }
}

/// What a line may say points at it. A `check` is a command the operator
/// configured, and is the only kind that can produce a result on its own. A
/// `scenario` names a customer task that would exercise the line; this node
/// holds no scenario records yet, and says so rather than pretending. An
/// `observation` is what someone was seen to do, and is context for a
/// person's judgement rather than a result.
pub const LINK_KINDS: &[&str] = &["check", "scenario", "observation"];

/// One thing a line points at, written indented under it. Its provenance is
/// the same question as the line's, one level down: `decided` is the
/// operator's, `inferred` is somebody's proposal, `observed` is what was seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub provenance: Provenance,
    /// One of [`LINK_KINDS`].
    pub kind: String,
    /// The command, scenario name, or what was observed.
    pub value: String,
    #[serde(default)]
    pub refs: Vec<Ref>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub provenance: Provenance,
    pub text: String,
    #[serde(default)]
    pub refs: Vec<Ref>,
    /// What points at this line: its checks, scenarios and observations.
    #[serde(default)]
    pub links: Vec<Link>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    pub field: Field,
    pub heading: String,
    /// Prose written under the heading that is not a line of its own.
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub entries: Vec<Entry>,
}

/// A brief as this module reads it. `extra` is whatever was written under
/// headings this module does not know: kept verbatim, rendered back at the
/// end, never dropped because it did not fit the shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Brief {
    pub title: String,
    #[serde(default)]
    pub preamble: String,
    pub sections: Vec<Section>,
    #[serde(default)]
    pub extra: String,
}

impl Brief {
    /// An empty brief: the six headings and nothing claimed under them.
    pub fn empty(title: &str) -> Brief {
        Brief {
            title: title.trim().to_string(),
            preamble: String::new(),
            sections: FIELDS
                .iter()
                .map(|f| Section {
                    field: *f,
                    heading: f.heading().to_string(),
                    notes: String::new(),
                    entries: Vec::new(),
                })
                .collect(),
            extra: String::new(),
        }
    }

    pub fn section(&self, field: Field) -> Option<&Section> {
        self.sections.iter().find(|s| s.field == field)
    }

    /// The section, adding it if a parse did not find its heading. Public so
    /// `corpus::criteria` can write a link under a line it found; every line it
    /// writes still goes through [`check_link`] first.
    pub fn section_mut(&mut self, field: Field) -> &mut Section {
        if let Some(ix) = self.sections.iter().position(|s| s.field == field) {
            return &mut self.sections[ix];
        }
        self.sections.push(Section {
            field,
            heading: field.heading().to_string(),
            notes: String::new(),
            entries: Vec::new(),
        });
        let last = self.sections.len() - 1;
        &mut self.sections[last]
    }

    /// Put the sections back in the canonical order, adding any that a parse
    /// did not find, so a read of a hand-edited document still answers about
    /// every field.
    fn complete(mut self) -> Brief {
        let mut ordered: Vec<Section> = Vec::with_capacity(FIELDS.len());
        for field in FIELDS {
            let found = self
                .sections
                .iter()
                .position(|s| s.field == *field)
                .map(|ix| self.sections.remove(ix));
            ordered.push(found.unwrap_or(Section {
                field: *field,
                heading: field.heading().to_string(),
                notes: String::new(),
                entries: Vec::new(),
            }));
        }
        self.sections = ordered;
        self
    }

    /// The document, as this module writes it. Canonical: parsing a brief and
    /// rendering it again produces the same file.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("# {}\n", heading_title(&self.title)));
        if !self.preamble.trim().is_empty() {
            out.push_str(&format!("\n{}\n", self.preamble.trim_end()));
        }
        for section in &self.sections {
            out.push_str(&format!("\n## {}\n", section.heading));
            if !section.notes.trim().is_empty() {
                out.push_str(&format!("\n{}\n", section.notes.trim_end()));
            }
            if section.entries.is_empty() {
                out.push_str(&format!("\n_{}_\n", section.field.absent()));
                continue;
            }
            out.push('\n');
            for entry in &section.entries {
                out.push_str(&render_entry(entry));
                out.push('\n');
            }
        }
        if !self.extra.trim().is_empty() {
            out.push_str(&format!("\n{}\n", self.extra.trim_end()));
        }
        out
    }

    /// Read a document body. Tolerant on purpose: this file is meant to be
    /// edited by hand, and text that does not fit the shape is kept, not
    /// refused. Prose written under a heading is kept as the section's notes;
    /// the canonical render puts it above that section's lines, so a document
    /// with prose below its lines is reordered by a round trip, never cut.
    pub fn parse(body: &str) -> Brief {
        let mut brief = Brief {
            title: String::new(),
            preamble: String::new(),
            sections: Vec::new(),
            extra: String::new(),
        };
        // Where the lines being read belong: the preamble, one of the six
        // sections, or an unknown heading's text.
        enum At {
            Preamble,
            Known(Field),
            Unknown,
        }
        let mut at = At::Preamble;
        let mut preamble: Vec<String> = Vec::new();
        let mut extra: Vec<String> = Vec::new();
        for line in body.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("# ") {
                if brief.title.is_empty() {
                    brief.title = strip_title_prefix(rest);
                    continue;
                }
            }
            if let Some(rest) = trimmed.strip_prefix("## ") {
                match Field::parse(rest) {
                    Some(field) => {
                        let section = brief.section_mut(field);
                        section.heading = rest.trim().to_string();
                        at = At::Known(field);
                    }
                    None => {
                        at = At::Unknown;
                        extra.push(line.trim_end().to_string());
                    }
                }
                continue;
            }
            match at {
                At::Preamble => preamble.push(line.trim_end().to_string()),
                At::Unknown => extra.push(line.trim_end().to_string()),
                At::Known(field) => {
                    // An indented bullet naming a link kind belongs to the
                    // line above it, not beside it.
                    let link = line
                        .starts_with([' ', '\t'])
                        .then(|| parse_link(trimmed))
                        .flatten()
                        .filter(|_| {
                            brief.section(field).is_some_and(|s| !s.entries.is_empty())
                        });
                    if let Some(link) = link {
                        if let Some(entry) = brief.section_mut(field).entries.last_mut() {
                            entry.links.push(link);
                        }
                    } else if let Some(entry) = parse_entry(trimmed) {
                        brief.section_mut(field).entries.push(entry);
                    } else if is_absence_marker(trimmed, field) {
                        // What `render` writes for an empty section; reading
                        // it back as prose would grow the file every time.
                    } else {
                        let section = brief.section_mut(field);
                        let notes = &mut section.notes;
                        if !trimmed.is_empty() || !notes.is_empty() {
                            notes.push_str(line.trim_end());
                            notes.push('\n');
                        }
                    }
                }
            }
        }
        brief.preamble = join_prose(&preamble);
        brief.extra = join_prose(&extra);
        for section in &mut brief.sections {
            section.notes = section.notes.trim_end().to_string();
        }
        brief.complete()
    }
}

fn heading_title(title: &str) -> String {
    let title = title.trim();
    if title.is_empty() {
        "Brief".to_string()
    } else {
        format!("Brief: {title}")
    }
}

fn strip_title_prefix(heading: &str) -> String {
    let heading = heading.trim();
    let lower = heading.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("brief:") {
        return heading[heading.len() - rest.len()..].trim().to_string();
    }
    heading.to_string()
}

fn join_prose(lines: &[String]) -> String {
    lines.join("\n").trim().to_string()
}

fn is_absence_marker(line: &str, field: Field) -> bool {
    line.strip_prefix('_')
        .and_then(|rest| rest.strip_suffix('_'))
        .is_some_and(|inner| inner == field.absent())
}

fn render_entry(entry: &Entry) -> String {
    let mut line = String::from("- ");
    if let Some(marker) = entry.provenance.marker() {
        line.push_str(marker);
        line.push_str(": ");
    }
    line.push_str(entry.text.trim());
    for r in &entry.refs {
        line.push(' ');
        line.push_str(&r.render());
    }
    // Two spaces: what points at the line is written under it, so the
    // document reads as the structure rather than needing a table beside it.
    for link in &entry.links {
        line.push_str("\n  - ");
        if let Some(marker) = link.provenance.marker() {
            line.push_str(marker);
            line.push(' ');
        }
        line.push_str(link.kind.trim());
        line.push_str(": ");
        line.push_str(link.value.trim());
        for r in &link.refs {
            line.push(' ');
            line.push_str(&r.render());
        }
    }
    line
}

/// `- observed: they escalate by hand [doc:meeting-ops] [url:https://…]`
fn parse_entry(line: &str) -> Option<Entry> {
    let rest = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))?
        .trim();
    let (provenance, text) = match rest.split_once(':') {
        Some((word, after)) => match Provenance::parse(word) {
            Some(p) => (p, after.trim()),
            None => (Provenance::Unattributed, rest),
        },
        None => (Provenance::Unattributed, rest),
    };
    let (text, refs) = take_refs(text);
    if text.is_empty() && refs.is_empty() {
        return None;
    }
    Some(Entry {
        provenance,
        text,
        refs,
        links: Vec::new(),
    })
}

/// `  - inferred check: cargo test -p node triage`, indented under the line it
/// points at. Only an indented bullet naming one of [`LINK_KINDS`] is read as
/// a link: an indented bullet saying anything else is read as the flat line it
/// would have been read as before links existed, because inventing a nesting
/// for it would guess at what someone meant and would not survive a round
/// trip through the canonical render.
fn parse_link(line: &str) -> Option<Link> {
    let rest = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))?
        .trim();
    let (head, value) = rest.split_once(':')?;
    let mut words = head.split_whitespace();
    let first = words.next()?;
    // The kind is read first, so a lone `observation:` is the kind and not the
    // provenance word `observation` that [`Provenance::parse`] also accepts.
    // One word is the kind; two are a provenance and then the kind; anything
    // else, including `decided: …` — which is an indented *line*, not a link —
    // is left to `parse_entry`.
    let (provenance, kind) = match words.next() {
        Some(kind) => (Provenance::parse(first)?, kind),
        None => (Provenance::Unattributed, first),
    };
    if words.next().is_some() {
        return None;
    }
    let kind = kind.to_ascii_lowercase();
    if !LINK_KINDS.contains(&kind.as_str()) {
        return None;
    }
    let (value, refs) = take_refs(value.trim());
    if value.is_empty() {
        return None;
    }
    Some(Link {
        provenance,
        kind,
        value,
        refs,
    })
}

/// Peel `[kind:value]` off the end while the kind is one this module knows.
fn take_refs(text: &str) -> (String, Vec<Ref>) {
    let mut text = text.trim_end();
    let mut refs: Vec<Ref> = Vec::new();
    while let Some(open) = text.rfind('[') {
        if !text.ends_with(']') {
            break;
        }
        let inner = &text[open + 1..text.len() - 1];
        let Some((kind, value)) = inner.split_once(':') else {
            break;
        };
        let kind = kind.trim();
        if !REF_KINDS.contains(&kind) || value.trim().is_empty() {
            break;
        }
        refs.push(Ref::new(kind, value.trim()));
        text = text[..open].trim_end();
    }
    refs.reverse();
    (text.trim().to_string(), refs)
}

/// How many lines of each kind the brief holds, so the interface can say what
/// a brief rests on without counting prose.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub observed: usize,
    pub inferred: usize,
    pub decided: usize,
    pub unattributed: usize,
}

/// A brief as a reader gets it: the document it lives in, its structure, what
/// it rests on, and which sections say nothing yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BriefView {
    pub channel: String,
    pub slug: String,
    /// The item this brief is linked from, when it is linked from one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_item_id: Option<String>,
    /// The `If-Match` value for the next edit.
    pub hash: String,
    pub updated_ms: i64,
    #[serde(flatten)]
    pub brief: Brief,
    pub counts: Counts,
    /// The sections with no lines in them, by key, and what that means.
    pub absent: Vec<AbsentField>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbsentField {
    pub field: Field,
    pub heading: String,
    pub says: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BriefError {
    #[error("no work item {0}")]
    MissingItem(String),
    #[error("{0} has no brief")]
    NoBrief(String),
    #[error("a brief line needs text")]
    EmptyText,
    #[error("a brief line is longer than {MAX_TEXT} characters")]
    LongText,
    #[error("a brief holds at most {MAX_ENTRIES} lines")]
    TooManyEntries,
    #[error("a brief line points at at most {MAX_REFS} things")]
    TooManyRefs,
    #[error("a brief line may say at most {MAX_LINKS} things point at it")]
    TooManyLinks,
    #[error("{0:?} is not one of {LINK_KINDS:?}")]
    LinkKind(String),
    #[error("a link needs a value: the command, the scenario, or what was observed")]
    EmptyLink,
    #[error("{0:?} is not one of {REF_KINDS:?}")]
    RefKind(String),
    #[error("a [url:…] reference must be http or https")]
    RefUrl,
    #[error("a reference cannot be empty or contain ']' or a newline")]
    RefValue,
    #[error("no section named {0:?}")]
    Field(String),
    #[error("the brief is longer than {MAX_BYTES} bytes")]
    TooLong,
    #[error(
        "a session may not record an operator decision; record it as `inferred` and say it is a proposal"
    )]
    SessionDecision,
    #[error(
        "a session may only record an observation that points at something: add [doc:…], [url:…], [evidence:…] or [session:…], or record it as `inferred`"
    )]
    SessionObservation,
    #[error("the brief changed since it was read; its hash is now {hash}")]
    Conflict { hash: String },
    #[error("{0}")]
    Write(String),
    #[error("{0}")]
    Store(#[from] StoreError),
}

/// Who is writing. A session's entries are held to what a session can honestly
/// claim, and carry a `[session:…]` reference so the document itself records
/// which one wrote them.
fn write_err(e: docs::WriteError) -> BriefError {
    match e {
        docs::WriteError::Conflict { hash, .. } => BriefError::Conflict { hash },
        other => BriefError::Write(other.to_string()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Author {
    Operator,
    Session(String),
}

/// The brief document for an item: `brief-<id prefix>`, beside `plan-<prefix>`.
pub fn slug_for(item_id: &str) -> String {
    format!("brief-{}", &item_id[..12.min(item_id.len())])
}

/// Read the brief a work item points at.
pub fn for_item(store: &Store, item_id: &str) -> Result<Option<BriefView>, BriefError> {
    let item = store
        .work_get(item_id)?
        .ok_or_else(|| BriefError::MissingItem(item_id.to_string()))?;
    let Some(slug) = item.brief_slug.clone() else {
        return Ok(None);
    };
    let Some(view) = read(store, &item.channel, &slug)? else {
        return Ok(None);
    };
    Ok(Some(BriefView {
        work_item_id: Some(item.id),
        ..view
    }))
}

/// Read a brief document by slug. A document of another kind is not a brief,
/// and is reported as absent rather than parsed into a shape it never had.
pub fn read(store: &Store, channel: &str, slug: &str) -> Result<Option<BriefView>, BriefError> {
    let Some(doc) = store.doc_get(channel, slug)? else {
        return Ok(None);
    };
    if doc.kind != KIND || doc.format != "markdown" {
        return Ok(None);
    }
    Ok(Some(view_of(store, &doc)))
}

/// The view for a document already in hand, so a document read does not go
/// back to the store for its own row.
pub fn view_of(store: &Store, doc: &crate::store::DocumentRow) -> BriefView {
    let mut brief = Brief::parse(&doc.body);
    let counts = count(&brief);
    resolve(store, &doc.channel, &mut brief);
    let absent = brief
        .sections
        .iter()
        .filter(|s| s.entries.is_empty())
        .map(|s| AbsentField {
            field: s.field,
            heading: s.heading.clone(),
            says: s.field.absent().to_string(),
        })
        .collect();
    BriefView {
        channel: doc.channel.clone(),
        slug: doc.slug.clone(),
        work_item_id: None,
        hash: doc.hash.clone(),
        updated_ms: doc.updated_ms,
        brief,
        counts,
        absent,
    }
}

/// The document kind a brief is stored under.
pub const KIND: &str = "brief";

fn count(brief: &Brief) -> Counts {
    let mut counts = Counts::default();
    for entry in brief.sections.iter().flat_map(|s| s.entries.iter()) {
        match entry.provenance {
            Provenance::Observed => counts.observed += 1,
            Provenance::Inferred => counts.inferred += 1,
            Provenance::Decided => counts.decided += 1,
            Provenance::Unattributed => counts.unattributed += 1,
        }
    }
    counts
}

/// Say, for each reference this node could hold, whether it holds it. A
/// reference it has never seen is marked, not dropped: the brief may have
/// been written on another node, and a silent link is worse than a named gap.
fn resolve(store: &Store, channel: &str, brief: &mut Brief) {
    for entry in brief.sections.iter_mut().flat_map(|s| s.entries.iter_mut()) {
        let links = entry.links.iter_mut().flat_map(|l| l.refs.iter_mut());
        for r in entry.refs.iter_mut().chain(links) {
            match r.kind.as_str() {
                "doc" => match store.doc_get(channel, &r.value) {
                    Ok(Some(doc)) => {
                        r.known = Some(true);
                        r.label = Some(doc.title);
                    }
                    Ok(None) => r.known = Some(false),
                    Err(_) => {}
                },
                "work" => match store.work_get(&r.value) {
                    Ok(Some(item)) => {
                        r.known = Some(true);
                        r.label = Some(item.title);
                    }
                    Ok(None) => r.known = Some(false),
                    Err(_) => {}
                },
                "session" => match store.get_session(&r.value) {
                    Ok(Some(_)) => r.known = Some(true),
                    Ok(None) => r.known = Some(false),
                    Err(_) => {}
                },
                // A URL or a path is not this node's to vouch for.
                _ => {}
            }
        }
    }
}

/// What a caller sends to a brief. Everything is optional: a write that names
/// only a title leaves the lines alone, and a section the input does not name
/// is untouched.
#[derive(Debug, Default, Deserialize)]
pub struct BriefInput {
    pub title: Option<String>,
    pub preamble: Option<String>,
    pub sections: Option<Vec<SectionInput>>,
    /// The whole document, for a caller that edits the Markdown directly. It
    /// is parsed and written back canonically, so the two paths cannot drift.
    pub markdown: Option<String>,
    /// The hash the caller last read. A brief that moved under it is refused,
    /// and the check happens in the write's own transaction.
    pub if_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SectionInput {
    pub field: String,
    #[serde(default)]
    pub notes: Option<String>,
    /// Added to the section's lines, in order.
    #[serde(default)]
    pub entries: Option<Vec<EntryInput>>,
    /// Replace the section's lines with `entries` instead of adding to them.
    /// An empty list with this set clears the section, which then says it has
    /// nothing in it rather than disappearing.
    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Deserialize)]
pub struct EntryInput {
    #[serde(default)]
    pub provenance: Option<String>,
    pub text: String,
    #[serde(default)]
    pub refs: Vec<RefInput>,
    /// What points at the line. A caller that names none leaves the line
    /// pointing at nothing, which is what most lines do.
    #[serde(default)]
    pub links: Vec<LinkInput>,
}

#[derive(Debug, Deserialize)]
pub struct LinkInput {
    #[serde(default)]
    pub provenance: Option<String>,
    pub kind: String,
    pub value: String,
    #[serde(default)]
    pub refs: Vec<RefInput>,
}

#[derive(Debug, Deserialize)]
pub struct RefInput {
    pub kind: String,
    pub value: String,
}

/// Create or replace the brief of a work item, linking the item to it on the
/// first write.
pub fn write_for_item(
    store: &Store,
    bus: &Bus,
    site: &str,
    item_id: &str,
    input: BriefInput,
    author: &Author,
) -> Result<BriefView, BriefError> {
    let item = store
        .work_get(item_id)?
        .ok_or_else(|| BriefError::MissingItem(item_id.to_string()))?;
    let slug = item
        .brief_slug
        .clone()
        .unwrap_or_else(|| slug_for(&item.id));
    let current = store.doc_get(&item.channel, &slug)?;
    if let Some(want) = input.if_hash.as_deref() {
        let have = current.as_ref().map(|d| d.hash.clone()).unwrap_or_default();
        if have != want {
            return Err(BriefError::Conflict { hash: have });
        }
    }
    let base = match (&input.markdown, &current) {
        (Some(markdown), _) => Brief::parse(markdown),
        (None, Some(doc)) => Brief::parse(&doc.body),
        (None, None) => Brief::empty(&item.title),
    };
    let if_hash = input.if_hash.clone();
    let brief = apply(base, input, author)?;
    let body = brief.render();
    if body.len() > MAX_BYTES {
        return Err(BriefError::TooLong);
    }
    let doc = docs::write_document(
        store,
        bus,
        site,
        &item.channel,
        &slug,
        &body,
        // The read above answers "there is no brief yet" with a clear error;
        // this is the same check made where the write happens, so a brief
        // that moves in between is refused rather than overwritten.
        current.as_ref().and(if_hash.as_deref()),
        false,
        None,
        None,
    )
    .map_err(write_err)?;
    if item.brief_slug.as_deref() != Some(slug.as_str()) {
        super::work::set_brief(store, bus, site, &item.id, Some(&slug))
            .map_err(|e| BriefError::Write(e.to_string()))?;
    }
    Ok(BriefView {
        work_item_id: Some(item.id),
        ..view_of(store, &doc)
    })
}

/// Add lines to an item's brief without rewriting the rest of it: what a
/// session does when it has something to contribute, and what the interface
/// does for one added line.
pub fn append_for_item(
    store: &Store,
    bus: &Bus,
    site: &str,
    item_id: &str,
    additions: Vec<SectionInput>,
    author: &Author,
) -> Result<BriefView, BriefError> {
    write_for_item(
        store,
        bus,
        site,
        item_id,
        BriefInput {
            sections: Some(additions),
            ..Default::default()
        },
        author,
    )
}

/// Unlink an item from its brief. The document stays: the operator wrote it,
/// and the node deleting prose because a link was removed is not the node's
/// call.
pub fn unlink_item(
    store: &Store,
    bus: &Bus,
    site: &str,
    item_id: &str,
) -> Result<String, BriefError> {
    let item = store
        .work_get(item_id)?
        .ok_or_else(|| BriefError::MissingItem(item_id.to_string()))?;
    let slug = item
        .brief_slug
        .clone()
        .ok_or_else(|| BriefError::NoBrief(item_id.to_string()))?;
    super::work::set_brief(store, bus, site, &item.id, None)
        .map_err(|e| BriefError::Write(e.to_string()))?;
    Ok(slug)
}

/// Merge an input onto a brief, checking every line the caller may not write.
/// Sections the input does not name are left exactly as they were.
fn apply(mut brief: Brief, input: BriefInput, author: &Author) -> Result<Brief, BriefError> {
    if let Some(title) = input.title {
        brief.title = title.trim().to_string();
    }
    if let Some(preamble) = input.preamble {
        brief.preamble = preamble.trim().to_string();
    }
    for section in input.sections.unwrap_or_default() {
        let field =
            Field::parse(&section.field).ok_or_else(|| BriefError::Field(section.field.clone()))?;
        if let Some(notes) = section.notes {
            brief.section_mut(field).notes = notes.trim().to_string();
        }
        let Some(entries) = section.entries else {
            continue;
        };
        let mut checked = Vec::with_capacity(entries.len());
        for entry in entries {
            checked.push(check(entry, author)?);
        }
        let target = brief.section_mut(field);
        if section.replace {
            target.entries = checked;
        } else {
            target.entries.extend(checked);
        }
    }
    let total: usize = brief.sections.iter().map(|s| s.entries.len()).sum();
    if total > MAX_ENTRIES {
        return Err(BriefError::TooManyEntries);
    }
    Ok(brief)
}

/// One line, as the node will let it be written. The two session rules are
/// here and nowhere else, so every path that writes a brief is held to them.
fn check(entry: EntryInput, author: &Author) -> Result<Entry, BriefError> {
    let text = entry.text.trim().to_string();
    if text.is_empty() {
        return Err(BriefError::EmptyText);
    }
    if text.chars().count() > MAX_TEXT {
        return Err(BriefError::LongText);
    }
    let provenance = read_provenance(entry.provenance.as_deref())?;
    let refs = check_refs(entry.refs, provenance, author)?;
    if entry.links.len() > MAX_LINKS {
        return Err(BriefError::TooManyLinks);
    }
    let mut links = Vec::with_capacity(entry.links.len());
    for link in entry.links {
        links.push(check_link(link, author)?);
    }
    Ok(Entry {
        provenance,
        text,
        refs,
        links,
    })
}

/// One link, held to the same two rules as the line it hangs under: an agent
/// may propose what would settle a criterion, and may not decide it; an agent
/// claiming someone was observed needing it must say where that was seen.
/// Public because `corpus::criteria` writes links into an existing document
/// and must not be able to write one this would refuse.
pub fn check_link(link: LinkInput, author: &Author) -> Result<Link, BriefError> {
    let kind = link.kind.trim().to_ascii_lowercase();
    if !LINK_KINDS.contains(&kind.as_str()) {
        return Err(BriefError::LinkKind(link.kind));
    }
    let value = link.value.trim().to_string();
    if value.is_empty() {
        return Err(BriefError::EmptyLink);
    }
    if value.chars().count() > MAX_TEXT {
        return Err(BriefError::LongText);
    }
    // A link is one line of the document: a newline in its value would make
    // it several, and the second one would read as something else entirely.
    if value.contains('\n') {
        return Err(BriefError::EmptyLink);
    }
    let provenance = read_provenance(link.provenance.as_deref())?;
    let refs = check_refs(link.refs, provenance, author)?;
    Ok(Link {
        provenance,
        kind,
        value,
        refs,
    })
}

fn read_provenance(word: Option<&str>) -> Result<Provenance, BriefError> {
    match word {
        None | Some("") | Some("unattributed") => Ok(Provenance::Unattributed),
        Some(word) => {
            Provenance::parse(word).ok_or_else(|| BriefError::Field(format!("provenance {word:?}")))
        }
    }
}

/// What one line points at, and the two rules a session cannot be trusted to
/// keep about itself. Shared by a line and by a link under one, so neither
/// path is the loose one.
fn check_refs(
    input: Vec<RefInput>,
    provenance: Provenance,
    author: &Author,
) -> Result<Vec<Ref>, BriefError> {
    if input.len() > MAX_REFS {
        return Err(BriefError::TooManyRefs);
    }
    let mut refs = Vec::with_capacity(input.len());
    for r in input {
        let kind = r.kind.trim().to_ascii_lowercase();
        if !REF_KINDS.contains(&kind.as_str()) {
            return Err(BriefError::RefKind(r.kind));
        }
        let value = r.value.trim().to_string();
        if value.is_empty() || value.contains(']') || value.contains('\n') {
            return Err(BriefError::RefValue);
        }
        if kind == "url" && !(value.starts_with("http://") || value.starts_with("https://")) {
            return Err(BriefError::RefUrl);
        }
        refs.push(Ref::new(&kind, &value));
    }
    if let Author::Session(id) = author {
        if provenance == Provenance::Decided {
            return Err(BriefError::SessionDecision);
        }
        if provenance == Provenance::Observed
            && !refs
                .iter()
                .any(|r| matches!(r.kind.as_str(), "doc" | "url" | "evidence" | "session"))
        {
            return Err(BriefError::SessionObservation);
        }
        // The document says which session wrote the line, in the document.
        if !refs.iter().any(|r| r.kind == "session" && &r.value == id) {
            refs.push(Ref::new("session", id));
        }
    }
    if refs.len() > MAX_REFS {
        return Err(BriefError::TooManyRefs);
    }
    Ok(refs)
}

/// The one-line summary the interface and the orientation both use.
pub fn summary(view: &BriefView) -> String {
    let c = view.counts;
    let mut parts = vec![format!(
        "{} observed · {} inferred · {} decided",
        c.observed, c.inferred, c.decided
    )];
    if c.unattributed > 0 {
        parts.push(format!("{} unattributed", c.unattributed));
    }
    if !view.absent.is_empty() {
        parts.push(
            view.absent
                .iter()
                .map(|a| a.says.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(p: Provenance, text: &str, refs: &[(&str, &str)]) -> Entry {
        Entry {
            provenance: p,
            text: text.to_string(),
            refs: refs.iter().map(|(k, v)| Ref::new(k, v)).collect(),
            links: Vec::new(),
        }
    }

    fn link(p: Provenance, kind: &str, value: &str) -> Link {
        Link {
            provenance: p,
            kind: kind.to_string(),
            value: value.to_string(),
            refs: Vec::new(),
        }
    }

    #[test]
    fn a_rendered_brief_parses_back_to_the_same_brief() {
        let mut brief = Brief::empty("Overnight alert triage");
        brief.preamble = "Written after two ride-alongs.".into();
        brief.section_mut(Field::IntendedUser).entries.push(entry(
            Provenance::Observed,
            "Field ops leads on the overnight shift",
            &[("doc", "meeting-ops-ride-along")],
        ));
        brief.section_mut(Field::Problem).entries.push(entry(
            Provenance::Inferred,
            "The queue is ordered by arrival, so the urgent alert is found by scrolling",
            &[("doc", "meeting-ops-ride-along"), ("session", "s-17")],
        ));
        brief
            .section_mut(Field::SuccessCriteria)
            .entries
            .push(entry(
                Provenance::Decided,
                "Triage in under two minutes",
                &[],
            ));
        brief.section_mut(Field::Constraints).notes = "Offline is the normal case.".into();

        let rendered = brief.render();
        assert!(rendered.starts_with("# Brief: Overnight alert triage\n"));
        assert!(rendered.contains(
            "- observed: Field ops leads on the overnight shift [doc:meeting-ops-ride-along]\n"
        ));
        assert!(
            rendered.contains("_nothing cited_"),
            "an empty section says so: {rendered}"
        );
        let read = Brief::parse(&rendered);
        assert_eq!(read, brief, "round trip");
        assert_eq!(read.render(), rendered, "and the render is stable");
    }

    #[test]
    fn an_unmarked_line_stays_unattributed_and_unknown_brackets_stay_text() {
        let brief = Brief::parse(
            "# Brief: Thing\n\n## Problem\n\n- the queue is slow\n- inferred: it is the sort [ticket:4821]\n",
        );
        let problem = brief.section(Field::Problem).unwrap();
        assert_eq!(problem.entries[0].provenance, Provenance::Unattributed);
        assert!(problem.entries[0].refs.is_empty());
        assert_eq!(problem.entries[1].provenance, Provenance::Inferred);
        assert_eq!(
            problem.entries[1].text, "it is the sort [ticket:4821]",
            "a bracket this module cannot resolve is prose"
        );
    }

    #[test]
    fn hand_written_headings_and_prose_survive_a_read() {
        let brief = Brief::parse(
            "# Overnight triage\n\nsome preamble\n\n## SUCCESS  CRITERIA\n\nprose under the heading\n\n- decided: under two minutes\n\n## Rollout notes\n\nkept verbatim\n",
        );
        assert_eq!(brief.title, "Overnight triage");
        assert_eq!(brief.preamble, "some preamble");
        let criteria = brief.section(Field::SuccessCriteria).unwrap();
        assert_eq!(criteria.notes, "prose under the heading");
        assert_eq!(criteria.entries.len(), 1);
        assert_eq!(brief.sections.len(), FIELDS.len(), "all six are answered");
        assert!(brief.extra.contains("## Rollout notes"));
        assert!(brief.extra.contains("kept verbatim"));
        assert!(
            brief.render().contains("## Rollout notes"),
            "and it is written back"
        );
    }

    #[test]
    fn what_points_at_a_line_is_written_under_it_and_read_back_there() {
        let mut brief = Brief::empty("Overnight alert triage");
        let mut criterion = entry(Provenance::Decided, "Triage in under two minutes", &[]);
        criterion
            .links
            .push(link(Provenance::Decided, "check", "cargo test -p node triage"));
        criterion
            .links
            .push(link(Provenance::Inferred, "scenario", "overnight-happy-path"));
        criterion.links.push(Link {
            provenance: Provenance::Observed,
            kind: "observation".into(),
            value: "two leads gave up after 40s".into(),
            refs: vec![Ref::new("doc", "meeting-ride-along")],
        });
        brief
            .section_mut(Field::SuccessCriteria)
            .entries
            .push(criterion);
        brief
            .section_mut(Field::SuccessCriteria)
            .entries
            .push(entry(Provenance::Inferred, "The count is never stale", &[]));

        let rendered = brief.render();
        assert!(
            rendered.contains(
                "- decided: Triage in under two minutes\n  - decided check: cargo test -p node triage\n"
            ),
            "a link is indented under the line it settles: {rendered}"
        );
        assert!(rendered.contains(
            "  - observed observation: two leads gave up after 40s [doc:meeting-ride-along]\n"
        ));
        let read = Brief::parse(&rendered);
        assert_eq!(read, brief, "round trip, links and all");
        assert_eq!(read.render(), rendered, "and the render is stable");
        let criteria = read.section(Field::SuccessCriteria).unwrap();
        assert_eq!(criteria.entries[0].links.len(), 3);
        assert!(
            criteria.entries[1].links.is_empty(),
            "a link belongs to the line above it, not to every line after"
        );
    }

    #[test]
    fn an_indented_bullet_that_names_no_link_kind_is_not_guessed_into_one() {
        let brief = Brief::parse(
            "# Brief: Thing\n\n## Success criteria\n\n- decided: under two minutes\n  - check: cargo test\n  - and it should feel quick\n",
        );
        let criteria = brief.section(Field::SuccessCriteria).unwrap();
        assert_eq!(
            criteria.entries[0].links,
            vec![link(Provenance::Unattributed, "check", "cargo test")],
            "the one that names a kind is the only link"
        );
        assert_eq!(
            criteria.entries[1].text, "and it should feel quick",
            "the other is kept as the line of its own it was read as before links \
             existed, rather than invented into a link: {criteria:?}"
        );
        assert!(criteria.entries[1].links.is_empty());
    }

    #[test]
    fn an_unmarked_link_is_read_as_its_kind_even_when_the_kind_is_also_a_provenance_word() {
        let mut brief = Brief::empty("Thing");
        let mut criterion = entry(Provenance::Decided, "under two minutes", &[]);
        // `observation` is also how `Provenance::parse` spells `observed`. One
        // word is the kind, so this renders and reads back as what it is.
        criterion
            .links
            .push(link(Provenance::Unattributed, "observation", "two leads gave up"));
        brief
            .section_mut(Field::SuccessCriteria)
            .entries
            .push(criterion);
        let rendered = brief.render();
        assert!(
            rendered.contains("\n  - observation: two leads gave up\n"),
            "no marker is written for an unattributed link: {rendered}"
        );
        assert_eq!(Brief::parse(&rendered), brief, "and it round-trips");
    }

    #[test]
    fn a_session_may_propose_what_would_settle_a_line_but_may_not_decide_it() {
        let session = Author::Session("s-9".into());
        let proposed = check(
            EntryInput {
                provenance: Some("inferred".into()),
                text: "Triage in under two minutes".into(),
                refs: vec![],
                links: vec![LinkInput {
                    provenance: Some("inferred".into()),
                    kind: "check".into(),
                    value: "cargo test -p node triage".into(),
                    refs: vec![],
                }],
            },
            &session,
        )
        .expect("an agent may propose a check");
        assert_eq!(proposed.links[0].provenance, Provenance::Inferred);
        assert_eq!(
            proposed.links[0].refs,
            vec![Ref::new("session", "s-9")],
            "the link says which session proposed it"
        );

        let decided = check(
            EntryInput {
                provenance: Some("inferred".into()),
                text: "Triage in under two minutes".into(),
                refs: vec![],
                links: vec![LinkInput {
                    provenance: Some("decided".into()),
                    kind: "check".into(),
                    value: "cargo test -p node triage".into(),
                    refs: vec![],
                }],
            },
            &session,
        );
        assert!(
            matches!(decided, Err(BriefError::SessionDecision)),
            "and may not make its own proposal the standard: {decided:?}"
        );

        let unknown = check_link(
            LinkInput {
                provenance: None,
                kind: "benchmark".into(),
                value: "x".into(),
                refs: vec![],
            },
            &Author::Operator,
        );
        assert!(matches!(unknown, Err(BriefError::LinkKind(k)) if k == "benchmark"));

        let too_many = check(
            EntryInput {
                provenance: None,
                text: "x".into(),
                refs: vec![],
                links: (0..MAX_LINKS + 1)
                    .map(|i| LinkInput {
                        provenance: None,
                        kind: "check".into(),
                        value: format!("cargo test {i}"),
                        refs: vec![],
                    })
                    .collect(),
            },
            &Author::Operator,
        );
        assert!(matches!(too_many, Err(BriefError::TooManyLinks)));
    }

    #[test]
    fn a_session_may_not_decide_and_may_not_observe_without_something_to_point_at() {
        let session = Author::Session("s-9".into());
        let decided = check(
            EntryInput {
                provenance: Some("decided".into()),
                text: "we will ship the ranking".into(),
                refs: vec![],
                links: vec![],
            },
            &session,
        );
        assert!(matches!(decided, Err(BriefError::SessionDecision)));

        let bare = check(
            EntryInput {
                provenance: Some("observed".into()),
                text: "they said it is too slow".into(),
                refs: vec![],
                links: vec![],
            },
            &session,
        );
        assert!(matches!(bare, Err(BriefError::SessionObservation)));

        let cited = check(
            EntryInput {
                provenance: Some("observed".into()),
                text: "they said it is too slow".into(),
                refs: vec![RefInput {
                    kind: "doc".into(),
                    value: "meeting-ops".into(),
                }],
                links: vec![],
            },
            &session,
        )
        .expect("an observation that points at something is allowed");
        assert_eq!(
            cited.refs,
            vec![Ref::new("doc", "meeting-ops"), Ref::new("session", "s-9")],
            "the line records which session wrote it"
        );

        let operator = check(
            EntryInput {
                provenance: Some("decided".into()),
                text: "we will ship the ranking".into(),
                refs: vec![],
                links: vec![],
            },
            &Author::Operator,
        )
        .expect("the operator decides");
        assert_eq!(operator.provenance, Provenance::Decided);
        assert!(
            operator.refs.is_empty(),
            "and is not attributed to a session"
        );
    }

    #[test]
    fn references_are_checked_and_a_bad_one_is_named() {
        let bad_kind = check(
            EntryInput {
                provenance: None,
                text: "x".into(),
                refs: vec![RefInput {
                    kind: "ticket".into(),
                    value: "4821".into(),
                }],
                links: vec![],
            },
            &Author::Operator,
        );
        assert!(matches!(bad_kind, Err(BriefError::RefKind(k)) if k == "ticket"));
        let bad_url = check(
            EntryInput {
                provenance: None,
                text: "x".into(),
                refs: vec![RefInput {
                    kind: "url".into(),
                    value: "ftp://host/x".into(),
                }],
                links: vec![],
            },
            &Author::Operator,
        );
        assert!(matches!(bad_url, Err(BriefError::RefUrl)));
    }

    #[test]
    fn an_empty_brief_names_every_section_it_does_not_answer() {
        let brief = Brief::empty("Nothing yet");
        let rendered = brief.render();
        for field in FIELDS {
            assert!(
                rendered.contains(field.heading()),
                "{} is written",
                field.heading()
            );
            assert!(rendered.contains(field.absent()), "{} says so", field.key());
        }
        assert_eq!(Brief::parse(&rendered), brief);
    }
}
