//! The working tree: `status`, `diff`, `show`, `checkout`, `restore`, `rm`,
//! and `export`.

mod common;

use std::collections::BTreeMap;

use common::{TestRepo, pseudo_random};
use nexus_core::hash::ObjectId;
use nexus_core::path::RepoPath;

/// A file big enough to be stored as chunks.
const LARGE: usize = 9 * 1024 * 1024;

fn commit_all(repo: &TestRepo, message: &str) -> ObjectId {
    repo.ok(&["add", "."]);
    repo.ok(&["commit", "-m", message]);
    repo.head_commit()
}

/// Asserts that the working tree holds exactly `commit`'s files, byte for
/// byte, with the executable bit where the tree says so (on Unix).
fn assert_matches_commit(repo: &TestRepo, commit: &ObjectId) {
    let expected = repo.files_at(commit);
    let on_disk = repo.disk_files();
    assert_eq!(
        on_disk.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>(),
        "the working tree has different files from {commit}"
    );
    for (path, bytes) in &expected {
        assert!(on_disk[path] == *bytes, "{path} differs from {commit}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let odb = repo.repo();
        let tree = repo.commit(commit).tree;
        for file in nexus_core::tree::flatten(odb.odb(), &tree).expect("the tree reads") {
            let mode = std::fs::metadata(repo.path(file.path.as_str()))
                .expect("the file exists")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o111 != 0,
                file.kind == nexus_core::index::FileKind::Exec,
                "{} has mode {mode:o}",
                file.path
            );
        }
    }
    assert!(
        repo.ok(&["status"])
            .contains("nothing to commit, working tree clean"),
        "{}",
        repo.ok(&["status"])
    );
}

#[test]
fn checking_out_each_of_three_commits_restores_the_exact_bytes() {
    let repo = TestRepo::new();
    let mut big = pseudo_random(LARGE, 7);
    repo.write("a.txt", "hello\n");
    repo.write("data.bin", b"\x00\x01\x02binary\x00\xff");
    repo.write("crlf.txt", "one\r\ntwo\r\n");
    repo.write("nested/dir/x.txt", "deep\n");
    repo.write("big.bin", &big);
    repo.write("tool.sh", "#!/bin/sh\necho hi\n");
    repo.ok(&["add", "."]);
    repo.ok(&["add", "--exec", "tool.sh"]);
    repo.ok(&["commit", "-m", "first"]);
    let first = repo.head_commit();

    repo.write("a.txt", "hello\nworld\n");
    repo.remove("nested/dir/x.txt");
    repo.write("nested/y.txt", "shallower\n");
    big[LARGE / 2..LARGE / 2 + 1024 * 1024].fill(0x5a);
    repo.write("big.bin", &big);
    repo.write("bin/run", "#!/bin/sh\n");
    repo.ok(&["add", "."]);
    repo.ok(&["add", "--exec", "bin/run"]);
    repo.ok(&["commit", "-m", "second"]);
    let second = repo.head_commit();

    // A directory becomes a file, and the large file goes away.
    std::fs::remove_dir_all(repo.path("nested")).unwrap();
    repo.write("nested", "now a file\n");
    repo.remove("big.bin");
    let third = commit_all(&repo, "third");
    repo.ok(&["tag", "v3"]);

    for (spec, commit) in [
        (first.short(), first),
        (second.to_string()[..6].to_owned(), second),
        ("v3".to_owned(), third),
        (first.to_string(), first),
        ("main".to_owned(), third),
    ] {
        repo.ok(&["checkout", &spec]);
        assert_matches_commit(&repo, &commit);
    }
}

#[test]
fn checkout_with_uncommitted_changes_refuses_and_changes_nothing() {
    let repo = TestRepo::new();
    repo.write("a.txt", "1\n");
    repo.write("b.txt", "1\n");
    repo.write("c.txt", "1\n");
    commit_all(&repo, "base");
    repo.ok(&["branch", "other"]);
    repo.write("a.txt", "2\n");
    repo.write("b.txt", "2\n");
    repo.write("c.txt", "2\n");
    commit_all(&repo, "main moves on");

    // a.txt: changed but not staged; b.txt: staged; c.txt: a new file staged
    // where the other branch has a different one.
    repo.write("a.txt", "local edit\n");
    repo.write("b.txt", "staged edit\n");
    repo.ok(&["add", "b.txt"]);
    repo.ok(&["rm", "--cached", "c.txt"]);
    repo.write("c.txt", "brand new\n");
    repo.ok(&["add", "c.txt"]);
    let before = repo.snapshot();
    let ops = repo.op_count();
    let output = repo.fails(&["checkout", "other"]);
    for expected in [
        "a.txt (changes not staged)",
        "b.txt (staged changes)",
        "c.txt (staged changes)",
        "nothing was changed",
    ] {
        assert!(output.contains(expected), "{output}");
    }
    assert_eq!(repo.snapshot(), before);
    assert_eq!(repo.op_count(), ops);
}

#[test]
fn changes_to_files_the_switch_does_not_touch_are_carried_over() {
    let repo = TestRepo::new();
    repo.write("same.txt", "same\n");
    repo.write("differs.txt", "1\n");
    commit_all(&repo, "base");
    repo.ok(&["branch", "other"]);
    repo.write("differs.txt", "2\n");
    commit_all(&repo, "main moves on");

    repo.write("same.txt", "edited\n");
    repo.ok(&["checkout", "other"]);
    assert_eq!(repo.read("differs.txt"), b"1\n");
    assert_eq!(repo.read("same.txt"), b"edited\n");
    assert!(repo.ok(&["status"]).contains("modified:   same.txt"));
}

#[test]
fn staged_changes_the_switch_does_not_touch_are_carried_over_too() {
    let repo = TestRepo::new();
    repo.write("same.txt", "same\n");
    repo.write("differs.txt", "1\n");
    commit_all(&repo, "base");
    repo.ok(&["branch", "other"]);
    repo.write("differs.txt", "2\n");
    commit_all(&repo, "main moves on");

    // Staged: an edit, a new file, and a deletion, all of paths the branches agree on.
    repo.write("same.txt", "staged edit\n");
    repo.write("new.txt", "staged new file\n");
    repo.ok(&["add", "same.txt", "new.txt"]);
    repo.ok(&["checkout", "other"]);
    assert_eq!(repo.read("differs.txt"), b"1\n");
    let status = repo.ok(&["status"]);
    assert!(
        status.contains("modified:   same.txt") && status.contains("new file:   new.txt"),
        "{status}"
    );
    assert!(!status.contains("Changes not staged"), "{status}");

    // A staged change that already matches the target needs nothing written.
    repo.ok(&["checkout", "main"]);
    repo.write("differs.txt", "1\n");
    repo.ok(&["add", "differs.txt"]);
    repo.ok(&["checkout", "other"]);
    assert!(!repo.ok(&["status"]).contains("differs.txt"));
}

#[test]
fn a_locally_deleted_file_is_replaced_by_the_target_version() {
    // Its content is in the index, so the switch loses nothing, as in Git.
    let repo = TestRepo::new();
    repo.write("a.txt", "1\n");
    repo.write("gone.txt", "1\n");
    commit_all(&repo, "base");
    repo.ok(&["branch", "other"]);
    repo.write("a.txt", "2\n");
    repo.remove("gone.txt");
    commit_all(&repo, "main moves on");
    repo.ok(&["checkout", "other"]);
    repo.remove("a.txt");
    repo.ok(&["checkout", "main"]);
    assert_eq!(repo.read("a.txt"), b"2\n");
    assert!(!repo.exists("gone.txt"));
    assert!(repo.ok(&["status"]).contains("working tree clean"));
}

#[cfg(unix)]
#[test]
fn checkout_never_writes_or_deletes_through_a_symlinked_directory() {
    let repo = TestRepo::new();
    repo.write("dir/a.txt", "1\n");
    repo.write("dir/b.txt", "1\n");
    commit_all(&repo, "base");
    repo.ok(&["branch", "other"]);
    repo.write("dir/a.txt", "2\n");
    repo.remove("dir/b.txt");
    commit_all(&repo, "main moves on");

    // Replace the directory with a link to somewhere outside the repository.
    let outside = repo.root.parent().expect("a parent").join("outside");
    std::fs::create_dir_all(&outside).expect("create a directory");
    std::fs::write(outside.join("a.txt"), "not yours\n").expect("write a file");
    std::fs::remove_dir_all(repo.path("dir")).expect("remove the directory");
    std::os::unix::fs::symlink(&outside, repo.path("dir")).expect("make a symlink");
    let output = repo.fails(&["checkout", "other"]);
    assert!(
        output.contains("the untracked file dir is in the way"),
        "{output}"
    );
    assert_eq!(
        std::fs::read(outside.join("a.txt")).expect("read"),
        b"not yours\n"
    );
    assert!(!outside.join("b.txt").exists());
}

#[test]
fn untracked_and_ignored_files_survive_checkout() {
    let repo = TestRepo::new();
    repo.write("a.txt", "1\n");
    commit_all(&repo, "base");
    repo.ok(&["branch", "other"]);
    repo.write("a.txt", "2\n");
    repo.write("added.txt", "only on main\n");
    commit_all(&repo, "main moves on");

    repo.write("notes.txt", "mine\n");
    repo.write("target/debug/out.o", "build output\n");
    repo.write(".env", "SECRET=1\n");
    repo.ok(&["checkout", "other"]);
    assert!(!repo.exists("added.txt"));
    repo.ok(&["checkout", "main"]);
    assert_eq!(repo.read("notes.txt"), b"mine\n");
    assert_eq!(repo.read("target/debug/out.o"), b"build output\n");
    assert_eq!(repo.read(".env"), b"SECRET=1\n");
}

#[test]
fn checkout_refuses_to_overwrite_untracked_files_in_the_way() {
    let repo = TestRepo::new();
    repo.write("a.txt", "1\n");
    commit_all(&repo, "base");
    repo.ok(&["branch", "other"]);
    repo.ok(&["checkout", "other"]);
    repo.write("new.txt", "tracked on other\n");
    repo.write("dir/inner.txt", "tracked on other\n");
    commit_all(&repo, "other adds files");
    repo.ok(&["checkout", "main"]);
    assert!(!repo.exists("new.txt") && !repo.exists("dir"));

    // An untracked file, and an untracked directory, in the way.
    repo.write("new.txt", "untracked\n");
    repo.write("dir/inner.txt/x", "untracked\n");
    let before = repo.snapshot();
    let output = repo.fails(&["checkout", "other"]);
    assert!(
        output.contains("new.txt (an untracked file is in the way)"),
        "{output}"
    );
    assert!(
        output.contains("dir/inner.txt (an untracked directory is in the way)"),
        "{output}"
    );
    assert_eq!(repo.snapshot(), before);

    // An untracked file where a tracked file's directory must go.
    repo.remove("new.txt");
    std::fs::remove_dir_all(repo.path("dir")).unwrap();
    repo.write("dir", "untracked file\n");
    let output = repo.fails(&["checkout", "other"]);
    assert!(
        output.contains("the untracked file dir is in the way"),
        "{output}"
    );
    repo.remove("dir");
    repo.ok(&["checkout", "other"]);
}

#[test]
fn checkout_moves_head_and_records_one_op() {
    let repo = TestRepo::new();
    repo.write("a.txt", "1\n");
    let base = commit_all(&repo, "base");
    repo.ok(&["branch", "feature"]);
    let ops = repo.op_count();
    let output = repo.ok(&["checkout", "feature"]);
    assert_eq!(output, "Switched to branch 'feature' (0 files changed)\n");
    assert_eq!(repo.op_count(), ops + 1);
    assert_eq!(repo.latest_op().1.command, "nexus checkout feature");
    assert_eq!(repo.ok(&["checkout", "feature"]), "Already on 'feature'\n");
    assert_eq!(repo.op_count(), ops + 1);

    // Detached: a warning, and commits advance HEAD itself.
    let output = repo.ok(&["checkout", &base.short()]);
    assert!(
        output.contains("HEAD is now at") && output.contains("detached"),
        "{output}"
    );
    assert!(
        repo.ok(&["status"])
            .starts_with(&format!("HEAD detached at {}", base.short()))
    );
    repo.write("a.txt", "2\n");
    let detached = commit_all(&repo, "on a detached HEAD");
    assert_eq!(repo.commit(&detached).parents, [base]);
    let refs = repo.repo().refs().list().unwrap();
    assert!(refs.values().all(|id| *id == base), "{refs:?}");
    assert!(
        repo.ok(&["branch"])
            .starts_with(&format!("* (HEAD detached at {})", detached.short()))
    );
}

#[test]
fn checkout_of_an_unknown_name_fails() {
    let repo = TestRepo::new();
    repo.write("a.txt", "1\n");
    commit_all(&repo, "base");
    let output = repo.fails(&["checkout", "nope"]);
    assert!(
        output.contains("nope isn't a branch, a tag, or an object ID"),
        "{output}"
    );
}

#[test]
fn status_reports_what_git_would() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.write("b.txt", "b\n");
    repo.write("staged.txt", "s\n");
    repo.write("gone.txt", "g\n");
    commit_all(&repo, "base");

    repo.write("new.txt", "n\n");
    repo.write("staged.txt", "s2\n");
    repo.remove("gone.txt");
    repo.ok(&["add", "new.txt", "staged.txt", "gone.txt"]);
    repo.write("a.txt", "a2\n");
    repo.remove("b.txt");
    repo.write("u.txt", "u\n");
    repo.write("dir/deep/file.txt", "f\n");
    assert_eq!(
        repo.ok(&["status"]),
        "On branch main
Changes to be committed:
  (use \"nexus restore --staged <file>...\" to unstage)
	deleted:    gone.txt
	new file:   new.txt
	modified:   staged.txt

Changes not staged for commit:
  (use \"nexus add <file>...\" to update what will be committed)
  (use \"nexus restore <file>...\" to discard changes in the working tree)
	modified:   a.txt
	deleted:    b.txt

Untracked files:
  (use \"nexus add <file>...\" to include in what will be committed)
	dir/
	u.txt

"
    );
}

#[test]
fn status_on_an_unborn_branch_and_a_clean_tree() {
    let repo = TestRepo::new();
    assert_eq!(
        repo.ok(&["status"]),
        "On branch main\n\nNo commits yet\n\nUntracked files:\n  (use \"nexus add <file>...\" to include in what \
         will be committed)\n\t.nexusignore\n\nnothing added to commit but untracked files present (use \"nexus \
         add\" to track)\n"
    );
    commit_all(&repo, "first");
    assert_eq!(
        repo.ok(&["status"]),
        "On branch main\nnothing to commit, working tree clean\n"
    );
}

#[test]
fn status_notices_a_same_size_edit_right_after_staging() {
    // The edit lands within the index's own timestamp granularity, so the
    // cached size and time match; the racy-timestamp rule must rehash it.
    let repo = TestRepo::new();
    repo.write("r.txt", "aaa\n");
    repo.ok(&["add", "r.txt"]);
    repo.write("r.txt", "bbb\n");
    assert!(repo.ok(&["status"]).contains("modified:   r.txt"));
}

#[test]
fn status_refreshes_the_index_without_recording_an_op() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    commit_all(&repo, "base");
    let ops = repo.op_count();
    repo.ok(&["status"]);
    repo.ok(&["status"]);
    assert_eq!(repo.op_count(), ops);
}

#[test]
fn diff_compares_the_working_tree_with_the_index() {
    let repo = TestRepo::new();
    repo.write("a.txt", "one\ntwo\nthree\n");
    repo.write("b.txt", "b\n");
    commit_all(&repo, "base");
    assert_eq!(repo.ok(&["diff"]), "");

    repo.write("a.txt", "one\n2\nthree\nfour");
    repo.remove("b.txt");
    let a_old =
        nexus_core::object::id_of(nexus_core::object::ObjectKind::Blob, b"one\ntwo\nthree\n");
    let a_new =
        nexus_core::object::id_of(nexus_core::object::ObjectKind::Blob, b"one\n2\nthree\nfour");
    let b_old = nexus_core::object::id_of(nexus_core::object::ObjectKind::Blob, b"b\n");
    assert_eq!(
        repo.ok(&["diff"]),
        format!(
            "diff --git a/a.txt b/a.txt
index {}..{} 100644
--- a/a.txt
+++ b/a.txt
@@ -1,3 +1,4 @@
 one
-two
+2
 three
+four
\\ No newline at end of file
diff --git a/b.txt b/b.txt
deleted file mode 100644
index {}..000000000000
--- a/b.txt
+++ /dev/null
@@ -1 +0,0 @@
-b
",
            a_old.short(),
            a_new.short(),
            b_old.short()
        )
    );
    // Path filters, after `--` or on their own.
    assert!(!repo.ok(&["diff", "--", "a.txt"]).contains("b.txt"));
    assert!(!repo.ok(&["diff", "b.txt"]).contains("a.txt"));
    let output = repo.fails(&["diff", "nonexistent"]);
    assert!(
        output.contains("isn't a branch, a tag, or an object ID")
            && output.contains("put it after `--`"),
        "{output}"
    );
    // A name that is both a commit and a path is ambiguous, as in Git.
    repo.ok(&["branch", "a.txt"]);
    let output = repo.fails(&["diff", "a.txt"]);
    assert!(output.contains("both a commit and a path"), "{output}");
    assert!(repo.ok(&["diff", "--", "a.txt"]).contains("+four"));
}

#[test]
fn diff_staged_and_between_commits() {
    let repo = TestRepo::new();
    repo.write("a.txt", "1\n");
    let first = commit_all(&repo, "first");
    repo.write("a.txt", "2\n");
    repo.write("new.txt", "new\n");
    repo.ok(&["add", "a.txt"]);
    let staged = repo.ok(&["diff", "--staged"]);
    assert!(
        staged.contains("-1\n+2\n") && !staged.contains("new.txt"),
        "{staged}"
    );
    assert_eq!(repo.ok(&["diff"]), "");

    repo.ok(&["add", "new.txt"]);
    repo.ok(&["commit", "-m", "second"]);
    let second = repo.head_commit();
    let between = repo.ok(&["diff", &first.short(), &second.short()]);
    assert!(
        between.contains("-1\n+2\n") && between.contains("new file mode 100644"),
        "{between}"
    );
    let reversed = repo.ok(&["diff", &second.short(), &first.short()]);
    assert!(
        reversed.contains("-2\n+1\n") && reversed.contains("deleted file mode"),
        "{reversed}"
    );
    // One commit: the working tree against it.
    repo.write("a.txt", "3\n");
    let against = repo.ok(&["diff", &first.short(), "--", "a.txt"]);
    assert!(against.contains("-1\n+3\n"), "{against}");
    assert!(
        repo.ok(&["diff", "--staged", &first.short()])
            .contains("-1\n+2\n")
    );
}

#[test]
fn diff_options_change_context_and_algorithm() {
    let repo = TestRepo::new();
    let old = (1..=10)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    repo.write("n.txt", &old);
    commit_all(&repo, "base");
    repo.write("n.txt", old.replace("5\n", "five\n"));
    let narrow = repo.ok(&["diff", "-U", "1"]);
    assert!(
        narrow.contains("@@ -4,3 +4,3 @@\n 4\n-5\n+five\n 6\n"),
        "{narrow}"
    );
    let myers = repo.ok(&["diff", "--diff-algorithm", "myers"]);
    assert!(myers.contains("@@ -2,7 +2,7 @@"), "{myers}");
}

#[test]
fn diff_reports_binary_large_and_line_ending_changes() {
    let repo = TestRepo::new();
    let mut big = pseudo_random(LARGE, 3);
    repo.write("data.bin", b"\x00one");
    repo.write("big.bin", &big);
    repo.write("crlf.txt", "a\nb\n");
    commit_all(&repo, "base");
    repo.write("data.bin", b"\x00two");
    big[100..200].fill(1);
    repo.write("big.bin", &big);
    repo.write("crlf.txt", "a\r\nb\r\n");
    let output = repo.ok(&["diff"]);
    assert!(
        output.contains("Binary files a/data.bin and b/data.bin differ"),
        "{output}"
    );
    assert!(
        output.contains("Large file: 9.0 MiB -> 9.0 MiB, 1 of "),
        "{output}"
    );
    // The note sits after the hunk header's closing `@@`, where `git apply`
    // ignores text.
    assert!(
        output.contains("@@ (only the line endings changed: CRLF and LF)\n"),
        "{output}"
    );
}

#[test]
fn diff_lines_carry_the_exact_bytes_and_display_safely() {
    let repo = TestRepo::new();
    repo.write("f.txt", b"caf\xe9\ncolor \x1b[31mred\x1b[0m\nlast\r\n");
    commit_all(&repo, "base");
    repo.write("f.txt", b"caf\xe9\ncolor \x1b[31mred\x1b[0m\nLAST\r\n");
    let result = repo.run(&["diff"]);
    assert_eq!(result.exit_code, 0);
    let lines: Vec<_> = result
        .lines
        .iter()
        .filter(|line| line.bytes.is_some() && !line.text.starts_with("@@"))
        .collect();
    let bytes: Vec<&[u8]> = lines
        .iter()
        .map(|line| line.bytes.as_deref().unwrap())
        .collect();
    assert_eq!(
        bytes,
        [
            &b" caf\xe9"[..],
            b" color \x1b[31mred\x1b[0m",
            b"-last\r",
            b"+LAST\r"
        ]
    );
    // What a terminal gets: no escape sequences, no stray CR.
    let shown: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
    assert_eq!(
        shown,
        [" caf\u{fffd}", " color ^[[31mred^[[0m", "-last", "+LAST"]
    );
}

#[test]
fn added_and_deleted_empty_files_have_no_hunk_headers() {
    let repo = TestRepo::new();
    repo.write("gone.txt", "");
    commit_all(&repo, "base");
    repo.ok(&["rm", "gone.txt"]);
    repo.write("new.txt", "");
    repo.ok(&["add", "new.txt"]);
    let output = repo.ok(&["diff", "--staged"]);
    assert!(
        output.contains("deleted file mode 100644") && output.contains("new file mode 100644"),
        "{output}"
    );
    assert!(
        !output.contains("---") && !output.contains("+++"),
        "{output}"
    );
}

#[test]
fn a_huge_context_is_one_hunk() {
    let repo = TestRepo::new();
    let old = (1..=30)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    repo.write("n.txt", &old);
    commit_all(&repo, "base");
    repo.write(
        "n.txt",
        old.replace("5\n", "five\n").replace("13\n", "thirteen\n"),
    );
    let output = repo.ok(&["diff", "-U", "4294967295"]);
    assert_eq!(output.matches("@@ -").count(), 1, "{output}");
}

#[test]
fn diff_against_a_commit_uses_the_index_for_what_is_tracked() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.write("b.txt", "b\n");
    commit_all(&repo, "base");
    repo.ok(&["rm", "--cached", "b.txt"]);
    repo.write("b.txt", "b2\n");
    // b.txt is untracked now, so against HEAD it's deleted, as `git diff HEAD` says.
    let output = repo.ok(&["diff", "HEAD"]);
    assert!(
        output.contains("deleted file mode 100644") && output.contains("-b\n"),
        "{output}"
    );
    assert!(!output.contains("+b2"), "{output}");
}

#[test]
fn status_shows_paths_relative_to_the_current_directory() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.write("dir/b.txt", "b\n");
    commit_all(&repo, "base");
    repo.write("a.txt", "a2\n");
    repo.write("dir/b.txt", "b2\n");
    repo.write("dir/new.txt", "n\n");
    repo.write("other/x.txt", "x\n");
    let output = repo.run_in(&repo.path("dir"), &["status"]).text();
    for line in [
        "modified:   ../a.txt",
        "modified:   b.txt",
        "\tnew.txt",
        "\t../other/",
    ] {
        assert!(output.contains(line), "{line}: {output}");
    }
    // The hint works from there.
    assert_eq!(
        repo.run_in(&repo.path("dir"), &["restore", "b.txt"])
            .exit_code,
        0
    );
    assert_eq!(repo.read("dir/b.txt"), b"b\n");
}

#[test]
fn a_different_size_counts_as_modified_without_reading_the_file() {
    use nexus_core::worktree::{Checker, Working};
    let repo = TestRepo::new();
    repo.write("a.txt", "short\n");
    commit_all(&repo, "base");
    let store = repo.repo();
    let path = RepoPath::parse("a.txt").unwrap();
    let entry = *store.load_index().unwrap().get(&path).unwrap();
    // A trusted cached size (the time is set) that differs from the file's.
    let cached = nexus_core::index::IndexEntry {
        size: 999,
        mtime_ns: 1,
        ..entry
    };
    let checked = Checker::new(&store)
        .unwrap()
        .check_all(vec![(path.clone(), cached)], BTreeMap::new())
        .unwrap();
    assert!(
        matches!(&checked[0].1, Working::Modified(file) if file.id.is_none()),
        "{checked:?}"
    );
    // Without a cached time (as `restore --staged` leaves it), the file is hashed.
    let uncached = nexus_core::index::IndexEntry {
        size: 0,
        mtime_ns: 0,
        ..entry
    };
    let checked = Checker::new(&store)
        .unwrap()
        .check_all(vec![(path, uncached)], BTreeMap::new())
        .unwrap();
    assert!(
        matches!(&checked[0].1, Working::Clean { .. }),
        "{checked:?}"
    );
}

#[cfg(windows)]
#[test]
fn status_counts_a_file_another_program_has_locked_as_modified() {
    use std::os::windows::fs::OpenOptionsExt as _;
    let repo = TestRepo::new();
    repo.write("locked.txt", "1\n");
    repo.write("other.txt", "1\n");
    commit_all(&repo, "base");
    repo.write("locked.txt", "2\n");
    // Share mode 0: nobody else may open it, as databases and VM images do.
    let _held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(repo.path("locked.txt"))
        .unwrap();
    let output = repo.ok(&["status"]);
    assert!(output.contains("modified:   locked.txt"), "{output}");
}

#[cfg(windows)]
#[test]
fn checkout_and_restore_replace_read_only_files() {
    let repo = TestRepo::new();
    repo.write("a.txt", "1\n");
    commit_all(&repo, "base");
    repo.ok(&["branch", "other"]);
    repo.write("a.txt", "2\n");
    commit_all(&repo, "main moves on");
    let read_only = |yes: bool| {
        let mut permissions = std::fs::metadata(repo.path("a.txt")).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(yes);
        std::fs::set_permissions(repo.path("a.txt"), permissions).unwrap();
    };
    read_only(true);
    repo.ok(&["checkout", "other"]);
    assert_eq!(repo.read("a.txt"), b"1\n");
    read_only(true);
    std::fs::write(repo.path("a.txt"), "x").unwrap_err();
    repo.ok(&["restore", "--source", "main", "a.txt"]);
    assert_eq!(repo.read("a.txt"), b"2\n");
}

#[test]
fn restore_records_an_op_even_when_nothing_needed_saving() {
    let repo = TestRepo::new();
    repo.write("f.txt", "f\n");
    commit_all(&repo, "base");
    repo.remove("f.txt");
    let ops = repo.op_count();
    repo.ok(&["restore", "f.txt"]);
    assert_eq!(repo.read("f.txt"), b"f\n");
    assert_eq!(repo.op_count(), ops + 1);
    assert_eq!(repo.latest_op().1.command, "nexus restore f.txt");
}

#[test]
fn checkout_replaces_a_directory_left_with_only_empty_directories() {
    let repo = TestRepo::new();
    repo.write("a/x", "x\n");
    commit_all(&repo, "base");
    repo.ok(&["branch", "other"]);
    repo.ok(&["checkout", "other"]);
    repo.ok(&["rm", "a/x"]);
    repo.write("a", "now a file\n");
    commit_all(&repo, "a becomes a file");
    repo.ok(&["checkout", "main"]);
    std::fs::create_dir_all(repo.path("a/empty/deeper")).unwrap();
    repo.ok(&["checkout", "other"]);
    assert_eq!(repo.read("a"), b"now a file\n");
}

#[test]
fn trees_can_never_write_into_the_repository_directory() {
    use nexus_core::index::{FileKind, Index, IndexEntry};
    use nexus_core::object::ObjectKind;
    let repo = TestRepo::new();
    repo.write("base.txt", "base\n");
    commit_all(&repo, "base");
    // A tree built by hand (nexus add never stages `.nexus`).
    let store = repo.repo();
    let id = store.odb().write(ObjectKind::Blob, b"hijacked\n").unwrap();
    let mut index = Index::default();
    for name in [".nexus/hooks/evil", ".NEXUS/x", "sub/.nexus/y", "base.txt"] {
        let entry = IndexEntry {
            kind: FileKind::File,
            id,
            size: 0,
            mtime_ns: 0,
        };
        index.insert(RepoPath::parse(name).unwrap(), entry);
    }
    let tree =
        nexus_core::tree::write_index_tree(&index, nexus_core::content::Sink::Store(store.odb()))
            .unwrap();
    let commit = nexus_core::object::Commit {
        tree,
        parents: vec![repo.head_commit()],
        author: repo.commit(&repo.head_commit()).author,
        message: "evil\n".to_owned(),
    };
    let commit = store
        .odb()
        .write(ObjectKind::Commit, &commit.encode())
        .unwrap();
    let output = repo.fails(&["checkout", &commit.to_string()]);
    assert!(
        output.contains(".nexus/hooks/evil (the name can't be used on this system)"),
        "{output}"
    );
    assert!(
        output.contains("sub/.nexus/y (the name can't be used"),
        "{output}"
    );
    assert!(!repo.path(".nexus/hooks").exists() && !repo.exists("sub"));
}

#[cfg(unix)]
#[test]
fn names_with_control_characters_are_quoted() {
    let repo = TestRepo::new();
    repo.write("ok.txt", "x\n");
    commit_all(&repo, "base");
    repo.write("evil\x1b]0;title\x07.txt", "x\n");
    let output = repo.ok(&["status"]);
    assert!(output.contains("\"evil\\033]0;title\\a.txt\""), "{output}");
    assert!(!output.contains('\x1b'), "{output}");
}

#[test]
fn show_prints_the_header_and_the_diff_against_the_first_parent() {
    let repo = TestRepo::new();
    repo.write("a.txt", "1\n");
    let first = commit_all(&repo, "first");
    repo.write("a.txt", "2\n");
    let second = commit_all(&repo, "second");
    repo.ok(&["tag", "v2"]);

    let output = repo.ok(&["show"]);
    assert!(
        output.starts_with(&format!(
            "commit {second} (HEAD -> main, tag: v2)\nAuthor: Ada Lovelace <ada@example.com>\n"
        )),
        "{output}"
    );
    assert!(
        output.contains("    second\n\ndiff --git a/a.txt b/a.txt\n"),
        "{output}"
    );
    assert!(output.ends_with("@@ -1 +1 @@\n-1\n+2\n"), "{output}");
    assert_eq!(repo.ok(&["show", "v2"]), output);

    // A root commit shows every file as new.
    let root = repo.ok(&["show", &first.short()]);
    assert!(
        root.contains("new file mode 100644") && root.contains("+++ b/a.txt"),
        "{root}"
    );
}

#[test]
fn restore_discards_changes_and_saves_them_in_the_op() {
    let repo = TestRepo::new();
    repo.write("a.txt", "committed\n");
    repo.write("b.txt", "committed\n");
    commit_all(&repo, "base");
    repo.write("a.txt", "precious work\n");
    repo.remove("b.txt");
    let ops = repo.op_count();
    assert_eq!(
        repo.ok(&["restore", "a.txt", "b.txt"]),
        "Restored 2 files\n"
    );
    assert_eq!(repo.read("a.txt"), b"committed\n");
    assert_eq!(repo.read("b.txt"), b"committed\n");
    assert_eq!(repo.op_count(), ops + 1);
    let saved = repo.saved_files();
    assert_eq!(
        saved,
        BTreeMap::from([("a.txt".to_owned(), b"precious work\n".to_vec())])
    );
    assert!(repo.ok(&["status"]).contains("working tree clean"));

    // Restoring a directory restores what's inside.
    repo.write("dir/x.txt", "x\n");
    commit_all(&repo, "dir");
    repo.write("dir/x.txt", "changed\n");
    repo.ok(&["restore", "dir"]);
    assert_eq!(repo.read("dir/x.txt"), b"x\n");

    let output = repo.fails(&["restore", "nothing-here.txt"]);
    assert!(
        output.contains("doesn't match any tracked files"),
        "{output}"
    );
}

#[test]
fn restore_from_a_commit_overwrites_and_deletes_and_keeps_the_index() {
    let repo = TestRepo::new();
    repo.write("a.txt", "1\n");
    let first = commit_all(&repo, "first");
    repo.write("a.txt", "2\n");
    repo.write("b.txt", "added later\n");
    commit_all(&repo, "second");

    repo.ok(&["restore", "--source", &first.short(), "."]);
    assert_eq!(repo.read("a.txt"), b"1\n");
    assert!(!repo.exists("b.txt"));
    assert_eq!(
        repo.saved_files(),
        BTreeMap::from([
            ("a.txt".to_owned(), b"2\n".to_vec()),
            ("b.txt".to_owned(), b"added later\n".to_vec()),
        ])
    );
    // The index still has the second commit's files.
    let status = repo.ok(&["status"]);
    assert!(
        status.contains("modified:   a.txt") && status.contains("deleted:    b.txt"),
        "{status}"
    );
    assert!(!status.contains("Changes to be committed"), "{status}");
}

#[test]
fn restore_staged_unstages_and_leaves_the_file_alone() {
    let repo = TestRepo::new();
    repo.write("a.txt", "1\n");
    commit_all(&repo, "first");
    repo.write("a.txt", "2\n");
    repo.write("new.txt", "new\n");
    repo.ok(&["add", "."]);
    assert_eq!(
        repo.ok(&["restore", "--staged", "a.txt", "new.txt"]),
        "Unstaged 2 files\n"
    );
    assert_eq!(repo.read("a.txt"), b"2\n");
    assert_eq!(repo.read("new.txt"), b"new\n");
    let status = repo.ok(&["status"]);
    assert!(!status.contains("Changes to be committed"), "{status}");
    assert!(
        status.contains("modified:   a.txt") && status.contains("\tnew.txt"),
        "{status}"
    );

    // From another commit, and on an unborn branch.
    let unborn = TestRepo::new();
    unborn.write("x.txt", "x\n");
    unborn.ok(&["add", "x.txt"]);
    unborn.ok(&["restore", "--staged", "x.txt"]);
    assert!(unborn.repo().load_index().unwrap().is_empty());
}

#[test]
fn rm_deletes_files_and_stops_tracking_them() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.write("dir/b.txt", "b\n");
    repo.write("dir/c.txt", "c\n");
    repo.write("keep.txt", "k\n");
    commit_all(&repo, "base");

    assert_eq!(repo.ok(&["rm", "a.txt"]), "removed  a.txt\n");
    assert!(!repo.exists("a.txt"));
    assert_eq!(
        repo.saved_files(),
        BTreeMap::from([("a.txt".to_owned(), b"a\n".to_vec())])
    );
    repo.ok(&["rm", "dir"]);
    assert!(!repo.exists("dir"), "the emptied directory is removed too");
    assert_eq!(
        repo.ok(&["rm", "--cached", "keep.txt"]),
        "untracked keep.txt (kept on disk)\n"
    );
    assert_eq!(repo.read("keep.txt"), b"k\n");
    let status = repo.ok(&["status"]);
    for line in [
        "deleted:    a.txt",
        "deleted:    dir/b.txt",
        "deleted:    keep.txt",
        "\tkeep.txt",
    ] {
        assert!(status.contains(line), "{status}");
    }
    let index = repo.repo().load_index().unwrap();
    assert!(index.get(&RepoPath::parse("keep.txt").unwrap()).is_none());
}

#[test]
fn rm_refuses_to_lose_uncommitted_work() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.write("b.txt", "b\n");
    commit_all(&repo, "base");
    repo.write("a.txt", "edited\n");
    repo.write("b.txt", "staged\n");
    repo.ok(&["add", "b.txt"]);
    let before = repo.snapshot();
    let output = repo.fails(&["rm", "a.txt", "b.txt"]);
    assert!(
        output.contains("a.txt (changes not staged)") && output.contains("b.txt (staged changes)"),
        "{output}"
    );
    assert_eq!(repo.snapshot(), before);

    // --cached is fine unless the staged content would exist nowhere else.
    repo.ok(&["rm", "--cached", "a.txt"]);
    repo.write("b.txt", "edited again\n");
    let output = repo.fails(&["rm", "--cached", "b.txt"]);
    assert!(
        output.contains("staged content differs from both the file and HEAD"),
        "{output}"
    );
    assert!(
        repo.fails(&["rm", "missing.txt"])
            .contains("doesn't match any tracked files")
    );
}

#[test]
fn export_writes_a_commits_files_into_an_empty_directory() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.write("nested/b.bin", pseudo_random(LARGE, 9));
    repo.write("tool.sh", "#!/bin/sh\n");
    repo.ok(&["add", "."]);
    repo.ok(&["add", "--exec", "tool.sh"]);
    repo.ok(&["commit", "-m", "base"]);
    let commit = repo.head_commit();
    repo.write("a.txt", "uncommitted\n");

    let out = repo.root.parent().unwrap().join("exported");
    let output = repo.ok(&["export", "HEAD", out.to_str().unwrap()]);
    assert!(
        output.starts_with(&format!("Exported 4 files from {}", commit.short())),
        "{output}"
    );
    for (path, bytes) in repo.files_at(&commit) {
        let mut on_disk = out.clone();
        on_disk.extend(path.split('/'));
        assert!(std::fs::read(&on_disk).unwrap() == bytes, "{path}");
    }
    assert!(!out.join(".nexus").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(out.join("tool.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0);
    }

    let output = repo.fails(&["export", "HEAD", out.to_str().unwrap()]);
    assert!(output.contains("isn't empty"), "{output}");
    let output = repo.fails(&["export", "HEAD", ".nexus/inside"]);
    assert!(
        output.contains("inside the repository's own .nexus directory"),
        "{output}"
    );
}

/// Whether the filesystem holding `dir` ignores letter case (Windows and
/// macOS by default).
fn ignores_case(dir: &std::path::Path) -> bool {
    std::fs::write(dir.join("case-probe"), "").expect("write a probe file");
    let ignores = dir.join("CASE-PROBE").exists();
    std::fs::remove_file(dir.join("case-probe")).expect("remove the probe file");
    ignores
}

#[test]
fn checkout_refuses_a_name_that_reaches_an_untracked_file_through_letter_case() {
    let repo = TestRepo::new();
    if !ignores_case(&repo.root) {
        return;
    }
    repo.write("d/a.txt", "orig\n");
    commit_all(&repo, "base");
    repo.ok(&["branch", "upper"]);
    repo.ok(&["checkout", "upper"]);
    repo.ok(&["rm", "d/a.txt"]);
    repo.write("D/a.txt", "orig\n");
    commit_all(&repo, "rename the directory");
    repo.ok(&["checkout", "main"]);
    // The user renames the file's case and edits it: now it's untracked.
    std::fs::rename(repo.path("d/a.txt"), repo.path("d/A.txt")).unwrap();
    repo.write("d/A.txt", "precious\n");
    let output = repo.fails(&["checkout", "upper"]);
    assert!(output.contains("differs only in letter case"), "{output}");
    assert_eq!(repo.read("d/A.txt"), b"precious\n");
}

#[cfg(windows)]
#[test]
fn checkout_refuses_a_name_that_is_a_short_name_of_an_untracked_file() {
    let repo = TestRepo::new();
    repo.write("base.txt", "base\n");
    commit_all(&repo, "base");
    repo.ok(&["branch", "other"]);
    repo.ok(&["checkout", "other"]);
    repo.write("VERYLO~1.TXT", "tracked on other\n");
    commit_all(&repo, "a name that looks like a short name");
    repo.ok(&["checkout", "main"]);
    repo.write("verylongfilename.txt", "precious\n");
    if !repo.exists("VERYLO~1.TXT") {
        // This volume doesn't make 8.3 names.
        return;
    }
    let output = repo.fails(&["checkout", "other"]);
    assert!(
        output.contains("something on disk already answers to VERYLO~1.TXT"),
        "{output}"
    );
    assert_eq!(repo.read("verylongfilename.txt"), b"precious\n");
}

#[cfg(windows)]
#[test]
fn a_restore_that_fails_partway_still_records_what_it_discarded() {
    use std::os::windows::fs::OpenOptionsExt as _;
    let repo = TestRepo::new();
    repo.write("a.txt", "1\n");
    repo.write("b.txt", "1\n");
    commit_all(&repo, "base");
    repo.write("a.txt", "precious A\n");
    repo.write("b.txt", "precious B\n");
    let ops = repo.op_count();
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1) // FILE_SHARE_READ: readable, but not replaceable
        .open(repo.path("b.txt"))
        .unwrap();
    let output = repo.fails(&["restore", "a.txt", "b.txt"]);
    drop(held);
    assert!(
        output.contains("previous content is saved in the op log"),
        "{output}"
    );
    assert_eq!(repo.read("a.txt"), b"1\n");
    assert_eq!(repo.op_count(), ops + 1);
    let saved = repo.saved_files();
    assert_eq!(saved["a.txt"], b"precious A\n");
}

#[cfg(unix)]
#[test]
fn a_restore_that_fails_partway_still_records_what_it_discarded() {
    use std::os::unix::fs::PermissionsExt as _;
    let repo = TestRepo::new();
    repo.write("a/x.txt", "1\n");
    repo.write("b/y.txt", "1\n");
    commit_all(&repo, "base");
    repo.write("a/x.txt", "precious A\n");
    repo.write("b/y.txt", "precious B\n");
    let ops = repo.op_count();
    // A directory nothing can be created in, so replacing b/y.txt fails.
    std::fs::set_permissions(repo.path("b"), std::fs::Permissions::from_mode(0o555)).unwrap();
    let result = repo.run(&["restore", "a", "b"]);
    std::fs::set_permissions(repo.path("b"), std::fs::Permissions::from_mode(0o755)).unwrap();
    if result.exit_code == 0 {
        // Running as root, which ignores permissions.
        return;
    }
    assert!(
        result
            .text()
            .contains("previous content is saved in the op log"),
        "{}",
        result.text()
    );
    assert_eq!(repo.read("a/x.txt"), b"1\n");
    assert_eq!(repo.op_count(), ops + 1);
    assert_eq!(repo.saved_files()["a/x.txt"], b"precious A\n");
}

/// Sets a file's modification time.
fn set_mtime(path: &std::path::Path, time: std::time::SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open for writing")
        .set_modified(time)
        .expect("set the modification time");
}

/// The index entry for `path`.
fn entry(repo: &TestRepo, path: &str) -> nexus_core::index::IndexEntry {
    *repo
        .repo()
        .load_index()
        .expect("the index loads")
        .get(&RepoPath::parse(path).expect("a valid path"))
        .expect("the path is in the index")
}

#[test]
fn a_racy_edit_stays_visible_after_another_command_rewrites_the_index() {
    use std::time::{Duration, SystemTime};
    let repo = TestRepo::new();
    repo.write("a.txt", "one\n");
    repo.write("b.txt", "b\n");
    commit_all(&repo, "base");
    // Emulate a coarse filesystem clock: a.txt's entry, the index, and an
    // edit of the same size all share one timestamp.
    let tick = SystemTime::now() - Duration::from_secs(10);
    set_mtime(&repo.path("a.txt"), tick);
    repo.ok(&["add", "a.txt"]);
    set_mtime(&repo.root.join(".nexus/index"), tick);
    repo.write("a.txt", "two\n");
    set_mtime(&repo.path("a.txt"), tick);
    // b.txt's time changes but not its content, so status refreshes its
    // entry and rewrites the index with a newer timestamp...
    set_mtime(&repo.path("b.txt"), tick - Duration::from_secs(5));
    assert!(repo.ok(&["status"]).contains("modified:   a.txt"));
    // ...and the edit must still be seen afterwards, by status and by
    // checkout's safety check.
    assert!(repo.ok(&["status"]).contains("modified:   a.txt"));
    repo.ok(&["restore", "b.txt"]);
    assert!(repo.ok(&["diff"]).contains("+two"));
}

#[test]
fn status_saves_refreshed_times_so_the_next_one_can_skip_the_file() {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    commit_all(&repo, "base");
    // Out of the racy window: an older time than the index file's.
    let old = SystemTime::now() - Duration::from_secs(30);
    set_mtime(&repo.path("a.txt"), old);
    repo.ok(&["status"]);
    let refreshed = entry(&repo, "a.txt");
    let expected = i64::try_from(old.duration_since(UNIX_EPOCH).unwrap().as_nanos()).unwrap();
    assert_eq!((refreshed.size, refreshed.mtime_ns), (2, expected));
}

#[test]
fn diff_hashes_a_file_whose_size_alone_showed_it_changed() {
    use std::time::{Duration, SystemTime};
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    commit_all(&repo, "base");
    set_mtime(
        &repo.path("a.txt"),
        SystemTime::now() - Duration::from_secs(30),
    );
    repo.ok(&["status"]);
    assert_ne!(entry(&repo, "a.txt").mtime_ns, 0, "the entry is trusted");
    repo.write("a.txt", "a longer line\n");
    let new = nexus_core::object::id_of(nexus_core::object::ObjectKind::Blob, b"a longer line\n");
    let output = repo.ok(&["diff"]);
    assert!(
        output.contains(&format!("..{} 100644", new.short())),
        "{output}"
    );
    assert!(output.contains("-a\n+a longer line\n"), "{output}");
}

#[test]
fn an_unknown_cached_time_never_matches_a_file_dated_1970() {
    let repo = TestRepo::new();
    repo.write("a.txt", "hello\n");
    commit_all(&repo, "base");
    repo.write("a.txt", "");
    set_mtime(&repo.path("a.txt"), std::time::UNIX_EPOCH);
    repo.ok(&["add", "a.txt"]);
    repo.ok(&["restore", "--staged", "a.txt"]);
    // The entry is HEAD's content with size 0 and time 0, like the empty file.
    assert!(repo.ok(&["status"]).contains("modified:   a.txt"));
    assert!(repo.ok(&["add", "a.txt"]).contains("staged   a.txt"));
}

#[test]
fn a_scoped_diff_ignores_files_outside_the_scope() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.write("b.txt", "b\n");
    commit_all(&repo, "base");
    repo.write("a.txt", "a2\n");
    repo.write("b.txt", "b2\n");
    let output = repo.ok(&["diff", "--", "a.txt"]);
    assert!(
        output.contains("+a2") && !output.contains("b.txt"),
        "{output}"
    );
}

#[test]
fn a_deleted_tracked_file_named_like_a_branch_is_no_ambiguity() {
    let repo = TestRepo::new();
    repo.write("feature", "f\n");
    repo.write("other", "o\n");
    commit_all(&repo, "base");
    repo.ok(&["branch", "feature"]);
    repo.remove("feature");
    // As in Git, only a file on disk makes the name ambiguous.
    let output = repo.ok(&["diff", "feature"]);
    assert!(output.contains("deleted file mode 100644"), "{output}");
}

/// A commit with exactly these files (all holding `content`), on top of
/// HEAD, built by hand so it can hold paths `nexus add` never stages.
fn hand_built_commit(repo: &TestRepo, paths: &[&str], content: &[u8]) -> ObjectId {
    use nexus_core::index::{FileKind, Index, IndexEntry};
    use nexus_core::object::{Commit, ObjectKind};
    let store = repo.repo();
    let id = store
        .odb()
        .write(ObjectKind::Blob, content)
        .expect("store a blob");
    let mut index = Index::default();
    for path in paths {
        let entry = IndexEntry {
            kind: FileKind::File,
            id,
            size: 0,
            mtime_ns: 0,
        };
        index.insert(RepoPath::parse(path).expect("a valid path"), entry);
    }
    let tree =
        nexus_core::tree::write_index_tree(&index, nexus_core::content::Sink::Store(store.odb()))
            .expect("store the tree");
    let head = repo.head_commit();
    let commit = Commit {
        tree,
        parents: vec![head],
        author: repo.commit(&head).author,
        message: "by hand\n".to_owned(),
    };
    store
        .odb()
        .write(ObjectKind::Commit, &commit.encode())
        .expect("store the commit")
}

#[test]
fn new_directories_with_several_files_are_checked_out_and_restored() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    commit_all(&repo, "base");
    repo.ok(&["branch", "other"]);
    repo.ok(&["checkout", "other"]);
    repo.write("d/1.txt", "1\n");
    repo.write("d/2.txt", "2\n");
    repo.write("d/e/3.txt", "3\n");
    repo.write("d/e/4.txt", "4\n");
    let other = commit_all(&repo, "a directory of files");
    repo.ok(&["checkout", "main"]);
    assert!(!repo.exists("d"));
    repo.ok(&["checkout", "other"]);
    assert_matches_commit(&repo, &other);
    std::fs::remove_dir_all(repo.path("d")).unwrap();
    assert_eq!(repo.ok(&["restore", "d"]), "Restored 4 files\n");
    assert_matches_commit(&repo, &other);
}

#[test]
fn a_directory_case_rename_survives_untracked_files_in_the_old_directory() {
    let repo = TestRepo::new();
    if !ignores_case(&repo.root) {
        return;
    }
    repo.write("MyPkg/a.py", "a\n");
    repo.write("MyPkg/b.py", "b\n");
    let main = commit_all(&repo, "base");
    repo.ok(&["branch", "lower"]);
    repo.ok(&["checkout", "lower"]);
    repo.ok(&["rm", "MyPkg"]);
    repo.write("mypkg/a.py", "a\n");
    repo.write("mypkg/b.py", "b\n");
    commit_all(&repo, "rename the directory");
    // Something untracked keeps the directory alive across the switch.
    repo.write("mypkg/cache.tmp", "untracked\n");
    repo.ok(&["checkout", "main"]);
    let status = repo.ok(&["status"]);
    assert!(!status.contains("deleted"), "{status}");
    assert!(status.contains("MyPkg/cache.tmp"), "{status}");
    assert_eq!(repo.read("MyPkg/cache.tmp"), b"untracked\n");
    let _ = main;
}

#[test]
fn a_tree_with_names_differing_only_in_case_is_refused_where_they_collide() {
    let repo = TestRepo::new();
    repo.write("base.txt", "base\n");
    commit_all(&repo, "base");
    let commit = hand_built_commit(&repo, &["README", "readme", "base.txt"], b"x\n");
    if ignores_case(&repo.root) {
        let output = repo.fails(&["checkout", &commit.to_string()]);
        assert!(
            output.contains("can't hold names that differ only in letter case"),
            "{output}"
        );
        assert_eq!(repo.read("base.txt"), b"base\n");
    } else {
        repo.ok(&["checkout", &commit.to_string()]);
    }
}

#[test]
fn restore_never_writes_or_deletes_inside_the_repository_directory() {
    let repo = TestRepo::new();
    repo.write("base.txt", "base\n");
    commit_all(&repo, "base");
    let config = repo.read(".nexus/config.toml");
    let evil = hand_built_commit(
        &repo,
        &[".nexus/config.toml", "base.txt"],
        b"[user]\nname = \"Mallory\"\n",
    );
    let output = repo.fails(&["restore", "--source", &evil.to_string(), "."]);
    assert!(
        output.contains(".nexus/config.toml (the name can't be used on this system)"),
        "{output}"
    );
    let output = repo.fails(&["restore", "--staged", "--source", &evil.to_string(), "."]);
    assert!(
        output.contains("inside the repository's own .nexus directory"),
        "{output}"
    );
    assert_eq!(repo.read(".nexus/config.toml"), config);
    assert_eq!(repo.read("base.txt"), b"base\n");
}

#[test]
fn a_refusal_names_each_file_once() {
    let repo = TestRepo::new();
    repo.write("a.txt", "1\n");
    commit_all(&repo, "base");
    repo.ok(&["branch", "other"]);
    repo.write("a.txt", "2\n");
    commit_all(&repo, "main moves on");
    repo.write("a.txt", "local\n");
    let output = repo.fails(&["checkout", "other"]);
    assert_eq!(output.matches("a.txt (").count(), 1, "{output}");
}

#[test]
fn restore_from_a_commit_of_files_already_gone_changes_nothing() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    let first = commit_all(&repo, "first");
    repo.write("b.txt", "b\n");
    commit_all(&repo, "second");
    repo.remove("b.txt");
    let ops = repo.op_count();
    let output = repo.ok(&["restore", "--source", &first.short(), "b.txt"]);
    assert!(output.contains("nothing to restore"), "{output}");
    assert_eq!(repo.op_count(), ops);
}

#[test]
fn export_skips_a_file_whose_directory_another_file_already_answers_to() {
    let repo = TestRepo::new();
    repo.write("base.txt", "base\n");
    commit_all(&repo, "base");
    let commit = hand_built_commit(&repo, &["A", "a/x", "z.txt"], b"x\n");
    let out = repo.root.parent().unwrap().join("exported");
    let output = repo.ok(&["export", &commit.to_string(), out.to_str().unwrap()]);
    assert!(out.join("z.txt").exists(), "later files are still exported");
    if ignores_case(&repo.root) {
        assert!(output.contains("skipped a/x"), "{output}");
    } else {
        assert!(out.join("a/x").exists());
    }
}

#[cfg(windows)]
#[test]
fn a_checkout_that_fails_before_changing_anything_records_nothing() {
    use std::os::windows::fs::OpenOptionsExt as _;
    let repo = TestRepo::new();
    repo.write("a.txt", "1\n");
    commit_all(&repo, "base");
    repo.ok(&["branch", "other"]);
    repo.write("a.txt", "2\n");
    commit_all(&repo, "main moves on");
    let ops = repo.op_count();
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1) // FILE_SHARE_READ: readable, but not replaceable
        .open(repo.path("a.txt"))
        .unwrap();
    let output = repo.fails(&["checkout", "other"]);
    drop(held);
    assert!(!output.contains("had already changed"), "{output}");
    assert_eq!(repo.op_count(), ops);
    assert!(repo.ok(&["status"]).contains("working tree clean"));
}

#[cfg(windows)]
#[test]
fn an_rm_that_fails_partway_records_what_it_deleted() {
    use std::os::windows::fs::OpenOptionsExt as _;
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.write("b.txt", "b\n");
    commit_all(&repo, "base");
    let ops = repo.op_count();
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(repo.path("b.txt"))
        .unwrap();
    let output = repo.fails(&["rm", "a.txt", "b.txt"]);
    drop(held);
    assert!(output.contains("had already changed"), "{output}");
    assert!(!repo.exists("a.txt"));
    assert_eq!(repo.op_count(), ops + 1);
    assert_eq!(repo.saved_files()["a.txt"], b"a\n");
}

#[test]
fn hunk_headers_name_the_enclosing_function_as_git_does() {
    let repo = TestRepo::new();
    let old = "fn main() {\n    one();\n    two();\n    three();\n    four();\n    five();\n}\n";
    repo.write("m.rs", old);
    commit_all(&repo, "base");
    repo.write("m.rs", old.replace("five", "FIVE"));
    let output = repo.ok(&["diff"]);
    assert!(output.contains("@@ -3,5 +3,5 @@ fn main() {\n"), "{output}");
}

#[test]
fn names_with_spaces_end_their_file_lines_with_a_tab() {
    let repo = TestRepo::new();
    repo.write("with space.txt", "1\n");
    commit_all(&repo, "base");
    repo.write("with space.txt", "2\n");
    let output = repo.ok(&["diff"]);
    assert!(
        output.contains("--- a/with space.txt\t\n+++ b/with space.txt\t\n"),
        "{output}"
    );
}

#[test]
fn binary_detection_looks_at_the_first_8000_bytes_like_git() {
    let repo = TestRepo::new();
    let mut text = "x".repeat(8100);
    text.push('\n');
    repo.write("f.txt", format!("{text}\0old\n"));
    commit_all(&repo, "base");
    repo.write("f.txt", format!("{text}\0new\n"));
    let output = repo.ok(&["diff"]);
    assert!(output.contains("+^@new"), "{output}");
}

#[cfg(unix)]
#[test]
fn warnings_never_carry_escape_sequences() {
    let repo = TestRepo::new();
    std::os::unix::fs::symlink("target", repo.path("link\x1b]0;title\x07")).unwrap();
    let result = repo.run(&["status"]);
    assert!(!result.text().contains('\x1b'), "{}", result.text());
    assert!(
        result.text().contains("symlinks aren't supported"),
        "{}",
        result.text()
    );
}
