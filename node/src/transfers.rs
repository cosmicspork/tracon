//! Explicit continuity transfer. A package is an immutable candidate capture,
//! selected context, and a source-free file set. It is signed by its origin;
//! receiving it only stages bytes. A separate, operator-confirmed import owns
//! workspace creation and starting a new session.

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
/// A portable package is intentionally much smaller than a runtime workspace
/// volume. This bounds both browser upload memory and SQLite package storage.
pub const MAX_TRANSFER_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_TRANSFER_FILES: usize = 20_000;
pub const MAX_CONTEXT_DOCUMENTS: usize = 128;
pub const MAX_CONTEXT_MEMORIES: usize = 512;
/// Direct mesh envelopes add authenticated encryption and base64 overhead.
pub const MAX_MESH_TRANSFER_BYTES: usize = 2 * 1024 * 1024;
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
    #[error("online delivery has a smaller bounded limit; download the portable package instead")]
    MeshTooLarge,
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
        if self.payload.version != TRANSFER_VERSION {
            return Err(TransferError::Invalid("unsupported transfer version".into()));
        }
        let bytes = self.payload_bytes()?;
        validate_files(&self.payload.files)?;
        if self.payload.context.documents.len() > MAX_CONTEXT_DOCUMENTS
            || self.payload.context.memories.len() > MAX_CONTEXT_MEMORIES
        {
            return Err(TransferError::Invalid("selected context is too large".into()));
        }
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        if hex::encode(digest) != self.sha256.to_ascii_lowercase() {
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

/// Make a portable package from immutable candidate rows. No session path,
/// worktree metadata, credentials, or live harness state is read or carried.
pub fn export(
    store: &Store,
    identity: &proto::keys::Identity,
    candidate_id: &str,
    selection: ContextSelection,
    target_node: Option<String>,
) -> Result<SignedTransfer, TransferError> {
    if selection.documents.len() > MAX_CONTEXT_DOCUMENTS || selection.memories.len() > MAX_CONTEXT_MEMORIES {
        return Err(TransferError::Invalid("too many selected context records".into()));
    }
    let candidate = store.candidate(candidate_id)?.ok_or(TransferError::CandidateNotFound)?;
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
    let payload = TransferPayload {
        version: TRANSFER_VERSION,
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
        return Err(TransferError::ReceiverUnauthorized(transfer.payload.channel.clone()));
    }
    let members = store.nodes_in_channel(&transfer.payload.channel)?;
    if !members.iter().any(|node| node == &transfer.payload.origin_node) {
        return Err(TransferError::OriginUnauthorized(transfer.payload.channel.clone()));
    }
    Ok(())
}

pub fn record_portable_receipt(store: &Store, transfer: &SignedTransfer) -> Result<(), TransferError> {
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
    let selected = crate::workspace::staging_path(&id);
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
            fs::create_dir_all(parent).map_err(|error| TransferError::Workspace(error.to_string()))?;
            fs::write(&destination, bytes).map_err(|error| TransferError::Workspace(error.to_string()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = if file.mode & 0o111 == 0 { 0o644 } else { 0o755 };
                fs::set_permissions(&destination, fs::Permissions::from_mode(mode))
                    .map_err(|error| TransferError::Workspace(error.to_string()))?;
            }
        }
        crate::workspace::write_context(
            &selected,
            &serde_json::json!({
                "transfer_id": transfer.id(),
                "candidate_id": transfer.payload.candidate_id,
                "origin_node": transfer.payload.origin_node,
                "channel": transfer.payload.channel,
                "created_ms": transfer.payload.created_ms,
                "candidate": transfer.payload.candidate,
                "evidence": transfer.payload.evidence,
                "context": transfer.payload.context,
            }),
        )
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

pub fn validate_files(files: &[TransferFile]) -> Result<(), TransferError> {
    if files.len() > MAX_TRANSFER_FILES {
        return Err(TransferError::TooManyFiles);
    }
    let mut total = 0usize;
    let mut paths = std::collections::HashSet::with_capacity(files.len());
    for file in files {
        safe_path(&file.path)?;
        // Git captures regular modes as 100644/100755. Symlinks and special
        // files are rejected rather than being reconstructed ambiguously.
        if file.mode & 0o170000 != 0o100000 {
            return Err(TransferError::UnsafeFileMode(file.path.clone()));
        }
        if !paths.insert(file.path.as_str()) {
            return Err(TransferError::Invalid(format!("duplicate file {:?}", file.path)));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(file.content_b64.as_bytes())
            .map_err(|_| TransferError::Invalid(format!("bad base64 for {:?}", file.path)))?;
        total = total.checked_add(bytes.len()).ok_or(TransferError::TooLarge)?;
        if total > MAX_TRANSFER_BYTES {
            return Err(TransferError::TooLarge);
        }
    }
    Ok(())
}

fn safe_path(path: &str) -> Result<(), TransferError> {
    if path.is_empty() || path.len() > 1024 || path.contains('\0') {
        return Err(TransferError::UnsafePath(path.into()));
    }
    let parsed = Path::new(path);
    if parsed.components().any(|component| !matches!(component, Component::Normal(_)))
        || parsed.components().next().is_none()
        || parsed.components().any(|component| component.as_os_str() == ".git")
        || parsed.components().any(|component| component.as_os_str() == ".tracon")
        || parsed.components().any(|component| component.as_os_str() == ".tracon-transfer")
    {
        return Err(TransferError::UnsafePath(path.into()));
    }
    Ok(())
}

fn unique(values: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::with_capacity(values.len());
    values.into_iter().filter(|value| seen.insert(value.clone())).collect()
}
