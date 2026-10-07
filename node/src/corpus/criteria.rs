//! Acceptance criteria bound to what would settle them.
//!
//! A criterion is not a new record. It is a line in the Success criteria
//! section of the product brief, and three claims follow from that.
//!
//! *Its name comes from its own text.* `sc-` and twelve hex characters of the
//! hash of the trimmed line. Reordering the section changes nothing, while
//! changing punctuation, case, or wording mints a new name. An old verdict is
//! reported as orphaned instead of silently following a changed requirement.
//!
//! *What points at a criterion lives in the document.* An indented line under
//! it naming a check (`corpus::brief::Link`); a retired `scenario` or
//! `observation` link an older brief holds is shown and counts for nothing. The
//! brief travels with the channel, is hand-editable, round-trips, and needs no
//! second store — and the standing of a link is already enforced one level up:
//! a session may write `inferred` and is refused `decided`. So "an agent may
//! propose what good means" needs no new rule; it is the rule the brief keeps.
//!
//! *Only a person can say a criterion was met.* A check result can raise a
//! criterion as far as *its checks pass*. The word **met** comes from
//! `store::criteria`, whose only writer is [`judge`], which refuses a session.
//! Passing one's own checks is not the customer agreeing with the standard.
//!
//! What this module adds is the reading: per criterion, whether the standard is
//! the operator's or only an agent's proposal, and how far the evidence for one
//! candidate got. Beside the criteria it carries the gaps — criteria nothing
//! points at, criteria nobody has judged, the assumptions the brief rests on,
//! and the questions it says are still open — because a coverage view that only
//! lists what is covered reads as though that were everything.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::brief::{
    self, Author, Brief, BriefError, BriefInput, Field, Link, LinkInput, Provenance, Ref,
};
use crate::config::Config;
use crate::review::checks;
use crate::store::criteria::CriterionJudgementRow;
use crate::store::{now_ms, Store, StoreError};
use crate::stream::Bus;

/// Whether the standard itself is agreed, or is still somebody's proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Standard {
    /// The customer was observed needing it, or the operator decided it.
    Agreed,
    /// An agent reasoned to it. It may be right; nobody has agreed to it.
    Proposed,
    /// Written with no marker, so it says nothing about who is behind it.
    Unattributed,
}

impl Standard {
    fn of(provenance: Provenance) -> Standard {
        match provenance {
            Provenance::Observed | Provenance::Decided => Standard::Agreed,
            Provenance::Inferred => Standard::Proposed,
            Provenance::Unattributed => Standard::Unattributed,
        }
    }

    pub fn says(self) -> &'static str {
        match self {
            Standard::Agreed => "agreed",
            Standard::Proposed => "the agent's proposal",
            Standard::Unattributed => "nobody said who is behind it",
        }
    }
}

/// How far the evidence for one candidate got. In precedence order: a person's
/// verdict outranks a machine's, and a failure outranks a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    JudgedMet,
    JudgedNotMet,
    JudgedUnclear,
    /// An agreed check's latest run for this candidate did not pass.
    Failing,
    /// An agreed check has no run against this candidate yet.
    NoResultYet,
    /// Every agreed check passed. Not *met*: nobody has said so.
    ChecksPass,
    /// Something agreed points at it, but nothing that can produce a result.
    /// Only a person can settle it, and no one has.
    AwaitsJudgement,
    /// Links exist, but none of them is the operator's.
    OnlyProposed,
    /// Nothing points at it at all.
    NothingPointsAtIt,
}

impl Coverage {
    /// What the state means, in the operator's terms rather than as a name.
    pub fn says(self) -> &'static str {
        match self {
            Coverage::JudgedMet => "judged met",
            Coverage::JudgedNotMet => "judged not met",
            Coverage::JudgedUnclear => "judged unclear as written",
            Coverage::Failing => "a check is failing",
            Coverage::NoResultYet => "no result against this candidate yet",
            Coverage::ChecksPass => "checks pass · nobody has judged it",
            Coverage::AwaitsJudgement => "only a person can settle this, and no one has",
            Coverage::OnlyProposed => "only the agent's proposal points at it",
            Coverage::NothingPointsAtIt => "nothing points at it",
        }
    }

    /// Whether a person has spoken. The one distinction the whole view exists
    /// to keep: everything else is a machine's opinion of its own standard.
    pub fn judged(self) -> bool {
        matches!(
            self,
            Coverage::JudgedMet | Coverage::JudgedNotMet | Coverage::JudgedUnclear
        )
    }
}

/// One thing a criterion points at, and what this node can say about it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkView {
    /// Its position under the criterion, which is how it is removed.
    pub index: usize,
    pub provenance: Provenance,
    pub standard: Standard,
    /// One of `brief::LINK_KINDS`.
    pub kind: String,
    pub value: String,
    pub refs: Vec<Ref>,
    /// For a check: `passed`, `failed`, `running`, `interrupted`, `cancelled`,
    /// or absent when this candidate has no run of it. Never present for a
    /// retired link kind, which produces no result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// The run the outcome came from, so the operator can open it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Why this link contributes no result, said plainly. A check the operator
    /// never configured, or a retired link kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unresolved: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgementView {
    pub id: String,
    pub verdict: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_id: Option<String>,
    /// The criterion as it read when this verdict was given.
    pub criterion_text: String,
    pub judged_ms: i64,
}

impl From<&CriterionJudgementRow> for JudgementView {
    fn from(row: &CriterionJudgementRow) -> Self {
        Self {
            id: row.id.clone(),
            verdict: row.verdict.clone(),
            note: row.note.clone(),
            candidate_id: row.candidate_id.clone(),
            criterion_text: row.criterion_text.clone(),
            judged_ms: row.judged_ms,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriterionView {
    /// `sc-` and twelve hex characters of the line's own text.
    pub key: String,
    pub text: String,
    pub provenance: Provenance,
    pub standard: Standard,
    pub refs: Vec<Ref>,
    pub links: Vec<LinkView>,
    /// The standing verdict on this criterion for the candidate being read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judgement: Option<JudgementView>,
    /// A verdict about another attempt — or about no attempt at all, when the
    /// operator judged the criterion itself. It is shown, and it does not
    /// settle this candidate: the code changed since someone looked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub earlier_judgement: Option<JudgementView>,
    pub coverage: Coverage,
    /// Two lines of the section say the same thing. Said rather than merged:
    /// they share a name, so a verdict on one reads as a verdict on both.
    #[serde(default)]
    pub duplicate: bool,
}

/// An assumption the brief rests on: any line, in any section, that nobody
/// observed and the operator did not decide.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assumption {
    pub field: Field,
    pub heading: String,
    pub text: String,
    pub provenance: Provenance,
}

/// What the criteria do not say, which is the half a coverage view usually
/// leaves out.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Gaps {
    /// Criteria nothing points at, or that only an agent's proposal points at.
    pub uncovered: Vec<String>,
    /// Criteria no person has judged for this candidate, however green.
    pub unjudged: Vec<String>,
    pub assumptions: Vec<Assumption>,
    /// The Unresolved questions section, verbatim.
    pub questions: Vec<String>,
    /// Verdicts whose criterion is no longer in the brief — reworded, or
    /// removed. Kept visible: a verdict that quietly vanished is worse.
    pub orphaned_judgements: Vec<JudgementView>,
}

/// The candidate the coverage was read against, named so the reader knows what
/// the greens and reds are about.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateRef {
    pub id: String,
    pub head_sha: String,
    pub captured_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriteriaView {
    pub work_item_id: String,
    pub channel: String,
    pub brief_slug: String,
    /// The brief's hash when this was read: the `if_hash` for writing a link.
    pub hash: String,
    /// The candidate the results are about, when there is one. Without it the
    /// links are still read; they simply have no results yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate: Option<CandidateRef>,
    pub criteria: Vec<CriterionView>,
    /// What the Success criteria section says when it is empty, in the brief's
    /// own idiom, rather than an empty list with no explanation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub absent: Option<String>,
    pub gaps: Gaps,
    pub summary: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CriteriaError {
    #[error("no work item {0}")]
    MissingItem(String),
    #[error("{0} has no brief")]
    NoBrief(String),
    #[error("no criterion {0:?} in this brief")]
    MissingCriterion(String),
    #[error("{0:?} names more than one criterion: {1}")]
    AmbiguousCriterion(String, String),
    #[error("this criterion says {0} things point at it; there is no {1}th")]
    MissingLink(usize, usize),
    #[error("a verdict is one of met, not_met or unclear; {0:?} is not")]
    Verdict(String),
    #[error(
        "a verdict on a criterion is the operator's: a session passing its own checks does not establish that the customer agreed with the standard"
    )]
    SessionJudgement,
    #[error("no candidate {0}")]
    MissingCandidate(String),
    #[error("no revision {0}")]
    MissingRevision(String),
    #[error("revision does not match the work item and candidate")]
    RevisionMismatch,
    #[error(transparent)]
    Brief(#[from] BriefError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// A criterion's identity preserves every meaningful byte; only outer
/// whitespace is insignificant in a brief line.
fn criterion_text(text: &str) -> &str {
    text.trim()
}

pub fn key_for(text: &str) -> String {
    let digest = Sha256::digest(criterion_text(text).as_bytes());
    format!("sc-{}", &hex::encode(digest)[..12])
}

/// The item's newest candidate, which is what a reader that named none is
/// asking about. `None` when nothing has been captured for the item yet: there
/// is then no attempt for a check result or a verdict to be about, and the view
/// says so rather than reading the criteria against nothing in particular.
pub fn newest_candidate(store: &Store, item_id: &str) -> Result<Option<String>, StoreError> {
    Ok(store
        .list_candidates(&crate::store::CandidateListFilter {
            work_item_id: Some(item_id.to_string()),
            limit: 1,
            ..Default::default()
        })?
        .into_iter()
        .next()
        .map(|c| c.candidate.id))
}

/// Read an item's criteria and how far the evidence for one candidate got.
/// `None` when the item points at no brief: an item without one is not
/// incomplete, and this says nothing about it rather than inventing criteria.
pub fn for_item(
    store: &Store,
    cfg: &Config,
    item_id: &str,
    candidate: Option<&str>,
) -> Result<Option<CriteriaView>, CriteriaError> {
    let Some(view) = brief::for_item(store, item_id)? else {
        return Ok(None);
    };
    let candidate = match candidate {
        Some(id) => Some(
            store
                .candidate_for_item(id, item_id, &view.channel)?
                .ok_or_else(|| CriteriaError::MissingCandidate(id.to_string()))?,
        ),
        None => None,
    };
    let runs = match &candidate {
        Some(c) => store.check_runs_for_candidate(&c.id)?,
        None => Vec::new(),
    };
    // The candidate's repository decides which checks are required of it;
    // with no candidate yet, the node-wide list is all there is to show.
    let configured = match &candidate {
        Some(c) => checks::candidate_environment(store, cfg, c)
            .map(|environment| environment.checks)
            .unwrap_or_else(|_| checks::required_definitions(cfg)),
        None => checks::required_definitions(cfg),
    };
    let judgements: Vec<_> = store
        .criterion_judgements_for_item(item_id)?
        .into_iter()
        .filter(|row| row.channel == view.channel && row.brief_slug == view.slug)
        .collect();
    let candidate_id = candidate.as_ref().map(|c| c.id.clone());

    let entries = view
        .brief
        .section(Field::SuccessCriteria)
        .map(|s| s.entries.clone())
        .unwrap_or_default();
    let mut criteria: Vec<CriterionView> = Vec::with_capacity(entries.len());
    for entry in &entries {
        let key = key_for(&entry.text);
        let links: Vec<LinkView> = entry
            .links
            .iter()
            .enumerate()
            .map(|(index, link)| resolve_link(index, link, &configured, &runs))
            .collect();
        // The standing verdict is the newest one about this candidate; the
        // newest about any other is reported as being about another attempt.
        let judgement = judgements
            .iter()
            .find(|j| j.criterion_key == key && j.candidate_id == candidate_id);
        let earlier_judgement = judgement
            .is_none()
            .then(|| judgements.iter().find(|j| j.criterion_key == key))
            .flatten();
        let standard = Standard::of(entry.provenance);
        let coverage = read_coverage(judgement.map(|j| j.verdict.as_str()), &links);
        criteria.push(CriterionView {
            key,
            text: entry.text.clone(),
            provenance: entry.provenance,
            standard,
            refs: entry.refs.clone(),
            links,
            judgement: judgement.map(JudgementView::from),
            earlier_judgement: earlier_judgement.map(JudgementView::from),
            coverage,
            duplicate: false,
        });
    }

    let mut counts = HashMap::with_capacity(criteria.len());
    for criterion in &criteria {
        *counts.entry(criterion.key.clone()).or_insert(0usize) += 1;
    }
    for criterion in &mut criteria {
        criterion.duplicate = counts[&criterion.key] > 1;
    }
    let gaps = read_gaps(&view.brief, &criteria, &judgements);
    let summary = summarize(&criteria, &gaps);
    Ok(Some(CriteriaView {
        work_item_id: item_id.to_string(),
        channel: view.channel.clone(),
        brief_slug: view.slug.clone(),
        hash: view.hash.clone(),
        candidate: candidate.as_ref().map(|c| CandidateRef {
            id: c.id.clone(),
            head_sha: c.head_sha.clone(),
            captured_ms: c.captured_ms,
        }),
        criteria,
        absent: entries
            .is_empty()
            .then(|| Field::SuccessCriteria.absent().to_string()),
        gaps,
        summary,
    }))
}

/// What one link is, and what the candidate's runs say about it.
fn resolve_link(
    index: usize,
    link: &Link,
    configured: &[String],
    runs: &[crate::store::CheckRunRow],
) -> LinkView {
    let mut view = LinkView {
        index,
        provenance: link.provenance,
        standard: Standard::of(link.provenance),
        kind: link.kind.clone(),
        value: link.value.clone(),
        refs: link.refs.clone(),
        outcome: None,
        run_id: None,
        unresolved: None,
    };
    match link.kind.as_str() {
        "check" => {
            if !configured.iter().any(|c| c == &link.value) {
                // A command the operator did not configure is not a check this
                // node will run. Saying so is the point: a criterion pointing
                // at it is not covered, however plausible the command reads.
                view.unresolved =
                    Some("not one of the checks the operator configured, so it never runs".into());
                return view;
            }
            // The latest run wins: an earlier failure that was rerun green is
            // not what the candidate is, and a rerun that failed is.
            if let Some(run) = runs
                .iter()
                .rfind(|r| r.command.as_deref() == Some(link.value.as_str()))
            {
                view.outcome = Some(checks::effective_outcome(run).to_string());
                view.run_id = Some(run.id.clone());
            }
        }
        // Written before scenarios and observations were retired: nothing
        // produces either, so it settles nothing, and says so.
        kind => {
            view.unresolved = Some(format!(
                "{kind} links are retired and settle nothing; judge this criterion, or point a \
                 configured check at it"
            ));
        }
    }
    view
}

/// The precedence in one place. A person's verdict first, then a failure, then
/// the absence of a result, and only then a pass — which is still not *met*.
fn read_coverage(verdict: Option<&str>, links: &[LinkView]) -> Coverage {
    match verdict {
        Some("met") => return Coverage::JudgedMet,
        Some("not_met") => return Coverage::JudgedNotMet,
        Some("unclear") => return Coverage::JudgedUnclear,
        _ => {}
    }
    if links.is_empty() {
        return Coverage::NothingPointsAtIt;
    }
    let agreed: Vec<&LinkView> = links
        .iter()
        .filter(|l| l.standard == Standard::Agreed)
        .collect();
    if agreed.is_empty() {
        return Coverage::OnlyProposed;
    }
    let runnable: Vec<&LinkView> = agreed
        .iter()
        .copied()
        .filter(|l| l.kind == "check" && l.unresolved.is_none())
        .collect();
    if runnable.is_empty() {
        return Coverage::AwaitsJudgement;
    }
    if runnable
        .iter()
        .any(|l| l.outcome.as_deref() == Some("failed"))
    {
        return Coverage::Failing;
    }
    // `running`, `interrupted` and `cancelled` are each neither a pass nor a
    // failure of the candidate, so they read the same as no run at all.
    if runnable
        .iter()
        .any(|l| l.outcome.as_deref() != Some("passed"))
    {
        return Coverage::NoResultYet;
    }
    Coverage::ChecksPass
}

fn read_gaps(
    brief: &Brief,
    criteria: &[CriterionView],
    judgements: &[CriterionJudgementRow],
) -> Gaps {
    let mut gaps = Gaps::default();
    for c in criteria {
        if matches!(
            c.coverage,
            Coverage::NothingPointsAtIt | Coverage::OnlyProposed
        ) {
            gaps.uncovered.push(c.key.clone());
        }
        if !c.coverage.judged() {
            gaps.unjudged.push(c.key.clone());
        }
    }
    for section in &brief.sections {
        for entry in &section.entries {
            if matches!(
                entry.provenance,
                Provenance::Inferred | Provenance::Unattributed
            ) {
                gaps.assumptions.push(Assumption {
                    field: section.field,
                    heading: section.heading.clone(),
                    text: entry.text.clone(),
                    provenance: entry.provenance,
                });
            }
        }
    }
    if let Some(section) = brief.section(Field::UnresolvedQuestions) {
        gaps.questions = section.entries.iter().map(|e| e.text.clone()).collect();
    }
    // A verdict whose criterion is not in the brief any more: the line was
    // reworded or removed, so the verdict is about something that no longer
    // reads that way. Newest first, one per criterion it was about.
    let mut seen: Vec<&str> = Vec::new();
    for row in judgements {
        if criteria.iter().any(|c| c.key == row.criterion_key)
            || seen.contains(&row.criterion_key.as_str())
        {
            continue;
        }
        seen.push(row.criterion_key.as_str());
        gaps.orphaned_judgements.push(JudgementView::from(row));
    }
    gaps
}

/// The one-line summary, in the style of `brief::summary`: whatever is zero is
/// left out, so what is said is what there is.
fn summarize(criteria: &[CriterionView], gaps: &Gaps) -> String {
    if criteria.is_empty() {
        return "no success criteria stated".to_string();
    }
    let mut parts = vec![format!(
        "{} criteri{}",
        criteria.len(),
        if criteria.len() == 1 { "on" } else { "a" }
    )];
    let count = |want: Coverage| criteria.iter().filter(|c| c.coverage == want).count();
    for (state, says) in [
        (Coverage::NothingPointsAtIt, "nothing points at them"),
        (Coverage::OnlyProposed, "only the agent's proposal"),
        (Coverage::Failing, "failing"),
        (Coverage::NoResultYet, "no result yet"),
        (Coverage::AwaitsJudgement, "await a person"),
        (Coverage::ChecksPass, "checks pass, unjudged"),
        (Coverage::JudgedMet, "judged met"),
        (Coverage::JudgedNotMet, "judged not met"),
        (Coverage::JudgedUnclear, "judged unclear"),
    ] {
        let n = count(state);
        if n > 0 {
            parts.push(format!("{n} {says}"));
        }
    }
    let proposed = criteria
        .iter()
        .filter(|c| c.standard != Standard::Agreed)
        .count();
    if proposed > 0 {
        parts.push(format!("{proposed} not an agreed standard"));
    }
    if !gaps.assumptions.is_empty() {
        parts.push(format!("{} assumptions", gaps.assumptions.len()));
    }
    if !gaps.questions.is_empty() {
        parts.push(format!("{} open questions", gaps.questions.len()));
    }
    if !gaps.orphaned_judgements.is_empty() {
        parts.push(format!(
            "{} verdicts on criteria that were reworded",
            gaps.orphaned_judgements.len()
        ));
    }
    parts.join(" · ")
}

/// What `orientation` needs about an item's criteria, without a config, a
/// candidate, or the cost of resolving anything: how many there are and how
/// many nothing agreed points at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Standing {
    pub total: usize,
    /// Criteria with no links at all.
    pub unlinked: usize,
    /// Criteria whose only links are somebody's proposal.
    pub only_proposed: usize,
}

pub fn standing(brief: &Brief) -> Standing {
    let mut standing = Standing::default();
    let Some(section) = brief.section(Field::SuccessCriteria) else {
        return standing;
    };
    for entry in &section.entries {
        standing.total += 1;
        if entry.links.is_empty() {
            standing.unlinked += 1;
        } else if !entry
            .links
            .iter()
            .any(|l| Standard::of(l.provenance) == Standard::Agreed)
        {
            standing.only_proposed += 1;
        }
    }
    standing
}

/// Say what would settle a criterion. The link is checked by `corpus::brief`
/// before it is written, so a session gets the same two refusals here as it
/// does writing a line: it may propose, and it may not decide.
#[allow(clippy::too_many_arguments)]
pub fn add_link(
    store: &Store,
    bus: &Bus,
    site: &str,
    item_id: &str,
    criterion: &str,
    input: LinkInput,
    if_hash: Option<&str>,
    author: &Author,
) -> Result<(), CriteriaError> {
    let link = brief::check_link(input, author)?;
    edit(store, bus, site, item_id, criterion, if_hash, |entry| {
        if entry.links.len() >= brief::MAX_LINKS {
            return Err(BriefError::TooManyLinks.into());
        }
        entry.links.push(link);
        Ok(())
    })
}

/// Remove one of a criterion's links, by the index the view gave it.
pub fn remove_link(
    store: &Store,
    bus: &Bus,
    site: &str,
    item_id: &str,
    criterion: &str,
    index: usize,
    if_hash: Option<&str>,
) -> Result<(), CriteriaError> {
    edit(store, bus, site, item_id, criterion, if_hash, |entry| {
        if index >= entry.links.len() {
            return Err(CriteriaError::MissingLink(entry.links.len(), index + 1));
        }
        entry.links.remove(index);
        Ok(())
    })
}

/// Find the criterion, change it, and write the document back.
///
/// Deliberately not the `EntryInput` path: round-tripping the section through
/// it would append this session's reference to every criterion it did not
/// write, and would be refused outright on any line the operator decided. The
/// lines are left exactly as they are; only the link is new, and only the link
/// was checked.
fn edit(
    store: &Store,
    bus: &Bus,
    site: &str,
    item_id: &str,
    criterion: &str,
    if_hash: Option<&str>,
    change: impl FnOnce(&mut brief::Entry) -> Result<(), CriteriaError>,
) -> Result<(), CriteriaError> {
    let item = store
        .work_get(item_id)?
        .ok_or_else(|| CriteriaError::MissingItem(item_id.to_string()))?;
    let slug = item
        .brief_slug
        .clone()
        .ok_or_else(|| CriteriaError::NoBrief(item_id.to_string()))?;
    let doc = store
        .doc_get(&item.channel, &slug)?
        .ok_or_else(|| CriteriaError::NoBrief(item_id.to_string()))?;
    let mut parsed = Brief::parse(&doc.body);
    let index = locate(&parsed, criterion)?;
    change(&mut parsed.section_mut(Field::SuccessCriteria).entries[index])?;
    let body = parsed.render();
    brief::write_for_item(
        store,
        bus,
        site,
        item_id,
        BriefInput {
            markdown: Some(body),
            // The document moved under the caller only if it asked about a
            // hash; the write's own transaction is where that is decided.
            if_hash: if_hash.map(str::to_string).or(Some(doc.hash.clone())),
            ..Default::default()
        },
        // The lines are unchanged, so no line is re-checked against the
        // author; the link was checked on its own before we got here.
        &Author::Operator,
    )?;
    Ok(())
}

/// Resolve what the caller called a criterion: its key, or its text. Text is
/// allowed because a key is not something a person or an agent reading the
/// brief has in hand, and refused when it names more than one line.
fn locate(brief: &Brief, criterion: &str) -> Result<usize, CriteriaError> {
    let entries = brief
        .section(Field::SuccessCriteria)
        .map(|s| s.entries.as_slice())
        .unwrap_or_default();
    let wanted = criterion.trim();
    let by_key: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| key_for(&e.text) == wanted)
        .map(|(i, _)| i)
        .collect();
    let matches = if by_key.is_empty() {
        entries
            .iter()
            .enumerate()
            .filter(|(_, e)| criterion_text(&e.text) == criterion_text(wanted))
            .map(|(i, _)| i)
            .collect()
    } else {
        by_key
    };
    match matches.as_slice() {
        [] => Err(CriteriaError::MissingCriterion(criterion.to_string())),
        [one] => Ok(*one),
        many => Err(CriteriaError::AmbiguousCriterion(
            criterion.to_string(),
            many.iter()
                .map(|i| format!("{:?}", entries[*i].text))
                .collect::<Vec<_>>()
                .join(", "),
        )),
    }
}

/// Say whether a criterion was met. The operator's, and only the operator's:
/// an agent that judged its own work against its own standard would make the
/// whole distinction this module exists for unreadable.
#[allow(clippy::too_many_arguments)]
pub fn judge(
    store: &Store,
    item_id: &str,
    criterion: &str,
    candidate_id: Option<&str>,
    revision_id: Option<&str>,
    verdict: &str,
    note: Option<&str>,
    author: &Author,
) -> Result<CriterionJudgementRow, CriteriaError> {
    if matches!(author, Author::Session(_)) {
        return Err(CriteriaError::SessionJudgement);
    }
    let verdict = verdict.trim().to_ascii_lowercase();
    if !crate::store::criteria::VERDICTS.contains(&verdict.as_str()) {
        return Err(CriteriaError::Verdict(verdict));
    }
    let item = store
        .work_get(item_id)?
        .ok_or_else(|| CriteriaError::MissingItem(item_id.to_string()))?;
    let slug = item
        .brief_slug
        .clone()
        .ok_or_else(|| CriteriaError::NoBrief(item_id.to_string()))?;
    let doc = store
        .doc_get(&item.channel, &slug)?
        .ok_or_else(|| CriteriaError::NoBrief(item_id.to_string()))?;
    let parsed = Brief::parse(&doc.body);
    let index = locate(&parsed, criterion)?;
    let text = parsed
        .section(Field::SuccessCriteria)
        .map(|s| s.entries[index].text.clone())
        .unwrap_or_default();
    if let Some(id) = candidate_id {
        if store
            .candidate_for_item(id, item_id, &item.channel)?
            .is_none()
        {
            return Err(CriteriaError::MissingCandidate(id.to_string()));
        }
    }
    if let Some(id) = revision_id {
        let revision = store
            .review_revision(id)?
            .ok_or_else(|| CriteriaError::MissingRevision(id.to_string()))?;
        if revision.requirements_work_item_id.as_deref() != Some(item_id)
            || candidate_id != Some(revision.candidate_id.as_str())
        {
            return Err(CriteriaError::RevisionMismatch);
        }
    }
    let row = CriterionJudgementRow {
        id: super::new_id(),
        channel: item.channel.clone(),
        work_item_id: item.id.clone(),
        brief_slug: slug,
        criterion_key: key_for(&text),
        criterion_text: text,
        candidate_id: candidate_id.map(str::to_string),
        revision_id: revision_id.map(str::to_string),
        verdict,
        note: note
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(String::from),
        judged_ms: now_ms(),
    };
    store.criterion_judgement_record(&row)?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn criterion(text: &str, provenance: Provenance, links: Vec<Link>) -> CriterionView {
        let coverage_links: Vec<LinkView> = links
            .iter()
            .enumerate()
            .map(|(index, link)| LinkView {
                index,
                provenance: link.provenance,
                standard: Standard::of(link.provenance),
                kind: link.kind.clone(),
                value: link.value.clone(),
                refs: Vec::new(),
                outcome: None,
                run_id: None,
                unresolved: None,
            })
            .collect();
        CriterionView {
            key: key_for(text),
            text: text.to_string(),
            provenance,
            standard: Standard::of(provenance),
            refs: Vec::new(),
            links: coverage_links.clone(),
            judgement: None,
            earlier_judgement: None,
            coverage: read_coverage(None, &coverage_links),
            duplicate: false,
        }
    }

    fn link(provenance: Provenance, kind: &str, value: &str) -> Link {
        Link {
            provenance,
            kind: kind.to_string(),
            value: value.to_string(),
            refs: Vec::new(),
        }
    }

    #[test]
    fn identity_preserves_comparisons_and_code_identifiers() {
        assert_ne!(
            key_for("Latency < 2 seconds"),
            key_for("Latency > 2 seconds")
        );
        assert_ne!(key_for("Status == READY"), key_for("Status == ready"));
        assert_eq!(key_for("  Status == READY  "), key_for("Status == READY"));
        assert!(key_for("Status == READY").starts_with("sc-"));
        assert_eq!(key_for("Status == READY").len(), 15);
    }

    #[test]
    fn passing_ones_own_checks_is_not_met() {
        let mut links = vec![LinkView {
            index: 0,
            provenance: Provenance::Decided,
            standard: Standard::Agreed,
            kind: "check".into(),
            value: "just check".into(),
            refs: Vec::new(),
            outcome: Some("passed".into()),
            run_id: Some("r1".into()),
            unresolved: None,
        }];
        assert_eq!(read_coverage(None, &links), Coverage::ChecksPass);
        assert!(
            !read_coverage(None, &links).judged(),
            "green is not somebody agreeing"
        );
        assert_eq!(
            read_coverage(Some("met"), &links),
            Coverage::JudgedMet,
            "only a person's verdict says met"
        );

        links[0].outcome = Some("failed".into());
        assert_eq!(read_coverage(None, &links), Coverage::Failing);
        links[0].outcome = Some("cancelled".into());
        assert_eq!(
            read_coverage(None, &links),
            Coverage::NoResultYet,
            "a cancelled run is neither a pass nor a failure"
        );
        links[0].outcome = None;
        assert_eq!(read_coverage(None, &links), Coverage::NoResultYet);
        assert_eq!(
            read_coverage(Some("not_met"), &links),
            Coverage::JudgedNotMet,
            "and a person's verdict outranks the absence of a result"
        );
    }

    #[test]
    fn an_agents_own_proposal_does_not_cover_a_criterion() {
        assert_eq!(
            read_coverage(None, &[]),
            Coverage::NothingPointsAtIt,
            "a criterion with no links is not covered"
        );
        let proposed = criterion(
            "Triage in under two minutes",
            Provenance::Inferred,
            vec![link(Provenance::Inferred, "check", "just check")],
        );
        assert_eq!(proposed.coverage, Coverage::OnlyProposed);
        assert_eq!(proposed.standard, Standard::Proposed);

        let agreed = criterion(
            "Triage in under two minutes",
            Provenance::Decided,
            vec![
                link(Provenance::Inferred, "check", "just check"),
                link(Provenance::Decided, "scenario", "overnight-happy-path"),
            ],
        );
        assert_eq!(
            agreed.coverage,
            Coverage::AwaitsJudgement,
            "an agreed link that cannot produce a result says a person must settle it"
        );
    }

    #[test]
    fn a_check_the_operator_never_configured_covers_nothing() {
        let link = link(Provenance::Decided, "check", "cargo test --all-features");
        let view = resolve_link(0, &link, &["just check".to_string()], &[]);
        assert!(view.outcome.is_none());
        assert!(
            view.unresolved.is_some(),
            "the view says why it contributes nothing"
        );
        assert_eq!(
            read_coverage(None, &[view]),
            Coverage::AwaitsJudgement,
            "an unrunnable check is not a passing one"
        );
    }

    #[test]
    fn a_retired_link_kind_says_so_and_settles_nothing() {
        for kind in crate::corpus::brief::RETIRED_LINK_KINDS {
            let retired = resolve_link(
                0,
                &link(Provenance::Decided, kind, "overnight-happy-path"),
                &[],
                &[],
            );
            assert!(retired.outcome.is_none());
            assert!(
                retired.unresolved.as_deref().unwrap().contains("retired"),
                "{retired:?}"
            );
        }
    }

    #[test]
    fn the_summary_leaves_out_what_is_zero_and_says_no_criteria_when_there_are_none() {
        assert_eq!(
            summarize(&[], &Gaps::default()),
            "no success criteria stated"
        );
        let criteria = vec![
            criterion("a", Provenance::Decided, vec![]),
            criterion(
                "b",
                Provenance::Inferred,
                vec![link(Provenance::Inferred, "check", "just check")],
            ),
        ];
        let gaps = Gaps {
            questions: vec!["how fast is fast".into()],
            ..Default::default()
        };
        let summary = summarize(&criteria, &gaps);
        assert!(summary.starts_with("2 criteria · "), "{summary}");
        assert!(summary.contains("1 nothing points at them"), "{summary}");
        assert!(summary.contains("1 only the agent's proposal"), "{summary}");
        assert!(summary.contains("1 not an agreed standard"), "{summary}");
        assert!(summary.contains("1 open questions"), "{summary}");
        assert!(
            !summary.contains("judged met") && !summary.contains("assumptions"),
            "nothing zero is mentioned: {summary}"
        );
        assert_eq!(
            summarize(&criteria[..1], &Gaps::default()),
            "1 criterion · 1 nothing points at them",
            "one criterion is not 1 criteria"
        );
    }

    #[test]
    fn standing_reads_what_the_orientation_needs_from_the_brief_alone() {
        let mut brief = Brief::empty("Overnight triage");
        let section = brief.section_mut(Field::SuccessCriteria);
        section.entries.push(brief::Entry {
            provenance: Provenance::Decided,
            text: "Triage in under two minutes".into(),
            refs: Vec::new(),
            links: vec![link(Provenance::Decided, "check", "just check")],
        });
        section.entries.push(brief::Entry {
            provenance: Provenance::Inferred,
            text: "The count is never stale".into(),
            refs: Vec::new(),
            links: vec![link(Provenance::Inferred, "check", "just check")],
        });
        section.entries.push(brief::Entry {
            provenance: Provenance::Observed,
            text: "Nobody scrolls to find the urgent one".into(),
            refs: Vec::new(),
            links: Vec::new(),
        });
        assert_eq!(
            standing(&brief),
            Standing {
                total: 3,
                unlinked: 1,
                only_proposed: 1,
            }
        );
        assert_eq!(standing(&Brief::empty("x")), Standing::default());
    }
}
