//! The installed app keeps OpenCode's own view, in a real browser.
//!
//! Gate D's mobile row turns on one uncomfortable fact: the shell is a page on
//! tracon's origin and the view inside it is a page on the node's *other*
//! origin, so every cookie the view sets is **third-party** by the browser's
//! reckoning — and third-party cookies are blocked by default on the phones
//! this is for. Nothing about that is visible from a unit test, from a `curl`,
//! or from two loopback *ports*, which share a site and are therefore not a
//! cross-site pair at all. It needs a browser, a genuinely cross-site pair of
//! origins, and the browser's blocking turned on.
//!
//! So this file stands the two origins up on distinct loopback hosts —
//! `127.0.0.1` for the operator and `127.0.0.2` for the UI origin, different
//! sites because neither has a registrable domain and the host is therefore
//! the site — serves the real SPA and the real UI router, and drives Chromium
//! at 390×844 with `--test-third-party-cookie-phaseout`.
//!
//! ## What is real and what is not
//!
//! Real: the shell route and its component, the mint endpoint, the bootstrap
//! tracon splices into what it serves, `POST /boot`, the cookie, the origin
//! guard, the mediated gateway, and the browser.
//!
//! Stood in for: upstream's 34 MiB JavaScript bundle, which is not in git and
//! is not what is under test. The UI origin's `/` serves a two-line page with
//! the *real* [`ui::splice_bootstrap`] applied, whose "app module" makes one
//! authorised call through the gateway and writes the answer into the DOM. If
//! the cookie did not survive being third-party, that call is a 401 and the
//! page says so.
//!
//! ## The control
//!
//! A test that only shows the new cookie working would not show that the pair
//! is cross-site at all — a same-site pair would pass it too. So the same
//! browser flow is run twice against the same node, with the second run's
//! `Set-Cookie` downgraded *in this test's own middleware* to what #206 sent
//! (`SameSite=Strict`, no `Partitioned`). The first must authorise and the
//! second must be refused. Only the pair of results says anything.

#[path = "support/mod.rs"]
mod support;
use support::fake::FakeAdapter;
use support::fake_opencode::{serve as serve_fake, Fake, SESSION};
use support::state;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::{
    extract::Request,
    http::{header, HeaderValue},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use base64::Engine;
use serde_json::Value;

use tracon::{
    adapter::NativeApi,
    config::Config,
    http::{api::AppState, auth::AuthState, ui},
    session::Manager,
    store::Store,
    stream::Bus,
};

const TRACON_SESSION: &str = "s-ui";
const PASSWORD: &str = "the-node-minted-this";
/// The operator's own origin. `127.0.0.1` because `http::auth::guard` grants
/// the loopback exemption to exactly this host, and a browser at the machine
/// is the operator.
const OPERATOR_HOST: &str = "127.0.0.1";
/// The UI origin's host. Still loopback — so it is a *potentially trustworthy*
/// origin, which is what lets a `Secure` cookie be set over plain HTTP — but a
/// different **site** from `127.0.0.1`, which is what makes the frame
/// third-party and the test worth running.
const UI_HOST: &str = "127.0.0.2";

// ---------------------------------------------------------------------------
// The node, on two origins
// ---------------------------------------------------------------------------

struct Rig {
    operator_origin: String,
    ui_origin: String,
    /// Set to downgrade the UI cookie to what #206 sent, for the control run.
    downgrade: Arc<AtomicBool>,
    /// Every `Set-Cookie` the UI origin sent the browser, in order. The
    /// browser's own refusal of one is silent from the page's side, so what
    /// the node actually put on the wire has to be read here rather than
    /// inferred from what the page then failed to do.
    cookies_sent: Arc<Mutex<Vec<String>>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

async fn stand_up() -> Rig {
    state::isolate();
    let fake = Fake::new("1.18.30", usize::MAX);
    *fake.password.lock().unwrap() = PASSWORD.to_string();
    let endpoint = serve_fake(fake).await;

    // Bind first, configure second: the UI origin is part of the cookie's
    // audience and of the CSP, so both have to know the port before anything
    // is built.
    let operator_listener = tokio::net::TcpListener::bind(format!("{OPERATOR_HOST}:0"))
        .await
        .expect("a loopback port");
    let ui_listener = tokio::net::TcpListener::bind(format!("{UI_HOST}:0"))
        .await
        .expect("the second loopback host is bindable");
    let operator_origin = format!("http://{}", operator_listener.local_addr().unwrap());
    let ui_origin = format!("http://{}", ui_listener.local_addr().unwrap());

    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
    let mut cfg = Config::default();
    cfg.ui.opencode_url = Some(ui_origin.clone());
    let cfg = Arc::new(cfg);
    assert_eq!(cfg.ui.opencode_origin().unwrap(), ui_origin);
    assert!(
        cfg.ui.opencode_is_loopback(),
        "the second host must still be loopback, or the cookie loses `Secure` on plain HTTP"
    );

    let tools = Arc::new(tracon::mcp::Tools {
        broker: Arc::new(Default::default()),
        cfg: cfg.clone(),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::new(),
        session: Default::default(),
    });
    let manager = Manager::new(
        store.clone(),
        Bus::new(),
        cfg.clone(),
        "n1".into(),
        tools.clone(),
        Default::default(),
        Arc::new(tracon::runner::local::LocalBackend),
    );
    let app_state = AppState {
        manager: manager.clone(),
        cfg: cfg.clone(),
        adapter: Arc::new(FakeAdapter {
            tx: Arc::new(tokio::sync::Mutex::new(None)),
            tokens: Arc::new(tokio::sync::Mutex::new(0)),
        }),
        node_id: "n1".into(),
        tools,
        mesh: None,
        auth: Arc::new(AuthState::load(&store, OPERATOR_HOST.into())),
        enroll: Default::default(),
    };

    let mut row = support::rows::session_row(TRACON_SESSION, "n1", "personal");
    row.harness_id = "opencode".into();
    row.harness_session_id = Some(SESSION.into());
    store.insert_session(&row).unwrap();
    manager
        .register_native_api_for_test(
            TRACON_SESSION,
            NativeApi {
                base: format!("http://{endpoint}"),
                // The credential the gateway injects on the node. Never the
                // browser's: what reaches the harness is this, and what the
                // page holds is the cookie.
                authorization: format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD
                        .encode(format!("opencode:{PASSWORD}"))
                ),
                directory: "/work".into(),
                session_id: SESSION.into(),
            },
        )
        .await;

    let operator = tracon::http::router(app_state.clone()).layer(
        axum::middleware::from_fn_with_state(app_state.clone(), tracon::http::auth::guard),
    );

    let downgrade = Arc::new(AtomicBool::new(false));
    let cookies_sent: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let ui_state = ui::UiState::new(app_state.clone(), None, &operator_origin);
    // Test-only, on the test's own router rather than the node's: a way for
    // the browser to make something happen between the page loading and the
    // page being told it is visible again, so the resume path is asserted by
    // sequence rather than by sleeping and hoping.
    let control = Router::new()
        .route("/__end-session", post(end_session))
        .with_state(store.clone());
    let ui_app = Router::new()
        // The one stand-in: upstream's bundle is not in git, and the page it
        // would serve is not what this asserts about. What *is* real is the
        // bootstrap, taken from the node rather than copied.
        .route("/", get(probe_page))
        .route("/__shell-probe.js", get(probe_app))
        .merge(control)
        .fallback_service(ui::router(ui_state))
        .layer(axum::middleware::from_fn_with_state(
            (downgrade.clone(), cookies_sent.clone()),
            watch_cookie,
        ));

    let mut tasks = Vec::new();
    tasks.push(tokio::spawn(async move {
        let _ = axum::serve(
            operator_listener,
            operator.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    }));
    tasks.push(tokio::spawn(async move {
        let _ = axum::serve(
            ui_listener,
            ui_app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    }));

    Rig {
        operator_origin,
        ui_origin,
        downgrade,
        cookies_sent,
        tasks,
    }
}

/// End the session, as something on the node would while the phone slept.
/// Reached by the browser between the frame settling and the page being told
/// it is visible again, so "what the shell does on resume" is asserted in
/// order rather than against a clock.
async fn end_session(
    axum::extract::State(store): axum::extract::State<Arc<Store>>,
) -> &'static str {
    store
        .update_session(TRACON_SESSION, tracon::store::SessionPatch::state("closed"))
        .expect("the session ends");
    "ended"
}

/// The page the UI origin serves in place of the pinned bundle: upstream's
/// shape (one module script with a root-absolute src) with tracon's own
/// bootstrap spliced in by the node's own function.
async fn probe_page() -> Response {
    const SHELL: &str = concat!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>probe</title>",
        "<script type=\"module\" crossorigin src=\"/__shell-probe.js\"></script>",
        "</head><body><div id=\"probe\">booting</div></body></html>"
    );
    let page = ui::splice_bootstrap(SHELL).expect("the node's bootstrap splices into this shape");
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        page,
    )
        .into_response()
}

/// What runs once the bootstrap has spent the token: one authorised call
/// through the mediated gateway, and the answer written where the driver can
/// read it. This is the whole assertion — if the cookie did not survive being
/// third-party, the gateway never sees the call and this says `refused:401`.
async fn probe_app() -> Response {
    const APP: &str = r#"
const probe = document.getElementById("probe")
try {
  const r = await fetch("/global/health", { credentials: "same-origin" })
  probe.textContent = (r.ok ? "authorised:" : "refused:") + r.status
} catch (e) {
  probe.textContent = "threw:" + e
}
"#;
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        APP,
    )
        .into_response()
}

/// The control. With the flag set, the UI origin's cookie goes out as #206
/// sent it — `SameSite=Strict`, no `Partitioned`, no `Secure` on loopback —
/// which is what the browser is expected to refuse in a third-party frame.
/// Written here rather than behind a production switch: a node must not be
/// able to be configured into the broken shape.
type Watch = (Arc<AtomicBool>, Arc<Mutex<Vec<String>>>);

async fn watch_cookie(
    axum::extract::State((flag, sent)): axum::extract::State<Watch>,
    req: Request,
    next: Next,
) -> Response {
    let mut response = next.run(req).await;
    if !flag.load(Ordering::SeqCst) {
        for v in response.headers().get_all(header::SET_COOKIE) {
            if let Ok(v) = v.to_str() {
                sent.lock().unwrap().push(v.to_string());
            }
        }
        return response;
    }
    let rewritten: Vec<HeaderValue> = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(|v| {
            v.replace("; Secure; SameSite=None; Partitioned", "; SameSite=Strict")
                .replace("; SameSite=None; Partitioned", "; SameSite=Strict")
        })
        .filter_map(|v| HeaderValue::from_str(&v).ok())
        .collect();
    if !rewritten.is_empty() {
        response.headers_mut().remove(header::SET_COOKIE);
        for v in rewritten {
            if let Ok(s) = v.to_str() {
                sent.lock().unwrap().push(s.to_string());
            }
            response.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    response
}

// ---------------------------------------------------------------------------
// The browser
// ---------------------------------------------------------------------------

enum Run {
    Skipped(String),
    Saw(Value),
}

/// Drive the shell once and report what the browser saw.
async fn drive(rig: &Rig, resume: Option<&str>) -> Run {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the node crate sits in the workspace")
        .to_path_buf();
    let spa = repo.join("spa");
    let driver = spa.join("tests/opencode-shell-driver.mjs");
    if !spa.join("node_modules/playwright-core").is_dir() {
        return Run::Skipped(
            "the SPA's dev dependencies are not installed; run `bun install` in spa/".into(),
        );
    }
    // `node/build.rs` writes a placeholder page when nobody has built the SPA,
    // so the Rust job never needs the JS toolchain. Driving a browser over that
    // placeholder would fail for a reason that has nothing to do with the
    // shell, so say which it is.
    let index = std::fs::read_to_string(spa.join("dist/index.html")).unwrap_or_default();
    if !index.contains("/assets/") {
        return Run::Skipped("spa/dist is the placeholder page; run `just spa`".into());
    }
    let Some(bun) = which("bun") else {
        return Run::Skipped("bun is not on PATH, so the browser driver cannot run".into());
    };

    let config = serde_json::json!({
        "shell_url": format!("{}/sessions/{TRACON_SESSION}/opencode", rig.operator_origin),
        "operator_origin": rig.operator_origin,
        "ui_origin": rig.ui_origin,
        "scope": "/",
        "block_third_party": true,
        "settle_ms": 2500,
        "timeout_ms": 30000,
        "resume": resume.is_some(),
        "before_resume_url": resume.map(|p| format!("{}{p}", rig.ui_origin)),
    });

    let out = tokio::process::Command::new(bun)
        .arg(&driver)
        .arg(config.to_string())
        .current_dir(&spa)
        .output()
        .await
        .expect("the driver runs");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if let Some(why) = stdout.lines().find_map(|l| l.strip_prefix("SKIP ")) {
        return Run::Skipped(why.to_string());
    }
    let Some(line) = stdout.lines().find_map(|l| l.strip_prefix("RESULT ")) else {
        panic!("the driver printed no result\nstdout:\n{stdout}\nstderr:\n{stderr}");
    };
    Run::Saw(serde_json::from_str(line).expect("the driver prints JSON"))
}

fn which(binary: &str) -> Option<std::path::PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(binary))
        .find(|c| c.is_file())
}

/// Everything the browser is asked about the shell, in one run because
/// standing two origins and a browser up is expensive and the claims are one
/// claim: the installed app keeps the native view, and the view works.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_installed_app_keeps_the_native_view_with_third_party_cookies_blocked() {
    let rig = stand_up().await;

    let saw = match drive(&rig, None).await {
        Run::Skipped(why) => {
            eprintln!("skipped: {why}");
            return;
        }
        Run::Saw(v) => v,
    };
    let ctx = || format!("\n{}", serde_json::to_string_pretty(&saw).unwrap());

    // 1. The shell stayed in scope. Nothing opened a tab, nothing left the
    //    origin, and the address the installed app is sitting on is still a
    //    path the manifest claims.
    assert_eq!(
        saw["shell_url"].as_str().unwrap(),
        format!("{}/sessions/{TRACON_SESSION}/opencode", rig.operator_origin),
        "the shell navigated away{}",
        ctx()
    );
    assert_eq!(saw["in_scope"], true, "{}", ctx());
    assert_eq!(
        saw["shell_url_has_boot"],
        false,
        "the capability reached the shell's own address bar{}",
        ctx()
    );

    // 2. The view is framed, and it is the *other* origin. An iframe on the
    //    operator's own origin would be the isolation quietly gone.
    let frames = saw["frames"].as_array().unwrap();
    assert_eq!(
        frames.len(),
        1,
        "expected exactly one frame — if the shell rendered the session screen \
         instead, spa/dist is older than this branch; run `just spa`{}",
        ctx()
    );
    assert_eq!(frames[0]["origin"], rig.ui_origin, "{}", ctx());

    // 3. The frame's attributes are the ones the boundary depends on. No
    //    `sandbox` (it would make the frame's origin opaque, which this origin
    //    refuses outright), nothing delegated by `allow`, and no referrer.
    let attrs = &saw["frame_attrs"];
    assert!(attrs["sandbox"].is_null(), "{}", ctx());
    assert_eq!(attrs["allow"], "", "{}", ctx());
    assert_eq!(attrs["referrerpolicy"], "no-referrer", "{}", ctx());
    assert_eq!(attrs["src_has_fragment"], true, "{}", ctx());

    // 4. The point of the run. With third-party cookies blocked, the frame
    //    spent its token, the node's partitioned cookie was stored, and the
    //    call it made afterwards was authorised by it. What the node put on
    //    the wire is read from the wire, because a cookie the browser silently
    //    refuses is indistinguishable, from the page, from one never sent.
    let sent = rig.cookies_sent.lock().unwrap().clone();
    let ui_cookie = sent
        .iter()
        .find(|c| c.starts_with("tracon_opencode_ui="))
        .unwrap_or_else(|| panic!("the node set no UI cookie; it sent {sent:?}{}", ctx()));
    assert!(
        ui_cookie.contains("; Partitioned") && ui_cookie.contains("; SameSite=None"),
        "the node did not partition the framed cookie: {ui_cookie}"
    );

    let requests = saw["ui_requests"].as_array().unwrap();
    let boot = requests
        .iter()
        .find(|r| r["path"] == "/boot")
        .unwrap_or_else(|| panic!("the frame never reached /boot{}", ctx()));
    assert_eq!(boot["status"], 200, "{}", ctx());
    let health = requests
        .iter()
        .find(|r| r["path"] == "/global/health")
        .unwrap_or_else(|| panic!("the frame made no authorised call{}", ctx()));
    assert_eq!(
        health["status"],
        200,
        "the partitioned cookie was not sent back{}",
        ctx()
    );
    assert_eq!(
        saw["frame_text"].as_str().map(str::trim),
        Some("authorised:200"),
        "{}",
        ctx()
    );

    // And it is not in the ordinary jar. A partitioned cookie is not listed
    // among the origin's unpartitioned cookies, so its *absence* here next to
    // a 200 above is the shape CHIPS is meant to have: usable inside tracon's
    // own page, invisible everywhere else.
    assert!(
        saw["ui_cookies"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["name"] != "tracon_opencode_ui"),
        "the UI cookie is in the origin's unpartitioned jar{}",
        ctx()
    );

    // 5. A phone screen that scrolls sideways is a broken screen.
    let overflow = &saw["overflow"];
    assert_eq!(
        overflow["scroll_width"],
        overflow["client_width"],
        "the shell overflows horizontally at 390px{}",
        ctx()
    );

    // 6. Nothing threw, and nothing on the page wrote the capability anywhere.
    assert_eq!(saw["page_errors"].as_array().unwrap().len(), 0, "{}", ctx());
    for line in saw["console"].as_array().unwrap() {
        let line = line.as_str().unwrap_or_default();
        assert!(
            !line.contains("boot="),
            "the capability was logged to the console: {line}"
        );
    }
}

/// The control: the same browser, the same pair of origins, and the cookie
/// #206 sent. It must fail — otherwise the run above proves nothing, because
/// a same-site pair would pass it too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cookie_the_desktop_window_uses_is_refused_in_the_shells_frame() {
    let rig = stand_up().await;
    rig.downgrade.store(true, Ordering::SeqCst);

    let saw = match drive(&rig, None).await {
        Run::Skipped(why) => {
            eprintln!("skipped: {why}");
            return;
        }
        Run::Saw(v) => v,
    };
    let ctx = || format!("\n{}", serde_json::to_string_pretty(&saw).unwrap());

    // The exchange still succeeds — the node answered, and the token was
    // spent. What fails is everything after it: the browser refused to store a
    // `SameSite=Strict` cookie set in a third-party frame, so the frame's next
    // call arrives with no capability at all.
    let requests = saw["ui_requests"].as_array().unwrap();
    assert!(
        requests.iter().any(|r| r["path"] == "/boot"),
        "the frame never reached /boot{}",
        ctx()
    );
    let health = requests
        .iter()
        .find(|r| r["path"] == "/global/health")
        .unwrap_or_else(|| panic!("the frame made no call to fail{}", ctx()));
    assert_eq!(
        health["status"],
        401,
        "a SameSite=Strict cookie was accepted in a third-party frame, so this pair of \
         origins is not cross-site and the partitioned run proves nothing{}",
        ctx()
    );
    assert_eq!(
        saw["frame_text"].as_str().map(str::trim),
        Some("refused:401"),
        "{}",
        ctx()
    );
}

/// Background and resume: an installed app is put away far more often than it
/// is closed, and what it shows when it comes back must be the truth rather
/// than whatever was on screen when the phone locked.
///
/// The session is ended from outside the page — the browser asks the node to
/// do it, so this is an ordering rather than a race — and only then is the
/// page told it is visible again. The shell must notice, say so, and offer the
/// way out rather than a reconnect that cannot work.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_that_ended_while_the_app_slept_is_noticed_on_resume() {
    let rig = stand_up().await;

    let saw = match drive(&rig, Some("/__end-session")).await {
        Run::Skipped(why) => {
            eprintln!("skipped: {why}");
            return;
        }
        Run::Saw(v) => v,
    };
    let ctx = || format!("\n{}", serde_json::to_string_pretty(&saw).unwrap());

    assert_eq!(saw["before_resume_status"], 200, "{}", ctx());
    // The frame is gone, replaced by a sentence about why.
    assert_eq!(saw["frame_still_there"], false, "{}", ctx());
    let said = saw["after_resume_text"].as_str().unwrap_or_default();
    assert!(
        said.contains("while the app was away"),
        "the shell said nothing about the session ending: {said}{}",
        ctx()
    );
    // A session that ended is not something a fresh capability fixes, so the
    // offer is the way back rather than a reconnect.
    assert_eq!(saw["reconnect_visible"], false, "{}", ctx());
    assert_eq!(saw["back_link_visible"], true, "{}", ctx());
    // And it is still the shell route: noticing is not navigating.
    assert_eq!(
        saw["after_resume_url"].as_str().unwrap(),
        format!("{}/sessions/{TRACON_SESSION}/opencode", rig.operator_origin),
        "{}",
        ctx()
    );
}

/// Before the browser is asked anything: the framed exchange is a working
/// capability on the wire. Minted, spent with `framed: true`, and the cookie
/// that comes back authorises a mediated call — so a refusal in the browser is
/// the browser's, and this test says which.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_framed_exchange_is_a_working_capability_on_the_wire() {
    let rig = stand_up().await;
    let http = reqwest::Client::builder().no_proxy().build().unwrap();

    let minted: Value = http
        .post(format!(
            "{}/api/sessions/{TRACON_SESSION}/opencode-boot",
            rig.operator_origin
        ))
        .header("origin", &rig.operator_origin)
        .send()
        .await
        .expect("the node answers")
        .json()
        .await
        .expect("JSON");
    let url = minted["url"].as_str().unwrap_or_else(|| panic!("{minted}"));
    let token = url
        .split_once('#')
        .and_then(|(_, f)| f.split_once('?'))
        .and_then(|(_, q)| q.split('&').find_map(|kv| kv.strip_prefix("boot=")))
        .expect("a capability in the fragment")
        .to_string();
    assert_eq!(minted["cookie_ttl_ms"], tracon::http::ui::COOKIE_TTL_MS);

    let booted = http
        .post(format!("{}/boot", rig.ui_origin))
        .header("origin", &rig.ui_origin)
        .json(&serde_json::json!({ "boot": token, "framed": true }))
        .send()
        .await
        .expect("the UI origin answers");
    assert_eq!(booted.status(), 200);
    let set = booted
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("tracon_opencode_ui="))
        .expect("the exchange sets the UI cookie")
        .to_string();
    assert!(set.contains("; Partitioned"), "{set}");
    assert!(set.contains("; SameSite=None"), "{set}");
    assert!(set.contains("; Secure"), "{set}");
    let value = set
        .split(';')
        .next()
        .and_then(|kv| kv.strip_prefix("tracon_opencode_ui="))
        .unwrap()
        .to_string();

    let call = http
        .get(format!("{}/global/health", rig.ui_origin))
        .header("cookie", format!("tracon_opencode_ui={value}"))
        .send()
        .await
        .expect("the UI origin answers");
    assert_eq!(
        call.status(),
        200,
        "the framed cookie is not a working capability: {}",
        call.text().await.unwrap_or_default()
    );
}
