//! Enrollment: how a second node joins.
//!
//! ```text
//! inviter (enrolled)                        hub                         invitee (fresh)
//! open_invite: PUT /v0/enroll/{code} ─────►  slot
//!   prints URL + QR + own fingerprint
//!                                                        accept: POST /v0/enroll/{code} {keys, name}
//!                                                          prints own fingerprint, waits
//! poll_invite: GET /v0/enroll/{code} ◄────  fetch-and-delete
//!   operator compares fingerprints
//! admit: POST /v0/admit; direct frames on @mesh: key_handoff, policy_bundle
//!                                                        opens the handoff, installs, done
//! ```
//!
//! Nothing in the slot is secret — public keys and a name — so the hub holds
//! it in the clear. What defeats a hub that swaps keys is the fingerprint
//! comparison the operator makes before admitting.

use std::sync::Arc;
use std::time::Duration;

use proto::auth::signed_headers;
use proto::enroll::{fingerprint_hex, invite_url, new_code, EnrollRequest};
use proto::frame::{ChannelHandoff, Envelope, Payload, MESH_CHANNEL};
use proto::keyring::Keyring;
use proto::keys::{key32, Identity};
use serde::Serialize;
use serde_json::{json, Value};

use crate::store::{now_ms, Store};

#[derive(Debug, thiserror::Error)]
pub enum EnrollError {
    #[error("hub unreachable: {0}")]
    Transport(String),
    #[error("hub refused ({status}): {body}")]
    Refused { status: u16, body: String },
    #[error("{0}")]
    Local(String),
}

fn local(e: impl std::fmt::Display) -> EnrollError {
    EnrollError::Local(e.to_string())
}

/// An invitation this node has open.
#[derive(Clone, Debug, Serialize)]
pub struct Invite {
    pub code: String,
    pub url: String,
    pub channels: Vec<String>,
    pub expires_at: u64,
    /// The invitee's request, once it has answered.
    pub received: Option<EnrollRequest>,
    pub admitted: bool,
}

impl Invite {
    /// The code as shown to a human: `7KQ4 M2XA`.
    pub fn display_code(&self) -> String {
        format!("{} {}", &self.code[..4], &self.code[4..])
    }

    /// The fingerprint of the invitee, once received.
    pub fn received_fingerprint(&self) -> Option<String> {
        self.received
            .as_ref()
            .and_then(|r| fingerprint_hex(&r.node_id))
    }
}

/// Revoke a member on the hub: `DELETE /v0/admit/{node_id}`. The hub allows
/// it for the member itself or the node that admitted it.
pub async fn remove_member(
    identity: &Identity,
    hub_url: &str,
    node_id: &str,
) -> Result<(), EnrollError> {
    let path = format!("/v0/admit/{}", node_id.to_ascii_lowercase());
    let (st, text) = send(identity, hub_url, "DELETE", &path, None).await?;
    if st == 204 {
        Ok(())
    } else {
        Err(EnrollError::Refused {
            status: st,
            body: text,
        })
    }
}

// ----------------------------------------------------------------- transport

async fn send(
    identity: &Identity,
    hub_url: &str,
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
) -> Result<(u16, String), EnrollError> {
    let hub_url = hub_url.trim_end_matches('/');
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(local)?;
    let url = format!("{hub_url}{path}");
    let mut req = match method {
        "PUT" => client.put(&url),
        "POST" => client.post(&url),
        "DELETE" => client.delete(&url),
        _ => client.get(&url),
    };
    let body = body.unwrap_or_default();
    let ts = (now_ms() / 1000).max(0) as u64;
    for (k, v) in signed_headers(identity, method, path, &body, ts) {
        req = req.header(k, v);
    }
    let res = req
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| EnrollError::Transport(e.to_string()))?;
    Ok((res.status().as_u16(), res.text().await.unwrap_or_default()))
}

fn ok(status: u16, body: String) -> Result<Value, EnrollError> {
    if (200..300).contains(&status) {
        Ok(serde_json::from_str(&body).unwrap_or(Value::Null))
    } else {
        Err(EnrollError::Refused { status, body })
    }
}

// ------------------------------------------------------------------- inviter

/// Open an invitation on the hub. `channels` are what the invitee will be
/// handed; `@mesh` is always included.
pub async fn open_invite(
    identity: &Identity,
    hub_url: &str,
    channels: &[String],
    ttl_secs: Option<u64>,
) -> Result<Invite, EnrollError> {
    let code = new_code();
    let body = serde_json::to_vec(&json!({ "ttl_secs": ttl_secs })).map_err(local)?;
    let (st, text) = send(
        identity,
        hub_url,
        "PUT",
        &format!("/v0/enroll/{code}"),
        Some(body),
    )
    .await?;
    let v = ok(st, text)?;
    let mut chans: Vec<String> = vec![MESH_CHANNEL.to_string()];
    for c in channels {
        if !chans.contains(c) {
            chans.push(c.clone());
        }
    }
    Ok(Invite {
        url: invite_url(hub_url, &code),
        code,
        channels: chans,
        expires_at: v["expires_at"].as_u64().unwrap_or(0),
        received: None,
        admitted: false,
    })
}

/// Has the invitee answered? `None` until it has; the slot is consumed once
/// it returns `Some`.
pub async fn poll_invite(
    identity: &Identity,
    hub_url: &str,
    code: &str,
) -> Result<Option<EnrollRequest>, EnrollError> {
    let (st, text) = send(
        identity,
        hub_url,
        "GET",
        &format!("/v0/enroll/{code}"),
        None,
    )
    .await?;
    match st {
        204 => Ok(None),
        _ => {
            let v = ok(st, text)?;
            serde_json::from_value(v).map(Some).map_err(local)
        }
    }
}

pub async fn cancel_invite(
    identity: &Identity,
    hub_url: &str,
    code: &str,
) -> Result<(), EnrollError> {
    let (st, text) = send(
        identity,
        hub_url,
        "DELETE",
        &format!("/v0/enroll/{code}"),
        None,
    )
    .await?;
    ok(st, text).map(|_| ())
}

/// The keyrings this node holds for `channels`, re-wrapped to `grantee`.
pub fn handoff_payload(
    store: &Store,
    identity: &Identity,
    grantee_x25519: &x25519_dalek::PublicKey,
    channels: &[String],
) -> Result<Payload, EnrollError> {
    let mut out = Vec::new();
    for name in channels {
        let row = store.channel_get(name).map_err(local)?.ok_or_else(|| {
            EnrollError::Local(format!("this node holds no key for channel {name}"))
        })?;
        let ring = Keyring::from_bytes(&row.keyring).map_err(local)?;
        let wrapped = ring.wrap_for(identity, grantee_x25519).map_err(local)?;
        out.push(ChannelHandoff {
            name: name.clone(),
            keyring: wrapped,
            bindings_json: row.bindings_json,
        });
    }
    Ok(Payload::KeyHandoff { channels: out })
}

/// Admit a node on the hub and hand it the keys for `channels` plus this
/// node's policy bundle, direct-sealed so the hub relays ciphertext. Posted
/// straight to the hub: the operator is waiting, and the frames must land
/// even if no node is running here.
/// Tell the hub which channels this node holds. The hub's record of a node
/// is routing metadata it learns only from admits, so a channel created after
/// `mesh init` is unknown to it until this runs — and the hub refuses to let
/// a node grant a channel it is not recorded in. A member may always extend
/// its own record (the hub is not the authority on keys), so this is safe to
/// call whenever the local list may have moved: before inviting or admitting,
/// and on `channel create`.
pub async fn sync_own_channels(
    store: &Store,
    identity: &Identity,
    hub_url: &str,
    name: &str,
) -> Result<Vec<String>, EnrollError> {
    let mut chans: Vec<String> = vec![MESH_CHANNEL.to_string()];
    for c in store.channel_list().map_err(local)? {
        if !chans.contains(&c.name) {
            chans.push(c.name);
        }
    }
    let body = serde_json::to_vec(&json!({
        "node_id": identity.node_id(),
        "x25519_pub": identity.x25519_hex(),
        "name": name,
        "channels": chans,
    }))
    .map_err(local)?;
    let (st, text) = send(identity, hub_url, "POST", "/v0/admit", Some(body)).await?;
    ok(st, text)?;
    Ok(chans)
}

#[allow(clippy::too_many_arguments)]
pub async fn admit(
    store: &Store,
    identity: &Identity,
    hub_url: &str,
    node_id: &str,
    x25519_pub: &str,
    name: &str,
    channels: &[String],
    credentials: &[(String, crate::broker::Credential)],
) -> Result<(), EnrollError> {
    // The hub must know this node holds what it is about to grant.
    let own_name = crate::config::Config::load().node_name;
    sync_own_channels(store, identity, hub_url, &own_name).await?;
    if node_id.eq_ignore_ascii_case(&identity.node_id()) {
        // Extending our own record is the whole job; there is no one to hand
        // keys to.
        return Ok(());
    }
    let grantee = key32(x25519_pub)
        .map(x25519_dalek::PublicKey::from)
        .ok_or_else(|| EnrollError::Local("the node's sealing key is malformed".into()))?;
    let mut chans: Vec<String> = vec![MESH_CHANNEL.to_string()];
    for c in channels {
        if !chans.contains(c) {
            chans.push(c.clone());
        }
    }
    // Keys are built before the hub is touched: a channel this node cannot
    // hand off should fail here, not after the member record exists.
    let keys = handoff_payload(store, identity, &grantee, &chans)?;

    let body = serde_json::to_vec(&json!({
        "node_id": node_id, "x25519_pub": x25519_pub, "name": name, "channels": chans,
    }))
    .map_err(local)?;
    let (st, text) = send(identity, hub_url, "POST", "/v0/admit", Some(body)).await?;
    ok(st, text)?;

    post_direct(identity, hub_url, node_id, &grantee, &keys).await?;
    if let Some((toml, sig, key)) = crate::policy::bundle::export() {
        let p = Payload::PolicyBundle {
            toml,
            sig_hex: sig,
            pubkey_hex: key,
        };
        post_direct(identity, hub_url, node_id, &grantee, &p).await?;
    }
    // Credentials pinned to the new node travel with its keys, sealed the
    // same way; the hub relays ciphertext it cannot open.
    if !credentials.is_empty() {
        let p = Payload::CredentialHandoff {
            credentials: crate::broker::Broker::handoff_rows(credentials),
        };
        post_direct(identity, hub_url, node_id, &grantee, &p).await?;
    }
    // Record the new member locally too, so the interface lists it before
    // its first hello arrives.
    let _ = store.ensure_peer_node(node_id);
    let _ = store.node_channels_set(node_id, &chans);
    Ok(())
}

pub async fn post_direct(
    identity: &Identity,
    hub_url: &str,
    recipient: &str,
    recipient_key: &x25519_dalek::PublicKey,
    payload: &Payload,
) -> Result<(), EnrollError> {
    let env = Envelope::seal_direct(
        identity,
        MESH_CHANNEL,
        recipient,
        recipient_key,
        payload,
        now_ms(),
    )
    .map_err(local)?;
    let body = serde_json::to_vec(&env).map_err(local)?;
    let (st, text) = send(identity, hub_url, "POST", "/v0/frames", Some(body)).await?;
    ok(st, text).map(|_| ())
}

/// Share channels with the hub's replica: admit the hub's own id as a member
/// of them (role `hub`) and hand it the keyrings with a `processing: "hub"`
/// binding, so the hub — and only the hub — batches them.
pub async fn share_with_hub(
    store: &Store,
    identity: &Identity,
    hub_url: &str,
    channels: &[String],
) -> Result<Vec<String>, EnrollError> {
    let (st, text) = send(identity, hub_url, "GET", "/v0/info", None).await?;
    let info = ok(st, text)?;
    let hub_id = info["hub_node_id"]
        .as_str()
        .ok_or_else(|| EnrollError::Local("this hub runs no replica".into()))?
        .to_string();
    let grantee = info["hub_x25519_pub"]
        .as_str()
        .and_then(key32)
        .map(x25519_dalek::PublicKey::from)
        .ok_or_else(|| EnrollError::Local("the hub's sealing key is malformed".into()))?;
    let own_name = crate::config::Config::load().node_name;
    sync_own_channels(store, identity, hub_url, &own_name).await?;
    let chans: Vec<String> = channels
        .iter()
        .filter(|c| c.as_str() != MESH_CHANNEL)
        .cloned()
        .collect();
    if chans.is_empty() {
        return Err(EnrollError::Local("name at least one channel".into()));
    }
    let Payload::KeyHandoff { mut channels } = handoff_payload(store, identity, &grantee, &chans)?
    else {
        unreachable!("handoff_payload builds a key handoff");
    };
    for h in &mut channels {
        let mut b: Value = serde_json::from_str(&h.bindings_json).unwrap_or(json!({}));
        b["processing"] = json!("hub");
        h.bindings_json = b.to_string();
        // Recorded locally too, so this node knows not to batch it.
        if let Ok(Some(row)) = store.channel_get(&h.name) {
            let _ = store.channel_put(&h.name, &row.keyring, &h.bindings_json);
        }
    }
    let body = serde_json::to_vec(&json!({
        "node_id": hub_id, "x25519_pub": info["hub_x25519_pub"], "name": "hub", "channels": chans, "role": "hub",
    }))
    .map_err(local)?;
    let (st, text) = send(identity, hub_url, "POST", "/v0/admit", Some(body)).await?;
    ok(st, text)?;
    post_direct(
        identity,
        hub_url,
        &hub_id,
        &grantee,
        &Payload::KeyHandoff { channels },
    )
    .await?;
    Ok(chans)
}

/// Hand a channel again to every member the hub records in it (and the hub
/// itself when it holds the channel), so a bindings change lands everywhere.
/// Members merge the keyring by union and take the bindings as sent.
pub async fn rehand_channel(
    store: &Store,
    identity: &Identity,
    hub_url: &str,
    channel: &str,
) -> Result<usize, EnrollError> {
    let (st, text) = send(identity, hub_url, "GET", "/v0/members", None).await?;
    let members: Vec<Value> = serde_json::from_value(ok(st, text)?).map_err(local)?;
    let me = identity.node_id();
    let mut n = 0;
    for m in members {
        let (Some(id), Some(x)) = (m["node_id"].as_str(), m["x25519_pub"].as_str()) else {
            continue;
        };
        if id == me || x.is_empty() {
            continue;
        }
        let in_channel = m["channels"]
            .as_array()
            .is_some_and(|a| a.iter().any(|c| c.as_str() == Some(channel)));
        if !in_channel {
            continue;
        }
        let Some(pk) = key32(x).map(x25519_dalek::PublicKey::from) else {
            continue;
        };
        let payload = handoff_payload(store, identity, &pk, &[channel.to_string()])?;
        post_direct(identity, hub_url, id, &pk, &payload).await?;
        n += 1;
    }
    Ok(n)
}

/// Hand this node's policy bundle to every member of the hub.
pub async fn push_policy(identity: &Identity, hub_url: &str) -> Result<usize, EnrollError> {
    let (toml, sig, key) = crate::policy::bundle::export()
        .ok_or_else(|| EnrollError::Local("this node has no policy bundle to push".into()))?;
    let payload = Payload::PolicyBundle {
        toml,
        sig_hex: sig,
        pubkey_hex: key,
    };
    let (st, text) = send(identity, hub_url, "GET", "/v0/members", None).await?;
    let members: Vec<Value> = serde_json::from_value(ok(st, text)?).map_err(local)?;
    let me = identity.node_id();
    let mut n = 0;
    for m in members {
        let (Some(id), Some(x)) = (m["node_id"].as_str(), m["x25519_pub"].as_str()) else {
            continue;
        };
        if id == me || x.is_empty() {
            continue;
        }
        let Some(pk) = key32(x).map(x25519_dalek::PublicKey::from) else {
            continue;
        };
        post_direct(identity, hub_url, id, &pk, &payload).await?;
        n += 1;
    }
    Ok(n)
}

// ------------------------------------------------------------------- invitee

/// Merge handed-off keyrings into the channel table; returns how many
/// channels this node now holds keys for from this handoff.
pub fn apply_key_handoff(store: &Store, self_id: &str, channels: &[ChannelHandoff]) -> usize {
    let mut n = 0;
    for h in channels {
        let ring = match store.channel_get(&h.name) {
            Ok(Some(existing)) => match Keyring::from_bytes(&existing.keyring) {
                Ok(mine) => Keyring::merge(&mine, &h.keyring),
                Err(_) => h.keyring.clone(),
            },
            _ => h.keyring.clone(),
        };
        if store
            .channel_put(&h.name, &ring.to_bytes(), &h.bindings_json)
            .is_ok()
        {
            let _ = store.node_channel_add(self_id, &h.name);
            n += 1;
        }
    }
    n
}

/// What `tracon enroll` reports as it goes.
pub trait Progress: Sync {
    fn say(&self, line: &str);
}

/// Join a hub: announce this node's keys under the invitation code, then wait
/// for the inviter to admit it and hand off keys. Blocks up to `timeout`.
#[allow(clippy::too_many_arguments)]
pub async fn accept(
    store: Arc<Store>,
    identity: &Identity,
    hub_url: &str,
    code: &str,
    name: &str,
    facts: &str,
    timeout: Duration,
    progress: &dyn Progress,
) -> Result<Vec<String>, EnrollError> {
    let hub_url = hub_url.trim_end_matches('/');
    let req = EnrollRequest {
        node_id: identity.node_id(),
        x25519_pub: identity.x25519_hex(),
        name: name.to_string(),
        contract: proto::CONTRACT_VERSION,
        facts: facts.to_string(),
    };
    let client = reqwest::Client::new();
    let res = client
        .post(format!("{hub_url}/v0/enroll/{code}"))
        .json(&req)
        .send()
        .await
        .map_err(|e| EnrollError::Transport(e.to_string()))?;
    if !res.status().is_success() {
        return Err(EnrollError::Refused {
            status: res.status().as_u16(),
            body: res.text().await.unwrap_or_default(),
        });
    }
    progress.say(&format!(
        "sent; this node's fingerprint is {} — confirm it on the inviting node",
        proto::enroll::fingerprint(&identity.verifying_key().to_bytes())
    ));

    // Wait for admission, then for the direct handoff on @mesh. Nothing here
    // needs a channel key: handoffs are sealed to this identity.
    let deadline = tokio::time::Instant::now() + timeout;
    let mut announced = false;
    let mut cursor: u64 = 0;
    let mut got: Vec<String> = Vec::new();
    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(EnrollError::Local(
                "timed out waiting to be admitted; run tracon enroll again with a fresh invitation"
                    .into(),
            ));
        }
        let path = format!("/v0/frames?channel=@mesh&after={cursor}&limit=200");
        match send(identity, hub_url, "GET", &path, None).await {
            Ok((403, _)) | Ok((401, _)) => {}
            Ok((410, body)) => {
                let oldest = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v["oldest"].as_u64())
                    .unwrap_or(cursor + 1);
                cursor = oldest.saturating_sub(1);
                continue;
            }
            Ok((st, text)) => {
                let page = ok(st, text)?;
                if !announced {
                    progress.say("admitted; waiting for channel keys");
                    announced = true;
                }
                for item in page["frames"].as_array().cloned().unwrap_or_default() {
                    if let Some(seq) = item["seq"].as_u64() {
                        cursor = seq;
                    }
                    let Ok(env) = serde_json::from_value::<Envelope>(item["envelope"].clone())
                    else {
                        continue;
                    };
                    if !env.is_direct() || env.verify().is_err() {
                        continue;
                    }
                    let _ = store.seen_insert(&env.id, now_ms());
                    match env.open_direct(identity) {
                        Ok(Payload::KeyHandoff { channels }) => {
                            apply_key_handoff(&store, &identity.node_id(), &channels);
                            got.extend(channels.iter().map(|c| c.name.clone()));
                            progress.say(&format!(
                                "keys received for {}",
                                channels
                                    .iter()
                                    .map(|c| c.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ));
                        }
                        Ok(Payload::CredentialHandoff { credentials }) => {
                            let key = identity.credential_store_key();
                            match crate::broker::Broker::load(&key) {
                                Ok(mut b) => {
                                    let n = b.apply_handoff(&identity.node_id(), &credentials);
                                    if n > 0 {
                                        if let Err(e) = b.save(&key) {
                                            progress
                                                .say(&format!("credential store not written: {e}"));
                                        } else {
                                            progress.say(&format!(
                                                "{n} credential{} received",
                                                if n == 1 { "" } else { "s" }
                                            ));
                                        }
                                    }
                                }
                                Err(e) => {
                                    progress.say(&format!("credential store not opened: {e}"))
                                }
                            }
                        }
                        Ok(Payload::PolicyBundle {
                            toml,
                            sig_hex,
                            pubkey_hex,
                        }) => {
                            match crate::policy::bundle::install(&toml, &sig_hex, &pubkey_hex, true)
                            {
                                Ok(p) => progress.say(&format!(
                                    "policy bundle installed ({} rules)",
                                    p.rules.len()
                                )),
                                Err(e) => progress.say(&format!("policy bundle refused: {e}")),
                            }
                        }
                        _ => {}
                    }
                }
                let _ = store.cursor_set(MESH_CHANNEL, cursor);
                if got.iter().any(|c| c == MESH_CHANNEL) {
                    return Ok(got);
                }
            }
            Err(EnrollError::Transport(e)) => {
                progress.say(&format!("hub unreachable, retrying: {e}"))
            }
            Err(e) => return Err(e),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// A QR of the invitation URL as terminal text; `None` if it does not fit.
pub fn qr_text(url: &str) -> Option<String> {
    use qrcode::render::unicode;
    let code = qrcode::QrCode::new(url.as_bytes()).ok()?;
    Some(code.render::<unicode::Dense1x2>().quiet_zone(true).build())
}

/// A QR of the invitation URL as inline SVG, for the interface.
pub fn qr_svg(url: &str) -> Option<String> {
    use qrcode::render::svg;
    let code = qrcode::QrCode::new(url.as_bytes()).ok()?;
    Some(
        code.render::<svg::Color>()
            .min_dimensions(132, 132)
            .dark_color(svg::Color("#000000"))
            .light_color(svg::Color("#ffffff"))
            .build(),
    )
}
