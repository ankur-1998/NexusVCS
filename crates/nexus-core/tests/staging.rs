//! `nexus init`, `nexus add`, ignore rules, the index, the lock, and config.

mod common;

use std::fs;

use common::TestRepo;
use nexus_core::index::FileKind;
use nexus_core::path::RepoPath;

fn staged(repo: &TestRepo) -> Vec<String> {
    repo.repo()
        .load_index()
        .expect("the index loads")
        .iter()
        .map(|(path, _)| path.as_str().to_owned())
        .collect()
}

#[test]
fn init_creates_the_layout_and_refuses_nested_repositories() {
    let repo = TestRepo::uninitialized();
    let output = repo.ok(&["init"]);
    assert!(
        output.starts_with("Initialized empty NexusVCS repository in "),
        "{output}"
    );
    let dot = repo.root.join(".nexus");
    assert_eq!(
        fs::read_to_string(dot.join("HEAD")).unwrap(),
        "ref: refs/heads/main\n"
    );
    for path in ["objects", "refs/heads", "config.toml", "OPLOG"] {
        assert!(dot.join(path).exists(), "{path}");
    }
    let ignore = fs::read_to_string(repo.root.join(".nexusignore")).unwrap();
    assert!(
        ignore.contains("node_modules/") && ignore.contains("*.log"),
        "{ignore}"
    );

    fs::create_dir_all(repo.path("sub")).unwrap();
    let nested = repo.run_in(&repo.path("sub"), &["init"]);
    assert_eq!(nested.exit_code, 1);
    assert!(
        nested
            .text()
            .contains("already inside a NexusVCS repository"),
        "{}",
        nested.text()
    );

    // `init <dir>` creates the directory, and keeps an existing .nexusignore.
    let other = TestRepo::uninitialized();
    other.write("project/.nexusignore", "custom\n");
    other.ok(&["init", "project"]);
    assert!(other.path("project/.nexus/HEAD").is_file());
    assert_eq!(
        fs::read_to_string(other.path("project/.nexusignore")).unwrap(),
        "custom\n"
    );
}

#[test]
fn commands_work_from_subdirectories() {
    let repo = TestRepo::new();
    repo.write("src/deep/file.rs", "x");
    repo.write("top.txt", "y");
    let sub = repo.path("src");
    let result = repo.run_in(&sub, &["add", "."]);
    assert_eq!(result.exit_code, 0, "{}", result.text());
    assert_eq!(staged(&repo), ["src/deep/file.rs"]);
    let result = repo.run_in(&sub, &["add", "../top.txt"]);
    assert_eq!(result.exit_code, 0, "{}", result.text());
    assert_eq!(staged(&repo), ["src/deep/file.rs", "top.txt"]);
    assert!(
        repo.run_in(&sub, &["add", "../../outside.txt"])
            .text()
            .contains("outside the repository")
    );
}

#[test]
fn honors_ignore_rules_but_keeps_tracked_files_tracked() {
    let repo = TestRepo::new();
    repo.write("app.js", "code");
    repo.write("debug.log", "noise");
    repo.write("node_modules/lib/index.js", "dependency");
    repo.write("build/out.txt", "artifact");
    repo.write("keep/me.txt", "kept");
    repo.write("keep/secret.key", "hidden");
    repo.write("keep/.nexusignore", "*.key\n");
    repo.ok(&["add", "."]);
    assert_eq!(
        staged(&repo),
        [".nexusignore", "app.js", "keep/.nexusignore", "keep/me.txt"]
    );

    // Naming an ignored file explicitly warns instead of staging it.
    let output = repo.ok(&["add", "debug.log"]);
    assert!(output.contains("debug.log is ignored"), "{output}");
    assert!(!staged(&repo).contains(&"debug.log".to_owned()));

    // A tracked file that becomes ignored stays tracked, and changes still stage.
    repo.write(".nexusignore", "app.js\n");
    repo.ok(&["add", "."]);
    repo.write("app.js", "changed code");
    let output = repo.ok(&["add", "."]);
    assert!(output.contains("staged   app.js"), "{output}");
    assert!(staged(&repo).contains(&"app.js".to_owned()));

    // .nexus is never staged, whatever the rules say.
    repo.write(".nexusignore", "");
    repo.ok(&["add", "."]);
    assert!(
        staged(&repo)
            .iter()
            .all(|path| !path.starts_with(".nexus/"))
    );
}

#[test]
fn add_removes_deleted_files_and_rejects_typos() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a");
    repo.write("dir/b.txt", "b");
    repo.write("dir/c.txt", "c");
    repo.ok(&["add", "."]);

    repo.remove("a.txt");
    let output = repo.ok(&["add", "a.txt"]);
    assert!(output.contains("removed  a.txt"), "{output}");

    fs::remove_dir_all(repo.path("dir")).unwrap();
    repo.ok(&["add", "dir"]);
    assert_eq!(staged(&repo), [".nexusignore"]);

    let before = fs::read(repo.root.join(".nexus/index")).unwrap();
    repo.write("new.txt", "new");
    let output = repo.fails(&["add", "new.txt", "no-such-file.txt"]);
    assert!(
        output.contains("no-such-file.txt doesn't match any files"),
        "{output}"
    );
    assert_eq!(
        fs::read(repo.root.join(".nexus/index")).unwrap(),
        before,
        "a failed add must change nothing"
    );
}

#[test]
fn a_file_replaced_by_a_directory_is_restaged() {
    let repo = TestRepo::new();
    repo.write("thing", "a file");
    repo.ok(&["add", "."]);
    repo.remove("thing");
    repo.write("thing/inside.txt", "now a directory");
    repo.ok(&["add", "."]);
    assert_eq!(staged(&repo), [".nexusignore", "thing/inside.txt"]);
    repo.ok(&["commit", "-m", "directory"]);
}

#[test]
fn unchanged_files_are_not_restaged() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a");
    repo.ok(&["add", "."]);
    let output = repo.ok(&["add", "."]);
    assert!(output.contains("nothing to stage"), "{output}");
}

#[test]
fn exec_flag_marks_files_executable() {
    let repo = TestRepo::new();
    repo.write("run.sh", "#!/bin/sh\necho hi\n");
    repo.write("plain.txt", "text");
    repo.ok(&["add", "--exec", "run.sh"]);
    repo.ok(&["add", "plain.txt"]);
    let index = repo.repo().load_index().unwrap();
    assert_eq!(
        index.get(&RepoPath::parse("run.sh").unwrap()).unwrap().kind,
        FileKind::Exec
    );
    assert_eq!(
        index
            .get(&RepoPath::parse("plain.txt").unwrap())
            .unwrap()
            .kind,
        FileKind::File
    );
    // On Windows the index keeps the kind; on Unix the file is now executable too.
    repo.write("run.sh", "#!/bin/sh\necho changed\n");
    repo.ok(&["add", "run.sh"]);
    let index = repo.repo().load_index().unwrap();
    assert_eq!(
        index.get(&RepoPath::parse("run.sh").unwrap()).unwrap().kind,
        FileKind::Exec
    );
}

#[cfg(unix)]
#[test]
fn reads_the_executable_bit_on_unix_and_skips_symlinks() {
    use std::os::unix::fs::PermissionsExt as _;
    let repo = TestRepo::new();
    repo.write("tool", "#!/bin/sh\n");
    fs::set_permissions(repo.path("tool"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("tool", repo.path("link")).unwrap();
    let output = repo.ok(&["add", "."]);
    assert!(output.contains("symlinks aren't supported"), "{output}");
    let index = repo.repo().load_index().unwrap();
    assert_eq!(
        index.get(&RepoPath::parse("tool").unwrap()).unwrap().kind,
        FileKind::Exec
    );
    assert!(index.get(&RepoPath::parse("link").unwrap()).is_none());

    fs::set_permissions(repo.path("tool"), fs::Permissions::from_mode(0o644)).unwrap();
    repo.ok(&["add", "."]);
    let index = repo.repo().load_index().unwrap();
    assert_eq!(
        index.get(&RepoPath::parse("tool").unwrap()).unwrap().kind,
        FileKind::File
    );
}

#[cfg(target_os = "linux")]
#[test]
fn warns_about_paths_that_break_on_other_systems() {
    let repo = TestRepo::new();
    repo.write("what?.txt", "x");
    repo.write("README.md", "upper");
    repo.write("readme.md", "lower");
    let output = repo.ok(&["add", "."]);
    assert!(
        output.contains("what?.txt can't be checked out on Windows"),
        "{output}"
    );
    assert!(
        output.contains("README.md and readme.md differ only in letter case"),
        "{output}"
    );
}

#[test]
fn rejects_a_damaged_index() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a");
    repo.ok(&["add", "."]);
    let path = repo.root.join(".nexus/index");
    let mut bytes = fs::read(&path).unwrap();
    bytes[14] ^= 0xff;
    fs::write(&path, bytes).unwrap();
    let output = repo.fails(&["debug", "index"]);
    assert!(
        output.contains("the index is corrupt: the checksum doesn't match"),
        "{output}"
    );
    assert!(repo.fails(&["add", "."]).contains("index is corrupt"));
}

#[test]
fn debug_index_lists_entries() {
    let repo = TestRepo::new();
    repo.write("a.txt", "hello\n");
    repo.ok(&["add", "a.txt"]);
    let output = repo.ok(&["debug", "index"]);
    let mut lines = output.lines();
    assert_eq!(lines.next(), Some("index version 1, 1 entries"));
    let entry = lines.next().unwrap();
    assert!(
        entry.starts_with("file 2cf8d83d9ee29543b34a87727421fdecb7e3f3a183d337639025de576db9ebb4"),
        "{entry}"
    );
    assert!(entry.ends_with(" a.txt"), "{entry}");
}

#[test]
fn a_held_lock_blocks_mutating_commands() {
    let repo = TestRepo::new();
    fs::write(
        repo.root.join(".nexus/lock"),
        "pid 1234\ncommand nexus add .\n",
    )
    .unwrap();
    repo.write("a.txt", "a");
    let output = repo.fails(&["add", "."]);
    assert!(
        output.contains("another nexus command is running (pid 1234, command nexus add .)"),
        "{output}"
    );
    // Reading still works.
    repo.ok(&["log"]);
}

#[test]
fn config_reads_and_writes_both_files() {
    let repo = TestRepo::new();
    assert_eq!(repo.ok(&["config", "user.name"]), "Ada Lovelace\n");
    repo.ok(&["config", "user.name", "Repo Override"]);
    assert_eq!(repo.ok(&["config", "user.name"]), "Repo Override\n");
    assert_eq!(
        repo.ok(&["config", "--global", "user.name"]),
        "Ada Lovelace\n"
    );
    repo.ok(&["config", "--global", "user.email", "new@example.com"]);
    assert_eq!(repo.ok(&["config", "user.email"]), "new@example.com\n");
    assert!(
        repo.fails(&["config", "user.email", "not an email"])
            .contains("can't contain")
    );
    let repo_config = fs::read_to_string(repo.root.join(".nexus/config.toml")).unwrap();
    assert!(
        repo_config.starts_with("# This repository's settings."),
        "{repo_config}"
    );
}

#[test]
fn a_same_size_edit_within_the_timestamp_granularity_is_still_staged() {
    use std::time::{Duration, SystemTime};
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    repo.ok(&["add", "a.txt"]);
    // Emulate a coarse filesystem clock: the edit keeps the same size and the
    // same modification time, which also equals the index's.
    let tick = SystemTime::now() - Duration::from_secs(10);
    let set_time = |path: &std::path::Path| {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(tick)
            .unwrap();
    };
    set_time(&repo.path("a.txt"));
    // Restage so the entry records `tick`, with the index written at `tick` too.
    repo.write("a.txt", "one\n");
    set_time(&repo.path("a.txt"));
    repo.ok(&["add", "a.txt"]);
    set_time(&repo.root.join(".nexus/index"));
    repo.write("a.txt", "two\n");
    set_time(&repo.path("a.txt"));
    // An unrelated add rewrites the index with a newer timestamp...
    repo.write("b.txt", "b\n");
    repo.ok(&["add", "b.txt"]);
    // ...and the edit must still be noticed.
    let output = repo.ok(&["add", "a.txt"]);
    assert!(output.contains("staged   a.txt"), "{output}");
}

#[test]
fn a_directory_replaced_by_a_file_is_restaged() {
    let repo = TestRepo::new();
    repo.write("thing/inside.txt", "a directory");
    repo.ok(&["add", "."]);
    fs::remove_dir_all(repo.path("thing")).unwrap();
    repo.write("thing", "now a file");
    repo.ok(&["add", "."]);
    assert_eq!(staged(&repo), [".nexusignore", "thing"]);
}

#[test]
fn naming_a_path_under_a_file_stages_nothing_else() {
    let repo = TestRepo::new();
    repo.write("thing/inside.txt", "a directory");
    repo.ok(&["add", "."]);
    fs::remove_dir_all(repo.path("thing")).unwrap();
    repo.write("thing", "now a file");
    let output = repo.ok(&["add", "thing/inside.txt"]);
    assert!(output.contains("removed  thing/inside.txt"), "{output}");
    assert_eq!(
        staged(&repo),
        [".nexusignore"],
        "`thing` itself wasn't named"
    );
}

#[test]
fn explains_named_paths_that_match_nothing() {
    let repo = TestRepo::new();
    fs::create_dir_all(repo.path("empty")).unwrap();
    let output = repo.ok(&["add", "empty"]);
    assert!(output.contains("empty/ has nothing to stage"), "{output}");

    repo.write("Readme.md", "x");
    let result = repo.run(&["add", "readme.md"]);
    // Case-insensitive filesystems find the file by another spelling; others don't.
    if repo.path("readme.md").exists() {
        assert_eq!(result.exit_code, 1);
        assert!(
            result.text().contains("did you mean Readme.md?"),
            "{}",
            result.text()
        );
    } else {
        assert!(
            result.text().contains("doesn't match any files"),
            "{}",
            result.text()
        );
    }
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
fn a_case_only_rename_replaces_the_old_name() {
    let repo = TestRepo::new();
    repo.write("readme.md", "x");
    repo.write("Docs/guide.md", "y");
    repo.ok(&["add", "."]);
    fs::rename(repo.path("readme.md"), repo.path("README.md")).unwrap();
    fs::rename(repo.path("Docs"), repo.path("docs")).unwrap();
    repo.ok(&["add", "."]);
    assert_eq!(
        staged(&repo),
        [".nexusignore", "README.md", "docs/guide.md"]
    );
}

/// A tracked directory replaced by a link to somewhere else must not pull
/// that content into the repository.
#[test]
fn tracked_files_are_not_read_through_linked_directories() {
    let repo = TestRepo::new();
    repo.write("assets/logo.txt", "inside");
    repo.ok(&["add", "."]);
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("logo.txt"), "OUTSIDE THE REPOSITORY").unwrap();
    fs::remove_dir_all(repo.path("assets")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), repo.path("assets")).unwrap();
    #[cfg(windows)]
    {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(repo.path("assets"))
            .arg(outside.path())
            .output()
            .unwrap()
            .status;
        assert!(status.success(), "couldn't create a junction");
    }
    let output = repo.ok(&["add", "."]);
    assert!(output.contains("removed  assets/logo.txt"), "{output}");
    assert!(!staged(&repo).contains(&"assets/logo.txt".to_owned()));
}

#[test]
fn absolute_paths_are_accepted_whatever_route_they_take() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a");
    let canonical = fs::canonicalize(repo.path("a.txt")).unwrap();
    let output = repo.ok(&["add", canonical.to_str().unwrap()]);
    assert!(output.contains("staged   a.txt"), "{output}");
}

/// A failed `add --exec` leaves file modes alone.
#[cfg(unix)]
#[test]
fn a_failed_add_exec_changes_no_modes() {
    use std::os::unix::fs::PermissionsExt as _;
    let repo = TestRepo::new();
    repo.write("a.sh", "a\n");
    repo.write("b.sh", "b\n");
    fs::set_permissions(repo.path("b.sh"), fs::Permissions::from_mode(0o000)).unwrap();
    let result = repo.run(&["add", "--exec", "a.sh", "b.sh"]);
    fs::set_permissions(repo.path("b.sh"), fs::Permissions::from_mode(0o644)).unwrap();
    if result.exit_code == 0 {
        // Running as root, which reads the file anyway.
        return;
    }
    let mode = fs::metadata(repo.path("a.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0, "{mode:o}");
}
