//! `tracon setup` and `tracon check-boundary` act on the node's own boundary,
//! so they refuse a `node.toml` that is there but does not parse rather than
//! act on the defaults: on those, setup would retire a gateway the file keeps
//! (`sandbox_tunnels = false`).

#[path = "support/mod.rs"]
mod support;

use support::state;

#[tokio::test]
async fn setup_and_check_boundary_refuse_a_node_toml_that_does_not_parse() {
    let dir = state::scratch("boundary_config_refused");
    std::fs::write(
        dir.join("node.toml"),
        "[runtime.podman\nsandbox_tunnels = false\n",
    )
    .unwrap();
    // No podman to find: had either command gone on to the boundary, it would
    // fail on that instead, with another message.
    let empty = state::scratch("boundary_config_refused_path");
    for args in [&["setup"][..], &["check-boundary", "--deep"][..]] {
        let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_tracon"))
            .args(args)
            .env("TRACON_STATE_DIR", &dir)
            .env("TRACON_CONFIG_DIR", &dir)
            .env("PATH", &empty)
            .output()
            .await
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?}: {stderr}");
        assert!(
            stderr.contains("node.toml does not load, so the boundary is left as it is"),
            "{args:?}: {stderr}"
        );
        assert!(stderr.contains("runtime.podman"), "{args:?}: {stderr}");
    }
}
