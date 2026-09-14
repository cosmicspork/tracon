//! The native UI's route trace: every request upstream's own JavaScript makes
//! on tracon's origin, and what answered it.
//!
//! Gate D's last row asks two things of the native interface, and they are one
//! question asked from both ends. *Is every route the app actually uses either
//! declared by `http::ui`'s app-route table or classified by the gateway's
//! matrix?* And *does a route nobody declared fail closed?* Reading the bundle
//! cannot answer either: what the app calls is a property of the built
//! JavaScript, and the only honest way to find out is to run it.
//!
//! So there are two halves here, and only one of them needs a browser.
//!
//! **The trace** (`docs/reference/opencode-v1.18.30/ui-route-trace.tsv`) is
//! captured by driving the real bundle, on the real origin, in front of the
//! pinned binary, with headless Chromium over CDP — a scripted tour that loads
//! a session page, sends a prompt, answers a permission the model's tool call
//! raised, opens the changes view, switches model, opens settings, tries to
//! share, tries to fork, and opens a second session's URL. Every request the
//! browser made is recorded with its method, its path, the status it got, and
//! the class that answered.
//!
//! **The check** re-derives that class for every recorded row from the same
//! two functions the router and the gateway use — `http::ui::trace_case` and
//! `gateway::opencode::trace_class` — with no browser, no harness and no
//! bundle on the machine. That is what runs in CI. A row the matrix cannot
//! place is a failure; a row on the deny list that was not a 403 with a
//! `gateway_refused` behind it is a failure. So the trace is not a document
//! that describes the code, it is a document the code is checked against, and
//! a route table edited without the trace agreeing fails here.
//!
//! Nothing in the check is allowed to widen anything. When the tour finds a
//! route that is neither declared nor classified, the fix is a row in the
//! app-route table or a row in the matrix — never a catch-all.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::collections::BTreeMap;
use std::path::PathBuf;

use tracon::{
    gateway::opencode::{trace as gw, trace_class, trace_class_refuses},
    http::ui,
};

/// Where the captured trace lives. Checked in, reviewed, and re-derived here.
const TRACE: &str = "docs/reference/opencode-v1.18.30/ui-route-trace.tsv";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the node crate has a parent")
        .to_path_buf()
}

// ---------------------------------------------------------------------------
// The trace file
// ---------------------------------------------------------------------------

/// One line of the trace: a request shape the tour saw, and what answered it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Row {
    method: String,
    /// Path only — no query. Opaque ids are rewritten to stable placeholders
    /// (see [`Trace::PLACEHOLDERS`]) so the checked-in file is the same file
    /// from one capture to the next and a diff means something changed.
    path: String,
    status: u16,
    class: String,
    count: usize,
    /// The tour steps this shape appeared in, comma-separated.
    steps: String,
    /// `gateway_refused` when the node recorded one for this call, `-` when
    /// there was nothing to record.
    evidence: String,
}

#[derive(Debug, Default)]
struct Trace {
    headers: BTreeMap<String, String>,
    rows: Vec<Row>,
}

impl Trace {
    /// What the tour rewrites before recording a path, so two captures of the
    /// same tour produce the same file. Every one of these is an opaque
    /// segment the matrix matches with `{session}` or `*`, so rewriting one
    /// cannot change what the classifier makes of the row.
    const PLACEHOLDERS: &'static [(&'static str, &'static str)] = &[
        ("ses_", "ses_x"),
        ("per_", "per_x"),
        ("msg_", "msg_x"),
        ("prt_", "prt_x"),
        ("pty_", "pty_x"),
        ("qst_", "qst_x"),
        ("tool_", "tool_x"),
    ];

    fn parse(text: &str) -> Self {
        let mut trace = Trace::default();
        for line in text.lines() {
            let line = line.trim_end();
            if let Some(rest) = line.strip_prefix("# ") {
                if let Some((key, value)) = rest.split_once('\t') {
                    trace
                        .headers
                        .insert(key.trim().to_string(), value.trim().to_string());
                }
                continue;
            }
            if line.is_empty() || line.starts_with('#') || line.starts_with("method\t") {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            assert!(
                fields.len() == 7,
                "{TRACE}: a row has {} fields, not 7: {line}",
                fields.len()
            );
            trace.rows.push(Row {
                method: fields[0].into(),
                path: fields[1].into(),
                status: fields[2].parse().unwrap_or_else(|_| {
                    panic!("{TRACE}: {} is not a status", fields[2]);
                }),
                class: fields[3].into(),
                count: fields[4].parse().unwrap_or(1),
                steps: fields[5].into(),
                evidence: fields[6].into(),
            });
        }
        trace
    }

    fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(
            "# OpenCode's native interface, traced on tracon's UI origin.\n\
             #\n\
             # Captured by `node/tests/opencode_route_trace.rs` with TRACON_UI_TRACE=1: the\n\
             # pinned bundle on the UI origin, the pinned binary behind the mediated gateway,\n\
             # a fake upstream model behind the model gateway, and headless Chromium over CDP\n\
             # driving a scripted tour. Every row is a request the browser made; `class` is\n\
             # what answered it, and `node/tests/opencode_route_trace.rs` re-derives that\n\
             # class from `http::ui::trace_case` and `gateway::opencode::trace_class` in CI.\n\
             #\n",
        );
        for (key, value) in &self.headers {
            out.push_str(&format!("# {key}\t{value}\n"));
        }
        out.push_str(
            "#\n\
             # A step named `-probe` is a same-origin `fetch` the tour made from the loaded\n\
             # page — same cookie, same origin, same router — rather than a click; it is how\n\
             # the deny list and the views whose controls a headless DOM cannot reliably find\n\
             # are asked about. Every other step is the app's own traffic.\n\
             #\n\
             # class:  page      the app shell, at `/` or a route the app's router declares\n\
             #         asset     a file in the pinned bundle\n\
             #         boot      the origin's own bootstrap exchange\n\
             #         readable  forwarded by the gateway, directory pinned, credential injected\n\
             #         stream    the same, proxied as a stream (the one durably replayable one)\n\
             #         synthesised  served by the node from what it already reads, never forwarded\n\
             #         mediated  decided by tracon before anything reached the harness\n\
             #         unavailable  on the matrix, and refused until tracon owns what it makes\n\
             #         forbidden the deny list, refused by name\n\
             #         not-found nobody's: asked for on purpose, and 404 with no catch-all\n\
             #\n",
        );
        out.push_str("method\tpath\tstatus\tclass\tcount\tsteps\tevidence\n");
        for row in &self.rows {
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                row.method, row.path, row.status, row.class, row.count, row.steps, row.evidence
            ));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// The check: what runs in CI
// ---------------------------------------------------------------------------

/// Re-derive every recorded row's class, and refuse the trace if anything in
/// it is unplaced.
///
/// This is the row's actual claim, made without a browser. Each recorded class
/// is computed again from the two functions the running code uses, so a matrix
/// row deleted or an app route dropped fails here rather than in a browser
/// nobody is watching.
#[test]
fn every_route_the_native_ui_used_is_declared_or_classified() {
    state::isolate();
    let path = repo_root().join(TRACE);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let trace = Trace::parse(&text);
    assert!(!trace.rows.is_empty(), "{TRACE} records no requests");

    // The trace is of one bundle and one binary, and says which.
    assert_eq!(
        trace.headers.get("bundle-digest").map(String::as_str),
        Some(ui::PINNED_DIGEST.trim()),
        "{TRACE} was captured against another bundle"
    );
    assert_eq!(
        trace.headers.get("binary-version").map(String::as_str),
        Some(ui::PINNED_VERSION),
        "{TRACE} was captured against another binary"
    );

    let mut unplaced = Vec::new();
    for row in &trace.rows {
        assert_ne!(
            row.class,
            gw::UNKNOWN,
            "{TRACE}: {} {} was answered by nobody the code can name — declare it in \
             the app-route table or classify it in the matrix, never widen the origin",
            row.method,
            row.path
        );
        // What `dispatch` would make of this request on a machine with no
        // vendored tree: every case but `asset` is decided without one, and an
        // `asset` is exactly the case nothing else claims.
        let case = ui::trace_case(None, &row.method, &row.path);
        match row.class.as_str() {
            ui::trace::ASSET => {
                assert_eq!(
                    case,
                    ui::trace::NONE,
                    "{} {} is recorded as a bundle file but {case} claims it",
                    row.method,
                    row.path
                );
                assert_eq!(
                    row.status, 200,
                    "{} {} is recorded as a bundle file that was not served",
                    row.method, row.path
                );
            }
            // The deliberate 404s: a path nobody owns, asked for on purpose.
            // Recorded rather than dropped, because "there is no catch-all" is
            // a claim that needs the attempt in evidence — and held to a 404,
            // so a future catch-all would fail here.
            ui::trace::NONE => {
                assert_eq!(
                    case,
                    ui::trace::NONE,
                    "{} {} is recorded as nobody's but {case} claims it",
                    row.method,
                    row.path
                );
                assert_eq!(
                    row.status, 404,
                    "{} {} is nobody's route and was answered {}; the one thing this \
                     origin must never become is a tunnel to the harness's catch-all",
                    row.method, row.path, row.status
                );
            }
            class @ (ui::trace::PAGE | ui::trace::BOOT) => assert_eq!(
                case, class,
                "{} {} is recorded as {class} but the router makes it {case}",
                row.method, row.path
            ),
            class => {
                assert_eq!(
                    case,
                    ui::trace::API,
                    "{} {} is recorded as {class} but the router makes it {case}",
                    row.method,
                    row.path
                );
                let derived = trace_class(&row.method, &row.path);
                if derived != class {
                    unplaced.push(format!(
                        "{} {} recorded as {class}, the matrix says {derived}",
                        row.method, row.path
                    ));
                }
            }
        }
    }
    assert!(
        unplaced.is_empty(),
        "the matrix and the trace disagree:\n  {}",
        unplaced.join("\n  ")
    );
}

/// A route on the deny list that the tour touched was refused, and the refusal
/// is on the session's own record.
///
/// The three the manifest names as the ones that matter for a browser: sharing
/// publishes the transcript off the node, a fork is a session nothing
/// supervises, and a config write installs plugins and MCP servers into the
/// runner. The tour asks for all three from the loaded page, same-origin, with
/// the app's own cookie — which is the strongest form of the question.
#[test]
fn the_deny_list_the_tour_touched_failed_closed() {
    state::isolate();
    let text = std::fs::read_to_string(repo_root().join(TRACE)).expect("the trace is checked in");
    let trace = Trace::parse(&text);

    for row in &trace.rows {
        if !trace_class_refuses(&row.class) {
            continue;
        }
        assert_eq!(
            row.status, 403,
            "{} {} is classified {} but answered {}",
            row.method, row.path, row.class, row.status
        );
        assert_eq!(
            row.evidence, "gateway_refused",
            "{} {} was refused with nothing on the session's record; a refusal the \
             operator cannot see is indistinguishable from a request never made",
            row.method, row.path
        );
    }

    let touched = |needle: &str, method: &str| {
        trace
            .rows
            .iter()
            .find(|row| row.method == method && row.path.contains(needle))
            .unwrap_or_else(|| {
                panic!(
                    "the tour did not try {method} …{needle}…; the trace proves nothing about it"
                )
            })
    };
    for (needle, method) in [("/share", "POST"), ("/fork", "POST"), ("/config", "PATCH")] {
        let row = touched(needle, method);
        assert!(
            trace_class_refuses(&row.class),
            "{method} {} is on the deny list but the trace calls it {}",
            row.path,
            row.class
        );
        assert_eq!(row.status, 403, "{method} {}", row.path);
    }
}

/// Every class the trace uses is one of the names the code defines. A typo in
/// a hand-edited row would otherwise sail past the checks above by matching
/// nothing.
#[test]
fn the_trace_uses_only_the_names_the_code_defines() {
    state::isolate();
    let text = std::fs::read_to_string(repo_root().join(TRACE)).expect("the trace is checked in");
    for row in Trace::parse(&text).rows {
        assert!(
            matches!(
                row.class.as_str(),
                ui::trace::ASSET
                    | ui::trace::PAGE
                    | ui::trace::BOOT
                    | ui::trace::NONE
                    | gw::READABLE
                    | gw::STREAM
                    | gw::SYNTHESISED
                    | gw::MEDIATED
                    | gw::UNAVAILABLE
                    | gw::FORBIDDEN
            ),
            "{} is not a class this code produces ({} {})",
            row.class,
            row.method,
            row.path
        );
        assert!(row.path.starts_with('/'), "{} is not a path", row.path);
        assert!(!row.path.contains('?'), "{} carries a query", row.path);
    }
}

// ---------------------------------------------------------------------------
// The tour: what captures the trace
// ---------------------------------------------------------------------------

#[path = "opencode_route_trace/tour.rs"]
mod tour;

/// Drive the real bundle through the real origin in a real browser and rewrite
/// the trace.
///
/// Gated three ways — `TRACON_UI_TRACE=1`, the vendored bundle, the pinned
/// binary and a Chromium — because every one of them is a thing a machine may
/// not have and none of them can be faked into an equivalent. An ordinary
/// `cargo test` skips this and checks the captured file instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn the_native_ui_tour_captures_the_trace() {
    if std::env::var_os("TRACON_UI_TRACE").is_none() {
        eprintln!(
            "skipped: set TRACON_UI_TRACE=1 to drive the bundle in a browser and rewrite {TRACE}"
        );
        return;
    }
    tour::run(&repo_root().join(TRACE)).await;
}
