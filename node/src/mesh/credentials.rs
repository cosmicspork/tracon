//! The mesh as provider credentials use it: claims on the hub, channel
//! membership from the member list, and direct-sealed handoffs.

use serde_json::json;

use super::client::{HubError, MeshClient};
use crate::broker::{Broker, Credential};
use crate::providers::mesh::{ClaimResult, CredentialMesh};

/// How long a claimed refresh may take before another holder may try. A
/// refresh is one HTTPS round trip; this is generous for it.
const CLAIM_TTL_SECS: u64 = 120;

#[async_trait::async_trait]
impl CredentialMesh for MeshClient {
    async fn claim(&self, key: &str, version: u64) -> ClaimResult {
        let body = json!({ "key": key, "version": version, "ttl_secs": CLAIM_TTL_SECS });
        match self.post("/v0/claims", body.to_string().into_bytes()).await {
            Ok(_) => ClaimResult::Won,
            Err(HubError::Refused { status: 409, .. }) => ClaimResult::Lost,
            Err(HubError::Refused {
                status: 404 | 405, ..
            }) => ClaimResult::Unsupported,
            Err(error) => ClaimResult::Unreachable(error.to_string()),
        }
    }

    fn members_in(&self, channels: &[String]) -> Vec<String> {
        let self_id = self.node_id();
        let mut members = Vec::new();
        for channel in channels {
            for node in self.store.nodes_in_channel(channel).unwrap_or_default() {
                let sealable = self
                    .store
                    .get_node(&node)
                    .ok()
                    .flatten()
                    .is_some_and(|row| row.x25519_pub.is_some());
                if node != self_id && sealable && !members.contains(&node) {
                    members.push(node);
                }
            }
        }
        members
    }

    fn hand_off(&self, name: &str, credential: &Credential, to: &[String]) {
        let rows = Broker::handoff_rows(&[(name.to_string(), credential.clone())]);
        for node in to {
            if let Err(error) = self.send_credential_handoff(node, rows.clone()) {
                tracing::warn!(credential = %name, to = %node, %error, "could not hand off a credential");
            }
        }
    }
}
