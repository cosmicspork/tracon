//! Explicit continuity transfer. A package is an immutable candidate capture,
//! selected context, and a source-free file set. It is signed by its origin;
//! receiving it only stages bytes. A separate, operator-confirmed import owns
//! workspace creation and starting a new session.
//!
//! A version 2 package also carries the branch's Git history (a bundle of its
//! commits from its base, the base commit, the branch name and the forge the
//! work came from), so the import recreates the real branch on the receiver's
//! own clone, and, for a move, the source session's links (its work item,
//! project, budget and last state), so the new session takes the work over.
//! A version 1 package (files only) still verifies and imports as before.

use std::{
    fs,
    path::{Component, Path},
};

use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::store::{DocumentRow, MemoryRow, Store, TransferRow};

pub const TRANSFER_VERSION: u32 = 1;
/// A package carrying Git history or a moved session. A peer that only knows
/// version 1 refuses it as an unsupported version, which is what it should
/// do: re-serialising fields it does not know would only fail the digest.
pub const TRANSFER_VERSION_HISTORY: u32 = 2;
/// Bounds on what a package says about the session it came from.
pub const MAX_SOURCE_ITEMS: usize = 64;
const MAX_SOURCE_FIELD_BYTES: usize = 1024;
/// A portable package is intentionally much smaller than a runtime workspace
/// volume. This bounds both browser upload memory and SQLite package storage.
pub const MAX_TRANSFER_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_TRANSFER_FILES: usize = 20_000;
pub const MAX_CONTEXT_DOCUMENTS: usize = 128;
pub const MAX_CONTEXT_MEMORIES: usize = 512;
pub const MAX_HANDOFF_NOTE_BYTES: usize = 16 * 1024;
pub const MAX_CANDIDATE_ID_BYTES: usize = 256;
pub const MAX_HEAD_SHA_BYTES: usize = 128;
/// Direct mesh envelopes add authenticated encryption and base64 overhead.
/// A package larger than this travels in pieces of `MESH_CHUNK_BYTES`.
pub const MAX_MESH_TRANSFER_BYTES: usize = 2 * 1024 * 1024;
/// One piece of a package sent in pieces, before base64 and sealing.
pub const MESH_CHUNK_BYTES: usize = 1024 * 1024;
/// The most pieces a package can be: the largest package, in pieces.
pub const MAX_MESH_CHUNKS: u32 = (MAX_TRANSFER_BYTES / MESH_CHUNK_BYTES) as u32 + 2;
/// Pieces of a package that never completed are dropped after this long.
const PARTIAL_TRANSFER_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 86_400);
const SIGNING_DOMAIN: &[u8] = b"tracon/candidate-transfer/v1\0";

#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error("candidate not found")]
    CandidateNotFound,
    #[error("selected document {0} is not a live Markdown document on the candidate channel")]
    DocumentNotFound(String),
    #[error("selected memory {0} is not live on the candidate channel")]
    MemoryNotFound(String),
    #[error("transfer exceeds its bounded portable size")]
    TooLarge,
    #[error("transfer contains too many files")]
    TooManyFiles,
    #[error("transfer file {0:?} is unsafe")]
    UnsafePath(String),
    #[error("transfer carries non-regular source metadata at {0:?}")]
    UnsafeFileMode(String),
    #[error("transfer digest does not match its payload")]
    BadDigest,
    #[error("transfer signature does not verify")]
    BadSignature,
    #[error("transfer origin identity is malformed")]
    BadOrigin,
    #[error("transfer sender is not the signed origin")]
    SenderMismatch,
    #[error("transfer envelope channel does not match its payload")]
    ChannelMismatch,
    #[error("transfer targets another node")]
    WrongTarget,
    #[error("origin is not currently a member of channel {0}")]
    OriginUnauthorized(String),
    #[error("this node does not hold authority for channel {0}")]
    ReceiverUnauthorized(String),
    #[error("online delivery is unavailable; download the portable package instead")]
    MeshUnavailable,
    #[error("destination node is not currently a member of channel {0}")]
    DestinationUnauthorized(String),
    #[error("invalid transfer package: {0}")]
    Invalid(String),
    #[error("transfer workspace preparation failed: {0}")]
    Workspace(String),
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ContextSelection {
    #[serde(default)]
    pub documents: Vec<String>,
    #[serde(default)]
    pub memories: Vec<String>,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferFile {
    pub path: String,
    pub mode: u32,
    /// Standard base64 keeps the package JSON portable through a browser or an
    /// offline removable medium without trusting archive extraction.
    pub content_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferContext {
    pub documents: Vec<DocumentRow>,
    pub memories: Vec<MemoryRow>,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferPayload {
    pub version: u32,
    pub candidate_id: String,
    pub channel: String,
    pub origin_node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_node: Option<String>,
    pub created_ms: i64,
    /// Candidate records and their evidence are serialised values so this
    /// module remains a consumer of the evidence contract, not a second model.
    pub candidate: Value,
    pub evidence: Value,
    pub files: Vec<TransferFile>,
    pub context: TransferContext,
    /// Version 2: the branch's history. Absent from a version 1 package, and
    /// left out of its serialisation, so a version 1 digest is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<TransferGit>,
    /// Version 2: the session the work is moving from, for a move.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<TransferSource>,
}

/// The branch's Git objects: a bundle of its commits from `history.base_sha`
/// (which the receiver reads from its own clone of `origin`) to
/// `history.head`, and what the receiver needs to recreate the branch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferGit {
    /// The branch the work is on, at the source.
    pub branch: String,
    /// The forge repository the work came from (`host/owner/name`), which the
    /// receiver resolves to its own managed clone. `None` for a workspace
    /// with no forge, whose bundle carries its whole history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    pub history: crate::workspace::BranchHistory,
    /// Standard base64 of the bundle; empty when the head is the base.
    pub bundle_b64: String,
}

/// What the receiving session does to the session it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferIntent {
    /// The package is a handoff: nothing about the source changes, and the
    /// new session holds nothing of the source's.
    #[default]
    Handoff,
    /// The work moves: the new session takes over the source's work item,
    /// project and budget, links back to it, and the source is closed once
    /// the receiver has imported.
    Move,
}

/// The project the source session ran in, as the receiver records it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransferProject {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
}

/// A review or report the source still had open.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransferReview {
    pub id: String,
    pub kind: String,
    pub state: String,
    pub title: String,
}

/// The last check that ran for the source session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransferCheck {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<i64>,
}

/// Something the source session was waiting on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransferWait {
    pub tool: String,
    pub id: String,
    pub about: String,
}

/// The session a move takes the work from, and its last state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferSource {
    pub intent: TransferIntent,
    pub session_id: String,
    pub phase: String,
    pub state: String,
    pub model: String,
    pub harness: String,
    pub budget_tokens: i64,
    pub tokens_used: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<TransferProject>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_reviews: Vec<TransferReview>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check: Option<TransferCheck>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting_on: Vec<TransferWait>,
}

/// What a package carries beyond the candidate and its context.
#[derive(Debug, Clone, Default)]
pub struct Extras {
    pub git: Option<TransferGit>,
    pub source: Option<TransferSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedTransfer {
    pub payload: TransferPayload,
    pub sha256: String,
    pub signature: String,
}

impl SignedTransfer {
    pub fn id(&self) -> &str {
        &self.sha256
    }

    fn payload_bytes(&self) -> Result<Vec<u8>, TransferError> {
        let bytes = serde_json::to_vec(&self.payload)?;
        if bytes.len() > MAX_TRANSFER_BYTES {
            return Err(TransferError::TooLarge);
        }
        Ok(bytes)
    }

    pub fn verify(&self) -> Result<(), TransferError> {
        let extended = self.payload.git.is_some() || self.payload.source.is_some();
        let expected = if extended {
            TRANSFER_VERSION_HISTORY
        } else {
            TRANSFER_VERSION
        };
        if self.payload.version != expected {
            return Err(TransferError::Invalid(
                "unsupported transfer version".into(),
            ));
        }
        if let Some(git) = &self.payload.git {
            verify_git(git)?;
        }
        if let Some(source) = &self.payload.source {
            verify_source(source)?;
        }
        let bytes = self.payload_bytes()?;
        validate_files(&self.payload.files)?;
        if self.payload.context.documents.len() > MAX_CONTEXT_DOCUMENTS
            || self.payload.context.memories.len() > MAX_CONTEXT_MEMORIES
        {
            return Err(TransferError::Invalid(
                "selected context is too large".into(),
            ));
        }
        if self.payload.context.note.len() > MAX_HANDOFF_NOTE_BYTES
            || self.payload.candidate_id.len() > MAX_CANDIDATE_ID_BYTES
            || self.payload.channel.len() > 64
        {
            return Err(TransferError::Invalid(
                "transfer manifest field is too large".into(),
            ));
        }
        let candidate = &self.payload.candidate;
        let head_sha = candidate["head_sha"].as_str();
        if candidate["id"].as_str() != Some(self.payload.candidate_id.as_str())
            || candidate["channel"].as_str() != Some(self.payload.channel.as_str())
            || head_sha.is_none_or(|head| {
                head.is_empty()
                    || head.len() > MAX_HEAD_SHA_BYTES
                    || !head.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            || self.payload.candidate_id
                != format!("{}:{}", head_sha.unwrap_or_default(), self.payload.channel)
        {
            return Err(TransferError::Invalid(
                "candidate identity does not bind the transfer id and channel".into(),
            ));
        }
        // The embedded evidence must describe the very same candidate, not
        // just an id string that happens to match: an authorized signer must
        // not be able to label a package with one candidate's identity while
        // its evidence (checks, review history) describes another.
        if &self.payload.evidence["candidate"] != candidate {
            return Err(TransferError::Invalid(
                "embedded evidence describes a different candidate than the transfer manifest"
                    .into(),
            ));
        }
        // Bind the transferred files themselves to the candidate's own
        // recorded git tree, so a signer cannot ship a tree that does not
        // match the commit its labels claim.
        let tree_sha = candidate["tree_sha"].as_str().filter(|sha| !sha.is_empty());
        let Some(tree_sha) = tree_sha else {
            return Err(TransferError::Invalid(
                "candidate has no recorded tree hash to bind the transferred files to".into(),
            ));
        };
        if tree_sha.len() > MAX_HEAD_SHA_BYTES
            || !tree_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(TransferError::Invalid(
                "candidate tree hash is malformed".into(),
            ));
        }
        if crate::git_tree::tree_sha1(&self.payload.files) != tree_sha {
            return Err(TransferError::Invalid(
                "transferred files do not match the candidate's recorded tree".into(),
            ));
        }
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        if hex::encode(digest) != self.sha256 {
            return Err(TransferError::BadDigest);
        }
        let key = proto::keys::key32(&self.payload.origin_node).ok_or(TransferError::BadOrigin)?;
        let verifying = VerifyingKey::from_bytes(&key).map_err(|_| TransferError::BadOrigin)?;
        let signature: [u8; 64] = hex::decode(&self.signature)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or(TransferError::BadSignature)?;
        let mut message = Vec::with_capacity(SIGNING_DOMAIN.len() + digest.len());
        message.extend_from_slice(SIGNING_DOMAIN);
        message.extend_from_slice(&digest);
        if !proto::keys::verify(&verifying, &message, &Signature::from_bytes(&signature)) {
            return Err(TransferError::BadSignature);
        }
        Ok(())
    }

    pub fn serialized_len(&self) -> Result<usize, TransferError> {
        Ok(serde_json::to_vec(self)?.len())
    }

    pub fn as_row(&self) -> Result<TransferRow, TransferError> {
        self.verify()?;
        let package_json = serde_json::to_string(self)?;
        if package_json.len() > MAX_TRANSFER_BYTES {
            return Err(TransferError::TooLarge);
        }
        Ok(TransferRow {
            id: self.sha256.clone(),
            candidate_id: self.payload.candidate_id.clone(),
            channel: self.payload.channel.clone(),
            origin_node: self.payload.origin_node.clone(),
            target_node: self.payload.target_node.clone(),
            payload_sha256: self.sha256.clone(),
            package_json,
            file_count: i64::try_from(self.payload.files.len()).unwrap_or(i64::MAX),
            document_count: i64::try_from(self.payload.context.documents.len()).unwrap_or(i64::MAX),
            memory_count: i64::try_from(self.payload.context.memories.len()).unwrap_or(i64::MAX),
            handoff_note: self.payload.context.note.clone(),
            created_ms: self.payload.created_ms,
        })
    }
}

fn verify_git(git: &TransferGit) -> Result<(), TransferError> {
    let commit =
        |id: &str| matches!(id.len(), 40 | 64) && id.bytes().all(|b| b.is_ascii_hexdigit());
    let bad = |what: &str| Err(TransferError::Invalid(format!("transfer history {what}")));
    if !commit(&git.history.head) || git.history.base_sha.as_deref().is_some_and(|b| !commit(b)) {
        return bad("names a commit that is not a commit id");
    }
    if !branch_name(&git.branch)
        || git
            .history
            .base_branch
            .as_deref()
            .is_some_and(|b| !branch_name(b))
    {
        return bad("names a branch that is not a branch name");
    }
    if let Some(origin) = git.origin.as_deref() {
        if crate::mesh::recipe::valid_identity(origin).is_err() {
            return bad("names an origin that is not a forge repository");
        }
    }
    if git.history.base_sha.is_some() && git.origin.is_none() {
        return bad("starts from a base with no origin to read it from");
    }
    if git.bundle_b64.is_empty() && git.history.base_sha.as_deref() != Some(&git.history.head) {
        return bad("carries no commits for a head that is not its base");
    }
    Ok(())
}

/// A branch name as a package may carry it: one Git would take, never an
/// option, and bounded.
pub fn branch_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.starts_with('-')
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("@{")
        && name
            .bytes()
            .all(|b| b > 0x20 && b != 0x7f && !b"~^:?*[\\".contains(&b))
}

fn verify_source(source: &TransferSource) -> Result<(), TransferError> {
    let long = |value: &str| value.len() > MAX_SOURCE_FIELD_BYTES;
    let mut fields = vec![
        source.session_id.as_str(),
        source.phase.as_str(),
        source.state.as_str(),
        source.model.as_str(),
        source.harness.as_str(),
    ];
    fields.extend(source.work_item_id.as_deref());
    if let Some(project) = &source.project {
        fields.extend([project.id.as_str(), project.name.as_str()]);
        fields.extend(project.remote_url.as_deref());
    }
    for review in &source.open_reviews {
        fields.extend([
            review.id.as_str(),
            review.kind.as_str(),
            review.state.as_str(),
            review.title.as_str(),
        ]);
    }
    if let Some(check) = &source.last_check {
        fields.extend([check.id.as_str(), check.outcome.as_str()]);
        fields.extend(check.command.as_deref());
    }
    for wait in &source.waiting_on {
        fields.extend([wait.tool.as_str(), wait.id.as_str(), wait.about.as_str()]);
    }
    if source.session_id.is_empty()
        || fields.into_iter().any(long)
        || source.open_reviews.len() > MAX_SOURCE_ITEMS
        || source.waiting_on.len() > MAX_SOURCE_ITEMS
        || source.budget_tokens < 0
    {
        return Err(TransferError::Invalid(
            "the moved session's record is malformed or too large".into(),
        ));
    }
    Ok(())
}

/// Make a portable package from immutable candidate rows. No session path,
/// worktree metadata, credentials, or live harness state is read or carried.
pub fn export(
    store: &Store,
    identity: &proto::keys::Identity,
    candidate_id: &str,
    selection: ContextSelection,
    target_node: Option<String>,
) -> Result<SignedTransfer, TransferError> {
    export_with(
        store,
        identity,
        candidate_id,
        selection,
        target_node,
        Extras::default(),
    )
}

/// `export`, with the branch's history and the moved session as `extras`
/// carries them (`capture_git`, `describe_source`). A package with either is
/// version 2.
pub fn export_with(
    store: &Store,
    identity: &proto::keys::Identity,
    candidate_id: &str,
    selection: ContextSelection,
    target_node: Option<String>,
    extras: Extras,
) -> Result<SignedTransfer, TransferError> {
    if selection.documents.len() > MAX_CONTEXT_DOCUMENTS
        || selection.memories.len() > MAX_CONTEXT_MEMORIES
    {
        return Err(TransferError::Invalid(
            "too many selected context records".into(),
        ));
    }
    let candidate = store
        .candidate(candidate_id)?
        .ok_or(TransferError::CandidateNotFound)?;
    let channel = candidate.channel.clone();
    if store.channel_get(&channel)?.is_none() {
        return Err(TransferError::ReceiverUnauthorized(channel));
    }
    let evidence = serde_json::to_value(store.candidate_evidence(candidate_id)?)?;
    let candidate_value = serde_json::to_value(&candidate)?;
    let mut documents = Vec::with_capacity(selection.documents.len());
    for slug in unique(selection.documents) {
        let row = store
            .doc_get(&channel, &slug)?
            .filter(|row| row.format == "markdown" && row.archived == 0)
            .ok_or_else(|| TransferError::DocumentNotFound(slug.clone()))?;
        documents.push(row);
    }
    let materialized = serde_json::from_str::<Value>(&candidate.capture_json)
        .ok()
        .and_then(|capture| capture["materialized"].as_bool())
        .unwrap_or(false);
    if !materialized {
        return Err(TransferError::Invalid(
            "candidate has no immutable materialized source set".into(),
        ));
    }
    let mut memories = Vec::with_capacity(selection.memories.len());
    for id in unique(selection.memories) {
        let row = store
            .memory_get(&id)?
            .filter(|row| row.channel == channel && row.deleted == 0)
            .ok_or_else(|| TransferError::MemoryNotFound(id.clone()))?;
        memories.push(row);
    }
    let candidate_files = store.candidate_files(candidate_id)?;
    if candidate_files.len() > MAX_TRANSFER_FILES {
        return Err(TransferError::TooManyFiles);
    }
    let mut source_bytes = 0usize;
    for file in &candidate_files {
        safe_path(&file.path)?;
        if file.mode & 0o170000 != 0o100000 {
            return Err(TransferError::UnsafeFileMode(file.path.clone()));
        }
        source_bytes = source_bytes
            .checked_add(file.content.len())
            .ok_or(TransferError::TooLarge)?;
        if source_bytes > MAX_TRANSFER_BYTES {
            return Err(TransferError::TooLarge);
        }
    }
    let files: Vec<TransferFile> = candidate_files
        .into_iter()
        .map(|file| TransferFile {
            path: file.path,
            mode: file.mode,
            content_b64: base64::engine::general_purpose::STANDARD.encode(file.content),
        })
        .collect();
    validate_files(&files)?;
    let extended = extras.git.is_some() || extras.source.is_some();
    let payload = TransferPayload {
        version: if extended {
            TRANSFER_VERSION_HISTORY
        } else {
            TRANSFER_VERSION
        },
        candidate_id: candidate.id,
        channel,
        origin_node: identity.node_id(),
        target_node,
        created_ms: crate::store::now_ms(),
        candidate: candidate_value,
        evidence,
        files,
        context: TransferContext {
            documents,
            memories,
            note: selection.note,
        },
        git: extras.git,
        source: extras.source,
    };
    let payload_bytes = serde_json::to_vec(&payload)?;
    if payload_bytes.len() > MAX_TRANSFER_BYTES {
        return Err(TransferError::TooLarge);
    }
    let digest: [u8; 32] = Sha256::digest(&payload_bytes).into();
    let mut message = Vec::with_capacity(SIGNING_DOMAIN.len() + digest.len());
    message.extend_from_slice(SIGNING_DOMAIN);
    message.extend_from_slice(&digest);
    let transfer = SignedTransfer {
        payload,
        sha256: hex::encode(digest),
        signature: hex::encode(identity.sign(&message).to_bytes()),
    };
    store.insert_transfer(&transfer.as_row()?)?;
    store.append_transfer_event(transfer.id(), "exported", None, None)?;
    Ok(transfer)
}

/// Verify a direct mesh delivery and stage it. This never creates a workspace
/// or a session: only an operator's later confirmed import can do that.
pub fn receive_mesh(
    store: &Store,
    receiver_node: &str,
    envelope_sender: &str,
    envelope_channel: &str,
    value: Value,
) -> Result<String, TransferError> {
    let transfer: SignedTransfer = serde_json::from_value(value)?;
    transfer.verify()?;
    if transfer.payload.origin_node != envelope_sender {
        return Err(TransferError::SenderMismatch);
    }
    if transfer.payload.channel != envelope_channel {
        return Err(TransferError::ChannelMismatch);
    }
    if transfer.payload.target_node.as_deref() != Some(receiver_node) {
        return Err(TransferError::WrongTarget);
    }
    authorize_receiver(store, receiver_node, &transfer)?;
    let row = transfer.as_row()?;
    if store.insert_transfer(&row)? {
        store.append_transfer_event(transfer.id(), "received_mesh", None, None)?;
    }
    Ok(transfer.sha256)
}

/// One piece of a package sent over the mesh in pieces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshChunk {
    pub transfer_id: String,
    pub index: u32,
    pub count: u32,
    pub sha256: String,
    pub data_b64: String,
}

/// The package as the pieces a mesh delivery sends: its JSON, cut into
/// `MESH_CHUNK_BYTES` pieces, each with its own digest. `only` sends just
/// those pieces again (a resend of what a receiver is missing).
pub fn mesh_chunks(
    transfer: &SignedTransfer,
    only: Option<&[u32]>,
) -> Result<Vec<MeshChunk>, TransferError> {
    let bytes = serde_json::to_vec(transfer)?;
    if bytes.len() > MAX_TRANSFER_BYTES + MESH_CHUNK_BYTES {
        return Err(TransferError::TooLarge);
    }
    let count = u32::try_from(bytes.len().div_ceil(MESH_CHUNK_BYTES)).unwrap_or(u32::MAX);
    Ok(bytes
        .chunks(MESH_CHUNK_BYTES)
        .enumerate()
        .map(|(index, piece)| (index as u32, piece))
        .filter(|(index, _)| only.is_none_or(|only| only.contains(index)))
        .map(|(index, piece)| MeshChunk {
            transfer_id: transfer.sha256.clone(),
            index,
            count,
            sha256: hex::encode(Sha256::digest(piece)),
            data_b64: base64::engine::general_purpose::STANDARD.encode(piece),
        })
        .collect())
}

/// What one piece came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkOutcome {
    /// Kept; `have` of `count` pieces are here.
    Partial { have: u32, count: u32 },
    /// The last piece: the package verified and is staged under its id.
    Staged(String),
    /// The package is already staged here; the piece is not needed.
    AlreadyStaged,
}

fn partial_dir(transfer_id: &str, sender: &str) -> std::path::PathBuf {
    crate::config::Config::state_dir()
        .join("transfer-chunks")
        .join(format!(
            "{transfer_id}-{}",
            sender.get(..16).unwrap_or(sender)
        ))
}

/// Drop the pieces of packages that never completed.
fn sweep_partials() {
    let root = crate::config::Config::state_dir().join("transfer-chunks");
    let Ok(entries) = fs::read_dir(&root) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > PARTIAL_TRANSFER_TTL);
        if stale {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// A package arriving in pieces that is not complete yet: which pieces are
/// still missing, for the sender to send again (`POST /api/transfers/{id}/send`
/// with those `pieces`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PartialDelivery {
    pub transfer_id: String,
    /// The start of the sending node's id.
    pub sender: String,
    pub count: u32,
    pub missing: Vec<u32>,
}

pub fn partial_deliveries() -> Vec<PartialDelivery> {
    let root = crate::config::Config::state_dir().join("transfer-chunks");
    let Ok(entries) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((transfer_id, sender)) = name.split_once('-') else {
            continue;
        };
        let dir = entry.path();
        let Some(count) = fs::read_to_string(dir.join("count"))
            .ok()
            .and_then(|count| count.trim().parse::<u32>().ok())
            .filter(|count| *count <= MAX_MESH_CHUNKS)
        else {
            continue;
        };
        out.push(PartialDelivery {
            transfer_id: transfer_id.to_string(),
            sender: sender.to_string(),
            count,
            missing: (0..count)
                .filter(|index| !dir.join(format!("{index:05}")).exists())
                .collect(),
        });
    }
    out.sort_by(|a, b| a.transfer_id.cmp(&b.transfer_id));
    out
}

/// Keep one piece of a package a peer is sending in pieces, and once every
/// piece is here, verify and stage the package as `receive_mesh` does: its
/// signature and its signed sha256 (which must be the `transfer_id` every
/// piece named), its sender and its target. Pieces are kept on disk per
/// sender, so a delivery cut off by a restart on either side completes when
/// the rest arrive, and a piece sent again is taken once. Only a member of
/// a channel this node holds is kept for at all.
pub fn receive_chunk(
    store: &Store,
    receiver_node: &str,
    envelope_sender: &str,
    envelope_channel: &str,
    chunk: MeshChunk,
) -> Result<ChunkOutcome, TransferError> {
    let hex64 = |value: &str| value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit());
    if !hex64(&chunk.transfer_id)
        || !hex64(&chunk.sha256)
        || chunk.count == 0
        || chunk.count > MAX_MESH_CHUNKS
        || chunk.index >= chunk.count
    {
        return Err(TransferError::Invalid("malformed transfer piece".into()));
    }
    if store.channel_get(envelope_channel)?.is_none() {
        return Err(TransferError::ReceiverUnauthorized(envelope_channel.into()));
    }
    if !store
        .nodes_in_channel(envelope_channel)?
        .iter()
        .any(|node| node == envelope_sender)
    {
        return Err(TransferError::OriginUnauthorized(envelope_channel.into()));
    }
    if store.transfer(&chunk.transfer_id)?.is_some() {
        return Ok(ChunkOutcome::AlreadyStaged);
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(chunk.data_b64.as_bytes())
        .map_err(|_| TransferError::Invalid("bad base64 in a transfer piece".into()))?;
    if bytes.len() > MESH_CHUNK_BYTES || hex::encode(Sha256::digest(&bytes)) != chunk.sha256 {
        return Err(TransferError::Invalid(
            "a transfer piece does not match its digest".into(),
        ));
    }
    sweep_partials();
    let io = |error: std::io::Error| TransferError::Workspace(error.to_string());
    let dir = partial_dir(&chunk.transfer_id, envelope_sender);
    fs::create_dir_all(&dir).map_err(io)?;
    let count_file = dir.join("count");
    match fs::read_to_string(&count_file) {
        Ok(count) if count.trim() != chunk.count.to_string() => {
            return Err(TransferError::Invalid(
                "transfer pieces disagree on how many there are".into(),
            ));
        }
        Ok(_) => {}
        Err(_) => fs::write(&count_file, chunk.count.to_string()).map_err(io)?,
    }
    let piece = dir.join(format!("{:05}", chunk.index));
    if !piece.exists() {
        let partial = dir.join(format!("{:05}.part", chunk.index));
        fs::write(&partial, &bytes).map_err(io)?;
        fs::rename(&partial, &piece).map_err(io)?;
    }
    let have = (0..chunk.count)
        .filter(|index| dir.join(format!("{index:05}")).exists())
        .count() as u32;
    if have < chunk.count {
        return Ok(ChunkOutcome::Partial {
            have,
            count: chunk.count,
        });
    }
    let mut whole = Vec::new();
    for index in 0..chunk.count {
        whole.extend(fs::read(dir.join(format!("{index:05}"))).map_err(io)?);
    }
    // The pieces are spent either way: a package that does not verify is
    // not completed by keeping them, and a resend starts clean.
    let _ = fs::remove_dir_all(&dir);
    let value: Value = serde_json::from_slice(&whole)?;
    if value["sha256"].as_str() != Some(chunk.transfer_id.as_str()) {
        return Err(TransferError::BadDigest);
    }
    let id = receive_mesh(
        store,
        receiver_node,
        envelope_sender,
        envelope_channel,
        value,
    )?;
    Ok(ChunkOutcome::Staged(id))
}

/// Portable imports are deliberately allowed without a source-selected target,
/// but always require the receiving node to hold this channel and an operator
/// confirmation before files leave the package.
pub fn authorize_receiver(
    store: &Store,
    receiver_node: &str,
    transfer: &SignedTransfer,
) -> Result<(), TransferError> {
    transfer.verify()?;
    if transfer
        .payload
        .target_node
        .as_deref()
        .is_some_and(|target| target != receiver_node)
    {
        return Err(TransferError::WrongTarget);
    }
    if store.channel_get(&transfer.payload.channel)?.is_none() {
        return Err(TransferError::ReceiverUnauthorized(
            transfer.payload.channel.clone(),
        ));
    }
    let members = store.nodes_in_channel(&transfer.payload.channel)?;
    if !members
        .iter()
        .any(|node| node == &transfer.payload.origin_node)
    {
        return Err(TransferError::OriginUnauthorized(
            transfer.payload.channel.clone(),
        ));
    }
    Ok(())
}

pub fn record_portable_receipt(
    store: &Store,
    transfer: &SignedTransfer,
) -> Result<(), TransferError> {
    let row = transfer.as_row()?;
    if store.insert_transfer(&row)? {
        store.append_transfer_event(transfer.id(), "received_portable", None, None)?;
    }
    Ok(())
}

/// Materialize a verified package in a fresh, runtime-owned workspace. The
/// selected directory is short-lived: only the backend receives the safe
/// snapshot, never an operator checkout, mounted home, or live session state.
pub async fn import_workspace(
    transfer: &SignedTransfer,
    git: &str,
    backend: &dyn crate::boundary::Backend,
) -> Result<crate::workspace::Workspace, TransferError> {
    transfer.verify()?;
    let id = format!("transfer-{}", uuid::Uuid::now_v7());
    let selected = crate::config::Config::state_dir()
        .join("transfer-imports")
        .join(&id);
    fs::create_dir_all(&selected).map_err(|error| TransferError::Workspace(error.to_string()))?;

    let result = async {
        for file in &transfer.payload.files {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(file.content_b64.as_bytes())
                .map_err(|_| TransferError::Invalid(format!("bad base64 for {:?}", file.path)))?;
            let destination = crate::workspace::selected_path(&selected, &file.path)
                .map_err(|error| TransferError::Workspace(error.to_string()))?;
            let parent = destination
                .parent()
                .ok_or_else(|| TransferError::UnsafePath(file.path.clone()))?;
            fs::create_dir_all(parent)
                .map_err(|error| TransferError::Workspace(error.to_string()))?;
            fs::write(&destination, bytes)
                .map_err(|error| TransferError::Workspace(error.to_string()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = if file.mode & 0o111 == 0 { 0o644 } else { 0o755 };
                fs::set_permissions(&destination, fs::Permissions::from_mode(mode))
                    .map_err(|error| TransferError::Workspace(error.to_string()))?;
            }
        }
        crate::workspace::write_context(&selected, &context_json(transfer))
            .map_err(|error| TransferError::Workspace(error.to_string()))?;
        let workspace = crate::workspace::seed_from_files(git, &selected, &id)
            .await
            .map_err(|error| TransferError::Workspace(error.to_string()))?;
        crate::workspace::import(backend, &workspace, &crate::workspace::staging_path(&id))
            .await
            .map_err(|error| TransferError::Workspace(error.to_string()))?;
        Ok(workspace)
    }
    .await;

    let _ = fs::remove_dir_all(&selected);
    result
}

/// The branch's history out of a session's snapshot, as a package carries
/// it: its commits from where it left its base (only when it came from a
/// forge, `origin`, whose clone the receiver reads the base from) to the
/// candidate's head, or the branch's later tip when it contains it.
/// `snapshot` is a node-owned, sanitized export of the session's workspace.
pub async fn capture_git(
    git: &str,
    snapshot: &Path,
    head_sha: &str,
    branch: &str,
    origin: Option<String>,
) -> Result<TransferGit, TransferError> {
    let dir = crate::config::Config::state_dir().join("transfer-exports");
    fs::create_dir_all(&dir).map_err(|error| TransferError::Workspace(error.to_string()))?;
    let bundle = dir.join(format!("{}.bundle", uuid::Uuid::now_v7()));
    let captured = crate::workspace::bundle_history(
        git,
        snapshot,
        head_sha,
        branch,
        origin.is_some(),
        &bundle,
    )
    .await;
    let read = match &captured {
        Ok((_, true)) => fs::read(&bundle)
            .map(Some)
            .map_err(|error| TransferError::Workspace(error.to_string())),
        _ => Ok(None),
    };
    let _ = fs::remove_file(&bundle);
    let (history, _) = captured.map_err(|error| TransferError::Workspace(error.to_string()))?;
    let bytes = read?;
    if bytes
        .as_ref()
        .is_some_and(|bytes| bytes.len() > MAX_TRANSFER_BYTES)
    {
        return Err(TransferError::TooLarge);
    }
    Ok(TransferGit {
        branch: branch.to_string(),
        origin,
        history,
        bundle_b64: bytes
            .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes))
            .unwrap_or_default(),
    })
}

/// What a move says about the session it takes the work from: its links and
/// its last state (the reviews it still had open, its last check, and what
/// it was waiting on).
pub fn describe_source(
    store: &Store,
    session: &crate::store::SessionRow,
    intent: TransferIntent,
    waiting_on: Vec<crate::session::waits::Wait>,
) -> Result<TransferSource, TransferError> {
    let clip = |value: &str| {
        if value.len() <= MAX_SOURCE_FIELD_BYTES {
            return value.to_string();
        }
        let mut end = MAX_SOURCE_FIELD_BYTES - 3;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &value[..end])
    };
    let project = session
        .project_id
        .as_deref()
        .map(|id| store.project_get(id))
        .transpose()?
        .flatten()
        .map(|project| TransferProject {
            id: project.id,
            name: clip(&project.name),
            remote_url: project.remote_url,
        });
    let open_reviews = store
        .carried_reviews(&session.id)?
        .into_iter()
        .take(MAX_SOURCE_ITEMS)
        .map(|review| TransferReview {
            id: review.id,
            kind: review.kind,
            state: review.state,
            title: clip(&review.title),
        })
        .collect();
    let last_check = store
        .check_runs_for_session(&session.id)?
        .into_iter()
        .max_by_key(|run| (run.finished_ms.unwrap_or(run.started_ms), run.started_ms))
        .map(|run| TransferCheck {
            id: run.id,
            command: run.command.as_deref().map(clip),
            outcome: run.outcome,
            exit_code: run.exit_code,
            finished_ms: run.finished_ms,
        });
    let waiting_on = waiting_on
        .into_iter()
        .filter(|wait| wait.decided.is_none())
        .take(MAX_SOURCE_ITEMS)
        .map(|wait| TransferWait {
            tool: wait.tool.to_string(),
            id: clip(&wait.id),
            about: clip(&wait.about),
        })
        .collect();
    Ok(TransferSource {
        intent,
        session_id: session.id.clone(),
        phase: session.phase.clone(),
        state: session.state.clone(),
        model: clip(&session.model),
        harness: session.harness_id.clone(),
        budget_tokens: session.budget_tokens.max(0),
        tokens_used: session.tokens_used,
        work_item_id: session.work_item_id.clone(),
        project,
        open_reviews,
        last_check,
        waiting_on,
    })
}

/// Recreate a version 2 package's branch in a fresh, runtime-owned workspace:
/// the commits it carries, on `branch`, at the head it names, on top of
/// `base` (the receiver's own synced clone of the package's origin) when it
/// came from a forge. The head must contain the candidate and hold its
/// recorded tree, or nothing is imported. The workspace is the clone's as
/// any session's is (`origin/<base>`, `origin/HEAD`), and the transfer's
/// context is attached as for a version 1 import.
pub async fn import_history(
    transfer: &SignedTransfer,
    git: &str,
    backend: &dyn crate::boundary::Backend,
    base: Option<crate::workspace::HistoryBase<'_>>,
    branch: &str,
) -> Result<crate::workspace::Workspace, TransferError> {
    transfer.verify()?;
    let carried = transfer
        .payload
        .git
        .as_ref()
        .ok_or_else(|| TransferError::Invalid("the package carries no Git history".into()))?;
    if !branch_name(branch) {
        return Err(TransferError::Invalid(format!(
            "{branch:?} is not a branch name"
        )));
    }
    let head_sha = transfer.payload.candidate["head_sha"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let tree_sha = transfer.payload.candidate["tree_sha"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let id = format!("transfer-{}", uuid::Uuid::now_v7());
    let dir = crate::config::Config::state_dir().join("transfer-imports");
    fs::create_dir_all(&dir).map_err(|error| TransferError::Workspace(error.to_string()))?;
    let bundle_path = dir.join(format!("{id}.bundle"));
    let result = async {
        let bundle = if carried.bundle_b64.is_empty() {
            None
        } else {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(carried.bundle_b64.as_bytes())
                .map_err(|_| TransferError::Invalid("bad base64 for the Git bundle".into()))?;
            fs::write(&bundle_path, bytes)
                .map_err(|error| TransferError::Workspace(error.to_string()))?;
            Some(bundle_path.as_path())
        };
        let workspace = crate::workspace::seed_from_history(
            git,
            base,
            bundle,
            &carried.history,
            &head_sha,
            &tree_sha,
            branch,
            &id,
        )
        .await
        .map_err(|error| TransferError::Workspace(error.to_string()))?;
        let stage = crate::workspace::staging_path(&id);
        crate::workspace::write_context(&stage, &context_json(transfer))
            .map_err(|error| TransferError::Workspace(error.to_string()))?;
        // The context is provenance for the session, not part of its work:
        // keep it out of what a `git add -A` would put up for review.
        let exclude = stage.join(".git").join("info").join("exclude");
        let mut rules = fs::read_to_string(&exclude).unwrap_or_default();
        if !rules.lines().any(|line| line.trim() == "/.tracon/") {
            if !rules.is_empty() && !rules.ends_with('\n') {
                rules.push('\n');
            }
            rules.push_str("/.tracon/\n");
            fs::write(&exclude, rules)
                .map_err(|error| TransferError::Workspace(error.to_string()))?;
        }
        crate::workspace::import(backend, &workspace, &stage)
            .await
            .map_err(|error| TransferError::Workspace(error.to_string()))?;
        Ok(workspace)
    }
    .await;
    let _ = fs::remove_file(&bundle_path);
    let _ = fs::remove_dir_all(crate::workspace::staging_path(&id));
    result
}

/// A version 2 import as a node does it: the package's origin resolved to
/// this node's own managed clone (cloned with the channel's forge credential
/// when it is not here yet, and synced), the branch recreated on it, and the
/// clone's path returned for the workspace's origin record, so the session's
/// `fetch_branch`, `submit_review` and diffs find the repository. A package
/// with no origin is recreated on a repository of its own.
pub async fn import_onto_origin(
    transfer: &SignedTransfer,
    manager: &crate::session::Manager,
    node_id: &str,
    git: &str,
    branch: &str,
) -> Result<(crate::workspace::Workspace, Option<std::path::PathBuf>), TransferError> {
    let carried = transfer
        .payload
        .git
        .as_ref()
        .ok_or_else(|| TransferError::Invalid("the package carries no Git history".into()))?;
    let Some(origin) = carried.origin.as_deref() else {
        let workspace =
            import_history(transfer, git, manager.backend().as_ref(), None, branch).await?;
        return Ok((workspace, None));
    };
    let channel = &transfer.payload.channel;
    let state_dir = crate::config::Config::state_dir();
    let repo = crate::forge::session_repo(&state_dir, origin).map_err(TransferError::Workspace)?;
    if !repo.join(".git").exists() {
        crate::forge::ensure_clone_of(manager.broker(), channel, node_id, origin)
            .await
            .map_err(|error| {
                TransferError::Workspace(format!("{origin} could not be cloned: {error}"))
            })?;
    }
    let repo = crate::forge::managed_clone(&repo).map_err(TransferError::Workspace)?;
    let brokered =
        crate::forge::git_token_for(manager.broker(), &state_dir, channel, &repo, node_id);
    let credential = match &brokered {
        Some((forge, token)) => crate::git_remote::brokered(forge.git_user(), Some(token.as_str())),
        None => crate::git_remote::Credential::Anonymous,
    };
    // Held from the sync until the seed has read the clone, as a session
    // start holds it.
    let lock = crate::forge::clone_lock(&repo);
    let _guard = lock.lock().await;
    let base_branch = carried.history.base_branch.as_deref();
    let synced = match crate::forge::sync_managed(&repo, &credential, None, base_branch).await {
        Ok(synced) => synced,
        // The base branch is gone from the forge: the base commit may still
        // be on the default, and the seed looks for it on every branch.
        Err(_) if base_branch.is_some() => {
            crate::forge::sync_managed(&repo, &credential, None, None)
                .await
                .map_err(TransferError::Workspace)?
        }
        Err(error) => return Err(TransferError::Workspace(error)),
    };
    let base = (!synced.empty).then(|| crate::workspace::HistoryBase {
        clone: &repo,
        start: synced.source_ref.as_deref(),
        onto_start: synced.onto_base,
    });
    let workspace = import_history(transfer, git, manager.backend().as_ref(), base, branch).await?;
    Ok((workspace, Some(repo)))
}

fn context_json(transfer: &SignedTransfer) -> Value {
    serde_json::json!({
        "transfer_id": transfer.id(),
        "candidate_id": transfer.payload.candidate_id,
        "origin_node": transfer.payload.origin_node,
        "channel": transfer.payload.channel,
        "created_ms": transfer.payload.created_ms,
        "candidate": transfer.payload.candidate,
        "evidence": transfer.payload.evidence,
        "context": transfer.payload.context,
        "source": transfer.payload.source,
    })
}

/// What the new session is told as its first prompt: where the work came
/// from, the export note, and, for a move, the source's last state. `None`
/// for a handoff with no note, which starts as an import always has.
/// `history` is whether the branch's commits arrived (a version 1 package,
/// or one whose base the receiver could not read, has only the files).
pub fn first_prompt(transfer: &SignedTransfer, branch: &str, history: bool) -> Option<String> {
    let payload = &transfer.payload;
    let note = payload.context.note.trim();
    let source = payload.source.as_ref();
    if source.is_none() && note.is_empty() {
        return None;
    }
    let origin = crate::names::node(&payload.origin_node, None);
    let mut out = match source {
        Some(source) => format!(
            "This session continues {} (id {}), which was moved here from node {origin}. \
             You inherit none of its context: not its conversation, not its plan, not what \
             it had already tried.",
            crate::names::session(&source.session_id),
            source.session_id,
        ),
        None => format!(
            "This session was started from a transfer package that node {origin} exported \
             for candidate {}.",
            payload.candidate_id
        ),
    };
    out.push_str("\n\n");
    if history {
        out.push_str(&format!(
            "What you do have is its branch `{branch}`, with its commits, checked out on \
             top of the base it started from. Start by reading the workspace (the log and \
             the diff against the base) and say what you find before changing anything."
        ));
    } else {
        out.push_str(&format!(
            "What you do have is its files, on branch `{branch}`, without their Git \
             history. Start by reading the workspace and say what you find before \
             changing anything."
        ));
    }
    if !note.is_empty() {
        out.push_str("\n\nThe note it was sent with:\n\n");
        for line in note.lines() {
            out.push_str("> ");
            out.push_str(line);
            out.push('\n');
        }
    }
    let Some(source) = source else {
        return Some(out);
    };
    out.push_str(&format!(
        "\n\nWhen it was moved it was {} in the {} phase.",
        source.state, source.phase
    ));
    if let Some(item) = source.work_item_id.as_deref() {
        out.push_str(&format!(
            " It was working on item {item}, which is yours now."
        ));
    }
    if !source.open_reviews.is_empty() {
        out.push_str(
            "\n\nIt had these open on its own node, and they stay there as they are; they \
             are not yours to poll. When the work is ready, call `submit_review` from this \
             branch, which opens a new review here:\n\n",
        );
        for review in &source.open_reviews {
            out.push_str(&format!(
                "- {} `{}` ({}): {}\n",
                review.kind, review.id, review.state, review.title
            ));
        }
    }
    if let Some(check) = &source.last_check {
        let command = check
            .command
            .as_deref()
            .map(|command| format!(" `{command}`"))
            .unwrap_or_default();
        let exit = check
            .exit_code
            .map(|code| format!(", exit {code}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "\n\nIts last check{command} came to {}{exit}.",
            check.outcome
        ));
    }
    if !source.waiting_on.is_empty() {
        out.push_str(
            "\n\nIt was waiting on these, asked by the earlier session; ask again if you \
             still need them:\n\n",
        );
        for wait in &source.waiting_on {
            out.push_str(&format!("- {} `{}`: {}\n", wait.tool, wait.id, wait.about));
        }
    }
    Some(out)
}

/// Read a package off disk without a node, a store, or a harness.
///
/// A package is the archive of a piece of work, and an archive nobody can open
/// is not an archive. This is the offline reader: plain JSON in, plain records
/// out, no database and no network.
pub fn read_package(path: &Path) -> Result<SignedTransfer, TransferError> {
    let bytes = fs::read(path)
        .map_err(|error| TransferError::Invalid(format!("reading {}: {error}", path.display())))?;
    if bytes.len() > MAX_TRANSFER_BYTES {
        return Err(TransferError::TooLarge);
    }
    Ok(serde_json::from_slice(&bytes)?)
}

/// The package rendered as one JSON object per line, each tagged with a
/// `kind`, in a fixed order: the manifest, the candidate, its evidence, the
/// handoff note, the branch's history and the moved session (version 2), the
/// carried documents and memories, then one line per file.
///
/// File contents are left out and named by digest instead: a dump is for
/// reading, and the bytes are recovered by importing the package or by
/// decoding `content_b64` from the package itself. Every line is
/// self-describing, so `grep`, `jq`, and a text editor are sufficient tools.
pub fn package_jsonl(transfer: &SignedTransfer) -> Vec<Value> {
    let verified = transfer.verify();
    let payload = &transfer.payload;
    let mut out = vec![serde_json::json!({
        "kind": "package",
        "id": transfer.sha256,
        "version": payload.version,
        "candidate_id": payload.candidate_id,
        "channel": payload.channel,
        "origin_node": payload.origin_node,
        "target_node": payload.target_node,
        "created_ms": payload.created_ms,
        "files": payload.files.len(),
        "documents": payload.context.documents.len(),
        "memories": payload.context.memories.len(),
        "verified": verified.is_ok(),
        "verify_error": verified.err().map(|error| error.to_string()),
    })];
    out.push(serde_json::json!({ "kind": "candidate", "candidate": payload.candidate }));
    out.push(serde_json::json!({ "kind": "evidence", "evidence": payload.evidence }));
    if !payload.context.note.is_empty() {
        out.push(serde_json::json!({ "kind": "note", "text": payload.context.note }));
    }
    if let Some(git) = &payload.git {
        let bundle = base64::engine::general_purpose::STANDARD
            .decode(git.bundle_b64.as_bytes())
            .unwrap_or_default();
        out.push(serde_json::json!({
            "kind": "git",
            "branch": git.branch,
            "origin": git.origin,
            "head": git.history.head,
            "base_sha": git.history.base_sha,
            "base_branch": git.history.base_branch,
            "bundle_bytes": bundle.len(),
            "bundle_sha256": hex::encode(Sha256::digest(&bundle)),
        }));
    }
    if let Some(source) = &payload.source {
        let mut record = serde_json::to_value(source).unwrap_or_default();
        record["kind"] = serde_json::json!("source");
        out.push(record);
    }
    for document in &payload.context.documents {
        out.push(serde_json::json!({
            "kind": "document",
            "slug": document.slug,
            "title": document.title,
            "doc_kind": document.kind,
            "format": document.format,
            "updated_ms": document.updated_ms,
            "body": document.body,
        }));
    }
    for memory in &payload.context.memories {
        out.push(serde_json::json!({
            "kind": "memory",
            "id": memory.id,
            "memory_kind": memory.kind,
            "scope": memory.scope,
            "created_ms": memory.created_ms,
            "body": memory.body,
        }));
    }
    for file in &payload.files {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(file.content_b64.as_bytes())
            .unwrap_or_default();
        out.push(serde_json::json!({
            "kind": "file",
            "path": file.path,
            "mode": format!("{:o}", file.mode),
            "bytes": bytes.len(),
            "sha256": hex::encode(Sha256::digest(&bytes)),
        }));
    }
    out
}

pub fn validate_files(files: &[TransferFile]) -> Result<(), TransferError> {
    if files.len() > MAX_TRANSFER_FILES {
        return Err(TransferError::TooManyFiles);
    }
    let mut total = 0usize;
    // Source trees are portable only when they cannot collide on the
    // case-insensitive, normalization-sensitive filesystems this node may use.
    let mut paths = std::collections::HashSet::with_capacity(files.len());
    for file in files {
        safe_path(&file.path)?;
        // Git captures regular modes as 100644/100755. Symlinks and special
        // files are rejected rather than being reconstructed ambiguously.
        if file.mode & 0o170000 != 0o100000 {
            return Err(TransferError::UnsafeFileMode(file.path.clone()));
        }
        if !paths.insert(file.path.to_ascii_lowercase()) {
            return Err(TransferError::Invalid(format!(
                "case-folded duplicate file {:?}",
                file.path
            )));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(file.content_b64.as_bytes())
            .map_err(|_| TransferError::Invalid(format!("bad base64 for {:?}", file.path)))?;
        total = total
            .checked_add(bytes.len())
            .ok_or(TransferError::TooLarge)?;
        if total > MAX_TRANSFER_BYTES {
            return Err(TransferError::TooLarge);
        }
    }
    Ok(())
}

fn safe_path(path: &str) -> Result<(), TransferError> {
    if path.is_empty() || path.len() > 1024 || !path.is_ascii() || path.contains('\0') {
        return Err(TransferError::UnsafePath(path.into()));
    }
    let parsed = Path::new(path);
    let lower = path.to_ascii_lowercase();
    if parsed
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
        || parsed.components().next().is_none()
        || lower
            .split('/')
            .any(|component| matches!(component, ".git" | ".tracon" | ".tracon-transfer"))
    {
        return Err(TransferError::UnsafePath(path.into()));
    }
    Ok(())
}

fn unique(values: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::with_capacity(values.len());
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::store::{CandidateFile, CandidateRow};

    const HEAD_SHA: &str = "deadbeefcafebabedeadbeefcafebabedeadbeef";
    // The real git tree hash for one file `hello.txt` containing `hello\n`
    // at mode 100644, cross-checked against actual `git` in git_tree's tests.
    const TREE_SHA: &str = "aaa96ced2d9a1c8e72c56b253a0e2fe78393feb7";

    fn setup() -> (Store, proto::keys::Identity, String) {
        let store = Store::open_in_memory().unwrap();
        let identity = proto::keys::Identity::from_seed(&[9u8; 32]);
        store.channel_put("personal", b"ring", "{}").unwrap();
        let candidate_id = format!("{HEAD_SHA}:personal");
        store
            .insert_candidate(&CandidateRow {
                id: candidate_id.clone(),
                head_sha: HEAD_SHA.into(),
                tree_sha: Some(TREE_SHA.into()),
                channel: "personal".into(),
                owner_session_id: Some("s1".into()),
                source_kind: "git".into(),
                captured_ms: 1,
                capture_json: r#"{"materialized":true}"#.into(),
            })
            .unwrap();
        store
            .insert_candidate_files(
                &candidate_id,
                &[CandidateFile {
                    path: "hello.txt".into(),
                    mode: 0o100644,
                    content: b"hello\n".to_vec(),
                }],
            )
            .unwrap();
        (store, identity, candidate_id)
    }

    #[test]
    fn export_then_verify_round_trips() {
        let (store, identity, candidate_id) = setup();
        let transfer = export(
            &store,
            &identity,
            &candidate_id,
            ContextSelection::default(),
            None,
        )
        .unwrap();
        transfer.verify().unwrap();
    }

    /// A package is the archive of a piece of work. If the only thing that can
    /// open one is a running node with the right schema, it is not an archive
    /// — so the file reads back with nothing but the binary and a filesystem,
    /// and every record says what kind it is.
    #[test]
    fn a_package_reads_back_off_disk_without_a_node() {
        let (store, identity, candidate_id) = setup();
        store
            .write_change(
                "node",
                "personal",
                "document",
                tracon_sync::ChangeOp::Upsert,
                "doc-1",
                json!({
                    "channel": "personal",
                    "slug": "guide-a",
                    "kind": "guide",
                    "title": "A",
                    "body": "# A\n\nthe guide body",
                    "hash": "hash",
                    "created_ms": 1,
                    "updated_ms": 1,
                }),
            )
            .unwrap();
        store
            .write_change(
                "node",
                "personal",
                "memory",
                tracon_sync::ChangeOp::Upsert,
                "mem-1",
                json!({
                    "channel": "personal",
                    "scope": "global",
                    "scope_ref": Value::Null,
                    "kind": "directive",
                    "body": "always run the checks",
                    "source_session": Value::Null,
                    "source_node": Value::Null,
                    "confidence": 1.0,
                    "state": "active",
                    "created_ms": 1,
                    "updated_ms": 1,
                }),
            )
            .unwrap();
        let transfer = export(
            &store,
            &identity,
            &candidate_id,
            ContextSelection {
                documents: vec!["guide-a".into()],
                memories: vec!["mem-1".into()],
                note: "picked up after the node went down".into(),
            },
            None,
        )
        .unwrap();

        let dir = std::env::temp_dir().join(format!("tracon-package-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("package.json");
        std::fs::write(&path, serde_json::to_vec(&transfer).unwrap()).unwrap();

        let read = read_package(&path).unwrap();
        let records = package_jsonl(&read);
        let kinds: Vec<&str> = records
            .iter()
            .map(|r| r["kind"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "package",
                "candidate",
                "evidence",
                "note",
                "document",
                "memory",
                "file"
            ]
        );
        assert_eq!(records[0]["verified"], json!(true));
        assert_eq!(records[0]["verify_error"], Value::Null);
        assert_eq!(records[0]["candidate_id"], json!(candidate_id));
        assert_eq!(
            records[3]["text"],
            json!("picked up after the node went down")
        );
        assert_eq!(records[4]["slug"], json!("guide-a"));
        assert_eq!(records[4]["body"], json!("# A\n\nthe guide body"));
        assert_eq!(records[5]["body"], json!("always run the checks"));
        assert_eq!(records[6]["path"], json!("hello.txt"));
        assert_eq!(records[6]["bytes"], json!(6));
        assert_eq!(
            records[6]["sha256"],
            json!(hex::encode(Sha256::digest(b"hello\n")))
        );
        // Every record is one line of JSON, which is the whole of the format.
        for record in &records {
            let line = record.to_string();
            assert!(!line.contains('\n'), "{line}");
        }

        // A package that does not verify still reads: an archive that refuses
        // to render is worse than one that says plainly it was tampered with.
        let mut tampered = read;
        tampered.payload.context.note = "not what was signed".into();
        let records = package_jsonl(&tampered);
        assert_eq!(records[0]["verified"], json!(false));
        assert!(records[0]["verify_error"].is_string());
        assert_eq!(records[3]["text"], json!("not what was signed"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_rejects_evidence_naming_a_different_candidate() {
        let (store, identity, candidate_id) = setup();
        let mut transfer = export(
            &store,
            &identity,
            &candidate_id,
            ContextSelection::default(),
            None,
        )
        .unwrap();
        transfer.payload.evidence["candidate"]["id"] = json!("some-other-candidate");
        assert!(matches!(transfer.verify(), Err(TransferError::Invalid(_))));
    }

    #[test]
    fn verify_rejects_files_that_do_not_match_the_candidate_tree() {
        let (store, identity, candidate_id) = setup();
        let mut transfer = export(
            &store,
            &identity,
            &candidate_id,
            ContextSelection::default(),
            None,
        )
        .unwrap();
        transfer.payload.files[0].content_b64 =
            base64::engine::general_purpose::STANDARD.encode(b"tampered");
        assert!(matches!(transfer.verify(), Err(TransferError::Invalid(_))));
    }

    #[test]
    fn verify_rejects_a_candidate_with_no_recorded_tree_hash() {
        let (store, identity, candidate_id) = setup();
        let mut transfer = export(
            &store,
            &identity,
            &candidate_id,
            ContextSelection::default(),
            None,
        )
        .unwrap();
        transfer.payload.candidate["tree_sha"] = Value::Null;
        transfer.payload.evidence["candidate"]["tree_sha"] = Value::Null;
        assert!(matches!(transfer.verify(), Err(TransferError::Invalid(_))));
    }

    fn moved_extras() -> Extras {
        Extras {
            git: Some(TransferGit {
                branch: "feat/move".into(),
                origin: Some("github.com/owner/repo".into()),
                history: crate::workspace::BranchHistory {
                    head: HEAD_SHA.into(),
                    base_sha: Some("cafebabecafebabecafebabecafebabecafebabe".into()),
                    base_branch: Some("main".into()),
                },
                bundle_b64: base64::engine::general_purpose::STANDARD.encode(b"bundle"),
            }),
            source: Some(TransferSource {
                intent: TransferIntent::Move,
                session_id: "s1".into(),
                phase: "execute".into(),
                state: "running".into(),
                model: "m".into(),
                harness: "claude".into(),
                budget_tokens: 500_000,
                tokens_used: 1_000,
                work_item_id: Some("item-1".into()),
                project: Some(TransferProject {
                    id: "p1".into(),
                    name: "repo".into(),
                    remote_url: Some("github.com/owner/repo".into()),
                }),
                open_reviews: vec![TransferReview {
                    id: "rev-1".into(),
                    kind: "review".into(),
                    state: "pending".into(),
                    title: "Add the thing".into(),
                }],
                last_check: Some(TransferCheck {
                    id: "chk-1".into(),
                    command: Some("just check".into()),
                    outcome: "failed".into(),
                    exit_code: Some(1),
                    finished_ms: Some(2),
                }),
                waiting_on: vec![TransferWait {
                    tool: "ask_status".into(),
                    id: "q-1".into(),
                    about: "Which base?".into(),
                }],
            }),
        }
    }

    /// A package without history or a moved session is the version 1 package
    /// it always was, byte for byte: an older peer still verifies it.
    #[test]
    fn a_plain_package_stays_version_one() {
        let (store, identity, candidate_id) = setup();
        let transfer = export(
            &store,
            &identity,
            &candidate_id,
            ContextSelection::default(),
            None,
        )
        .unwrap();
        assert_eq!(transfer.payload.version, TRANSFER_VERSION);
        let json = serde_json::to_value(&transfer.payload).unwrap();
        assert!(
            json.get("git").is_none() && json.get("source").is_none(),
            "{json}"
        );
    }

    /// History and the moved session are signed with the rest of the package,
    /// and make it version 2; claiming version 1 for one, or changing either,
    /// fails verification.
    #[test]
    fn a_moved_package_is_version_two_and_signed() {
        let (store, identity, candidate_id) = setup();
        let transfer = export_with(
            &store,
            &identity,
            &candidate_id,
            ContextSelection::default(),
            Some("receiver".into()),
            moved_extras(),
        )
        .unwrap();
        assert_eq!(transfer.payload.version, TRANSFER_VERSION_HISTORY);
        transfer.verify().unwrap();
        let round: SignedTransfer =
            serde_json::from_slice(&serde_json::to_vec(&transfer).unwrap()).unwrap();
        round.verify().unwrap();

        let kinds: Vec<String> = package_jsonl(&transfer)
            .iter()
            .map(|record| record["kind"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            kinds,
            ["package", "candidate", "evidence", "git", "source", "file"]
        );

        let mut downgraded = transfer.clone();
        downgraded.payload.version = TRANSFER_VERSION;
        assert!(matches!(
            downgraded.verify(),
            Err(TransferError::Invalid(_))
        ));

        let mut retargeted = transfer.clone();
        retargeted.payload.source.as_mut().unwrap().budget_tokens = 9;
        assert!(matches!(retargeted.verify(), Err(TransferError::BadDigest)));

        let mut rebranched = transfer.clone();
        rebranched.payload.git.as_mut().unwrap().branch = "--upload-pack=x".into();
        assert!(matches!(
            rebranched.verify(),
            Err(TransferError::Invalid(_))
        ));

        let mut no_origin = transfer;
        no_origin.payload.git.as_mut().unwrap().origin = None;
        assert!(matches!(no_origin.verify(), Err(TransferError::Invalid(_))));
    }

    /// The new session's first prompt carries the export note and the
    /// source's last state: its open review (left where it is), its last
    /// check, and what it was waiting on.
    #[test]
    fn a_move_tells_the_new_session_the_note_and_the_last_state() {
        let (store, identity, candidate_id) = setup();
        let transfer = export_with(
            &store,
            &identity,
            &candidate_id,
            ContextSelection {
                note: "finish the migration, then resubmit".into(),
                ..Default::default()
            },
            None,
            moved_extras(),
        )
        .unwrap();
        let prompt = first_prompt(&transfer, "feat/move", true).unwrap();
        for expected in [
            "(id s1), which was moved here",
            "branch `feat/move`, with its commits",
            "> finish the migration, then resubmit",
            "item item-1, which is yours now",
            "review `rev-1` (pending): Add the thing",
            "`submit_review` from this branch",
            "last check `just check` came to failed, exit 1",
            "ask_status `q-1`: Which base?",
        ] {
            assert!(
                prompt.contains(expected),
                "{expected:?} missing from:\n{prompt}"
            );
        }
        let files_only = first_prompt(&transfer, "feat/move", false).unwrap();
        assert!(
            files_only.contains("without their Git history"),
            "{files_only}"
        );

        // A handoff with no note starts as an import always has.
        let plain = export(
            &store,
            &identity,
            &candidate_id,
            ContextSelection::default(),
            None,
        )
        .unwrap();
        assert_eq!(first_prompt(&plain, "x", false), None);
    }

    #[test]
    fn describe_source_reads_the_sessions_links_and_open_reviews() {
        let (store, _, _) = setup();
        store
            .project_put(&crate::store::ProjectRow {
                id: "p1".into(),
                channel: "personal".into(),
                name: "repo".into(),
                remote_url: Some("github.com/owner/repo".into()),
                created_ms: 1,
            })
            .unwrap();
        let session: crate::store::SessionRow = serde_json::from_value(json!({
            "id": "s1", "node_id": "n", "channel": "personal", "work_item_id": "item-1",
            "repo_path": "", "branch": "feat", "harness_id": "claude", "harness_version": "1",
            "model": "m", "project_id": "p1", "phase": "execute", "budget_tokens": 42,
            "tokens_used": 7, "state": "running", "turn_active": 0, "created_ms": 1,
            "updated_ms": 1, "open_turn_tokens": 0,
        }))
        .unwrap();
        let source = describe_source(&store, &session, TransferIntent::Move, Vec::new()).unwrap();
        assert_eq!(source.budget_tokens, 42);
        assert_eq!(source.work_item_id.as_deref(), Some("item-1"));
        assert_eq!(
            source.project.unwrap().remote_url.as_deref(),
            Some("github.com/owner/repo")
        );
        assert!(source.open_reviews.is_empty());
        assert_eq!(source.last_check, None);
    }

    /// A package of `bytes` of incompressible content, signed by `identity`
    /// and sent to `receiver`, with a receiving store that holds the channel
    /// with the origin in it.
    fn large_package(bytes: usize) -> (SignedTransfer, Store, String) {
        let store = Store::open_in_memory().unwrap();
        let identity = proto::keys::Identity::from_seed(&[5u8; 32]);
        store.channel_put("personal", b"ring", "{}").unwrap();
        let mut content = Vec::with_capacity(bytes);
        let mut state = 0x2545_f491_4f6c_dd1du64;
        while content.len() < bytes {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            content.extend_from_slice(&state.to_le_bytes());
        }
        let files = vec![TransferFile {
            path: "big.bin".into(),
            mode: 0o100644,
            content_b64: base64::engine::general_purpose::STANDARD.encode(&content),
        }];
        let tree = crate::git_tree::tree_sha1(&files);
        let candidate_id = format!("{HEAD_SHA}:personal");
        store
            .insert_candidate(&CandidateRow {
                id: candidate_id.clone(),
                head_sha: HEAD_SHA.into(),
                tree_sha: Some(tree),
                channel: "personal".into(),
                owner_session_id: None,
                source_kind: "git".into(),
                captured_ms: 1,
                capture_json: r#"{"materialized":true}"#.into(),
            })
            .unwrap();
        store
            .insert_candidate_files(
                &candidate_id,
                &[CandidateFile {
                    path: "big.bin".into(),
                    mode: 0o100644,
                    content,
                }],
            )
            .unwrap();
        let transfer = export(
            &store,
            &identity,
            &candidate_id,
            ContextSelection::default(),
            Some("receiver".into()),
        )
        .unwrap();
        let receiver = Store::open_in_memory().unwrap();
        receiver.channel_put("personal", b"ring", "{}").unwrap();
        receiver
            .node_channel_add(&identity.node_id(), "personal")
            .unwrap();
        (transfer, receiver, identity.node_id())
    }

    /// A package larger than one frame arrives in pieces, in any order and
    /// with repeats, and is staged once the last piece is in and the whole
    /// verifies to the signed sha256. Until then, what is missing is listed
    /// for a resend.
    #[test]
    fn a_large_package_arrives_in_pieces_and_resumes() {
        let (transfer, receiver, origin) = large_package(3 * MESH_CHUNK_BYTES);
        assert!(transfer.serialized_len().unwrap() > MAX_MESH_TRANSFER_BYTES);
        let mut chunks = mesh_chunks(&transfer, None).unwrap();
        assert!(chunks.len() >= 4, "{}", chunks.len());
        let count = chunks.len() as u32;
        let last = chunks.pop().unwrap();
        chunks.reverse();
        for chunk in chunks.iter().cloned().chain([chunks[0].clone()]) {
            let outcome = receive_chunk(&receiver, "receiver", &origin, "personal", chunk).unwrap();
            assert!(matches!(outcome, ChunkOutcome::Partial { count: c, .. } if c == count));
        }
        let pending = partial_deliveries()
            .into_iter()
            .find(|partial| partial.transfer_id == transfer.sha256)
            .unwrap();
        assert_eq!(pending.missing, vec![count - 1]);
        // The resend of only what is missing completes it.
        let resent = mesh_chunks(&transfer, Some(&[count - 1])).unwrap();
        assert_eq!(resent, vec![last]);
        let outcome = receive_chunk(
            &receiver,
            "receiver",
            &origin,
            "personal",
            resent[0].clone(),
        )
        .unwrap();
        assert_eq!(outcome, ChunkOutcome::Staged(transfer.sha256.clone()));
        assert!(receiver.transfer(&transfer.sha256).unwrap().is_some());
        assert!(!partial_deliveries()
            .iter()
            .any(|partial| partial.transfer_id == transfer.sha256));
        let again = receive_chunk(
            &receiver,
            "receiver",
            &origin,
            "personal",
            resent[0].clone(),
        )
        .unwrap();
        assert_eq!(again, ChunkOutcome::AlreadyStaged);
    }

    #[test]
    fn a_piece_is_refused_unless_it_matches_its_digest_and_comes_from_a_member() {
        let (transfer, receiver, origin) = large_package(2 * MESH_CHUNK_BYTES);
        let chunk = mesh_chunks(&transfer, Some(&[0])).unwrap().remove(0);
        let mut tampered = chunk.clone();
        tampered.data_b64 = base64::engine::general_purpose::STANDARD.encode(b"other bytes");
        assert!(matches!(
            receive_chunk(&receiver, "receiver", &origin, "personal", tampered),
            Err(TransferError::Invalid(_))
        ));
        let stranger = "ab".repeat(32);
        assert!(matches!(
            receive_chunk(&receiver, "receiver", &stranger, "personal", chunk.clone()),
            Err(TransferError::OriginUnauthorized(_))
        ));
        let mut beyond = chunk;
        beyond.index = beyond.count;
        assert!(matches!(
            receive_chunk(&receiver, "receiver", &origin, "personal", beyond),
            Err(TransferError::Invalid(_))
        ));
    }
}
