use super::*;

/// An archive entry written with raw header bytes, so it can say what the
/// `tar` crate's own path setters refuse to.
fn raw_entry(
    builder: &mut tar::Builder<Vec<u8>>,
    kind: tar::EntryType,
    name: &[u8],
    link: Option<&[u8]>,
    data: &[u8],
) {
    let mut header = tar::Header::new_gnu();
    {
        let gnu = header.as_gnu_mut().unwrap();
        gnu.name[..name.len()].copy_from_slice(name);
        if let Some(link) = link {
            gnu.linkname[..link.len()].copy_from_slice(link);
        }
    }
    header.set_entry_type(kind);
    header.set_mode(0o644);
    header.set_size(data.len() as u64);
    header.set_cksum();
    builder.append(&header, data).unwrap();
}

fn archive(build: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    build(&mut builder);
    builder.into_inner().unwrap()
}

fn unpack_into(
    bytes: &[u8],
    limits: &Limits,
) -> (tempfile::TempDir, Result<TreeSummary, FilesError>) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("dest")).unwrap();
    let result = unpack(bytes, &dir.path().join("dest"), None, limits);
    (dir, result)
}

#[test]
fn a_tree_round_trips_with_its_links_and_executable_bit() {
    let source = tempfile::tempdir().unwrap();
    let root = source.path();
    std::fs::create_dir_all(root.join("bin/deep")).unwrap();
    std::fs::write(root.join("bin/run"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(root.join("bin/run"), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(root.join("readme"), "hello").unwrap();
    std::fs::set_permissions(root.join("readme"), std::fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink("../readme", root.join("bin/readme")).unwrap();
    let long = "d".repeat(120);
    std::fs::create_dir_all(root.join(&long)).unwrap();
    std::fs::write(root.join(&long).join("f".repeat(120)), "long").unwrap();

    let mut bytes = Vec::new();
    let packed = pack(root, Exclusions::None, &mut bytes).unwrap();
    assert_eq!((packed.files, packed.links), (3, 1));
    let (dir, unpacked) = unpack_into(&bytes, &Limits::default());
    let unpacked = unpacked.unwrap();
    assert_eq!(
        (unpacked.files, unpacked.links, unpacked.bytes),
        (3, 1, packed.bytes)
    );
    let dest = dir.path().join("dest");
    assert_eq!(
        std::fs::read_to_string(dest.join("readme")).unwrap(),
        "hello"
    );
    let mode = |p: &str| {
        std::fs::metadata(dest.join(p))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode("bin/run"), 0o755);
    assert_eq!(mode("readme"), 0o644);
    assert_eq!(
        std::fs::read_link(dest.join("bin/readme")).unwrap(),
        Path::new("../readme")
    );
    assert!(dest.join("bin/deep").is_dir());
    assert_eq!(
        std::fs::read_to_string(dest.join(&long).join("f".repeat(120))).unwrap(),
        "long"
    );
}

#[test]
fn paths_that_climb_or_are_absolute_are_refused() {
    for name in [&b"../evil"[..], b"/etc/evil", b"a/../../evil"] {
        let bytes = archive(|b| raw_entry(b, tar::EntryType::Regular, name, None, b"x"));
        let (dir, result) = unpack_into(&bytes, &Limits::default());
        assert!(
            matches!(result, Err(FilesError::Unsafe(_))),
            "{}: {result:?}",
            String::from_utf8_lossy(name)
        );
        assert!(!dir.path().join("evil").exists());
    }
}

#[test]
fn links_must_resolve_inside_the_tree() {
    for (name, target) in [
        (&b"a"[..], &b"../outside"[..]),
        (b"sub/b", b"../../outside"),
        (b"c", b"/etc/passwd"),
        (b"sub/d", b"x/../../.."),
    ] {
        let bytes = archive(|b| raw_entry(b, tar::EntryType::Symlink, name, Some(target), b""));
        let (_dir, result) = unpack_into(&bytes, &Limits::default());
        assert!(
            matches!(result, Err(FilesError::Unsafe(_))),
            "{} -> {}: {result:?}",
            String::from_utf8_lossy(name),
            String::from_utf8_lossy(target)
        );
    }
    let bytes = archive(|b| raw_entry(b, tar::EntryType::Symlink, b"sub/ok", Some(b"../a"), b""));
    let (_dir, result) = unpack_into(&bytes, &Limits::default());
    assert_eq!(result.unwrap().links, 1);
}

fn unpack_at(
    bytes: &[u8],
    mount: &str,
    limits: &Limits,
) -> (tempfile::TempDir, Result<TreeSummary, FilesError>) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("dest")).unwrap();
    let result = unpack(
        bytes,
        &dir.path().join("dest"),
        Some(Path::new(mount)),
        limits,
    );
    (dir, result)
}

#[test]
fn an_absolute_link_is_read_against_the_trees_mount() {
    for (name, target) in [
        (&b"bun/x/1@@@1"[..], &b"/cache/bun/x@1@@@1"[..]),
        (b"top", b"/cache"),
        (b"a/b/c", b"/cache/./d"),
    ] {
        let bytes = archive(|b| raw_entry(b, tar::EntryType::Symlink, name, Some(target), b""));
        let (dir, result) = unpack_at(&bytes, "/cache", &Limits::default());
        assert_eq!(result.unwrap().links, 1);
        let name = std::str::from_utf8(name).unwrap();
        assert_eq!(
            std::fs::read_link(dir.path().join("dest").join(name)).unwrap(),
            Path::new(std::str::from_utf8(target).unwrap()),
            "kept as it was written"
        );
    }
    for (mount, target) in [
        ("/cache", &b"/cache/../etc/passwd"[..]),
        ("/cache", b"/cache/x/../../etc"),
        ("/cache", b"/cachefoo/x"),
        ("/cache", b"/etc/passwd"),
        ("/", b"/etc/passwd"),
        ("cache", b"cache/../.."),
    ] {
        let bytes = archive(|b| raw_entry(b, tar::EntryType::Symlink, b"l", Some(target), b""));
        let (_dir, result) = unpack_at(&bytes, mount, &Limits::default());
        assert!(
            matches!(result, Err(FilesError::Unsafe(_))),
            "{mount}: {}: {result:?}",
            String::from_utf8_lossy(target)
        );
    }
}

#[test]
fn a_prepared_tree_keeps_links_that_leave_it_without_following_them() {
    let inert = Limits {
        inert_links: true,
        ..Limits::default()
    };
    for target in [&b"/usr/bin/python3"[..], b"../../../outside", b"x/../../.."] {
        let bytes = archive(|b| {
            raw_entry(b, tar::EntryType::Directory, b".venv/bin/", None, b"");
            raw_entry(
                b,
                tar::EntryType::Symlink,
                b".venv/bin/python",
                Some(target),
                b"",
            );
        });
        let (_dir, refused) = unpack_at(&bytes, "/work", &Limits::default());
        assert!(matches!(refused, Err(FilesError::Unsafe(_))), "{refused:?}");
        let (dir, kept) = unpack_at(&bytes, "/work", &inert);
        assert_eq!(kept.unwrap().links, 1);
        assert_eq!(
            std::fs::read_link(dir.path().join("dest/.venv/bin/python")).unwrap(),
            Path::new(std::str::from_utf8(target).unwrap())
        );
    }

    // Nothing is written through one: not as a directory on the way to a
    // file, not as the file itself, not as a directory entry.
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("leaf"), "keep").unwrap();
    let outside_bytes = outside.as_os_str().as_encoded_bytes().to_vec();
    let leaf = outside.join("leaf");
    for entries in [
        vec![
            (
                tar::EntryType::Symlink,
                &b"alias"[..],
                Some(&outside_bytes[..]),
            ),
            (tar::EntryType::Regular, b"alias/planted", None),
        ],
        vec![
            (tar::EntryType::Symlink, b"alias", Some(&outside_bytes[..])),
            (tar::EntryType::Directory, b"alias/", None),
            (tar::EntryType::Regular, b"alias/leaf", None),
        ],
        vec![
            (tar::EntryType::Symlink, b"alias", Some(&outside_bytes[..])),
            (tar::EntryType::Directory, b"alias/sub/", None),
        ],
    ] {
        let dest = tempfile::tempdir_in(dir.path()).unwrap();
        let bytes = archive(|b| {
            for (kind, name, link) in &entries {
                let data: &[u8] = if *kind == tar::EntryType::Regular {
                    b"x"
                } else {
                    b""
                };
                raw_entry(b, *kind, name, *link, data);
            }
        });
        let result = unpack(&bytes[..], dest.path(), Some(Path::new("/work")), &inert);
        assert!(matches!(result, Err(FilesError::Unsafe(_))), "{result:?}");
        assert!(!outside.join("planted").exists());
        assert!(!outside.join("sub").exists());
        assert_eq!(std::fs::read_to_string(&leaf).unwrap(), "keep");
    }

    // A file over an inert link replaces the link.
    let dest = tempfile::tempdir_in(dir.path()).unwrap();
    std::os::unix::fs::symlink(&leaf, dest.path().join("leaf")).unwrap();
    let bytes = archive(|b| raw_entry(b, tar::EntryType::Regular, b"leaf", None, b"new"));
    unpack(&bytes[..], dest.path(), None, &inert).unwrap();
    assert_eq!(std::fs::read_to_string(&leaf).unwrap(), "keep");
    assert_eq!(
        std::fs::read_to_string(dest.path().join("leaf")).unwrap(),
        "new"
    );
}

#[test]
fn old_limits_without_inert_links_keep_them_refused() {
    let old = r#"{"max_bytes":1,"max_files":2,"max_dirs":3,"max_path_bytes":4}"#;
    let limits: Limits = serde_json::from_str(old).unwrap();
    assert!(!limits.inert_links);
}

#[test]
fn a_checkout_export_never_reads_ignore_rules_through_a_link_that_leaves() {
    let source = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("rules"), "*\n").unwrap();
    let root = source.path().join("tree");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/real"), "*.log\n").unwrap();
    std::fs::write(root.join("kept.txt"), "k").unwrap();

    // One that stays inside is read, as before.
    std::os::unix::fs::symlink("sub/real", root.join(".gitignore")).unwrap();
    std::fs::write(root.join("noise.log"), "n").unwrap();
    let kept = kept_paths(&root).unwrap().unwrap();
    assert!(kept.contains(Path::new("kept.txt")));
    assert!(!kept.contains(Path::new("noise.log")));

    // One that leaves, at the root, in a directory, through a link that
    // leaves, or as the repository's exclude file, refuses the export.
    std::fs::remove_file(root.join(".gitignore")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("rules"), root.join(".gitignore")).unwrap();
    assert!(matches!(kept_paths(&root), Err(FilesError::Unsafe(_))));
    std::fs::remove_file(root.join(".gitignore")).unwrap();

    std::os::unix::fs::symlink(outside.path(), root.join("away")).unwrap();
    std::os::unix::fs::symlink("../away/rules", root.join("sub/.gitignore")).unwrap();
    assert!(matches!(kept_paths(&root), Err(FilesError::Unsafe(_))));
    std::fs::remove_file(root.join("sub/.gitignore")).unwrap();

    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join(".git/info")).unwrap();
    std::fs::rename(outside.path().join("rules"), outside.path().join("exclude")).unwrap();
    assert!(matches!(kept_paths(&root), Err(FilesError::Unsafe(_))));
    std::fs::remove_file(root.join(".git/info")).unwrap();

    std::os::unix::fs::symlink("/dev/zero", root.join(".gitignore")).unwrap();
    assert!(matches!(kept_paths(&root), Err(FilesError::Unsafe(_))));
}

#[test]
fn nothing_is_written_through_a_link() {
    let bytes = archive(|b| {
        raw_entry(b, tar::EntryType::Directory, b"real/", None, b"");
        raw_entry(b, tar::EntryType::Symlink, b"alias", Some(b"real"), b"");
        raw_entry(b, tar::EntryType::Regular, b"alias/planted", None, b"x");
    });
    let (dir, result) = unpack_into(&bytes, &Limits::default());
    assert!(matches!(result, Err(FilesError::Unsafe(_))), "{result:?}");
    assert!(!dir.path().join("dest/real/planted").exists());

    // A file entry over an existing link replaces the link, not its target.
    let outside = dir.path().join("outside");
    std::fs::write(&outside, "keep").unwrap();
    std::os::unix::fs::symlink(&outside, dir.path().join("dest/leaf")).unwrap();
    let bytes = archive(|b| raw_entry(b, tar::EntryType::Regular, b"leaf", None, b"new"));
    unpack(
        &bytes[..],
        &dir.path().join("dest"),
        None,
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("dest/leaf")).unwrap(),
        "new"
    );
}

#[test]
fn hard_links_and_special_files_are_refused() {
    for kind in [
        tar::EntryType::Link,
        tar::EntryType::Fifo,
        tar::EntryType::Char,
        tar::EntryType::Block,
    ] {
        let link = (kind == tar::EntryType::Link).then_some(&b"/etc/shadow"[..]);
        let bytes = archive(|b| raw_entry(b, kind, b"thing", link, b""));
        let (_dir, result) = unpack_into(&bytes, &Limits::default());
        assert!(
            matches!(result, Err(FilesError::Unsupported { .. })),
            "{kind:?}: {result:?}"
        );
    }
}

#[test]
fn limits_bound_bytes_files_and_directories() {
    let limits = Limits {
        max_bytes: 10,
        max_files: 2,
        max_dirs: 2,
        max_path_bytes: 32,
        inert_links: false,
    };
    let big = archive(|b| raw_entry(b, tar::EntryType::Regular, b"big", None, &[0u8; 11]));
    assert!(matches!(
        unpack_into(&big, &limits).1,
        Err(FilesError::TooLarge(10))
    ));

    let many = archive(|b| {
        for name in [&b"a"[..], b"b", b"c"] {
            raw_entry(b, tar::EntryType::Regular, name, None, b"");
        }
    });
    assert!(matches!(
        unpack_into(&many, &limits).1,
        Err(FilesError::TooManyFiles(2))
    ));

    let deep = archive(|b| raw_entry(b, tar::EntryType::Regular, b"a/b/c/file", None, b""));
    assert!(matches!(
        unpack_into(&deep, &limits).1,
        Err(FilesError::TooManyDirs(2))
    ));

    let long = archive(|b| raw_entry(b, tar::EntryType::Regular, &[b'x'; 40], None, b""));
    assert!(matches!(
        unpack_into(&long, &limits).1,
        Err(FilesError::Unsafe(_))
    ));
}

#[test]
fn a_failed_pack_does_not_end_like_a_complete_archive() {
    if unsafe { libc::geteuid() } == 0 {
        return; // root reads the unreadable file
    }
    let source = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("a"), "fine").unwrap();
    std::fs::write(source.path().join("b"), "secret").unwrap();
    std::fs::set_permissions(
        source.path().join("b"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let mut bytes = Vec::new();
    assert!(pack(source.path(), Exclusions::None, &mut bytes).is_err());
    assert!(
        bytes.len() < 1024 || bytes[bytes.len() - 1024..].iter().any(|b| *b != 0),
        "end-of-archive blocks were written after the failure"
    );
}

#[test]
fn checkout_exclusions_match_git() {
    let have_git = std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !have_git {
        return;
    }
    let source = tempfile::tempdir().unwrap();
    let root = source.path();
    let git = |args: &[&str]| {
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
    };
    git(&["init", "-q"]);
    std::fs::write(root.join(".gitignore"), "*.log\nbuild/\n").unwrap();
    std::fs::write(root.join("tracked.txt"), "t").unwrap();
    std::fs::write(root.join("forced.log"), "f").unwrap();
    std::fs::create_dir_all(root.join("build/sub")).unwrap();
    std::fs::write(root.join("build/sub/out.bin"), "b").unwrap();
    git(&["add", ".gitignore", "tracked.txt"]);
    git(&["add", "-f", "forced.log"]);
    git(&["commit", "-qm", "init"]);
    std::fs::write(root.join("untracked.txt"), "u").unwrap();
    std::fs::write(root.join("noise.log"), "n").unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "").unwrap();
    std::fs::write(root.join(".git/info/exclude"), "secret.env\n").unwrap();
    std::fs::write(root.join("secret.env"), "s").unwrap();

    let mut bytes = Vec::new();
    pack(root, Exclusions::Checkout, &mut bytes).unwrap();
    let mut names: Vec<String> = tar::Archive::new(&bytes[..])
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with(".git/") && n != ".git")
        .collect();
    names.sort();
    let mut expected: Vec<String> = git(&["ls-files", "-co", "--exclude-standard"])
        .lines()
        .map(str::to_string)
        .collect();
    // Directories are archived as entries of their own.
    expected.push("src".into());
    expected.sort();
    assert_eq!(names, expected);
    let has_git = tar::Archive::new(&bytes[..])
        .entries()
        .unwrap()
        .any(|e| e.unwrap().path().unwrap() == Path::new(".git/HEAD"));
    assert!(has_git, "the repository travels whole");
}
