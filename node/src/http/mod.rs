pub mod admin;
pub mod api;
pub mod auth;
pub mod config_api;
mod config_prompt;
pub mod device;
pub mod environments;
pub mod evidence;
pub mod fork;
mod mcp;
pub mod metrics;
pub mod names;
pub mod passkey;
mod peer_admin;
mod peer_routes;
pub mod plugin_ui;
mod plugins;
mod policy_admin;
pub mod preview;
pub mod push;
pub mod recipes;
pub mod settings;
mod spa;
mod stream;
pub mod strict;
pub mod threads;

use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderName, HeaderValue},
    middleware::{self, Next},
    response::Response,
    routing::{delete, get, post, put},
    Router,
};
use tower_http::trace::TraceLayer;

use crate::{
    adapter::HarnessAdapter,
    boundary,
    config::Config,
    session::Manager,
    store::{now_ms, NodeRow, Store},
    stream::Bus,
};

use api::AppState;
#[derive(Clone)]
struct OperatorSecurityHeaders {
    csp: HeaderValue,
}

pub fn router(state: AppState) -> Router {
    // The adapter this state carries is the node's default harness, probed at
    // startup; sessions that name no harness run on it.
    state.manager.set_default_adapter(state.adapter.clone());
    state.manager.set_adapter(state.adapter.clone());
    let preview_origin = state
        .cfg
        .docs
        .preview_origin()
        .expect("validated preview origin");
    // The origins this interface is allowed to frame: the node's own
    // document preview listener, which is sandboxed untrusted HTML, and the
    // plugin UI listeners (`plugin_ui`), each a sandboxed origin of its own.
    let plugin_ui_origins = state
        .cfg
        .plugins
        .ui_ports()
        .map(|port| format!("http://127.0.0.1:{port}"))
        .collect::<Vec<_>>()
        .join(" ");
    let security_headers = OperatorSecurityHeaders {
        csp: format!(
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data: https:; connect-src 'self'; \
             frame-src {preview_origin} {plugin_ui_origins}; \
             object-src 'none'; base-uri 'none'; frame-ancestors 'none'"
        )
        .parse()
        .expect("validated origins are valid header values"),
    };
    Router::new()
        // The tools, for a harness the operator runs outside the boundary.
        // Guarded like the rest of this router; see `mcp::handle_external`.
        .route(
            "/mcp/external/{channel}",
            post(mcp::handle_external)
                .get(mcp::external_get)
                .delete(mcp::external_delete),
        )
        .route("/api/external", get(api::external))
        .route(
            "/api/external/{channel}/stop",
            post(api::external_stop).delete(api::external_start),
        )
        .route("/api/external/{channel}/start", post(api::external_start))
        .route("/api/external/{channel}/events", get(api::external_events))
        .route("/api/health", get(api::health))
        .route("/api/node", get(api::get_node))
        .route("/api/node/refresh-models", post(api::refresh_models))
        .route("/api/boundary/check", post(api::recheck_boundary))
        .route("/api/boundary/setup", post(api::run_setup))
        // All of node.toml: read with secrets redacted, edited as typed
        // operations against the hash read. Every write is an admin action
        // (`passkey::AdminRequest`): a passkey off this machine.
        .route(
            "/api/config",
            get(config_api::get_config)
                .put(api::put_config)
                .patch(config_api::patch_config),
        )
        // The launch manifest: what an operator customizes a channel's
        // sessions with. Reading it is an ordinary operator read; every write
        // puts code where a harness will run it, so each is an admin action,
        // and importing a skill package, which reads a directory on the
        // node's own machine, is done at the node.
        .route("/api/manifest", get(api::get_manifest))
        .route("/api/manifest/skills", post(api::post_skill))
        .route("/api/manifest/text", put(api::put_manifest_text))
        .route(
            "/api/manifest/{kind}/{name}",
            delete(api::delete_manifest_item),
        )
        .route("/api/admin/access", get(admin::access))
        .route("/api/admin/mesh", get(admin::mesh))
        .route(
            "/api/admin/mesh/invitations",
            post(admin::create_invitation),
        )
        .route(
            "/api/admin/mesh/invitations/{code}",
            get(admin::poll_invitation).delete(admin::cancel_invitation),
        )
        .route(
            "/api/admin/mesh/invitations/{code}/admit",
            post(admin::admit_invitation),
        )
        .route(
            "/api/admin/mesh/members/{id}",
            axum::routing::delete(admin::remove_member),
        )
        .route("/api/admin/mesh/hub-share", post(admin::share_with_hub))
        .route("/api/admin/maintenance", get(admin::maintenance))
        .route(
            "/api/admin/maintenance/boundary-check",
            post(admin::boundary_check),
        )
        .route("/api/restart", get(admin::restart_status))
        .route(
            "/api/admin/restart",
            post(admin::restart_node).delete(admin::cancel_restart),
        )
        .route(
            "/api/maintenance/storage",
            get(admin::storage_schedule).post(admin::storage),
        )
        .route("/api/maintenance/data", get(admin::data))
        .route(
            "/api/maintenance/service-volumes",
            get(admin::service_volumes),
        )
        .route(
            "/api/maintenance/service-volumes/reset",
            post(admin::reset_service_volume),
        )
        .route("/api/maintenance/export", get(admin::export))
        .route("/api/admin/maintenance/install", post(admin::install))
        .route("/api/admin/maintenance/uninstall", post(admin::uninstall))
        .route("/api/admin/policy", get(policy_admin::status))
        .route(
            "/api/admin/policy/initialize",
            post(policy_admin::initialize),
        )
        .route("/api/admin/policy/preview", post(policy_admin::preview))
        .route("/api/admin/policy/apply", post(policy_admin::apply))
        .route(
            "/api/admin/policy/rollouts/{id}/retry",
            post(policy_admin::retry),
        )
        .route(
            "/api/authority/grants",
            get(api::list_authority_grants).post(api::create_authority_grant),
        )
        .route(
            "/api/authority/grants/{id}",
            axum::routing::delete(api::revoke_authority_grant),
        )
        .route("/api/auth/qr", post(api::qr))
        .route("/api/mesh/init", post(api::mesh_init))
        .route("/api/mesh/unpair", post(api::mesh_unpair))
        .route(
            "/api/mesh/enroll",
            get(api::enroll_status).post(api::start_enroll),
        )
        .route(
            "/api/credentials/import",
            post(api::import_credentials).delete(api::delete_credential_named_import),
        )
        .route("/api/usage", get(api::usage))
        .route("/api/usage.csv", get(api::usage_csv))
        .route("/api/usage/infra", get(api::usage_infra))
        .route("/api/usage/infra.csv", get(api::usage_infra_csv))
        .route("/api/metrics", get(api::metrics))
        .route("/api/provenance/{sha}", get(api::provenance))
        .route(
            "/api/channels/{name}/bindings",
            put(api::put_channel_bindings),
        )
        .route(
            "/api/channels/{name}",
            axum::routing::delete(api::delete_channel),
        )
        .route("/api/promotions/batch", post(api::batch_promotions))
        .route("/api/promotions/{id}", get(api::get_promotion))
        .route("/api/promotions/{id}/verdict", post(api::decide_promotion))
        .route("/api/work", get(api::list_work).post(api::add_work))
        .route("/api/work/ready", get(api::ready_work))
        // Operator names for records, replicated; and what a name resolves to.
        .route("/api/names", get(names::list))
        .route(
            "/api/names/{kind}/{id}",
            put(names::put).delete(names::delete),
        )
        .route("/api/resolve", get(names::resolve))
        .route("/api/projects", get(api::list_projects))
        .route(
            "/api/work/{id}",
            get(api::get_work)
                .put(api::put_work)
                .delete(api::delete_work),
        )
        .route("/api/docs", get(api::list_docs))
        .route("/api/docs/reindex", post(api::reindex_docs))
        .route("/api/docs/kinds", get(api::doc_kinds))
        .route(
            "/api/docs/{channel}/{slug}",
            get(api::get_doc).put(api::put_doc).delete(api::delete_doc),
        )
        .route(
            "/api/docs/{channel}/{slug}/html",
            post(api::import_html).layer(DefaultBodyLimit::max(
                crate::corpus::html::MAX_BUNDLE_BYTES + 1024 * 1024,
            )),
        )
        .route(
            "/api/docs/{channel}/{slug}/download",
            get(api::download_doc),
        )
        .route("/api/docs/{channel}/{slug}/preview", post(api::preview_doc))
        .route(
            "/api/evidence/candidates/{candidate_id}/summary",
            get(evidence::candidate_summary),
        )
        .route(
            "/api/memories",
            get(api::list_memories).post(api::add_memory),
        )
        .route(
            "/api/memories/{id}",
            axum::routing::delete(api::delete_memory).patch(api::edit_memory),
        )
        .route("/api/credentials", get(api::list_credentials))
        .route(
            "/api/credentials/revokes",
            get(api::list_credential_revokes).delete(api::delete_credential_named_revokes),
        )
        .route(
            "/api/credentials/{name}",
            axum::routing::delete(api::delete_credential),
        )
        .route("/api/credentials/{name}/share", post(api::share_credential))
        .route(
            "/api/credentials/forge/{forge}",
            put(api::put_forge_credential).delete(api::delete_forge_credential),
        )
        .route(
            "/api/providers",
            get(api::list_providers).post(api::create_provider),
        )
        .route("/api/providers/{name}/connect", post(api::connect_provider))
        .route("/api/providers/{name}/code", post(api::provider_code))
        .route(
            "/api/providers/{name}/channels",
            put(api::set_provider_channels),
        )
        .route("/api/providers/{name}/key", put(api::put_provider_key))
        .route(
            "/api/providers/{name}/disconnect",
            post(api::disconnect_provider),
        )
        .route("/api/nodes", get(api::list_nodes))
        // A node's node.toml, this one's or an admin peer's over the mesh: a
        // peer answers only if its `[mesh] admin_peers` names this node, and
        // an edit is confirmed here and signed for it (`peer_admin`).
        .route(
            "/api/nodes/{id}/config",
            get(config_api::get_node_config).patch(config_api::patch_node_config),
        )
        // One of an admin peer's own admin routes, run there for an operator
        // administering it from here (`peer_admin::relay`, `peer_routes`).
        .route(
            "/api/nodes/{id}/admin/{*rest}",
            axum::routing::any(peer_admin::relay),
        )
        .route("/api/awake", get(api::awake))
        .route(
            "/api/nodes/{id}/providers/{name}/connect",
            post(api::node_connect_provider),
        )
        .route(
            "/api/nodes/{id}/providers/{name}/code",
            post(api::node_provider_code),
        )
        .route(
            "/api/nodes/{id}/providers/{name}/disconnect",
            post(api::node_disconnect_provider),
        )
        .route("/api/mesh", get(api::get_mesh))
        .route("/api/mesh/rollups", get(api::get_mesh_rollups))
        .route(
            "/api/transfers",
            get(api::list_transfers).post(api::export_transfer),
        )
        .route(
            "/api/transfers/stage",
            post(api::stage_transfer).layer(DefaultBodyLimit::max(
                crate::transfers::MAX_TRANSFER_BYTES + 1024 * 1024,
            )),
        )
        .route("/api/transfers/{id}/import", post(api::import_transfer))
        .route("/api/transfers/{id}/send", post(api::send_transfer))
        .route("/api/transfers/{id}", get(api::get_transfer))
        .route(
            "/api/channels",
            get(api::list_channels).post(api::create_channel),
        )
        // Plugins: adding or updating one raises its install card; rolling
        // back, removing and restarting an instance are admin actions
        // (`passkey::AdminAction`).
        .route("/api/plugins", get(plugins::list).post(plugins::add))
        .route(
            "/api/plugins/archive",
            post(plugins::add_archive).layer(DefaultBodyLimit::max(
                crate::plugin::install::MAX_ARCHIVE_BYTES,
            )),
        )
        .route("/api/plugins/{name}/update", post(plugins::update))
        .route("/api/plugins/{name}/rollback", post(plugins::rollback))
        .route("/api/plugins/{name}", delete(plugins::remove))
        .route("/api/plugins/{name}/logs", get(plugins::logs))
        .route("/api/plugins/{name}/restart", post(plugins::restart))
        .route("/api/plugins/{name}/ui", post(plugins::ui))
        .route("/api/repos/recent", get(api::recent_repos))
        .route("/api/repos/branches", get(api::repo_branches))
        .route("/api/repos/clone", post(api::clone_repo))
        .route(
            "/api/repos/environments",
            get(environments::list).put(environments::save),
        )
        .route("/api/repos/environments/build", post(environments::build))
        .route("/api/repos/offer", post(recipes::offer))
        .route("/api/repos/holds/start", post(recipes::start_held))
        .route("/api/repos/offers", get(recipes::list))
        .route("/api/repos/offers/{id}", get(recipes::view))
        .route("/api/repos/offers/{id}/accept", post(recipes::accept))
        .route("/api/repos/offers/{id}/decline", post(recipes::decline))
        .route(
            "/api/workspaces/import",
            post(api::import_workspace).layer(DefaultBodyLimit::max(
                crate::workspace::MAX_IMPORT_BYTES as usize + 1024 * 1024,
            )),
        )
        .route("/api/workspaces/{id}/export", post(api::export_workspace))
        .route("/api/workspaces/{id}/prepare", post(api::prepare_workspace))
        .route("/api/preparation", get(api::preparation_preview))
        .route(
            "/api/workspaces/{id}/download",
            get(api::download_workspace),
        )
        .route("/api/forge/repos", get(api::forge_repos))
        .route(
            "/api/sessions",
            get(api::list_sessions).post(api::create_session),
        )
        // One prompt: the item and the session that starts on it.
        .route("/api/compose", post(api::compose))
        .route("/api/readiness", get(api::readiness))
        .route("/api/mcp-servers", get(api::mcp_servers))
        .route("/api/sessions/archive-ended", post(api::archive_ended))
        .route("/api/sessions/archive-legacy", post(api::archive_legacy))
        .route("/api/sessions/{id}/reopen", post(api::reopen_session))
        .route("/api/sessions/{id}/continue", post(api::continue_session))
        .route("/api/sessions/{id}/fork", post(fork::fork_session))
        .route(
            "/api/sessions/{id}/continuation",
            get(api::session_continuation),
        )
        .route("/api/work/{id}/continuation", get(api::work_continuation))
        .route("/api/work/{id}/close", post(api::close_task))
        .route("/api/work/{id}/accept", post(api::accept_task))
        .route("/api/continuation/continue", post(api::carry_on))
        .route("/api/continuation/abandon", post(api::abandon_work))
        .route("/api/sessions/{id}/archive", post(api::archive_session))
        .route("/api/sessions/{id}/unarchive", post(api::unarchive_session))
        .route("/api/sessions/{id}", get(api::get_session))
        .route("/api/sessions/{id}/events", get(api::session_events))
        .route("/api/sessions/{id}/authority", get(api::session_authority))
        .route("/api/sessions/{id}/audit", get(api::session_audit))
        .route("/api/sessions/{id}/outcome", get(api::session_outcome))
        .route(
            "/api/sessions/{id}/prompt",
            post(api::prompt).layer(DefaultBodyLimit::max(
                crate::session::attachments::MAX_BODY_BYTES,
            )),
        )
        .route(
            "/api/sessions/{id}/ask",
            get(api::askable)
                .post(api::ask_session)
                .layer(DefaultBodyLimit::max(
                    crate::session::attachments::MAX_BODY_BYTES,
                )),
        )
        .route("/api/sessions/{id}/compact", post(api::compact))
        .route("/api/sessions/{id}/saved", get(api::saved_contexts))
        .route("/api/sessions/{id}/saved/{index}", get(api::saved_context))
        .route(
            "/api/sessions/{id}/saved/{index}/export",
            get(api::export_saved_context),
        )
        .route("/api/sessions/{id}/kill", post(api::kill))
        .route("/api/sessions/{id}/pause", post(api::pause))
        .route("/api/sessions/{id}/resume", post(api::resume))
        .route("/api/sessions/{id}/budget", post(api::set_budget))
        .route("/api/sessions/{id}/stop", post(api::stop))
        .route(
            "/api/sessions/{id}/draft",
            get(api::get_draft).put(api::put_draft),
        )
        .route("/api/permissions/{id}/answer", post(api::answer_permission))
        .route("/api/approvals/{id}", get(api::get_approval))
        .route("/api/approvals/{id}/release", post(api::release_approval))
        .route("/api/approvals/{id}/render", post(api::render_approval))
        .route("/api/approvals/{id}/preview", post(api::preview_approval))
        .route("/api/operator/questions", get(api::operator_questions))
        .route(
            "/api/operator/questions/{id}/answer",
            post(api::answer_operator_question),
        )
        .route(
            "/api/operator/questions/{id}/cancel",
            post(api::cancel_operator_question),
        )
        .route(
            "/api/operator/notifications",
            get(api::operator_notifications),
        )
        .route(
            "/api/operator/notifications/{id}",
            get(api::operator_notification),
        )
        .route("/api/operator/issues", get(api::operator_issues))
        .route("/api/operator/issues/{id}", get(api::operator_issue))
        .route(
            "/api/operator/issues/{id}/reconcile",
            post(api::reconcile_operator_issue),
        )
        .route(
            "/api/operator/issues/{id}/publish",
            post(api::publish_operator_issue),
        )
        .route(
            "/api/operator/issues/{id}/discard",
            post(api::discard_operator_issue),
        )
        .route("/api/reviews/{id}", get(api::get_review))
        .route("/api/reviews/{id}/file", get(api::review_file))
        .route("/api/reviews/{id}/context", get(api::review_context))
        .route("/api/reviews/{id}/verdict", post(api::decide_review))
        .route("/api/reviews/{id}/outgoing", post(api::review_outgoing))
        .route(
            "/api/reviews/{id}/draft",
            get(api::get_review_draft).put(api::put_review_draft),
        )
        .route(
            "/api/reviews/{id}/publication/recover",
            post(api::recover_publication),
        )
        // Settles a review on the operator's word about the forge, checked
        // against it: an admin action (`passkey::AdminAction`).
        .route(
            "/api/reviews/{id}/publication/reconcile",
            post(api::reconcile_publication),
        )
        .route("/api/reviews/{id}/threads", post(threads::create))
        .route(
            "/api/reviews/{id}/threads/{thread}/replies",
            post(threads::reply),
        )
        .route(
            "/api/reviews/{id}/threads/{thread}/resolve",
            post(threads::resolve),
        )
        .route(
            "/api/reviews/{id}/threads/{thread}/reopen",
            post(threads::reopen),
        )
        .route("/api/reviews/{id}/release", post(api::release_review))
        .route("/api/reviews/{id}/ready", post(api::mark_review_ready))
        .route(
            "/api/reviews/{id}/review-session",
            get(api::get_review_session).post(api::start_review_session),
        )
        .route("/api/evidence/candidates", get(evidence::list_candidates))
        .route(
            "/api/evidence/candidates/by-commit/{head_sha}",
            get(api::candidate_by_commit),
        )
        .route(
            "/api/evidence/candidates/{id}",
            get(api::candidate_evidence),
        )
        .route(
            "/api/evidence/candidates/{id}/demonstrations",
            post(api::attach_demonstration),
        )
        .route("/api/queue", get(api::queue))
        .route("/api/login", post(auth::login))
        .route("/api/logout", post(auth::logout))
        .route(
            "/api/auth/token",
            post(auth::put_token).delete(auth::delete_token),
        )
        .route("/api/auth/sessions", get(auth::list_sessions))
        // Passkeys: enrolled at pairing, then what confirms an admin action
        // from off this machine (`passkey::AdminAction`).
        .route("/api/passkeys", get(passkey::list))
        .route("/api/passkeys/{id}", delete(passkey::delete))
        .route(
            "/api/passkeys/register/options",
            post(passkey::register_options),
        )
        .route(
            "/api/passkeys/register/finish",
            post(passkey::register_finish),
        )
        .route(
            "/api/passkeys/assert/options",
            post(passkey::assert_options),
        )
        .route("/api/passkeys/assert/finish", post(passkey::assert_finish))
        // Device approval: a window without passkeys (the desktop app against
        // a remote node) signs in, or confirms an admin action, by having a
        // signed-in browser approve it with one.
        .route("/api/auth/device", post(device::start))
        .route("/api/auth/device/token", post(device::token))
        .route("/api/auth/device/stepup", post(device::step_up))
        .route("/api/auth/device/approvals/{id}", get(device::view))
        .route(
            "/api/auth/device/approvals/{id}/approve",
            post(device::approve),
        )
        .route("/api/auth/device/approvals/{id}/deny", post(device::deny))
        .route("/api/push/key", get(push::key))
        .route(
            "/api/push/subscriptions",
            get(push::list)
                .post(push::subscribe)
                .delete(push::forget_endpoint),
        )
        .route(
            "/api/push/subscriptions/{id}",
            axum::routing::delete(push::forget),
        )
        .route("/api/push/test", post(push::test))
        .route("/api/stream", get(stream::stream))
        // An unmatched `/api` or `/mcp` path answers 404 JSON; everything
        // else is the interface's (`spa::fallback`). The guard is layered
        // outside this router, so it answers first either way.
        .fallback(spa::fallback)
        .layer(middleware::from_fn_with_state(
            security_headers,
            security_headers_middleware,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn security_headers_middleware(
    State(security): State<OperatorSecurityHeaders>,
    req: Request,
    next: Next,
) -> Response {
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        security.csp,
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    response
}

/// The hostname portion of a `Host`/`Origin` value, minus any scheme, port, or
/// IPv6 brackets. `http://[::1]:7420` → `::1`, `127.0.0.1:7420` → `127.0.0.1`.
pub(crate) fn hostname(value: &str) -> &str {
    let v = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
        .unwrap_or(value);
    if let Some(rest) = v.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    v.rsplit_once(':').map(|(h, _)| h).unwrap_or(v)
}

/// What the harness can reach: a liveness probe and the node's tools. No
/// operator API, so a harness that finds the forward cannot drive sessions.
pub fn harness_router(state: AppState) -> Router {
    Router::new()
        .route("/harness/ping", get(|| async { "pong" }))
        .route("/mcp/{session_id}", post(mcp::handle))
        .route(
            "/model/{provider}/{*rest}",
            axum::routing::any(crate::gateway::model::handle),
        )
        .route(
            "/git/{*rest}",
            axum::routing::any(crate::gateway::git::handle),
        )
        .with_state(state)
}

pub async fn serve(listen: SocketAddr) -> Result<()> {
    // The state directory is this node's alone: a second `serve` on it
    // stops here, before it counts as a failed start of the first one's
    // file (`config::last_good`).
    let boots = crate::config::last_good::Paths::node();
    let state_lock = match crate::config::last_good::claim(&boots) {
        Ok(lock) => Some(lock),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            return Err(anyhow::anyhow!(error))
        }
        Err(error) => {
            tracing::warn!(%error, "could not lock the state directory; starts are not counted and node.toml.last-good is not kept");
            None
        }
    };
    // Counted before the load, so a file that keeps the node from starting
    // is found out, and the last one it ran well on put back.
    if state_lock.is_some() {
        if let Err(error) = crate::config::last_good::on_boot(&boots, crate::store::now_ms()) {
            tracing::warn!(%error, "could not count this start; node.toml.last-good will not be restored");
        }
    }
    // A config that does not parse is fatal here, not a warning: a typo in
    // `[mesh]` must not quietly run this node unmeshed.
    // Before the load, which may migrate the retired harness and so rewrite
    // the file: an interrupted edit is settled against this version too.
    let config_before_load = crate::config::edit::file_hash(&Config::config_path()).ok();
    let cfg = Arc::new(Config::try_load().map_err(|e| anyhow::anyhow!(e))?);
    // The file as it loaded (after any migration the load wrote): what a
    // restart would change is measured against it, and it is what becomes
    // last-good once the node has run on it.
    let boot_text = std::fs::read_to_string(Config::config_path()).unwrap_or_default();
    cfg.set_boot_text(&boot_text);
    // Before anything runs: every program the node spawns from here on is
    // checked against this, and a key that breaks a rule is reported now.
    crate::config::path_guard::install_for_serve(&cfg);
    crate::runner::protocol::configure(&cfg);
    let store = Arc::new(Store::open(&Config::db_path()).context("open store")?);
    // Only the serving node's own startup interrupts a stale `running` check;
    // see `Store::reconcile_interrupted_runs`. A CLI subcommand opening the
    // same database (mesh join, channel management, ...) must not.
    store
        .reconcile_interrupted_runs()
        .context("reconcile interrupted checks")?;
    // An edit of node.toml interrupted between its audit row and its rename.
    match crate::config::edit::reconcile_audit(&store, config_before_load.as_deref()) {
        Ok(0) => {}
        Ok(n) => tracing::warn!(
            rows = n,
            "settled node.toml audit rows an interrupted edit left pending"
        ),
        Err(error) => tracing::warn!(%error, "could not settle pending node.toml audit rows"),
    }
    match store.repair_search_index() {
        Ok(true) => {
            tracing::warn!("the full-text search index had drifted from its tables; rebuilt it")
        }
        Ok(false) => {}
        Err(error) => tracing::warn!(%error, "could not check the full-text search index"),
    }
    store
        .reconcile_repo_images()
        .context("reconcile interrupted image builds")?;
    // Serving does not need them archived — nothing relaunches a session on
    // startup — but until they are, the ones that never ended read as running
    // and hold up anything that waits for the node to be idle.
    for harness in crate::adapter::RETIRED {
        match store.unarchived_legacy_sessions(harness) {
            Ok(0) => {}
            Ok(sessions) => tracing::warn!(
                sessions,
                harness,
                "sessions of a retired harness are not archived; `tracon session archive-legacy` puts them away read-only"
            ),
            Err(e) => tracing::warn!(error = %e, harness, "could not count a retired harness's sessions"),
        }
    }
    let bus = Bus::new();
    let adapter: Arc<dyn HarnessAdapter> = crate::adapter::adapter_for(&cfg)?;
    // The identity comes first: the credential store is sealed under a key
    // derived from it. `init_node` loads the same seed again below.
    let (store_key_identity, _) = crate::mesh::identity::load_or_generate()?;
    let store_key = store_key_identity.credential_store_key();
    let broker = crate::broker::Broker::load(&store_key).unwrap_or_else(|e| {
        // A broken store must not silently broker nothing: say so loudly and
        // carry on without credentials.
        tracing::error!(error = %e, "credential store could not be read; brokering nothing");
        Default::default()
    });
    let broker = broker.shared();
    {
        let b = broker.read().unwrap();
        if b.is_empty() {
            tracing::info!(path = %crate::broker::Broker::path().display(), "no credentials; no tools offered");
        } else {
            tracing::info!(credentials = ?b.names(), "credential broker loaded");
        }
    }
    // A bundle that cannot be verified yields no rules, and no rules means every
    // request is asked. The failure mode of broken policy is more questions.
    let policy = Arc::new(parking_lot::RwLock::new(
        match crate::policy::bundle::load() {
            Ok(p) => {
                tracing::info!(rules = p.rules.len(), "policy bundle verified");
                p
            }
            Err(crate::policy::bundle::BundleError::Io(e))
                if e.kind() == std::io::ErrorKind::NotFound =>
            {
                tracing::info!(
                    path = %crate::policy::bundle::Paths::bundle().display(),
                    "no policy bundle; every request will be asked"
                );
                Default::default()
            }
            Err(e) => {
                tracing::error!(error = %e, "policy bundle refused; every request will be asked");
                Default::default()
            }
        },
    ));

    let tools = Arc::new(crate::mcp::Tools {
        broker: broker.clone(),
        cfg: cfg.clone(),
        policy: policy.clone(),
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("http client"),
        session: Default::default(),
        upstream: Default::default(),
    });

    let backend = boundary::backend_for(&cfg).await;
    tracing::info!(runtime = backend.kind(), "boundary backend");
    // Before anything syncs: no import, clone or sync outlives the process
    // that began it, so every staging copy beside the volumes kept for
    // sandboxes is left over, and a volume a swap was cut short in is put back.
    if let Some(files) = backend.sandbox_files_store() {
        let root = files.root().to_path_buf();
        match tokio::task::spawn_blocking(move || boundary::sweep_staging(&root)).await {
            Ok(Ok(swept)) => {
                if !swept.removed.is_empty() {
                    tracing::info!(removed = ?swept.removed, "removed staging copies left by work a restart ended");
                }
                if !swept.restored.is_empty() {
                    tracing::warn!(restored = ?swept.restored, "put back volumes a swap was cut short in");
                }
            }
            Ok(Err(error)) => tracing::warn!(%error, "could not sweep leftover staging copies"),
            Err(error) => tracing::warn!(%error, "could not sweep leftover staging copies"),
        }
    }
    let self_reconciles = cfg.boundary.auto_reconcile && backend.converges();
    // The node serves first and converges after (`boundary_upkeep`, once the
    // listeners are bound): marked pending, the startup check below waits on
    // what that converge fetches (a release's bundles are minutes on a slow
    // line) and says so, rather than fetching it before anything listens.
    if self_reconciles {
        boundary::reconcile::mark_pending();
    }
    let (node_id, identity) = init_node(&store, &cfg, adapter.as_ref(), backend.as_ref()).await?;
    crate::broker::lend_to_preparation(broker.clone(), &node_id);
    let cleaned = crate::session::reconcile_after_restart(&store, &node_id, backend.as_ref()).await;
    match store.reconcile_operator_issue_publications() {
        Ok(0) => {}
        Ok(count) => tracing::warn!(
            count,
            "operator issue publications need reconciliation after restart"
        ),
        Err(error) => tracing::error!(%error, "could not reconcile operator issue publications"),
    }
    if !cleaned.is_empty() {
        tracing::info!(
            sessions = cleaned.len(),
            "closed sessions left over from a previous run"
        );
    }
    // A dispatch still `pending` from before the restart never learned its own
    // outcome; relabel it honestly so an operator reconciles it rather than
    // the node silently presenting it as still in flight forever.
    match store.authority_action_reconcile_pending() {
        Ok(0) => {}
        Ok(n) => tracing::warn!(
            actions = n,
            "marked authority actions uncertain after a restart interrupted their dispatch"
        ),
        Err(e) => tracing::error!(error = %e, "could not reconcile pending authority actions"),
    }
    // The same for a publication: a record still in flight under another
    // instance was interrupted between a side effect and its outcome. It is
    // not retried here — recovery inspects the forge when the publication is
    // attempted again — but it stops looking live.
    match store.publication_reconcile_interrupted(crate::process::instance_id()) {
        Ok(0) => {}
        Ok(n) => tracing::warn!(
            publications = n,
            "marked publications uncertain after a restart interrupted them"
        ),
        Err(e) => tracing::error!(error = %e, "could not reconcile interrupted publications"),
    }
    match store.reconcile_publishing_without_record(&node_id) {
        Ok(0) => {}
        Ok(n) => tracing::warn!(
            reviews = n,
            "returned reviews to the queue that were claimed for publication but never reached it"
        ),
        Err(e) => tracing::error!(error = %e, "could not reconcile unrecorded publish claims"),
    }
    let manager = Manager::new(
        store.clone(),
        bus.clone(),
        cfg.clone(),
        node_id.clone(),
        tools.clone(),
        policy.clone(),
        backend.clone(),
    );
    let _ = tools.session.set(crate::mcp::SessionAccess {
        store: store.clone(),
        manager: manager.clone(),
    });
    // Plugin calls are signed with the node's identity, the key behind its
    // node id, which each plugin is started with as `TRACON_NODE_KEY`.
    manager.plugins().set_signer(store_key_identity);
    manager.record_egress_refusals();
    manager.report_workspace_syncs();
    // A node killed outright leaves its plugins running, and their sockets
    // behind: the processes it recorded are ended first, so their sockets
    // no longer answer when the sweep tries them.
    let plugin_state = Config::state_dir();
    tokio::task::spawn_blocking(move || {
        use crate::plugin::supervisor;
        supervisor::running::reap_orphans(&plugin_state, supervisor::Timings::default().stop_grace);
        supervisor::sweep_sockets();
    });
    // Off unless `[digitalocean]` names a credential and `[placement]
    // digitalocean` is not `off`.
    if let Err(error) = crate::provider::digitalocean::runtime::DoRuntime::start(
        &cfg,
        broker.clone(),
        &node_id,
        backend.clone(),
        store.clone(),
    )
    .await
    {
        tracing::error!(%error, "the DigitalOcean sandbox provider did not start");
    }
    // With a hub configured, every frame this node publishes is tapped into
    // the mesh client's outbox, and peer state is pulled into the same tables.
    let mesh = cfg.mesh.hub_url.as_ref().map(|url| {
        let client = crate::mesh::client::MeshClient::new(
            identity,
            url,
            store.clone(),
            bus.clone(),
            cfg.clone(),
            policy.clone(),
        );
        client.set_broker(broker.clone(), crate::broker::Broker::path());
        bus.with_tap(client.spawn());
        manager.set_mesh(client.clone());
        // A repository image built here is pushed for the channels' other
        // members, and one they pushed may be taken in place of a build.
        crate::repo_image_share::install(
            cfg.clone(),
            store.clone(),
            backend.clone(),
            broker.clone(),
            client.clone(),
        );
        tracing::info!(hub = %url, "mesh client started");
        let relay = &cfg.mesh.sandbox_relay;
        if relay.enabled && relay.target.is_empty() {
            tracing::warn!("[mesh.sandbox_relay] is on but names no target; not serving the relay");
        } else if relay.enabled {
            let relay =
                crate::mesh::sandbox_relay::SandboxRelay::new(client.identity_shared(), url, relay);
            relay.publish();
            relay.spawn();
        }
        client
    });
    if mesh.is_none() {
        tracing::info!("no hub configured; this node is standalone (tracon enroll to join a mesh)");
    }
    let state = AppState {
        manager,
        cfg: cfg.clone(),
        adapter,
        node_id,
        tools,
        mesh,
        auth: Arc::new(auth::AuthState::load(&store, listen.ip().to_string())),
        enroll: Arc::new(std::sync::Mutex::new(api::EnrollJob::default())),
    };
    if let Some(m) = &state.mesh {
        m.set_executor(Arc::new(state.clone()));
        // A node that takes its DigitalOcean templates from a peer asks for
        // them over the mesh.
        if let Some(runtime) = crate::provider::digitalocean::runtime::global() {
            runtime.set_mesh(m.clone());
        }
    }
    // External candidates' checks a restart interrupted, and the reviewers
    // that were waiting on them.
    tokio::spawn({
        let store = store.clone();
        let manager = state.manager.clone();
        async move { crate::mcp::review::reconcile_interrupted_checks(&store, &manager).await }
    });
    // The nightly batch, for channels this node processes.
    tokio::spawn(crate::corpus::promote::nightly(
        store.clone(),
        bus.clone(),
        state.node_id.clone(),
        cfg.clone(),
    ));
    // Provider sign-in runs in the node; a connected provider re-runs the
    // model probe.
    {
        let providers = crate::providers::Providers::new(
            cfg.clone(),
            broker.clone(),
            store_key,
            state.node_id.clone(),
            bus.clone(),
        );
        // Sign-ins are shared, refreshes claimed and renewed copies handed off
        // through the hub; a copy that arrives updates what this node says.
        if let Some(mesh) = state.mesh.clone() {
            providers.set_mesh(mesh.clone());
            let receiver = providers.clone();
            mesh.set_on_handoff(Box::new(move |names| receiver.handoff_received(names)));
            let revoked = providers.clone();
            mesh.set_on_revoke(Box::new(move |names| revoked.credentials_revoked(names)));
            let vouching = providers.clone();
            mesh.set_vouch(Box::new(move |name| vouching.can_hand_over(name)));
        }
        let probe_state = state.clone();
        let probe_backend = backend.clone();
        providers.set_on_connected(Box::new(move || {
            let s = probe_state.clone();
            let b = probe_backend.clone();
            tokio::spawn(async move {
                let _ = api::probe_models_into_store(&s, b.as_ref()).await;
                // A session waiting for this sign-in carries on now rather
                // than at the next tick.
                s.manager.wake_signed_in().await;
            });
        }));
        // Every provider change lands in this node's row and rides the hello,
        // so a peer's interface renders and drives these providers.
        let pub_store = store.clone();
        let pub_bus = bus.clone();
        let pub_id = state.node_id.clone();
        let carry = move |list: Vec<serde_json::Value>| {
            let json = serde_json::Value::Array(list).to_string();
            if pub_store.set_node_providers(&pub_id, &json).is_ok() {
                if let Ok(Some(row)) = pub_store.get_node(&pub_id) {
                    pub_bus.publish(crate::stream::Frame::Node(row.to_json()));
                }
            }
        };
        carry(providers.list_public()); // seed the row before the first hello
        providers.set_on_publish(Box::new(carry));
        state.manager.set_providers(providers.clone());
        tokio::spawn(providers.refresh_loop());
    }
    // The harness listener is separate from the operator's: it carries only the
    // MCP surface, and the gateway forwards to it from the internal network.
    // Where no gateway container carries the allowlist proxy, the node does.
    if let Some(port) = backend.proxy_port() {
        let allow = crate::gateway::proxy::Allowlist::new(&cfg.gateway.allow_hosts)
            .map_err(|e| anyhow::anyhow!("allowlist: {e}"))?;
        // Only a pod-hosted node serves it, beside the cluster's services:
        // what it will never dial is the backend's.
        tokio::spawn(crate::gateway::proxy::serve_refusing(
            port,
            allow,
            backend.egress_refusal(),
        ));
        tracing::info!(port, "connect proxy listening");
    }
    // Sandboxes with no network reach the allowlist proxy through a tunnel,
    // so the node serves it where the gateway container's tinyproxy would.
    let tunnelled = match cfg.runtime.kind {
        crate::config::RuntimeKind::Podman | crate::config::RuntimeKind::Apple => true,
        crate::config::RuntimeKind::Kubernetes => false,
    };
    if tunnelled {
        let allow = crate::gateway::proxy::Allowlist::new(&cfg.gateway.allow_hosts)
            .map_err(|e| anyhow::anyhow!("allowlist: {e}"))?;
        let path = cfg.allow_proxy_socket();
        let l = bind_unix_exclusive(&path)?;
        tokio::spawn(crate::gateway::proxy::run_unix_refusing(
            l,
            allow,
            backend.egress_refusal(),
        ));
        tracing::info!(socket = %path.display(), "allowlist proxy listening for sandbox tunnels");
    }
    // Where a gateway container does carry the harness proxy, the node still
    // serves the per-client one behind the gateway's second forward: who may
    // reach what is decided here, by the grant each client presents. A pod
    // reaches it directly, on the pod network, and what it may never dial
    // there is wider (`Backend::egress_refusal`).
    if let Some(grants) = backend.egress_grants() {
        let granted =
            crate::gateway::proxy::Granted::refusing(grants.clone(), backend.egress_refusal());
        let listen = cfg.egress_listen();
        tracing::info!(%listen, "egress listener");
        match listen {
            crate::config::HarnessListen::Tcp(addr) => {
                let l = tokio::net::TcpListener::bind(addr)
                    .await
                    .with_context(|| format!("bind egress listener {addr}"))?;
                tokio::spawn(crate::gateway::proxy::serve_granted(l, granted));
            }
            crate::config::HarnessListen::Unix(path) => {
                let l = bind_unix_exclusive(&path)?;
                tokio::spawn(crate::gateway::proxy::serve_granted_unix(l, granted));
            }
        }
    }
    let harness_app = harness_router(state.clone());
    tracing::info!(listen = %cfg.gateway.harness_listen, "harness listener");
    match &cfg.gateway.harness_listen {
        crate::config::HarnessListen::Tcp(addr) => {
            let l = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("bind {addr}"))?;
            tokio::spawn(async move {
                let _ = axum::serve(l, harness_app).await;
            });
        }
        crate::config::HarnessListen::Unix(path) => {
            let l = bind_unix_exclusive(path)?;
            tokio::spawn(async move {
                let _ = axum::serve(l, harness_app).await;
            });
        }
    }

    // Now that the harness can reach the gateway, ask it what models it offers.
    // The probe presents the node's own read-only token; without a model
    // credential there is nothing to inject, so the list stays empty and the interface says to connect a provider.

    {
        let probe_state = state.clone();
        let probe_backend = backend.clone();
        tokio::spawn(async move {
            let _ = api::probe_models_into_store(&probe_state, probe_backend.as_ref()).await;
        });
    }
    // With the harness listener up, so the checks that follow can reach it
    // through the gateway.
    if self_reconciles {
        tokio::spawn(boundary_upkeep(state.clone(), backend.clone()));
    }
    let preview_listen = cfg.docs.preview_listen;
    let preview_listener = tokio::net::TcpListener::bind(preview_listen)
        .await
        .with_context(|| format!("bind HTML preview listener {preview_listen}"))?;
    let preview_app = preview::router(store.clone(), state.manager.previews().clone());
    tracing::info!(
        listen = %preview_listen,
        origin = %cfg.docs.preview_origin().expect("validated configuration"),
        "HTML preview listener"
    );
    tokio::spawn(async move {
        if let Err(error) = axum::serve(preview_listener, preview_app).await {
            tracing::error!(%error, "HTML preview listener stopped");
        }
    });

    // The scrape target, only when configured: its own router, so nothing of
    // the operator's is reachable through it.
    crate::telemetry::telemetry()
        .set_build_info(env!("CARGO_PKG_VERSION"), cfg.runtime.kind.as_str());
    crate::telemetry::telemetry().set_catalogue(&cfg);
    if let Some(metrics_listen) = cfg.metrics.listen {
        if metrics_listen.port() == listen.port() {
            anyhow::bail!(
                "metrics.listen: port {} is the operator listener's; give the metrics listener a port of its own",
                listen.port()
            );
        }
        let metrics_listener = tokio::net::TcpListener::bind(metrics_listen)
            .await
            .with_context(|| format!("bind metrics listener {metrics_listen}"))?;
        let metrics_app = metrics::router(store.clone(), state.node_id.clone());
        tracing::info!(listen = %metrics_listen, "metrics listener");
        tokio::spawn(async move {
            if let Err(error) = axum::serve(metrics_listener, metrics_app).await {
                tracing::error!(%error, "metrics listener stopped");
            }
        });
    }

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    // Where this node answers, so a CLI on the same state directory reaches
    // it and not whatever holds the default port. Removed on a clean exit.
    let bound = listener.local_addr().unwrap_or(listen);
    let _serve_file =
        match crate::node_target::write_serve_file(&Config::state_dir(), bound, &state.node_id) {
            Ok(guard) => Some(guard),
            Err(error) => {
                tracing::warn!(%error, "could not record this node's address in serve.json");
                None
            }
        };
    // A claim measures attention. One left behind by a client that vanished
    // would report a review as attended forever, so claims lapse.
    //
    // The same tick applies deny-by-default to requests their supervisor never
    // got to. A session that ended with the process — or that was already
    // terminal when `reconcile_after_restart` walked past it — leaves its open
    // requests behind, and no answer can reach a harness that is gone. Left
    // alone they sit in the queue as decisions the operator cannot make, which
    // is exactly what the attention count must not contain.
    // The machine stays awake while a session works, and a suspend that
    // happens anyway is noticed and recorded.
    let awake = crate::awake::Awake::new(state.manager.clone());
    crate::awake::install(awake.clone());
    tokio::spawn(awake.clone().run());
    // Each `[[mcp_server]]` is listed ahead of the sessions that are offered
    // its tools, so an orientation names them, and a server that comes back
    // is noticed without anyone asking.
    {
        let tools = state.tools.clone();
        let node_id = state.node_id.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                tick.tick().await;
                tools.refresh_upstream(&node_id, false).await;
            }
        });
    }
    {
        // Before their sandboxes give up redialling.
        let manager = state.manager.clone();
        tokio::spawn(async move { manager.carry_on_after_restart().await });
    }
    // The restart status follows the work running here as it starts and
    // stops, so the banner and the Settings card name it live.
    tokio::spawn(crate::restart::watch(state.manager.clone()));
    {
        let store = store.clone();
        let manager = state.manager.clone();
        let node_id = state.node_id.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
            loop {
                tick.tick().await;
                // node.toml changed by another process (the CLI, an editor):
                // its live keys apply from here.
                cfg.refresh_live_from(&Config::config_path());
                crate::restart::publish(&manager).await;
                let grace = std::time::Duration::from_secs(cfg.live().session.claim_grace_secs);
                // A deadline is judged only after a suspend has moved it on.
                awake.catch_up().await;
                // An approval waits for the operator until it expires; the
                // caller is not blocked on it, so nothing else would end it.
                manager.expire_approvals().await;
                // A session waiting out its provider's limit is woken here,
                // and one bound for its fallback is carried on.
                manager.wake_exhausted().await;
                // A session waiting for a sign-in carries on once one is
                // here, or ends once it has waited too long.
                manager.wake_signed_in().await;
                // A suspended session whose item another node closed ends
                // here, and is not carried on.
                manager.end_closed_suspended().await;
                // A session the node stopped under while it waited on the
                // operator is carried on once they have decided.
                manager.continue_held().await;
                let stale = store
                    .stale_claims(&node_id, grace.as_millis() as i64)
                    .unwrap_or_default();
                for id in &stale {
                    let _ = store.release_review(&node_id, id);
                }
                let stale = store
                    .stale_approval_claims(grace.as_millis() as i64)
                    .unwrap_or_default();
                for id in &stale {
                    let _ = store.release_approval(id);
                }
                let orphaned = store.orphaned_permissions(&node_id).unwrap_or_default();
                for p in &orphaned {
                    // The monotonic clock the request was created against may
                    // belong to a process that is gone, so resolve at the
                    // created reading: duration 0 means not measured, which is
                    // the truth, rather than a negative span.
                    let _ = store.resolve_permission(&p.id, "expired", None, p.created_mono_ms);
                    let Some(session_id) = p.session_id.clone() else {
                        continue;
                    };
                    let _ = store.append_event(&crate::store::NewEvent {
                        session_id,
                        work_item_id: None,
                        kind: crate::session::state::event_kind::PERMISSION_EXPIRED.into(),
                        ref_id: Some(p.id.clone()),
                        payload: serde_json::json!({
                            "permission_id": p.id,
                            "reason": "denied: session ended without answering",
                        }),
                        at_ms: crate::store::now_ms(),
                        mono_ms: 0,
                    });
                }
                if stale.is_empty() && orphaned.is_empty() {
                    continue;
                }
                manager.publish_queue().await;
            }
        });
    }

    // A session whose work is published and which has gone quiet is put
    // away rather than left holding its container and its way out.
    // Read at every tick, so turning it on, off or changing it applies
    // without a restart.
    {
        let manager = state.manager.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                tick.tick().await;
                let after = cfg.live().session.suspend_published_after_secs;
                if after > 0 {
                    manager
                        .suspend_idle_published(std::time::Duration::from_secs(after))
                        .await;
                }
            }
        });
    }

    // What waits on the operator, pushed to the phones subscribed here. Every
    // node delivers for its own devices; a channel opts out by binding.
    tokio::spawn(crate::notify::run(
        store.clone(),
        bus.clone(),
        cfg.clone(),
        state.node_id.clone(),
    ));

    // Requests this node published, followed on their forge until they close.
    tokio::spawn(crate::follow::run(state.tools.clone()));

    // This node's own vector index. Derived state, rebuilt from the corpus, so
    // it starts after everything that writes the corpus and never gates it.
    tokio::spawn(crate::embed::run(
        cfg.clone(),
        store.clone(),
        bus.clone(),
        state.tools.http.clone(),
        state.manager.probe_token().to_string(),
    ));

    // The documents written back out as files, when `[docs] export_dir` says where.
    tokio::spawn(crate::corpus::export::run(store.clone(), cfg.clone()));

    // `tracon gc --apply` on the operator's timer, when `[gc] every_secs` sets one.
    tokio::spawn(crate::gc_schedule::Schedule::new().run(state.manager.clone(), cfg.clone()));

    tracing::info!(%listen, "serving");
    // Started: a restore of last-good is recorded and told; serving a short
    // while means this start did not fail; and once the node has run a
    // while on this file, it is last-good, which then follows live edits.
    if state_lock.is_some() {
        use crate::config::last_good::{self, HEALTHY_AFTER, STARTED_GRACE};
        last_good::settle(&boots, &store, &cfg);
        let boots = boots.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move {
            tokio::time::sleep(STARTED_GRACE).await;
            last_good::clean_stop(&boots);
            tokio::time::sleep(HEALTHY_AFTER.saturating_sub(STARTED_GRACE)).await;
            if let Err(error) = last_good::promote(&boots, &boot_text) {
                tracing::warn!(%error, "could not keep node.toml as last-known-good");
            }
            let mut previous = None;
            loop {
                last_good::keep_current(&boots, &cfg, &mut previous);
                tokio::time::sleep(HEALTHY_AFTER).await;
            }
        });
    }
    let manager = state.manager.clone();
    let bus = manager.bus().clone();
    let operator =
        router(state.clone()).layer(axum::middleware::from_fn_with_state(state, auth::guard));
    // ConnectInfo is how the guard tells a caller on this machine from one that
    // arrived over the network; without it every request is treated as remote.
    axum::serve(
        listener,
        operator.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        // End open SSE streams first: a keep-alive stream never completes on
        // its own, so graceful shutdown would otherwise wait on it until the
        // supervisor sends SIGKILL and orphans harness containers.
        bus.begin_shutdown();
    })
    .await
    .context("http server")?;
    // A session's sandbox that can resume stays for the next process to
    // carry the session on in; it ends itself if no process takes it back.
    let left = crate::runner::protocol::detach_all();
    if left > 0 {
        tracing::info!(
            sandboxes = left,
            "left sessions' sandboxes running for the next node process"
        );
    }
    // The listener is closed; end every live session so no harness container
    // outlives the process that gates it.
    manager.shutdown_all().await;
    // No plugin process outlives the node that supervises it.
    manager.plugins().shutdown().await;
    // A stop, not a failed start.
    if state_lock.is_some() {
        crate::config::last_good::clean_stop(&boots);
    }
    drop(state_lock);
    Ok(())
}

/// Verify the boundary and record what this node is. A node that fails the
/// check still serves the interface and says which check failed; it just does
/// not run harnesses.
///
/// The node id is its Ed25519 public key. A database written before identities
/// existed carries a uuid; it is rekeyed once, here, across every table.
async fn init_node(
    store: &Arc<Store>,
    cfg: &Arc<Config>,
    adapter: &dyn HarnessAdapter,
    backend: &dyn boundary::Backend,
) -> Result<(String, proto::keys::Identity)> {
    let (identity, fresh) = crate::mesh::identity::load_or_generate().context("node identity")?;
    let id = identity.node_id();
    crate::image_gc::set_node_id(&id);
    if fresh {
        tracing::info!(node_id = %id, "generated this node's identity");
    }
    match store.self_node_id()? {
        Some(old) if old != id => {
            store.rekey_self_node(&old, &id)?;
            tracing::info!(from = %old, to = %id, "node id rekeyed to the identity key");
        }
        _ => {}
    }
    if let Some(p) = crate::session::materialize::retire_harness_credentials() {
        tracing::warn!(
            path = %p.display(),
            "a credential store was on the harness volume; set aside — models are brokered through the gateway now"
        );
    }
    verify_node(store, cfg, adapter, backend, &id, Some(&identity)).await?;
    // This node is always bound to the channels it holds keys for.
    for c in store.channel_list()? {
        store.node_channel_add(&id, &c.name)?;
    }
    Ok((id, identity))
}

/// The version the installed harness bundle's manifest names for
/// `adapter`'s component: what this node records as found for it, read
/// without starting a sandbox (every backend runs its harnesses from that
/// bundle). `None` for a harness that is not in the bundle, or while no
/// bundle is installed. A manifest that cannot be read, or that lacks the
/// component, records "unknown", which does not equal the pin: new sessions
/// are blocked with the version pair shown rather than run on a harness the
/// node cannot name. Each sandbox's handshake still checks what it runs.
fn bundle_found(
    versions: Option<&Result<std::collections::BTreeMap<String, String>, String>>,
    adapter: &dyn HarnessAdapter,
) -> Option<String> {
    let component = adapter.bundle_component()?;
    Some(match versions? {
        Ok(versions) => versions.get(component).cloned().unwrap_or_else(|| {
            tracing::warn!(
                harness = adapter.id(),
                component,
                "the harness bundle's manifest does not name this harness; treating its version as unknown"
            );
            "unknown".into()
        }),
        Err(error) => {
            tracing::warn!(harness = adapter.id(), %error, "could not read the harness bundle's manifest; treating its version as unknown");
            "unknown".into()
        }
    })
}

/// Every harness this node runs, each with the version the installed
/// bundle's manifest names for it: a session runs whichever its model's
/// credential allows, so each one's version is a fact about this node. A
/// node whose configured harness is not a supported one (a test's fake) runs
/// only that.
async fn harness_states(
    cfg: &Config,
    adapter: &dyn HarnessAdapter,
    backend: &dyn boundary::Backend,
    versions: Option<&Result<std::collections::BTreeMap<String, String>, String>>,
) -> Vec<crate::store::HarnessState> {
    let images = backend.harness_images(cfg).await;
    let image_of = |id: &str| images.iter().find(|image| image.harness_id == id);
    let state = |other: &dyn HarnessAdapter, default: bool| crate::store::HarnessState {
        id: other.id().to_string(),
        pinned: other.pinned_version().to_string(),
        found: bundle_found(versions, other),
        default,
        image: image_of(other.id()).map(|image| image.image.clone()),
        image_state: image_of(other.id())
            .map_or("unknown", |image| image.state)
            .to_string(),
    };
    if !crate::adapter::is_enabled(cfg, adapter.id()) {
        return vec![state(adapter, true)];
    }
    let mut out = Vec::new();
    for id in crate::adapter::enabled(cfg) {
        if id == adapter.id() {
            out.push(state(adapter, true));
            continue;
        }
        let Ok(other) = crate::adapter::adapter_for_id(cfg, id) else {
            continue;
        };
        out.push(state(other.as_ref(), false));
    }
    out
}

/// One verification at a time. A re-check that began while the node was
/// setting up its boundary must not land after the check that followed the
/// setup, and record a verdict on images that have since been rebuilt.
static VERIFY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Run the boundary checks, read the harnesses' versions from the installed
/// bundle, and record what this node is.
///
/// Startup calls it, and so does a re-check from the interface: an operator
/// who has just started the runtime should not have to restart the node to be
/// believed. The versions are read here, not at startup, because the bundle
/// may arrive after the node starts (its converge fetches it), and a node
/// that goes refused → ready must record what it now runs. Nothing here
/// starts a sandbox but the boundary's own checks.
pub(crate) async fn verify_node(
    store: &Arc<Store>,
    cfg: &Arc<Config>,
    adapter: &dyn HarnessAdapter,
    backend: &dyn boundary::Backend,
    id: &str,
    identity: Option<&proto::keys::Identity>,
) -> Result<NodeRow> {
    let _one = VERIFY.lock().await;
    backend.recover(cfg).await;
    let report = backend.check_all(cfg, false).await;
    let mut failed = report.first_failure().cloned();
    if let (Some(f), Some(note)) = (failed.as_mut(), boundary::reconcile::note()) {
        f.detail = format!("{}; {note}", f.detail);
    }
    let ready = failed.is_none();
    match &failed {
        Some(f) if f.pending => tracing::info!(
            check = f.id.as_str(),
            detail = %f.detail,
            "not running harnesses until the boundary is set up"
        ),
        Some(f) => {
            tracing::warn!(check = f.id.as_str(), detail = %f.detail, "refusing to run harnesses")
        }
        None => {}
    }
    // What the installed bundle names, read here rather than from a
    // sandbox: a version pod would need a schedulable node, which a pool that
    // autoscales from zero does not have until a session asks for one.
    let versions = crate::bundle::installed_versions(cfg);
    let harnesses = harness_states(cfg, adapter, backend, versions.as_ref()).await;
    let found = harnesses
        .iter()
        .find(|h| h.default)
        .and_then(|h| h.found.clone());
    let previous = store.get_node(id).ok().flatten();
    // The model list is probed once the gateway is listening (see `serve`);
    // until then the previous run's list stands.
    let models: Vec<crate::adapter::ModelOption> = previous
        .as_ref()
        .and_then(|n| n.models_json.clone())
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    let policy = crate::policy::bundle::current().ok();

    let row = NodeRow {
        id: id.to_string(),
        is_self: 1,
        x25519_pub: identity
            .map(|i| i.x25519_hex())
            .or_else(|| previous.as_ref().and_then(|n| n.x25519_pub.clone())),
        last_seen_ms: Some(now_ms()),
        reachable: 1,
        // Only the providers' own publish writes this, and a re-check is not
        // one. The hello carries this row, so a summary dropped here would
        // show this node to every peer as offering no model.
        providers_json: previous.as_ref().and_then(|n| n.providers_json.clone()),
        app_version: Some(env!("CARGO_PKG_VERSION").into()),
        wire_contract: Some(proto::CONTRACT_VERSION),
        policy_identity: policy.as_ref().map(|bundle| bundle.pubkey_hex.clone()),
        policy_sha256: policy.as_ref().map(|bundle| bundle.sha256.clone()),
        policy_receipt_v1: Some(true),
        harnesses_json: serde_json::to_string(&harnesses).ok(),
        name: cfg.node_name.clone(),
        state: if ready { "ready" } else { "refused" }.into(),
        failed_check: failed.as_ref().map(|f| f.id.as_str().to_string()),
        failed_detail: failed.as_ref().map(|f| f.detail.clone()),
        harness_id: adapter.id().into(),
        harness_pinned: adapter.pinned_version().into(),
        harness_found: found,
        models_json: serde_json::to_string(&models).ok(),
        checked_at_ms: Some(now_ms()),
    };
    store.put_node(&row)?;
    Ok(row)
}

/// How often a running node looks for definitions that changed under it.
const DEFINITIONS_POLL: std::time::Duration = std::time::Duration::from_secs(300);

/// Keep the boundary in step with this binary without an operator: converge
/// once now, then again whenever what the images are built from changes
/// (`Backend::definitions`). A converge that changed something, or that
/// leaves the node still refused, is followed by the checks, which are what
/// anyone sees.
async fn boundary_upkeep(s: AppState, backend: Arc<dyn boundary::Backend>) {
    let mut seen = backend.definitions(&s.cfg);
    converge_and_check(&s, backend.as_ref()).await;
    loop {
        tokio::time::sleep(DEFINITIONS_POLL).await;
        let now = backend.definitions(&s.cfg);
        if now == seen {
            continue;
        }
        tracing::info!("the boundary's definitions changed; setting it up again");
        seen = now;
        converge_and_check(&s, backend.as_ref()).await;
    }
}

async fn converge_and_check(s: &AppState, backend: &dyn boundary::Backend) {
    let changed = boundary::reconcile::converge(backend, &s.cfg).await;
    match &changed {
        Ok(true) => tracing::info!("boundary brought in step with this build"),
        Ok(false) => tracing::info!("boundary already in step with this build"),
        Err(error) => tracing::warn!(%error, "could not set up the boundary"),
    }
    // Only said, never acted on: removing images is the operator's `tracon gc`.
    crate::gc::note_reclaimable_images(backend, &s.cfg, s.store(), &s.node_id).await;
    let ready = s
        .store()
        .get_node(&s.node_id)
        .ok()
        .flatten()
        .is_some_and(|n| n.state == "ready");
    if matches!(changed, Ok(false)) && ready {
        // Nothing moved, and the verdict that stands already passed; only
        // the interface has to hear that the setup is over.
        if let Ok(v) = api::node_json(s) {
            s.manager.bus().publish(crate::stream::Frame::Node(v));
        }
        return;
    }
    if let Err(error) = api::reverify(s).await {
        tracing::warn!(%error, "could not record the boundary check");
    }
}

/// Bind a Unix socket at `path`, replacing one a previous run left behind but
/// never one a running node still answers on. Unlinking a live socket leaves
/// its node listening on an inode nothing can reach, so a second node started
/// with the default listen paths would cut the first off from the gateway.
fn bind_unix_exclusive(path: &std::path::Path) -> Result<tokio::net::UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        anyhow::bail!(
            "{} is in use by another running node; stop it, or give this node its own \
             [gateway] harness_listen",
            path.display()
        );
    }
    // A socket left by a previous run refuses a fresh bind.
    let _ = std::fs::remove_file(path);
    tokio::net::UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {},
            _ = term.recv() => {},
            _ = crate::restart::exit_requested() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::select! {
        _ = ctrl_c => {},
        _ = crate::restart::exit_requested() => {},
    }
    crate::restart::note_shutdown();
    tracing::info!("shutting down");
}

#[cfg(test)]
mod tests {
    use super::{bind_unix_exclusive, hostname};

    #[tokio::test]
    async fn a_live_socket_is_refused_and_a_stale_one_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run").join("harness.sock");
        let first = bind_unix_exclusive(&path).unwrap();

        // Something answers: the path is left alone and the bind refused.
        let err = bind_unix_exclusive(&path).unwrap_err().to_string();
        assert!(err.contains("in use by another running node"), "{err}");
        assert!(tokio::net::UnixStream::connect(&path).await.is_ok());

        // Nothing answers once the first listener is gone: the stale path is
        // replaced. A child another test thread is forking holds a copy of the
        // listener until it execs (commands with their own `PATH` are forked
        // rather than spawned, and fork is slow on macOS), so allow it that.
        drop(first);
        assert!(path.exists());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let _second = loop {
            match bind_unix_exclusive(&path) {
                Ok(listener) => break listener,
                Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(e) => panic!("{e:#}"),
            }
        };
        assert!(tokio::net::UnixStream::connect(&path).await.is_ok());
    }

    #[test]
    fn hostname_strips_scheme_port_and_brackets() {
        assert_eq!(hostname("127.0.0.1:7420"), "127.0.0.1");
        assert_eq!(hostname("localhost"), "localhost");
        assert_eq!(hostname("http://[::1]:7420"), "::1");
        assert_eq!(hostname("http://evil.example"), "evil.example");
        assert_eq!(hostname("evil.example:80"), "evil.example");
    }

    /// A node on Kubernetes whose `[runtime.kubernetes.pool]` autoscales from
    /// zero and has no node yet: any pod but the boundary's gated probe
    /// would wait for a scale-up and be given up as unschedulable.
    mod version_from_the_bundle {
        use std::sync::{Arc, Mutex};

        use async_trait::async_trait;

        use crate::adapter::{claude::ClaudeAdapter, native::NativeAdapter};
        use crate::boundary::{Backend, BoundaryError, BoundaryReport, CheckId, CheckResult};
        use crate::config::Config;
        use crate::runner::{Mount, Runner, RunnerCommand, RunnerError, Spawned};
        use crate::store::Store;

        /// The pods the node created, by name.
        type Pods = Arc<Mutex<Vec<String>>>;

        /// The boundary's checks pass, as they do on the empty pool: their
        /// probe pod carries the `tracon.dev/probe` scheduling gate, so it
        /// is admitted and read back without ever being scheduled. Every
        /// other pod is created and stays unschedulable.
        struct EmptyPool {
            pods: Pods,
        }

        struct Unschedulable {
            pods: Pods,
        }

        impl Unschedulable {
            fn refuse(&self, cmd: &RunnerCommand) -> RunnerError {
                self.pods.lock().unwrap().push(cmd.name.clone());
                RunnerError::Other(format!(
                    "could not be scheduled: 0/1 nodes are available (pod {}, unschedulable for 60s)",
                    cmd.name
                ))
            }
        }

        #[async_trait]
        impl Runner for Unschedulable {
            async fn spawn(&self, cmd: RunnerCommand) -> Result<Spawned, RunnerError> {
                Err(self.refuse(&cmd))
            }
            async fn run_capture(
                &self,
                cmd: RunnerCommand,
            ) -> Result<std::process::Output, RunnerError> {
                Err(self.refuse(&cmd))
            }
            async fn kill(&self, _name: &str) -> Result<(), RunnerError> {
                Ok(())
            }
        }

        fn gated_probe() -> String {
            crate::config::resource_name("boundary-probe")
        }

        #[async_trait]
        impl Backend for EmptyPool {
            fn kind(&self) -> &'static str {
                "kubernetes"
            }
            async fn setup(&self, _cfg: &Config, _rebuild: bool) -> Result<(), BoundaryError> {
                Ok(())
            }
            async fn check_all(&self, _cfg: &Config, _deep: bool) -> BoundaryReport {
                self.pods.lock().unwrap().push(gated_probe());
                BoundaryReport {
                    checks: [
                        CheckId::Runtime,
                        CheckId::HarnessUnprivileged,
                        CheckId::NoRuntimeSocket,
                        CheckId::NetworkIsolated,
                    ]
                    .into_iter()
                    .map(|id| CheckResult::ok(id, "the gated probe pod"))
                    .collect(),
                }
            }
            fn runner(&self, _extra: Vec<Mount>) -> Arc<dyn Runner> {
                Arc::new(Unschedulable {
                    pods: self.pods.clone(),
                })
            }
            fn runner_for(&self, _harness_id: &str, _extra: Vec<Mount>) -> Arc<dyn Runner> {
                self.runner(Vec::new())
            }
            async fn import_volume(
                &self,
                _v: &str,
                _s: &std::path::Path,
            ) -> Result<(), BoundaryError> {
                Ok(())
            }
            async fn export_volume(
                &self,
                _v: &str,
                _d: &std::path::Path,
            ) -> Result<(), BoundaryError> {
                Ok(())
            }
            fn harness_host(&self) -> String {
                "10.244.0.9".into()
            }
            fn harness_home(&self) -> String {
                "/home/harness".into()
            }
            async fn reconcile(&self, _names: &[String]) {}
        }

        /// A bundle as `scripts/harness-bundle.sh` lays it out, whose
        /// manifest names these versions.
        fn bundle(dir: &std::path::Path, claude: &str, agent: &str) -> std::path::PathBuf {
            let bundle = dir.join("bundle");
            std::fs::create_dir_all(bundle.join("bin")).unwrap();
            let mut components = serde_json::Map::new();
            for (name, version) in [
                ("tracon-sandbox", agent),
                ("tracon-agent", agent),
                ("claude", claude),
            ] {
                std::fs::write(bundle.join("bin").join(name), name).unwrap();
                components.insert(
                    name.into(),
                    serde_json::json!({"path": format!("bin/{name}"), "version": version, "sha256": "00"}),
                );
            }
            std::fs::write(
                bundle.join("manifest.json"),
                serde_json::json!({"schema": 1, "version": agent, "arch": "amd64", "components": components})
                    .to_string(),
            )
            .unwrap();
            bundle
        }

        fn kubernetes(bundle: Option<std::path::PathBuf>) -> Arc<Config> {
            let mut cfg = Config::default();
            cfg.runtime.kind = crate::config::RuntimeKind::Kubernetes;
            cfg.harness.id = ClaudeAdapter::ID.into();
            // Given, so the test does not depend on the node's state directory.
            cfg.sandbox.bundle =
                Some(bundle.unwrap_or_else(|| std::path::PathBuf::from("/nonexistent/bundle")));
            Arc::new(cfg)
        }

        async fn verify(
            cfg: &Arc<Config>,
        ) -> (
            crate::store::NodeRow,
            Vec<crate::store::HarnessState>,
            Vec<String>,
        ) {
            let store = Arc::new(Store::open_in_memory().unwrap());
            let pods: Pods = Arc::default();
            let backend = EmptyPool { pods: pods.clone() };
            let adapter = crate::adapter::adapter_for_id(cfg, ClaudeAdapter::ID).unwrap();
            let row =
                super::super::verify_node(&store, cfg, adapter.as_ref(), &backend, "n1", None)
                    .await
                    .unwrap();
            let harnesses = row.harnesses();
            let pods = pods.lock().unwrap().clone();
            (row, harnesses, pods)
        }

        fn pins(cfg: &Config) -> (String, String) {
            let pin = |id| {
                crate::adapter::adapter_for_id(cfg, id)
                    .unwrap()
                    .pinned_version()
                    .to_string()
            };
            (pin(ClaudeAdapter::ID), pin(NativeAdapter::ID))
        }

        #[tokio::test]
        async fn an_empty_pool_is_not_scaled_up_for_the_harness_versions() {
            let tmp = tempfile::tempdir().unwrap();
            let (claude, native) = pins(&kubernetes(None));
            let cfg = kubernetes(Some(bundle(tmp.path(), &claude, &native)));
            let (row, harnesses, pods) = verify(&cfg).await;

            assert_eq!(pods, vec![gated_probe()], "no pod but the gated probe");
            assert_eq!(row.state, "ready", "{row:?}");
            assert_eq!(row.harness_found.as_deref(), Some(claude.as_str()));
            let found = |id: &str| {
                let h = harnesses.iter().find(|h| h.id == id).unwrap();
                assert!(!h.mismatch(), "{h:?}");
                h.found.clone()
            };
            assert_eq!(found(ClaudeAdapter::ID), Some(claude));
            assert_eq!(found(NativeAdapter::ID), Some(native));
        }

        /// A manifest that names another version than the pin is a
        /// mismatch, as a probe that read one was.
        #[tokio::test]
        async fn a_manifest_off_the_pin_is_a_mismatch() {
            let tmp = tempfile::tempdir().unwrap();
            let (_, native) = pins(&kubernetes(None));
            let cfg = kubernetes(Some(bundle(tmp.path(), "2.0.0-other", &native)));
            let (row, harnesses, pods) = verify(&cfg).await;

            assert_eq!(pods, vec![gated_probe()]);
            assert_eq!(row.harness_found.as_deref(), Some("2.0.0-other"));
            assert_eq!(row.to_json()["harness"]["mismatch"], true);
            let claude = harnesses
                .iter()
                .find(|h| h.id == ClaudeAdapter::ID)
                .unwrap();
            assert!(claude.mismatch(), "{claude:?}");
            let native = harnesses
                .iter()
                .find(|h| h.id == NativeAdapter::ID)
                .unwrap();
            assert!(!native.mismatch(), "{native:?}");
        }

        /// No bundle yet: nothing is found, which is not a mismatch.
        #[tokio::test]
        async fn without_a_bundle_nothing_is_found() {
            let cfg = kubernetes(None);
            let (row, harnesses, pods) = verify(&cfg).await;
            assert_eq!(pods, vec![gated_probe()]);
            assert_eq!(row.harness_found, None);
            assert_eq!(harnesses.len(), 2);
            for h in &harnesses {
                assert_eq!(h.found, None, "{h:?}");
                assert!(!h.mismatch());
            }
        }

        /// A bundle whose manifest cannot be read, or that does not name a
        /// harness, records it as unknown: a mismatch, fail closed.
        #[tokio::test]
        async fn an_unreadable_or_incomplete_manifest_is_unknown() {
            let tmp = tempfile::tempdir().unwrap();
            let (claude, native) = pins(&kubernetes(None));
            let dir = bundle(tmp.path(), &claude, &native);
            let manifest = dir.join("manifest.json");
            let text = std::fs::read_to_string(&manifest).unwrap();
            let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
            value["components"]
                .as_object_mut()
                .unwrap()
                .remove("tracon-agent");
            std::fs::write(&manifest, value.to_string()).unwrap();
            let cfg = kubernetes(Some(dir.clone()));
            let (_, harnesses, _) = verify(&cfg).await;
            let native = harnesses
                .iter()
                .find(|h| h.id == NativeAdapter::ID)
                .unwrap();
            assert_eq!(native.found.as_deref(), Some("unknown"));
            assert!(native.mismatch());

            std::fs::write(&manifest, "not json").unwrap();
            let (row, harnesses, _) = verify(&cfg).await;
            assert_eq!(row.harness_found.as_deref(), Some("unknown"));
            assert!(harnesses.iter().all(|h| h.mismatch()), "{harnesses:?}");
        }
    }
}
