//! Commits, history, and the operation log.

mod common;

use std::collections::BTreeMap;
use std::fs;

use common::{TIME, TestRepo};
use nexus_core::content::Sink;
use nexus_core::object::ObjectKind;
use nexus_core::oplog::{self, RECOVERED};
use nexus_core::refs::{DEFAULT_BRANCH, Head};
use nexus_core::tree;

fn snapshot(repo: &TestRepo, files: &[&str]) -> BTreeMap<String, Vec<u8>> {
    files
        .iter()
        .map(|file| {
            (
                (*file).to_owned(),
                fs::read(repo.path(file)).expect("the file exists"),
            )
        })
        .collect()
}

#[test]
fn three_commit_pipeline() {
    let repo = TestRepo::new();
    let ignore = ".nexusignore";

    // Round 1: a few files in nested directories.
    repo.write("a.txt", "first version\n");
    repo.write("src/main.rs", "fn main() {}\n");
    repo.write("docs/guide/intro.md", "# Intro\n");
    repo.ok(&["add", "."]);
    repo.ok(&["commit", "-m", "one"]);
    let one = repo.head_commit();
    let expected_one = snapshot(
        &repo,
        &[ignore, "a.txt", "docs/guide/intro.md", "src/main.rs"],
    );

    // Round 2: modify, add, delete.
    repo.write("a.txt", "second version, longer\n");
    repo.write("src/lib.rs", "pub fn lib() {}\n");
    repo.remove("docs/guide/intro.md");
    repo.ok(&["add", "."]);
    repo.ok(&["commit", "-m", "two"]);
    let two = repo.head_commit();
    let expected_two = snapshot(&repo, &[ignore, "a.txt", "src/lib.rs", "src/main.rs"]);

    // Round 3: a deeply nested binary file, another modification and deletion.
    repo.write("deep/er/est/data.bin", [0_u8, 159, 146, 150, 0, 255]);
    repo.write("src/main.rs", "fn main() { println!(\"hi\"); }\n");
    repo.remove("src/lib.rs");
    repo.ok(&["add", "."]);
    repo.ok(&["commit", "-m", "three"]);
    let three = repo.head_commit();
    let expected_three = snapshot(
        &repo,
        &[ignore, "a.txt", "deep/er/est/data.bin", "src/main.rs"],
    );

    // Log order, newest first, and parent links.
    let log = repo.ok(&["log", "--oneline"]);
    let subjects: Vec<&str> = log
        .lines()
        .map(|line| line.rsplit(' ').next().unwrap())
        .collect();
    assert_eq!(subjects, ["three", "two", "one"]);
    assert_eq!(repo.commit(&three).parents, [two]);
    assert_eq!(repo.commit(&two).parents, [one]);
    assert_eq!(repo.commit(&one).parents.len(), 0);

    // Every commit's tree rebuilds the exact bytes.
    assert_eq!(repo.files_at(&one), expected_one);
    assert_eq!(repo.files_at(&two), expected_two);
    assert_eq!(repo.files_at(&three), expected_three);
}

#[test]
fn every_mutating_command_records_exactly_one_op() {
    let repo = TestRepo::new();
    let r = repo.repo();
    let (init_id, init) = repo.latest_op();
    assert_eq!(init.parent, None);
    assert_eq!(init.command, "nexus init");
    assert_eq!(init.time.to_string(), TIME);
    assert_eq!(init.view.head, Head::Branch(DEFAULT_BRANCH.to_owned()));
    assert_eq!(init.view.refs.len(), 0);
    assert_eq!(
        init.view.index,
        tree::write_index_tree(&r.load_index().unwrap(), Sink::HashOnly).unwrap()
    );
    // Everything an op refers to exists.
    assert!(r.odb().contains(&init.view.index));
    assert!(r.odb().contains(&init.view.stash));

    repo.write("a.txt", "a\n");
    repo.ok(&["add", "a.txt"]);
    let (add_id, add) = repo.latest_op();
    assert_eq!(add.parent, Some(init_id));
    assert_eq!(add.command, "nexus add a.txt");
    assert_ne!(add.view.index, init.view.index);
    assert_eq!(
        add.view.index,
        tree::write_index_tree(&r.load_index().unwrap(), Sink::HashOnly).unwrap()
    );
    assert!(r.odb().contains(&add.view.index));

    repo.ok(&["commit", "-m", "first commit"]);
    let (commit_id, commit) = repo.latest_op();
    assert_eq!(commit.parent, Some(add_id));
    assert_eq!(commit.command, "nexus commit -m \"first commit\"");
    assert_eq!(
        commit.view.refs.get(DEFAULT_BRANCH),
        Some(&repo.head_commit())
    );
    assert_eq!(commit.view.index, add.view.index);

    // Commands that change nothing record nothing.
    repo.ok(&["add", "a.txt"]);
    repo.fails(&["commit", "-m", "again"]);
    repo.ok(&["config", "user.name", "Someone Else"]);
    repo.ok(&["log"]);
    assert_eq!(repo.latest_op().0, commit_id);

    // Walking the parents gets back to the first op.
    let mut count = 1;
    let mut op = commit;
    while let Some(parent) = op.parent {
        op = oplog::read_op(&r, &parent).unwrap();
        count += 1;
    }
    assert_eq!(count, 3);
}

#[test]
fn records_a_recovered_op_when_state_changed_outside_nexus() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.ok(&["add", "."]);
    repo.ok(&["commit", "-m", "one"]);
    let (before, _) = repo.latest_op();

    // Simulate a hand edit (or a crash after a ref was written but before OPLOG).
    let commit = repo.head_commit();
    fs::write(
        repo.root.join(".nexus/refs/heads/other"),
        format!("{commit}\n"),
    )
    .unwrap();

    repo.write("b.txt", "b\n");
    repo.ok(&["add", "b.txt"]);
    let (_, add) = repo.latest_op();
    assert_eq!(add.command, "nexus add b.txt");
    let recovered = oplog::read_op(&repo.repo(), &add.parent.unwrap()).unwrap();
    assert_eq!(recovered.command, RECOVERED);
    assert_eq!(recovered.parent, Some(before));
    assert!(recovered.view.refs.contains_key("refs/heads/other"));
}

#[test]
fn same_files_in_a_different_order_give_the_same_tree() {
    let files = [
        ("z.txt", "last"),
        ("a.txt", "first"),
        ("m/n/o.txt", "nested"),
        ("m/a.txt", "sibling"),
        ("B.txt", "upper"),
        ("caf\u{e9}.txt", "accent"),
    ];
    let forward = TestRepo::new();
    for (path, contents) in files {
        forward.write(path, contents);
    }
    let backward = TestRepo::new();
    for (path, contents) in files.iter().rev() {
        backward.write(path, contents);
    }
    for repo in [&forward, &backward] {
        repo.ok(&["add", "."]);
        repo.ok(&["commit", "-m", "same"]);
    }
    let tree_of = |repo: &TestRepo| repo.commit(&repo.head_commit()).tree;
    assert_eq!(tree_of(&forward), tree_of(&backward));
    // With the same clock and identity, the commits are identical too.
    assert_eq!(forward.head_commit(), backward.head_commit());
    // And the IDs are the same on every OS: CI runs this on Linux, macOS, and
    // Windows against these values. (Changing DEFAULT_IGNORE changes them,
    // because .nexusignore is part of the tree.)
    assert_eq!(tree_of(&forward).to_string(), GOLDEN_TREE);
    assert_eq!(forward.head_commit().to_string(), GOLDEN_COMMIT);
}

const GOLDEN_TREE: &str = "172e911cf576f3ec68286c6fc0bf9e98d716ce21a15b0024fef9ab54081eb3ca";
const GOLDEN_COMMIT: &str = "1a8109e04c139471d81467e4724f2272c2a67678b46fc06d8926dc4da743d4b2";

#[test]
fn commits_on_an_unborn_branch_and_logs_it() {
    let repo = TestRepo::new();
    assert!(repo.ok(&["log"]).contains("no commits yet on main"));
    repo.write("a.txt", "a\n");
    repo.ok(&["add", "."]);
    let output = repo.ok(&["commit", "-m", "Initial import", "-m", "With a body."]);
    let id = repo.head_commit();
    assert_eq!(
        output.trim(),
        format!("[main (root commit) {}] Initial import", id.short())
    );

    let commit = repo.commit(&id);
    assert_eq!(commit.parents.len(), 0);
    assert_eq!(commit.message, "Initial import\n\nWith a body.\n");
    assert_eq!(
        commit.author.to_string(),
        format!("Ada Lovelace <ada@example.com> {TIME}")
    );

    let log = repo.ok(&["log"]);
    let expected = format!(
        "commit {id} (HEAD -> main)\nAuthor: Ada Lovelace <ada@example.com>\nDate:   Mon Oct 6 13:30:00 2025 +0530\n\n    Initial import\n    \n    With a body.\n"
    );
    assert_eq!(log, expected);
}

#[test]
fn log_limits_and_oneline() {
    let repo = TestRepo::new();
    for n in 1..=4 {
        repo.write("n.txt", format!("{n}\n"));
        repo.ok(&["add", "."]);
        repo.ok(&["commit", "-m", &format!("commit {n}")]);
    }
    let head = repo.head_commit();
    let two = repo.ok(&["log", "-n", "2", "--oneline"]);
    assert_eq!(
        two,
        format!(
            "{} (HEAD -> main) commit 4\n{} commit 3\n",
            head.short(),
            repo.commit(&head).parents[0].short()
        )
    );
    assert_eq!(repo.ok(&["log", "-n", "0"]), "");
}

#[test]
fn refuses_empty_commits_unless_allowed() {
    let repo = TestRepo::new();
    assert!(
        repo.fails(&["commit", "-m", "nothing"])
            .contains("nothing to commit")
    );
    repo.write("a.txt", "a\n");
    repo.ok(&["add", "."]);
    repo.ok(&["commit", "-m", "one"]);
    assert!(
        repo.fails(&["commit", "-m", "same again"])
            .contains("nothing to commit")
    );
    repo.ok(&["commit", "-m", "marker", "--allow-empty"]);
    let marker = repo.commit(&repo.head_commit());
    assert_eq!(marker.tree, repo.commit(&marker.parents[0]).tree);
}

#[test]
fn commit_needs_an_identity_and_a_message() {
    let mut repo = TestRepo::new();
    repo.ctx.global_config = None;
    repo.write("a.txt", "a\n");
    repo.ok(&["add", "."]);
    let output = repo.fails(&["commit", "-m", "x"]);
    assert!(
        output.contains("nexus config --global user.name"),
        "{output}"
    );
    repo.ok(&["config", "user.name", "Repo Person"]);
    repo.ok(&["config", "user.email", "repo@example.com"]);
    assert!(
        repo.fails(&["commit", "-m", "   "])
            .contains("message is empty")
    );
    repo.ok(&["commit", "-m", "x"]);
    assert_eq!(repo.commit(&repo.head_commit()).author.name, "Repo Person");
}

#[test]
fn commit_objects_are_readable_with_cat_file() {
    let repo = TestRepo::new();
    repo.write("a.txt", "hello\n");
    repo.ok(&["add", "."]);
    repo.ok(&["commit", "-m", "first"]);
    let id = repo.head_commit();
    let commit = repo.ok(&["cat-file", "-p", &id.to_string()]);
    let tree = repo.commit(&id).tree;
    assert_eq!(
        commit,
        format!("tree {tree}\nauthor Ada Lovelace <ada@example.com> {TIME}\n\nfirst\n")
    );
    let listing = repo.ok(&["cat-file", "-p", &tree.to_string()]);
    assert!(
        listing.contains(" a.txt\n") && listing.starts_with("file "),
        "{listing}"
    );

    let op = repo.latest_op().0;
    let op_text = repo.ok(&["cat-file", "-p", &op.to_string()]);
    assert!(
        op_text.contains("command nexus commit -m first\n"),
        "{op_text}"
    );
    // Any unique prefix of 4 or more characters works.
    assert_eq!(repo.ok(&["cat-file", "-p", &id.short()]), commit);
    assert_eq!(repo.ok(&["cat-file", "-p", &id.to_string()[..4]]), commit);
    assert!(
        repo.fails(&["cat-file", "-p", &id.to_string()[..3]])
            .contains("4 or more")
    );
    let odb = repo.repo();
    assert_eq!(odb.odb().read(&id).unwrap().kind, ObjectKind::Commit);
}

#[test]
fn messages_may_start_with_a_hyphen() {
    let repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.ok(&["add", "."]);
    repo.ok(&["commit", "-m", "-fix the bug", "-m", "- one item"]);
    assert_eq!(
        repo.commit(&repo.head_commit()).message,
        "-fix the bug\n\n- one item\n"
    );
}

#[test]
fn an_invalid_hand_edited_identity_fails_before_anything_is_written() {
    let mut repo = TestRepo::new();
    repo.write("a.txt", "a\n");
    repo.ok(&["add", "."]);
    let (op_before, _) = repo.latest_op();
    let objects_before = repo.object_count();
    let global = repo.root.parent().unwrap().join("bad-global.toml");
    fs::write(
        &global,
        "[user]\nname = \"Ada <admin>\"\nemail = \"ada@example.com\"\n",
    )
    .unwrap();
    repo.ctx.global_config = Some(global);
    let output = repo.fails(&["commit", "-m", "x"]);
    assert!(output.contains("user.name can't contain"), "{output}");
    assert_eq!(repo.latest_op().0, op_before);
    assert_eq!(repo.object_count(), objects_before);
}

#[test]
fn a_repository_in_a_deep_directory_works() {
    // Object paths add about 90 characters to the root, which takes them past
    // Windows' classic 260-character limit.
    let repo = TestRepo::uninitialized();
    let mut deep = repo.root.clone();
    while deep.as_os_str().len() < 200 {
        deep.push("a-fairly-long-directory-name");
    }
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("a.txt"), "hello\n").unwrap();
    for args in [
        &["init"][..],
        &["add", "."],
        &["commit", "-m", "deep"],
        &["branch", "a-long-branch-name-for-good-measure"],
        &["checkout", "a-long-branch-name-for-good-measure"],
    ] {
        let result = repo.run_in(&deep, args);
        assert_eq!(result.exit_code, 0, "{args:?}: {}", result.text());
    }
}
