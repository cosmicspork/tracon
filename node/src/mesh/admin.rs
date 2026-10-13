//! Administering a node from a peer.
//!
//! A node takes admin commands (reading and editing its `node.toml`, signing
//! its providers in and out) only from the peers its own `[mesh]
//! admin_peers` names. Membership of a channel, or of the mesh, is not
//! enough: it says which node asked, not that this node's operator lets that
//! node run it.
//!
//! Every admin command but a read also carries a
//! [`proto::admin::AdminStatement`]: the sending node's operator confirmed
//! exactly this command, for exactly this node, with the sender's passkey
//! (or at the sender's own machine), and the sender signed that with its node
//! key. This node checks the statement against the command it is about to
//! run and spends it, so it runs once, within two minutes of the
//! confirmation, and never for another operation or another node
//! ([`admit`]).

use proto::admin::{AdminStatement, STATEMENT_TTL_MS};
use proto::frame::{Command, MESH_CHANNEL};
use serde_json::{json, Value};

use super::forward::DEADLINE_SKEW_MS;
use crate::config::Config;
use crate::store::Store;

/// The operation a command is, as a statement binds it: the command as it
/// serializes, with its statement and frame deadline left out.
pub fn op_hash(target: &str, command: &Command) -> String {
    proto::admin::op_hash(
        target,
        &serde_json::to_value(command).unwrap_or(Value::Null),
    )
}

/// The operation a `ConfigPatch` of `ops` against `expected_hash` is, before
/// it has a statement to carry: what the sender's operator confirms.
pub fn config_patch_op_hash(target: &str, expected_hash: &str, ops: &[Value]) -> String {
    proto::admin::op_hash(
        target,
        &json!({ "op": "config_patch", "expected_hash": expected_hash, "ops": ops }),
    )
}

/// Whether only an admin peer may send `command`.
pub fn is_admin_command(command: &Command) -> bool {
    matches!(
        command,
        Command::ProviderConnect { .. }
            | Command::ProviderCode { .. }
            | Command::ProviderDisconnect { .. }
            | Command::ConfigRead
            | Command::ConfigPatch { .. }
    )
}

fn statement_of(command: &Command) -> Option<&AdminStatement> {
    match command {
        Command::ProviderConnect { statement, .. }
        | Command::ProviderCode { statement, .. }
        | Command::ProviderDisconnect { statement, .. } => statement.as_ref(),
        Command::ConfigPatch { statement, .. } => Some(statement),
        _ => None,
    }
}

/// A node id as a message names it.
pub fn short(node_id: &str) -> &str {
    &node_id[..node_id.len().min(16)]
}

/// Who an admitted admin command is from, for the audit log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admitted {
    pub issuer: String,
    /// The issuing node's operator session that confirmed it, or `local`.
    pub session: String,
    /// What that operator was shown.
    pub summary: String,
}

impl Admitted {
    /// The actor an edit made on this statement's strength is audited as.
    pub fn actor(&self) -> String {
        format!("peer {} for {}", short(&self.issuer), self.session)
    }
}

/// Whether `sender` may run the admin command `command` here now. A read
/// needs only that this node's `[mesh] admin_peers` names the sender;
/// everything else also needs the statement it carries to hold for exactly
/// this command on this node, which is then spent. `Ok(None)` for a command
/// that is not an admin command, or a read.
pub fn admit(
    store: &Store,
    cfg: &Config,
    self_id: &str,
    sender: &str,
    command: &Command,
    now: i64,
) -> Result<Option<Admitted>, String> {
    if !is_admin_command(command) {
        return Ok(None);
    }
    let live = cfg.live();
    if !live.mesh.is_admin_peer(sender) {
        return Err(format!(
            "{} does not name {} in its [mesh] admin_peers, so it takes no admin command from \
             that node: add that node's id there, at {} itself or in its Settings with its passkey",
            node_name(cfg, self_id),
            short(sender),
            node_name(cfg, self_id),
        ));
    }
    // By this node's own record of the hub: a member the hub has since
    // removed is no longer one, whatever its node id still says.
    let member = store
        .nodes_in_channel(MESH_CHANNEL)
        .is_ok_and(|members| members.iter().any(|m| m == sender));
    if !member {
        return Err(format!(
            "{} is not a member of the mesh by this node's record",
            short(sender)
        ));
    }
    if matches!(command, Command::ConfigRead) {
        return Ok(None);
    }
    let Some(statement) = statement_of(command) else {
        return Err(
            "an admin command from a peer must carry the statement its operator confirmed with \
             a passkey there; the sender sent none (it may run an older build)"
                .into(),
        );
    };
    check(store, self_id, sender, command, statement, now).map(Some)
}

fn check(
    store: &Store,
    self_id: &str,
    sender: &str,
    command: &Command,
    statement: &AdminStatement,
    now: i64,
) -> Result<Admitted, String> {
    let refused = |why: &str| Err(format!("the admin statement is refused: {why}"));
    if statement.issuer != sender {
        return refused("it was issued by another node than the one that sent it");
    }
    if let Err(error) = statement.verify() {
        return refused(&error.to_string());
    }
    if statement.target != self_id {
        return refused("it is for another node");
    }
    if statement.op_hash != op_hash(self_id, command) {
        return refused("it was confirmed for another operation");
    }
    if statement
        .not_after_ms
        .saturating_sub(statement.asserted_at_ms)
        > STATEMENT_TTL_MS
        || statement.not_after_ms < statement.asserted_at_ms
    {
        return refused("it claims to live longer than a confirmation does");
    }
    if statement.asserted_at_ms > now.saturating_add(DEADLINE_SKEW_MS) {
        return refused("it was confirmed in the future by this node's clock");
    }
    if now > statement.not_after_ms.saturating_add(DEADLINE_SKEW_MS) {
        return refused("it has expired; confirm the action again");
    }
    let fresh = store
        .admin_statement_spend(
            &statement.nonce,
            &statement.issuer,
            &statement.op_hash,
            now,
            statement.not_after_ms.saturating_add(DEADLINE_SKEW_MS),
        )
        .map_err(|e| e.to_string())?;
    if !fresh {
        return refused("it was already used; confirm the action again");
    }
    Ok(Admitted {
        issuer: statement.issuer.clone(),
        session: crate::mesh::recipe::without_hidden(&statement.session)
            .chars()
            .take(16)
            .collect(),
        summary: crate::mesh::recipe::without_hidden(&statement.summary),
    })
}

fn node_name(cfg: &Config, self_id: &str) -> String {
    let name = crate::mesh::recipe::without_hidden(cfg.node_name.trim());
    if name.is_empty() {
        short(self_id).to_string()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::keys::Identity;

    fn ids() -> (Identity, Identity, Identity) {
        (
            Identity::from_seed(&[61u8; 32]),
            Identity::from_seed(&[62u8; 32]),
            Identity::from_seed(&[63u8; 32]),
        )
    }

    fn setup(admins: &[&Identity]) -> (Store, Config) {
        let store = Store::open_in_memory().unwrap();
        let (a, b, c) = ids();
        for id in [&a, &b, &c] {
            store
                .node_channels_set(&id.node_id(), &[MESH_CHANNEL.into()])
                .unwrap();
        }
        let mut cfg = Config {
            node_name: "beta".into(),
            ..Default::default()
        };
        cfg.mesh.admin_peers = admins.iter().map(|id| id.node_id()).collect();
        (store, cfg)
    }

    fn disconnect(statement: Option<AdminStatement>) -> Command {
        Command::ProviderDisconnect {
            name: "anthropic".into(),
            statement,
        }
    }

    fn signed(from: &Identity, target: &str, command: &Command, at: i64) -> AdminStatement {
        AdminStatement::sign(
            from,
            target,
            &op_hash(target, command),
            "On beta: x",
            "local",
            at,
        )
    }

    #[test]
    fn only_an_admin_peer_with_a_statement_for_this_very_command_is_admitted() {
        let (a, b, c) = ids();
        let (store, cfg) = setup(&[&a]);
        let bi = b.node_id();
        let now = 1_000_000;
        let bare = disconnect(None);

        // Not an admin command: nothing to decide here.
        let kill = Command::Kill {
            session_id: "s".into(),
        };
        assert_eq!(admit(&store, &cfg, &bi, &c.node_id(), &kill, now), Ok(None));
        // C is not an admin peer, statement or not.
        let s = signed(&c, &bi, &bare, now);
        let e = admit(&store, &cfg, &bi, &c.node_id(), &disconnect(Some(s)), now).unwrap_err();
        assert!(e.contains("admin_peers"), "{e}");
        let e = admit(&store, &cfg, &bi, &c.node_id(), &Command::ConfigRead, now).unwrap_err();
        assert!(e.contains("admin_peers"), "{e}");
        // A may read with no statement, and must carry one for anything else.
        assert_eq!(
            admit(&store, &cfg, &bi, &a.node_id(), &Command::ConfigRead, now),
            Ok(None)
        );
        let e = admit(&store, &cfg, &bi, &a.node_id(), &bare, now).unwrap_err();
        assert!(e.contains("carry the statement"), "{e}");

        // A statement for exactly this: admitted once, audited as A.
        let s = signed(&a, &bi, &bare, now);
        let admitted = admit(
            &store,
            &cfg,
            &bi,
            &a.node_id(),
            &disconnect(Some(s.clone())),
            now,
        )
        .unwrap()
        .unwrap();
        assert_eq!(admitted.issuer, a.node_id());
        assert!(
            admitted.actor().starts_with("peer "),
            "{}",
            admitted.actor()
        );
        // Replayed: refused.
        let e = admit(&store, &cfg, &bi, &a.node_id(), &disconnect(Some(s)), now).unwrap_err();
        assert!(e.contains("already used"), "{e}");

        // For another operation, another node, from another issuer, stale,
        // from the future, or stretched past its lifetime: refused.
        let other_op = Command::ProviderDisconnect {
            name: "openai".into(),
            statement: None,
        };
        let cases: Vec<(&str, AdminStatement)> = vec![
            ("another operation", signed(&a, &bi, &other_op, now)),
            ("another node", signed(&a, &c.node_id(), &bare, now)),
            ("another node than", signed(&c, &bi, &bare, now)),
            (
                "expired",
                signed(
                    &a,
                    &bi,
                    &bare,
                    now - STATEMENT_TTL_MS - DEADLINE_SKEW_MS - 1,
                ),
            ),
            ("future", signed(&a, &bi, &bare, now + DEADLINE_SKEW_MS + 1)),
        ];
        for (words, s) in cases {
            let e = admit(&store, &cfg, &bi, &a.node_id(), &disconnect(Some(s)), now).unwrap_err();
            assert!(e.contains(words), "{words}: {e}");
        }
        let mut long = signed(&a, &bi, &bare, now);
        long.not_after_ms += 1;
        let e = admit(
            &store,
            &cfg,
            &bi,
            &a.node_id(),
            &disconnect(Some(long)),
            now,
        )
        .unwrap_err();
        assert!(e.contains("signature"), "{e}");

        // A removed member is not admitted, even while still listed.
        store.node_channels_set(&a.node_id(), &[]).unwrap();
        let s = signed(&a, &bi, &bare, now);
        let e = admit(&store, &cfg, &bi, &a.node_id(), &disconnect(Some(s)), now).unwrap_err();
        assert!(e.contains("not a member of the mesh"), "{e}");
    }

    #[test]
    fn a_config_patch_is_confirmed_before_it_carries_its_statement() {
        let (a, b, _) = ids();
        let ops = vec![json!({ "op": "set", "key": "review.max_files", "value": 4 })];
        let before = config_patch_op_hash(&b.node_id(), "h", &ops);
        let statement = AdminStatement::sign(&a, &b.node_id(), &before, "s", "local", 1);
        let command = Command::ConfigPatch {
            expected_hash: "h".into(),
            ops,
            statement,
            not_after_ms: 99,
        };
        assert_eq!(op_hash(&b.node_id(), &command), before);
    }
}
