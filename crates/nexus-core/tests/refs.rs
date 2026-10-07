//! Branches, tags, and naming commits: `branch`, `tag`, abbreviated and
//! ambiguous IDs.

mod common;

use common::TestRepo;
use nexus_core::hash::ObjectId;
use nexus_core::object::{self, ObjectKind};

fn commit_file(repo: &TestRepo, path: &str, content: &str, message: &str) -> ObjectId {
    repo.write(path, content);
    repo.ok(&["add", "."]);
    repo.ok(&["commit", "-m", message]);
    repo.head_commit()
}

#[test]
fn branches_are_listed_created_and_deleted() {
    let repo = TestRepo::new();
    assert_eq!(repo.ok(&["branch"]), "* main (no commits yet)\n");
    assert!(
        repo.fails(&["branch", "early"])
            .contains("HEAD has no commits yet")
    );

    let first = commit_file(&repo, "a.txt", "1\n", "first");
    assert_eq!(
        repo.ok(&["branch", "feature/x"]),
        format!("Created branch feature/x at {}\n", first.short())
    );
    assert_eq!(repo.ok(&["branch"]), "  feature/x\n* main\n");
    assert!(
        repo.fails(&["branch", "feature/x"])
            .contains("already exists")
    );
    // A ref can't be both a file and a directory.
    assert!(
        repo.fails(&["branch", "feature"])
            .contains("clash with refs/heads/feature/x")
    );
    assert!(
        repo.fails(&["branch", "feature/x/y"])
            .contains("clash with refs/heads/feature/x")
    );
    for bad in [
        "-x", "a..b", "a b", "x.lock", "HEAD", "a/", ".hidden", "a~1", "a:b",
    ] {
        let output = repo.fails(&["branch", bad]);
        assert!(
            output.contains(&format!("branch name `{bad}`"))
                || output.contains("unexpected argument"),
            "{bad}: {output}"
        );
    }

    let second = commit_file(&repo, "a.txt", "2\n", "second");
    repo.ok(&["branch", "old", &first.short()]);
    assert_eq!(
        repo.repo().refs().get("refs/heads/old").unwrap(),
        Some(first)
    );

    // -d refuses an unmerged branch; -D doesn't care.
    repo.ok(&["checkout", "old"]);
    repo.ok(&["branch", "ahead", &second.short()]);
    let output = repo.fails(&["branch", "-d", "ahead"]);
    assert!(
        output.contains("isn't merged into the current branch"),
        "{output}"
    );
    assert!(
        repo.fails(&["branch", "-d", "old"])
            .contains("it's the current branch")
    );
    assert_eq!(
        repo.ok(&["branch", "-D", "ahead"]),
        format!("Deleted branch ahead (was {})\n", second.short())
    );
    repo.ok(&["checkout", "main"]);
    assert_eq!(
        repo.ok(&["branch", "-d", "old"]),
        format!("Deleted branch old (was {})\n", first.short())
    );
    assert_eq!(
        repo.ok(&["branch", "-d", "feature/x"]),
        format!("Deleted branch feature/x (was {})\n", first.short())
    );
    assert!(
        !repo.root.join(".nexus/refs/heads/feature").exists(),
        "the empty directory is pruned"
    );
    assert!(
        repo.fails(&["branch", "-d", "gone"])
            .contains("there's no branch named gone")
    );
    assert_eq!(repo.ok(&["branch"]), "* main\n");
}

#[test]
fn branch_commands_record_one_op_each() {
    let repo = TestRepo::new();
    commit_file(&repo, "a.txt", "1\n", "first");
    let ops = repo.op_count();
    repo.ok(&["branch", "b"]);
    assert_eq!(repo.op_count(), ops + 1);
    let (_, op) = repo.latest_op();
    assert_eq!(op.command, "nexus branch b");
    assert!(op.view.refs.contains_key("refs/heads/b"));
    repo.ok(&["branch", "-d", "b"]);
    assert_eq!(repo.op_count(), ops + 2);
    assert!(!repo.latest_op().1.view.refs.contains_key("refs/heads/b"));
    // Listing changes nothing.
    repo.ok(&["branch"]);
    assert_eq!(repo.op_count(), ops + 2);
}

#[test]
fn tags_are_listed_created_deleted_and_shown_in_log() {
    let repo = TestRepo::new();
    let first = commit_file(&repo, "a.txt", "1\n", "first");
    let second = commit_file(&repo, "a.txt", "2\n", "second");
    assert_eq!(repo.ok(&["tag"]), "");
    assert_eq!(
        repo.ok(&["tag", "v2"]),
        format!("Tagged {} as v2\n", second.short())
    );
    repo.ok(&["tag", "v1", &first.short()]);
    assert!(repo.fails(&["tag", "v1"]).contains("already exists"));
    assert_eq!(repo.ok(&["tag"]), "v1\nv2\n");
    assert_eq!(
        repo.ok(&["log", "--oneline"]),
        format!(
            "{} (HEAD -> main, tag: v2) second\n{} (tag: v1) first\n",
            second.short(),
            first.short()
        )
    );
    // Tags name commits anywhere a commit is expected.
    repo.ok(&["checkout", "v1"]);
    assert_eq!(repo.read("a.txt"), b"1\n");
    assert_eq!(
        repo.ok(&["tag", "-d", "v1"]),
        format!("Deleted tag v1 (was {})\n", first.short())
    );
    assert_eq!(repo.ok(&["tag"]), "v2\n");
    assert!(
        repo.fails(&["tag", "-d", "v1"])
            .contains("there's no tag named v1")
    );
}

#[test]
fn a_name_that_is_both_a_branch_and_a_tag_must_be_spelled_out() {
    let repo = TestRepo::new();
    let first = commit_file(&repo, "a.txt", "1\n", "first");
    repo.ok(&["tag", "same"]);
    repo.ok(&["branch", "same"]);
    let output = repo.fails(&["show", "same"]);
    assert!(output.contains("both a branch and a tag"), "{output}");
    assert!(
        repo.ok(&["show", "refs/tags/same"])
            .starts_with(&format!("commit {first}"))
    );
}

#[test]
fn commits_can_be_named_by_any_unique_prefix() {
    let repo = TestRepo::new();
    let first = commit_file(&repo, "a.txt", "1\n", "first");
    let hex = first.to_string();
    for len in [4, 7, 12, 64] {
        assert!(
            repo.ok(&["show", &hex[..len]])
                .starts_with(&format!("commit {first}")),
            "{len}"
        );
    }
    assert!(
        repo.ok(&["show", &hex[..8].to_ascii_uppercase()])
            .starts_with(&format!("commit {first}"))
    );
    assert!(repo.fails(&["show", &hex[..3]]).contains("4 or more"));
    let output = repo.fails(&["show", "0000000000"]);
    assert!(
        output.contains("no object ID starts with 0000000000") || output.contains("ambiguous"),
        "{output}"
    );
    let tree = repo.commit(&first).tree;
    let output = repo.fails(&["show", &tree.short()]);
    assert!(output.contains("is a tree, not a commit"), "{output}");
}

#[test]
fn an_ambiguous_prefix_lists_the_candidates() {
    let repo = TestRepo::new();
    let first = commit_file(&repo, "a.txt", "1\n", "first");
    let prefix = &first.to_string()[..4];

    // Store a blob whose ID starts with the commit's first 4 characters.
    let mut n = 0u64;
    let body = loop {
        let body = format!("collision {n}\n").into_bytes();
        if object::id_of(ObjectKind::Blob, &body)
            .to_string()
            .starts_with(prefix)
        {
            break body;
        }
        n += 1;
    };
    let blob = repo.repo().odb().write(ObjectKind::Blob, &body).unwrap();

    // Where a commit is expected, the one commit among them wins, as in Git.
    assert!(
        repo.ok(&["show", prefix])
            .starts_with(&format!("commit {first}"))
    );
    // Anywhere else the prefix is ambiguous, and every candidate is listed.
    let output = repo.fails(&["cat-file", "-p", prefix]);
    assert!(output.contains("is ambiguous"), "{output}");
    assert!(output.contains(&format!("{first} (commit)")), "{output}");
    assert!(output.contains(&format!("{blob} (blob)")), "{output}");
    // A longer prefix settles it.
    assert_eq!(
        repo.ok(&["cat-file", "-p", &blob.to_string()[..20]])
            .into_bytes(),
        body
    );
}

#[test]
fn ref_names_must_work_on_every_os_and_match_exactly() {
    let repo = TestRepo::new();
    let first = commit_file(
        &repo, "a.txt", "1
", "first",
    );
    repo.ok(&["branch", "feature"]);
    for bad in [
        "feature.",
        "nul",
        "CON",
        "aux.txt",
        "com1",
        "feat/lpt9",
        "a<b",
        "a|b",
        "a\"b",
    ] {
        let output = repo.fails(&["branch", bad]);
        assert!(
            output.contains(&format!("branch name `{bad}`")),
            "{bad}: {output}"
        );
    }
    assert!(
        repo.fails(&["branch", &first.to_string()])
            .contains("can't be a full object ID")
    );

    // Names that differ only in case are the same file on Windows and macOS,
    // so they can't be created, and lookups must match exactly everywhere.
    let output = repo.fails(&["branch", "MAIN"]);
    assert!(output.contains("clash with refs/heads/main"), "{output}");
    assert!(
        repo.fails(&["branch", "Feature/x"])
            .contains("clash with refs/heads/feature")
    );
    assert!(
        repo.fails(&["branch", "-d", "MAIN"])
            .contains("there's no branch named MAIN")
    );
    let output = repo.fails(&["checkout", "FEATURE"]);
    assert!(output.contains("FEATURE isn't a branch"), "{output}");
    // A trailing dot isn't a branch either (Windows would open `feature`).
    assert!(
        repo.fails(&["checkout", "feature."])
            .contains("isn't a branch")
    );
    assert_eq!(
        repo.ok(&["branch"]),
        "  feature
* main
"
    );
}

#[test]
fn refs_differing_in_case_anywhere_in_their_names_clash() {
    let repo = TestRepo::new();
    commit_file(&repo, "a.txt", "1\n", "first");
    repo.ok(&["branch", "Feature/y"]);
    for name in ["feature/x", "FEATURE/z", "feature/Y"] {
        let output = repo.fails(&["branch", name]);
        assert!(
            output.contains("clash with refs/heads/Feature/y"),
            "{name}: {output}"
        );
    }
    repo.ok(&["branch", "Feature/x"]);
}

#[test]
fn ref_names_typed_in_nfd_work_everywhere() {
    let repo = TestRepo::new();
    let first = commit_file(&repo, "a.txt", "1\n", "first");
    let nfd = "cafe\u{301}";
    repo.ok(&["branch", nfd]);
    assert_eq!(repo.ok(&["branch"]), "  caf\u{e9}\n* main\n");
    repo.ok(&["checkout", nfd]);
    assert!(
        repo.ok(&["show", nfd])
            .starts_with(&format!("commit {first}"))
    );
    repo.ok(&["tag", "v\u{301}"]);
    assert!(
        repo.ok(&["show", "v\u{301}"])
            .starts_with(&format!("commit {first}"))
    );
}
