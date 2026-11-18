//! The mesh as provider credentials use it: claims on the hub, channel
//! membership from the member list, and direct-sealed handoffs.

use proto::frame::{Payload, MESH_CHANNEL};
use serde_json::{json, Value};

use super::client::{HubError, MeshClient};
use crate::broker::{Broker, Credential};
use crate::providers::mesh::{ClaimResult, CredentialMesh, RenewRole};

/// How long a claimed refresh may take before another holder may try. A
/// refresh is one HTTPS round trip; this is generous for it.
const CLAIM_TTL_SECS: u64 = 120;

/// A `409` from `POST /v0/claims`: held by another member, or stale. A hub
/// from before `reason` says stale by naming no holder.
fn refused_claim(body: &str) -> ClaimResult {
    let body: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let node = |field: &str| body[field].as_str().map(str::to_string);
    if body["reason"] == "stale" || (body["reason"].is_null() && body["holder"].is_null()) {
        ClaimResult::Stale(node("renewed_by"))
    } else {
        ClaimResult::Held(node("holder"))
    }
}

#[async_trait::async_trait]
impl CredentialMesh for MeshClient {
    async fn claim(&self, key: &str, version: u64) -> ClaimResult {
        let body = json!({ "key": key, "version": version, "ttl_secs": CLAIM_TTL_SECS });
        match self.post("/v0/claims", body.to_string().into_bytes()).await {
            Ok(_) => ClaimResult::Won,
            Err(HubError::Refused { status: 409, body }) => refused_claim(&body),
            Err(HubError::Refused {
                status: 404 | 405, ..
            }) => ClaimResult::Unsupported,
            Err(error) => ClaimResult::Unreachable(error.to_string()),
        }
    }

    async fn renewed(&self, key: &str, version: u64) -> Result<(), String> {
        let body = json!({ "version": version });
        self.post(
            &format!("/v0/claims/{key}/renewed"),
            body.to_string().into_bytes(),
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
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

    fn request_copy(&self, name: &str, credential: &Credential, from: &[String]) {
        let Some(grant_id) = credential.grant_id.clone() else {
            return;
        };
        let payload = Payload::CredentialRequest {
            name: name.to_string(),
            grant_id,
            grant_version: credential.grant_version,
        };
        for node in from {
            if let Err(error) = self.enqueue_direct(MESH_CHANNEL, node, &payload) {
                tracing::warn!(credential = %name, to = %node, %error, "could not ask for a credential's current copy");
            }
        }
    }

    fn renew_role(&self, node: &str) -> Option<RenewRole> {
        let row = self.store.get_node(node).ok().flatten()?;
        let providers: Vec<Value> = serde_json::from_str(row.providers_json.as_deref()?).ok()?;
        providers
            .iter()
            .flat_map(|provider| provider["sign_ins"].as_array().into_iter().flatten())
            .find_map(|sign_in| sign_in["renews"].as_str().and_then(RenewRole::parse))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_claim_is_held_or_stale() {
        assert_eq!(
            refused_claim(r#"{"reason":"held","holder":"aa","version":3}"#),
            ClaimResult::Held(Some("aa".into()))
        );
        assert_eq!(
            refused_claim(r#"{"reason":"stale","version":4,"renewed_by":"bb"}"#),
            ClaimResult::Stale(Some("bb".into()))
        );
        // A hub from before `reason`.
        assert_eq!(
            refused_claim(
                r#"{"error":"another member holds this claim","holder":"aa","version":3}"#
            ),
            ClaimResult::Held(Some("aa".into()))
        );
        assert_eq!(
            refused_claim(r#"{"error":"a newer version has been claimed","version":4}"#),
            ClaimResult::Stale(None)
        );
    }
}
