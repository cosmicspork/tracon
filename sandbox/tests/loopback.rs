//! The protocol end to end: the real binary, dialling a node-side harness
//! over loopback.

mod support;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};
use tracon_sandbox::files::Limits;
use tracon_sandbox::node::{Endpoint, SandboxError, ServerOptions};
use tracon_sandbox::protocol::{
    Backoff, Event, Exclusions, ListenAddr, LogStream, Probe, Readiness, RestartPolicy,
    ServiceSpec, ServiceState, SpawnSpec, WinSize,
};

use support::{start, start_with, Carrier};

fn sh(script: &str) -> SpawnSpec {
    SpawnSpec::new(["sh", "-c", script])
}

#[tokio::test]
async fn spawn_with_cwd_env_and_uid() {
    let sandbox = start().await;
    let dir = support::scratch();
    let mut spec = sh(r#"echo "$PWD|$FOO|$(id -u)"; env | grep -c ^TRACON_SANDBOX_ || true"#);
    spec.cwd = Some(dir.path().to_path_buf());
    spec.env = BTreeMap::from([("FOO".into(), "bar".into())]);
    let uid = unsafe { libc::getuid() };
    spec.uid = Some(uid);
    let output = sandbox
        .conn
        .spawn(spec)
        .await
        .unwrap()
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    let canonical = dir.path().canonicalize().unwrap();
    assert_eq!(
        text,
        format!("{}|bar|{uid}\n0\n", canonical.display()),
        "no bootstrap variable reaches a child"
    );

    let status = sandbox
        .conn
        .spawn(sh("echo oops >&2; exit 3"))
        .await
        .unwrap()
        .output()
        .await
        .unwrap();
    assert_eq!(status.status.code, Some(3));
    assert_eq!(status.stderr, b"oops\n");

    let missing = sandbox
        .conn
        .spawn(SpawnSpec::new(["/definitely/not/here"]))
        .await;
    assert!(
        matches!(missing, Err(SandboxError::Remote { .. })),
        "{:?}",
        missing.err()
    );
    assert!(sandbox.finish().await.success());
}

#[tokio::test]
async fn signals_reach_the_process_and_its_group() {
    let sandbox = start().await;
    let mut spec = SpawnSpec::new(["sleep", "30"]);
    spec.stdout = false;
    spec.stderr = false;
    spec.stdin = false;
    let process = sandbox.conn.spawn(spec.clone()).await.unwrap();
    process.signal(libc::SIGTERM).await.unwrap();
    let status = process.wait().await.unwrap();
    assert_eq!((status.code, status.signal), (None, Some(libc::SIGTERM)));
    // Gone and forgotten: a second signal names no process.
    assert!(process.signal(libc::SIGTERM).await.is_err());

    let mut group = sh("sleep 30 & sleep 30; wait");
    group.stdout = false;
    group.stderr = false;
    group.stdin = false;
    let process = sandbox.conn.spawn(group).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    process.signal_group(libc::SIGKILL).await.unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), process.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.signal, Some(libc::SIGKILL));
    assert!(sandbox.finish().await.success());
}

#[tokio::test]
async fn stdout_backpressure_holds_the_writer() {
    let sandbox = start().await;
    const TOTAL: usize = 64 * 1024 * 1024;
    let mut spec = SpawnSpec::new(["head", "-c", &TOTAL.to_string(), "/dev/zero"]);
    spec.stdin = false;
    spec.stderr = false;
    let process = sandbox.conn.spawn(spec).await.unwrap();
    let mut stdout = process.stdout().await.unwrap();
    // Nobody reads: the windows and the pipe fill and `head` blocks, so it
    // cannot have finished writing 64 MiB.
    let early = tokio::time::timeout(Duration::from_millis(1500), process.wait()).await;
    assert!(early.is_err(), "the process ran ahead of its reader");
    let mut total = 0usize;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = stdout.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        assert!(buf[..n].iter().all(|b| *b == 0));
        total += n;
    }
    assert_eq!(total, TOTAL);
    assert!(process.wait().await.unwrap().success());
    assert!(sandbox.finish().await.success());
}

#[tokio::test]
async fn stdin_round_trips_through_cat() {
    let sandbox = start().await;
    let mut spec = SpawnSpec::new(["cat"]);
    spec.stderr = false;
    let process = sandbox.conn.spawn(spec).await.unwrap();
    let mut stdin = process.stdin().await.unwrap();
    let mut stdout = process.stdout().await.unwrap();
    let data: Vec<u8> = (0..8 * 1024 * 1024u32)
        .map(|i| (i * 7 % 251) as u8)
        .collect();
    let expected = Sha256::digest(&data);
    let writer = tokio::spawn(async move {
        stdin.write_all(&data).await.unwrap();
        stdin.shutdown().await.unwrap();
    });
    let mut echoed = Vec::new();
    stdout.read_to_end(&mut echoed).await.unwrap();
    writer.await.unwrap();
    assert_eq!(Sha256::digest(&echoed), expected);
    assert!(process.wait().await.unwrap().success());
    assert!(sandbox.finish().await.success());
}

#[tokio::test]
async fn a_terminal_reports_its_size_and_resizes() {
    let sandbox = start().await;
    let mut spec = sh("stty size; read line; stty size");
    spec.tty = Some(WinSize { rows: 24, cols: 80 });
    let process = sandbox.conn.spawn(spec).await.unwrap();
    let stdio = process.stdio().await.unwrap();
    assert!(stdio.stderr.is_none(), "a terminal merges stderr");
    let mut stdin = stdio.stdin.unwrap();
    let mut lines = tokio::io::BufReader::new(stdio.stdout.unwrap()).lines();
    assert_eq!(lines.next_line().await.unwrap().unwrap().trim(), "24 80");
    process
        .resize(WinSize {
            rows: 40,
            cols: 100,
        })
        .await
        .unwrap();
    stdin.write_all(b"go\n").await.unwrap();
    let mut seen = Vec::new();
    while let Ok(Some(line)) = lines.next_line().await {
        seen.push(line.trim().to_string());
        if seen.last().map(String::as_str) == Some("40 100") {
            break;
        }
    }
    assert_eq!(seen.last().map(String::as_str), Some("40 100"), "{seen:?}");
    assert!(process.wait().await.unwrap().success());
    assert!(sandbox.finish().await.success());
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Every path under `root` with its contents (or link target), for comparing
/// trees.
fn listing(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.file_type().is_symlink() {
                out.insert(
                    rel,
                    format!("-> {}", std::fs::read_link(&path).unwrap().display()),
                );
            } else if meta.is_dir() {
                out.insert(rel.clone(), "dir".into());
                walk(root, &path, out);
            } else {
                out.insert(
                    rel,
                    hex::encode(Sha256::digest(std::fs::read(&path).unwrap())),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[tokio::test]
async fn tar_round_trip_applies_checkout_exclusions() {
    let sandbox = start().await;
    let work = support::scratch();
    let repo = work.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join(".gitignore"), "*.log\nnode_modules/\n.env\n").unwrap();
    std::fs::write(repo.join("tracked.txt"), "t").unwrap();
    std::fs::write(repo.join("forced.log"), "f").unwrap();
    git(&repo, &["add", ".gitignore", "tracked.txt"]);
    git(&repo, &["add", "-f", "forced.log"]);
    git(&repo, &["commit", "-qm", "init"]);
    std::fs::write(repo.join("untracked.txt"), "u").unwrap();
    std::fs::write(repo.join("debug.log"), "noise").unwrap();
    std::fs::write(repo.join(".env"), "SECRET=1").unwrap();
    std::fs::create_dir_all(repo.join("node_modules/x")).unwrap();
    std::fs::write(repo.join("node_modules/x/index.js"), "").unwrap();
    std::os::unix::fs::symlink("tracked.txt", repo.join("alias")).unwrap();

    let snapshot = work.path().join("snapshot");
    let summary = sandbox
        .conn
        .export_tar(&repo, Exclusions::Checkout, &snapshot, Limits::default())
        .await
        .unwrap();
    assert!(summary.files > 0);
    let mut kept: Vec<String> = listing(&snapshot)
        .into_iter()
        .filter(|(p, _)| p != ".git" && !p.starts_with(".git/"))
        .map(|(p, _)| p)
        .collect();
    kept.sort();
    let mut expected: Vec<String> = git(&repo, &["ls-files", "-co", "--exclude-standard"])
        .lines()
        .map(str::to_string)
        .collect();
    expected.sort();
    assert_eq!(kept, expected);
    assert!(
        snapshot.join(".git/HEAD").is_file(),
        "the repository travels"
    );
    assert_eq!(
        git(&snapshot, &["status", "--porcelain"]),
        "?? alias\n?? untracked.txt\n"
    );

    // Back in, then out whole: the same tree.
    let inside = work.path().join("inside");
    sandbox
        .conn
        .import_dir(&snapshot, &inside, Exclusions::None)
        .await
        .unwrap();
    let again = work.path().join("again");
    sandbox
        .conn
        .export_tar(&inside, Exclusions::None, &again, Limits::default())
        .await
        .unwrap();
    assert_eq!(listing(&again), listing(&snapshot));

    // An export over the limits is refused, and leaves no tree behind.
    let refused = work.path().join("refused");
    let small = Limits {
        max_files: 2,
        ..Limits::default()
    };
    let result = sandbox
        .conn
        .export_tar(&repo, Exclusions::None, &refused, small)
        .await;
    assert!(matches!(result, Err(SandboxError::Files(_))), "{result:?}");
    assert!(!refused.exists());

    assert!(sandbox
        .conn
        .export_tar(
            work.path().join("nope"),
            Exclusions::None,
            &refused,
            Limits::default()
        )
        .await
        .is_err());
    assert!(sandbox.finish().await.success());
}

#[tokio::test]
async fn the_node_refuses_a_hostile_export_and_the_sandbox_a_hostile_import() {
    let sandbox = start().await;
    let work = support::scratch();
    let tree = work.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    std::fs::write(tree.join("ok"), "fine").unwrap();
    std::os::unix::fs::symlink("/etc/passwd", tree.join("escape")).unwrap();
    let dest = work.path().join("dest");
    let result = sandbox
        .conn
        .export_tar(&tree, Exclusions::None, &dest, Limits::default())
        .await;
    assert!(
        matches!(
            result,
            Err(SandboxError::Files(
                tracon_sandbox::files::FilesError::Unsafe(_)
            ))
        ),
        "{result:?}"
    );
    assert!(!dest.exists());
    let leftovers: Vec<_> = std::fs::read_dir(work.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        leftovers,
        vec![std::ffi::OsString::from("tree")],
        "no staging left"
    );

    let mut header = tar::Header::new_gnu();
    header.as_gnu_mut().unwrap().name[..10].copy_from_slice(b"../planted");
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(0o644);
    header.set_size(1);
    header.set_cksum();
    let mut builder = tar::Builder::new(Vec::new());
    builder.append(&header, &b"x"[..]).unwrap();
    let hostile = builder.into_inner().unwrap();
    let target = work.path().join("import");
    let result = sandbox.conn.import_tar(&target, &hostile[..], None).await;
    assert!(
        matches!(result, Err(SandboxError::Remote { .. })),
        "{result:?}"
    );
    assert!(!work.path().join("planted").exists());
    assert!(sandbox.finish().await.success());
}

/// What a preparation leaves: a cache whose absolute links name places in
/// itself (bun's), and a virtualenv whose interpreter is the image's.
#[tokio::test]
async fn a_prepared_trees_links_cross_and_are_never_followed() {
    let sandbox = start().await;
    let work = support::scratch();
    let prepared = Limits {
        inert_links: true,
        ..Limits::default()
    };
    let refused = |result: &Result<_, SandboxError>| {
        matches!(
            result,
            Err(SandboxError::Files(
                tracon_sandbox::files::FilesError::Unsafe(_)
            ))
        )
    };

    // An absolute link inside the tree, read against where it is mounted,
    // is one inside, for a checkout too.
    let cache = work.path().join("cache");
    std::fs::create_dir_all(cache.join("bun/x@1@@@1")).unwrap();
    std::fs::create_dir_all(cache.join("bun/x")).unwrap();
    std::os::unix::fs::symlink(cache.join("bun/x@1@@@1"), cache.join("bun/x/1@@@1")).unwrap();
    let back = work.path().join("cache-back");
    sandbox
        .conn
        .export_tar(&cache, Exclusions::None, &back, Limits::default())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_link(back.join("bun/x/1@@@1")).unwrap(),
        cache.join("bun/x@1@@@1")
    );
    // Sent in elsewhere, the same link names a place outside it.
    let elsewhere = work.path().join("elsewhere");
    let result = sandbox
        .conn
        .import_dir(&back, &elsewhere, Exclusions::None)
        .await;
    assert!(
        matches!(result, Err(SandboxError::Remote { .. })),
        "{result:?}"
    );
    // Back where it was mounted, it is inside again.
    std::fs::remove_dir_all(&cache).unwrap();
    sandbox
        .conn
        .import_dir(&back, &cache, Exclusions::None)
        .await
        .unwrap();
    assert!(cache.join("bun/x/1@@@1").is_dir());

    // A link that leaves crosses as a link in a prepared tree, either way,
    // and refuses a checkout.
    let venv = work.path().join("work");
    std::fs::create_dir_all(venv.join(".venv/bin")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/python3", venv.join(".venv/bin/python")).unwrap();
    let strict = work.path().join("strict");
    let result = sandbox
        .conn
        .export_tar(&venv, Exclusions::None, &strict, Limits::default())
        .await;
    assert!(refused(&result), "{result:?}");
    assert!(!strict.exists());
    let kept = work.path().join("kept");
    let summary = sandbox
        .conn
        .export_tar(&venv, Exclusions::None, &kept, prepared)
        .await
        .unwrap();
    assert_eq!(summary.links, 1);
    assert_eq!(
        std::fs::read_link(kept.join(".venv/bin/python")).unwrap(),
        Path::new("/usr/bin/python3")
    );
    let again = work.path().join("again");
    sandbox
        .conn
        .import_dir_within(&kept, &again, Exclusions::None, Some(prepared))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_link(again.join(".venv/bin/python")).unwrap(),
        Path::new("/usr/bin/python3")
    );
    let result = sandbox
        .conn
        .import_dir(&kept, work.path().join("checkout"), Exclusions::None)
        .await;
    assert!(
        matches!(result, Err(SandboxError::Remote { .. })),
        "{result:?}"
    );

    // Nothing is written through one.
    let outside = work.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    let mut builder = tar::Builder::new(Vec::new());
    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Symlink);
    link.set_size(0);
    builder.append_link(&mut link, "alias", &outside).unwrap();
    let mut file = tar::Header::new_gnu();
    file.set_entry_type(tar::EntryType::Regular);
    file.set_mode(0o644);
    file.set_size(1);
    builder
        .append_data(&mut file, "alias/planted", &b"x"[..])
        .unwrap();
    let through = builder.into_inner().unwrap();
    let result = sandbox
        .conn
        .import_tar(work.path().join("through"), &through[..], Some(prepared))
        .await;
    assert!(
        matches!(result, Err(SandboxError::Remote { .. })),
        "{result:?}"
    );
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    assert!(sandbox.finish().await.success());
}

async fn echo_server_tcp() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = stream.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn a_tunnel_reaches_the_nodes_echo_server() {
    let sandbox = start().await;
    let echo = echo_server_tcp().await;
    let tunnel = sandbox
        .conn
        .open_tunnel(
            "echo",
            ListenAddr::Tcp("127.0.0.1:0".into()),
            Endpoint::Tcp(echo),
        )
        .await
        .unwrap();
    let ListenAddr::Tcp(bound) = tunnel.bound().clone() else {
        panic!("tcp listener");
    };
    let mut clients = Vec::new();
    for i in 0..4 {
        let bound = bound.clone();
        clients.push(tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(&bound).await.unwrap();
            let message = format!("hello {i}").repeat(10_000);
            stream.write_all(message.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
            let mut back = String::new();
            stream.read_to_string(&mut back).await.unwrap();
            assert_eq!(back, message);
        }));
    }
    for client in clients {
        client.await.unwrap();
    }
    // A second tunnel by the same name is refused while the first is open.
    assert!(sandbox
        .conn
        .open_tunnel(
            "echo",
            ListenAddr::Tcp("127.0.0.1:0".into()),
            Endpoint::Tcp("127.0.0.1:1".into())
        )
        .await
        .is_err());

    // Unix to unix.
    let dir = support::scratch();
    let echo_path = dir.path().join("echo.sock");
    let echo_unix = tokio::net::UnixListener::bind(&echo_path).unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = echo_unix.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = stream.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    let inside = dir.path().join("inside.sock");
    let unix = sandbox
        .conn
        .open_tunnel(
            "unix",
            ListenAddr::Unix(inside.clone()),
            Endpoint::Unix(echo_path),
        )
        .await
        .unwrap();
    let mut stream = tokio::net::UnixStream::connect(&inside).await.unwrap();
    stream.write_all(b"ping").await.unwrap();
    let mut back = [0u8; 4];
    stream.read_exact(&mut back).await.unwrap();
    assert_eq!(&back, b"ping");
    unix.close().await.unwrap();
    assert!(!inside.exists(), "closing removes the socket");

    tunnel.close().await.unwrap();
    assert!(tokio::net::TcpStream::connect(&bound).await.is_err());
    assert!(sandbox.finish().await.success());
}

async fn next_event(events: &mut tracon_sandbox::node::EventStream) -> Event {
    tokio::time::timeout(Duration::from_secs(10), events.next())
        .await
        .expect("an event in time")
        .unwrap()
        .expect("the stream is open")
}

#[tokio::test]
async fn a_crashing_service_restarts_with_backoff() {
    let sandbox = start().await;
    let mut events = sandbox.conn.events().await.unwrap();
    let mut crash = ServiceSpec::new("crash", ["sh", "-c", "echo boom; exit 1"]);
    crash.backoff = Backoff {
        initial_ms: 50,
        max_ms: 200,
        reset_after_ms: 60_000,
    };
    crash.restart = RestartPolicy::OnFailure;
    sandbox.conn.services().start(vec![crash]).await.unwrap();

    let mut booms = 0;
    let mut delays = Vec::new();
    loop {
        match next_event(&mut events).await {
            Event::Log {
                service,
                stream,
                line,
            } => {
                assert_eq!(
                    (service.as_str(), stream, line.as_str()),
                    ("crash", LogStream::Stdout, "boom")
                );
                booms += 1;
            }
            Event::Service {
                state: ServiceState::Restarting { delay_ms, .. },
                ..
            } => delays.push(delay_ms),
            Event::Service {
                state: ServiceState::Starting { attempt: 3 },
                ..
            } => break,
            _ => {}
        }
    }
    assert!(booms >= 3, "each run's output reached the node");
    assert_eq!(delays, vec![50, 100, 200], "doubling, capped");
    let status = sandbox.conn.services().status().await.unwrap();
    assert!(status[0].restarts >= 3, "{status:?}");
    sandbox.conn.services().stop(vec![]).await.unwrap();
    let status = sandbox.conn.services().status().await.unwrap();
    assert_eq!(status[0].state, ServiceState::Stopped);
    assert!(sandbox.finish().await.success());
}

/// Answers each request on `listener` with the next of `statuses`, the last
/// one for ever after.
fn http_server(listener: tokio::net::TcpListener, statuses: &'static [u16]) {
    tokio::spawn(async move {
        let mut asked = 0;
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let status = statuses[asked.min(statuses.len() - 1)];
            asked += 1;
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut reader = tokio::io::BufReader::new(&mut stream);
                // Up to the blank line ending the head.
                loop {
                    let mut line = Vec::new();
                    if reader.read_until(b'\n', &mut line).await.unwrap_or(0) == 0
                        || line == b"\r\n"
                    {
                        break;
                    }
                    request.extend(line);
                }
                assert!(request.starts_with(b"GET /json/version HTTP/1.1"));
                let reply = format!("HTTP/1.1 {status} X\r\nContent-Length: 0\r\n\r\n");
                let _ = stream.write_all(reply.as_bytes()).await;
            });
        }
    });
}

#[tokio::test]
async fn an_http_probe_waits_for_a_2xx() {
    let sandbox = start().await;
    let mut events = sandbox.conn.events().await.unwrap();
    let late = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let late_addr = late.local_addr().unwrap().to_string();
    http_server(late, &[503, 503, 200]);
    let missing = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let missing_addr = missing.local_addr().unwrap().to_string();
    http_server(missing, &[404]);
    let mut web = ServiceSpec::new("web", ["sleep", "30"]);
    web.readiness = Some(Readiness {
        probe: Probe::Http {
            addr: late_addr,
            path: "/json/version".into(),
        },
        interval_ms: 50,
        timeout_ms: 5_000,
    });
    let mut wrong = ServiceSpec::new("wrong", ["sleep", "30"]);
    wrong.restart = RestartPolicy::Never;
    wrong.readiness = Some(Readiness {
        probe: Probe::Http {
            addr: missing_addr,
            path: "/json/version".into(),
        },
        interval_ms: 50,
        timeout_ms: 500,
    });
    sandbox
        .conn
        .services()
        .start(vec![web, wrong])
        .await
        .unwrap();
    let (mut ready, mut failed) = (None, None);
    while ready.is_none() || failed.is_none() {
        match next_event(&mut events).await {
            Event::Service {
                name,
                state: ServiceState::Ready,
            } => ready = Some(name),
            Event::Service {
                name,
                state: ServiceState::Failed { .. },
            } => failed = Some(name),
            _ => {}
        }
    }
    assert_eq!(ready.as_deref(), Some("web"));
    assert_eq!(failed.as_deref(), Some("wrong"), "a 404 is not ready");
    assert!(sandbox.finish().await.success());
}

#[tokio::test]
async fn readiness_probes_and_teardown() {
    let sandbox = start().await;
    let mut events = sandbox.conn.events().await.unwrap();
    let port = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut web = ServiceSpec::new(
        "web",
        [
            "sh",
            "-c",
            "trap 'echo stopping; exit 0' TERM; while :; do sleep 0.1; done",
        ],
    );
    web.readiness = Some(Readiness {
        probe: Probe::Tcp(port.local_addr().unwrap().to_string()),
        interval_ms: 50,
        timeout_ms: 5_000,
    });
    let mut worker = ServiceSpec::new("worker", ["sleep", "30"]);
    worker.readiness = Some(Readiness {
        probe: Probe::Command(vec!["true".into()]),
        interval_ms: 50,
        timeout_ms: 5_000,
    });
    let mut never = ServiceSpec::new("never", ["sleep", "30"]);
    never.restart = RestartPolicy::Never;
    never.readiness = Some(Readiness {
        probe: Probe::Tcp("127.0.0.1:1".into()),
        interval_ms: 50,
        timeout_ms: 300,
    });
    sandbox
        .conn
        .services()
        .start(vec![web, worker, never])
        .await
        .unwrap();
    let mut ready = std::collections::BTreeSet::new();
    let mut failed = None;
    while ready.len() < 2 || failed.is_none() {
        if let Event::Service { name, state } = next_event(&mut events).await {
            match state {
                ServiceState::Ready => {
                    ready.insert(name);
                }
                ServiceState::Failed { .. } => failed = Some(name),
                _ => {}
            }
        }
    }
    assert_eq!(ready, ["web".to_string(), "worker".to_string()].into());
    assert_eq!(failed.as_deref(), Some("never"));

    // Shutting down stops each service with its stop signal; `web` traps it.
    sandbox.conn.shutdown().await.unwrap();
    let mut stopping = false;
    let mut stopped = std::collections::BTreeSet::new();
    while let Ok(Ok(Some(event))) =
        tokio::time::timeout(Duration::from_secs(10), events.next()).await
    {
        match event {
            Event::Log { line, .. } if line == "stopping" => stopping = true,
            Event::Service {
                name,
                state: ServiceState::Stopped,
            } => {
                stopped.insert(name);
            }
            _ => {}
        }
    }
    assert!(stopping, "web saw its stop signal");
    assert_eq!(stopped, ["web".to_string(), "worker".to_string()].into());
    let mut child = sandbox.child;
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
}

/// A sandbox started with `resume_within` and a service that writes its pid;
/// the pid once written.
async fn with_a_service(options: ServerOptions) -> (support::Sandbox, i32) {
    let sandbox = start_with(Carrier::Tcp, options, |_| {}).await;
    let dir = support::scratch();
    let pidfile = dir.path().join("pid");
    let script = format!("echo $$ > {}; exec sleep 30", pidfile.display());
    sandbox
        .conn
        .services()
        .start(vec![ServiceSpec::new("sleeper", ["sh", "-c", &script])])
        .await
        .unwrap();
    // Written by the shell after it creates the file, so read until whole.
    let mut pid = None;
    for _ in 0..200 {
        pid = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok());
        if pid.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // The scratch directory may go; the service has written its pid.
    (sandbox, pid.expect("the service wrote its pid"))
}

#[tokio::test]
async fn a_lost_connection_tears_down_a_sandbox_that_cannot_resume() {
    let (sandbox, pid) = with_a_service(ServerOptions {
        resume_within: Duration::ZERO,
        ..ServerOptions::default()
    })
    .await;
    assert!(!sandbox.conn.is_resumable());
    let support::Sandbox {
        node,
        conn,
        mut child,
        ..
    } = sandbox;
    conn.sever();
    assert!(conn.is_closed(), "nothing to wait for");
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.code(), Some(3), "lost, not shut down");
    assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "the service went with it");
    drop(node);
}

#[tokio::test]
async fn a_run_the_node_has_ended_refuses_its_sandbox_back() {
    let (sandbox, pid) = with_a_service(ServerOptions::default()).await;
    assert!(sandbox.conn.is_resumable());
    let support::Sandbox {
        node,
        conn,
        mut child,
        ..
    } = sandbox;
    // The node lets go of the run without a shutdown: its resume secret is
    // revoked with it, so the sandbox's redial is refused and it tears
    // itself down rather than waiting out its window.
    drop(conn);
    assert!(matches!(node.accept().await, Err(SandboxError::BadToken)));
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.code(), Some(3), "lost, not shut down");
    assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "the service went with it");
}

#[tokio::test]
async fn a_sandbox_the_node_does_not_take_back_in_time_tears_itself_down() {
    let (sandbox, pid) = with_a_service(ServerOptions {
        resume_within: Duration::from_millis(300),
        ..ServerOptions::default()
    })
    .await;
    let support::Sandbox {
        node,
        conn,
        mut child,
        ..
    } = sandbox;
    conn.sever();
    assert!(!conn.is_closed(), "waiting for the sandbox to come back");
    // Nobody takes its redials: it keeps trying for twice the node's
    // window, then gives up.
    tokio::time::timeout(Duration::from_secs(5), conn.closed())
        .await
        .expect("the node gives up after its window");
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.code(), Some(3), "lost, not shut down");
    assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "the service went with it");
    drop(node);
}

#[tokio::test]
async fn a_bad_token_is_refused_and_a_token_works_once() {
    let node = support::Node::new(Carrier::Tcp, ServerOptions::default()).await;
    let token = node.server.tokens().issue("run");

    let mut wrong = node
        .command(node.identity.pin(), "not-a-token")
        .spawn()
        .unwrap();
    assert!(matches!(node.accept().await, Err(SandboxError::BadToken)));
    assert_eq!(wrong.wait().await.unwrap().code(), Some(1));

    let mut first = node.command(node.identity.pin(), &token).spawn().unwrap();
    let conn = node.accept().await.unwrap();
    assert_eq!(conn.run(), "run");
    assert_eq!(conn.info().version, env!("CARGO_PKG_VERSION"));

    let mut replay = node.command(node.identity.pin(), &token).spawn().unwrap();
    assert!(matches!(node.accept().await, Err(SandboxError::BadToken)));
    assert_eq!(replay.wait().await.unwrap().code(), Some(1));

    // A revoked token is as good as a used one.
    let revoked = node.server.tokens().issue("other");
    node.server.tokens().revoke("other");
    let mut late = node.command(node.identity.pin(), &revoked).spawn().unwrap();
    assert!(matches!(node.accept().await, Err(SandboxError::BadToken)));
    assert_eq!(late.wait().await.unwrap().code(), Some(1));

    conn.shutdown().await.unwrap();
    assert!(first.wait().await.unwrap().success());
}

#[tokio::test]
async fn a_wrong_pin_is_refused() {
    let node = support::Node::new(Carrier::Tcp, ServerOptions::default()).await;
    let token = node.server.tokens().issue("run");
    let impostor = tracon_sandbox::tls::NodeIdentity::generate().unwrap();
    let mut child = node.command(impostor.pin(), &token).spawn().unwrap();
    let result = node.accept().await;
    assert!(
        matches!(result, Err(SandboxError::Tls(_))),
        "{:?}",
        result.err()
    );
    assert_eq!(child.wait().await.unwrap().code(), Some(1));
    // The token was never presented, so it still works for the real sandbox.
    let mut real = node.command(node.identity.pin(), &token).spawn().unwrap();
    let conn = node.accept().await.unwrap();
    conn.shutdown().await.unwrap();
    assert!(real.wait().await.unwrap().success());
}

#[tokio::test]
async fn keepalive_notices_a_frozen_sandbox() {
    let sandbox = start_with(
        Carrier::Tcp,
        ServerOptions {
            keepalive: Duration::from_millis(200),
            ..ServerOptions::default()
        },
        |command| {
            command.env("TRACON_SANDBOX_KEEPALIVE_MS", "200");
        },
    )
    .await;
    // Pings flow both ways while both ends are alive.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!sandbox.conn.is_closed());
    let pid = sandbox.child.id().unwrap() as i32;
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    tokio::time::timeout(Duration::from_secs(3), sandbox.conn.disconnected())
        .await
        .expect("a missing pong closes the connection");
    assert!(
        !sandbox.conn.is_closed(),
        "the run waits for the sandbox to come back"
    );
    unsafe { libc::kill(pid, libc::SIGKILL) };
}

#[tokio::test]
async fn the_unix_carrier_carries_the_same_protocol() {
    let sandbox = start_with(Carrier::Unix, ServerOptions::default(), |_| {}).await;
    let output = sandbox
        .conn
        .spawn(sh("echo over unix"))
        .await
        .unwrap()
        .output()
        .await
        .unwrap();
    assert_eq!(output.stdout, b"over unix\n");
    assert!(sandbox.finish().await.success());
}

#[tokio::test]
async fn a_listening_sandbox_takes_the_node_connecting_in() {
    let node = support::Node::new(Carrier::Tcp, ServerOptions::default()).await;
    let token = node.server.tokens().issue("run");
    let socket = node.dir.path().join("sb.sock");
    let mut command = node.command(node.identity.pin(), &token);
    command
        .env_remove("TRACON_SANDBOX_ADDR")
        .env("TRACON_SANDBOX_LISTEN", &socket);
    let mut child = command.spawn().unwrap();
    // Started before the sandbox listens: the node keeps trying.
    let conn = node
        .server
        .connect_to(&socket, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(conn.run(), "run");
    let output = conn
        .spawn(sh("echo connected in"))
        .await
        .unwrap()
        .output()
        .await
        .unwrap();
    assert_eq!(output.stdout, b"connected in\n");
    assert!(
        !socket.exists(),
        "nothing else can connect once the node has"
    );
    conn.shutdown().await.unwrap();
    assert!(child.wait().await.unwrap().success());
}

#[tokio::test]
async fn a_listening_sandbox_still_refuses_a_bad_token() {
    let node = support::Node::new(Carrier::Tcp, ServerOptions::default()).await;
    let socket = node.dir.path().join("sb.sock");
    let mut command = node.command(node.identity.pin(), "not-a-token");
    command
        .env_remove("TRACON_SANDBOX_ADDR")
        .env("TRACON_SANDBOX_LISTEN", &socket);
    let mut child = command.spawn().unwrap();
    let refused = node
        .server
        .connect_to(&socket, Duration::from_secs(10))
        .await;
    assert!(
        matches!(refused, Err(SandboxError::BadToken)),
        "{:?}",
        refused.err()
    );
    assert_eq!(child.wait().await.unwrap().code(), Some(1));
}

#[tokio::test]
async fn a_sandbox_nobody_connects_to_gives_up() {
    let node = support::Node::new(Carrier::Tcp, ServerOptions::default()).await;
    let token = node.server.tokens().issue("run");
    let mut command = node.command(node.identity.pin(), &token);
    command
        .env_remove("TRACON_SANDBOX_ADDR")
        .env("TRACON_SANDBOX_LISTEN", node.dir.path().join("sb.sock"))
        .env("TRACON_SANDBOX_CONNECT_TIMEOUT_MS", "200");
    let status = tokio::time::timeout(Duration::from_secs(5), command.status())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.code(), Some(1));
}

/// A provider that starts the supervisor as root names its user and working
/// directory in the bootstrap; here, the user it already is, which is all a
/// test that is not root can become.
#[tokio::test]
async fn it_becomes_its_bootstrap_user_and_works_where_it_is_told() {
    let dir = support::scratch();
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let workdir = dir.path().to_path_buf();
    let sandbox = start_with(Carrier::Tcp, ServerOptions::default(), |command| {
        command
            .env("TRACON_SANDBOX_USER", format!("{uid}:{gid}"))
            .env("TRACON_SANDBOX_WORKDIR", &workdir);
    })
    .await;
    let output = sandbox
        .conn
        .spawn(sh(
            r#"echo "$(id -u):$(id -g) $PWD"; env | grep -c ^TRACON_SANDBOX_ || true; grep ^NoNewPrivs: /proc/self/status"#,
        ))
        .await
        .unwrap()
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "{uid}:{gid} {}\n0\nNoNewPrivs:\t1\n",
            dir.path().canonicalize().unwrap().display()
        )
    );
    assert_eq!(
        sandbox.conn.info().uid,
        Some(uid),
        "its hello says who it is"
    );
    assert!(sandbox.finish().await.success());
}

/// Anyone but root cannot become another user: the supervisor stops before
/// it dials, rather than run as someone the node did not ask for.
#[tokio::test]
async fn a_user_it_cannot_become_stops_it_before_it_dials() {
    let uid = unsafe { libc::getuid() };
    if uid == 0 {
        return;
    }
    let node = support::Node::new(Carrier::Tcp, ServerOptions::default()).await;
    let token = node.server.tokens().issue("run");
    for (user, why) in [
        (format!("{}:{}", uid + 1, uid + 1), "cannot become"),
        ("harness".to_string(), "not uid:gid"),
    ] {
        let mut command = node.command(node.identity.pin(), &token);
        command
            .env("TRACON_SANDBOX_USER", &user)
            .stderr(std::process::Stdio::piped());
        let out = tokio::time::timeout(Duration::from_secs(5), command.output())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.status.code(), Some(64), "{user}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(why), "{user}: {stderr}");
    }
    let workdir = node.dir.path().join("missing");
    let mut command = node.command(node.identity.pin(), &token);
    command
        .env("TRACON_SANDBOX_WORKDIR", &workdir)
        .stderr(std::process::Stdio::piped());
    let out = command.output().await.unwrap();
    assert_eq!(out.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&out.stderr).contains("TRACON_SANDBOX_WORKDIR"));
}

/// Nothing the supervisor starts may gain privileges by an exec, whether or
/// not it was asked to become a user: a setuid or file-capability binary in
/// the image runs with its caller's credentials.
#[tokio::test]
async fn nothing_it_starts_gains_privileges_by_an_exec() {
    let sandbox = start().await;
    let output = sandbox
        .conn
        .spawn(sh("grep ^NoNewPrivs: /proc/self/status"))
        .await
        .unwrap()
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "NoNewPrivs:\t1\n"
    );
    assert!(sandbox.finish().await.success());
}
