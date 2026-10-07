//! End-to-end checks of the built `nexus` binary.

use std::path::Path;
use std::process::{Command, Output};

/// Runs the binary in `dir` with color off, a pinned clock, and a private
/// global config, whatever the caller's environment says.
fn nexus_in(dir: &Path, global_config: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nexus"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .env("NEXUS_DATE", "1759737600 +0530")
        .env("NEXUS_CONFIG_GLOBAL", global_config)
        .output()
        .expect("the nexus binary runs")
}

fn nexus(args: &[&str]) -> Output {
    let dir = tempfile::tempdir().expect("a temp dir");
    nexus_in(dir.path(), &dir.path().join("global.toml"), args)
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("UTF-8 output")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("UTF-8 output")
}

#[test]
fn version_flag_prints_name_and_version() {
    let output = nexus(&["--version"]);
    assert!(output.status.success(), "exit status: {}", output.status);
    assert_eq!(
        stdout(&output).trim(),
        format!("nexus {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn no_arguments_shows_usage_and_fails() {
    let output = nexus(&[]);
    assert!(!output.status.success(), "should exit with an error status");
    assert!(
        stderr(&output).contains("Usage: nexus"),
        "unexpected output: {}",
        stderr(&output)
    );
}

/// The Phase 1 "done when" sequence, through the real binary.
#[test]
fn init_add_commit_log_cat_file() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let work = dir.path().join("work");
    std::fs::create_dir(&work).expect("create the work dir");
    let global = dir.path().join("global.toml");
    let run = |args: &[&str]| {
        let output = nexus_in(&work, &global, args);
        assert!(
            output.status.success(),
            "`nexus {}` failed: {}",
            args.join(" "),
            stderr(&output)
        );
        stdout(&output)
    };

    run(&["config", "--global", "user.name", "Ada Lovelace"]);
    run(&["config", "--global", "user.email", "ada@example.com"]);
    assert!(run(&["init"]).starts_with("Initialized empty NexusVCS repository in "));
    std::fs::write(work.join("a.txt"), "hello\n").expect("write a file");
    assert!(run(&["add", "."]).contains("staged   a.txt"));
    assert!(run(&["commit", "-m", "first"]).contains("(root commit)"));

    let log = run(&["log"]);
    let first_line = log.lines().next().expect("log output");
    let id = first_line
        .strip_prefix("commit ")
        .and_then(|rest| rest.split(' ').next())
        .expect("a commit line");
    assert_eq!(id.len(), 64, "{log}");
    let commit = run(&["cat-file", "-p", id]);
    assert!(
        commit.starts_with("tree ")
            && commit
                .contains("\nauthor Ada Lovelace <ada@example.com> 1759737600 +0530\n\nfirst\n"),
        "{commit}"
    );

    // Errors go to stderr with a nonzero status.
    let failed = nexus_in(&work, &global, &["commit", "-m", "again"]);
    assert_eq!(failed.status.code(), Some(1));
    assert!(
        stderr(&failed).starts_with("error: nothing to commit"),
        "{}",
        stderr(&failed)
    );
}

/// The Phase 2 "done when" sequence, through the real binary: branch, make
/// diverging commits, switch back and forth, tag, unstage, status, and diff.
#[test]
fn branch_diverge_switch_tag_unstage() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let work = dir.path().join("work");
    std::fs::create_dir(&work).expect("create the work dir");
    let global = dir.path().join("global.toml");
    std::fs::write(
        &global,
        "[user]\nname = \"Ada Lovelace\"\nemail = \"ada@example.com\"\n",
    )
    .expect("write the global config");
    let run = |args: &[&str]| {
        let output = nexus_in(&work, &global, args);
        assert!(
            output.status.success(),
            "`nexus {}` failed: {}",
            args.join(" "),
            stderr(&output)
        );
        stdout(&output)
    };
    let write =
        |name: &str, text: &str| std::fs::write(work.join(name), text).expect("write a file");
    let read = |name: &str| std::fs::read_to_string(work.join(name)).expect("read a file");

    run(&["init"]);
    write("a.txt", "base\n");
    run(&["add", "."]);
    run(&["commit", "-m", "base"]);
    run(&["branch", "feature"]);
    run(&["checkout", "feature"]);
    write("a.txt", "feature\n");
    run(&["add", "a.txt"]);
    run(&["commit", "-m", "on feature"]);
    run(&["tag", "v1"]);
    run(&["checkout", "main"]);
    assert_eq!(read("a.txt"), "base\n");
    write("a.txt", "main\n");
    run(&["add", "a.txt"]);
    run(&["commit", "-m", "on main"]);
    run(&["checkout", "v1"]);
    assert_eq!(read("a.txt"), "feature\n");
    run(&["checkout", "main"]);
    assert_eq!(read("a.txt"), "main\n");
    assert!(run(&["diff", "v1", "main"]).contains("-feature\n+main\n"));
    assert!(run(&["log", "--oneline"]).contains("(HEAD -> main) on main"));

    write("a.txt", "staged\n");
    run(&["add", "a.txt"]);
    assert!(run(&["status"]).contains("Changes to be committed:"));
    run(&["restore", "--staged", "a.txt"]);
    let status = run(&["status"]);
    assert!(
        status.contains("Changes not staged for commit:") && !status.contains("to be committed"),
        "{status}"
    );
    assert!(run(&["diff"]).contains("-main\n+staged\n"));

    // A refused checkout explains itself on stderr and exits with 1.
    let refused = nexus_in(&work, &global, &["checkout", "feature"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        stderr(&refused).starts_with("error: checking out would lose work")
            && stderr(&refused).contains("a.txt (changes not staged)"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(read("a.txt"), "staged\n");
}

/// Files nexus writes get the user's umask, as Git's do.
#[cfg(unix)]
#[test]
fn checked_out_files_respect_the_umask() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("a temp dir");
    let work = dir.path().join("work");
    std::fs::create_dir(&work).expect("create the work dir");
    let global = dir.path().join("global.toml");
    std::fs::write(
        &global,
        "[user]\nname = \"Ada Lovelace\"\nemail = \"ada@example.com\"\n",
    )
    .expect("write the global config");
    let run_with_umask = |umask: &str, args: &[&str]| {
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("umask {umask} && exec \"$0\" \"$@\""))
            .arg(env!("CARGO_BIN_EXE_nexus"))
            .args(args)
            .current_dir(&work)
            .env("NO_COLOR", "1")
            .env("NEXUS_DATE", "1759737600 +0530")
            .env("NEXUS_CONFIG_GLOBAL", &global)
            .output()
            .expect("sh runs");
        assert!(
            output.status.success(),
            "`nexus {}` failed: {}",
            args.join(" "),
            stderr(&output)
        );
    };
    let write =
        |name: &str, text: &str| std::fs::write(work.join(name), text).expect("write a file");
    let mode = |name: &str| {
        std::fs::metadata(work.join(name))
            .expect("the file exists")
            .permissions()
            .mode()
            & 0o777
    };

    run_with_umask("022", &["init"]);
    write("a.txt", "1\n");
    write("tool.sh", "#!/bin/sh\necho 1\n");
    run_with_umask("022", &["add", "."]);
    run_with_umask("022", &["add", "--exec", "tool.sh"]);
    run_with_umask("022", &["commit", "-m", "one"]);
    run_with_umask("022", &["branch", "other"]);
    write("a.txt", "2\n");
    write("tool.sh", "#!/bin/sh\necho 2\n");
    run_with_umask("022", &["add", "."]);
    run_with_umask("022", &["commit", "-m", "two"]);

    run_with_umask("077", &["checkout", "other"]);
    assert_eq!((mode("a.txt"), mode("tool.sh")), (0o600, 0o700));
    run_with_umask("022", &["checkout", "main"]);
    assert_eq!((mode("a.txt"), mode("tool.sh")), (0o644, 0o755));
}

/// A diff written to a pipe holds the files' exact bytes, and `git apply`
/// (when Git is installed) accepts it. Status's head line goes to stdout.
#[test]
fn piped_output_is_byte_exact_and_on_stdout() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let work = dir.path().join("work");
    std::fs::create_dir(&work).expect("create the work dir");
    let global = dir.path().join("global.toml");
    std::fs::write(
        &global,
        "[user]\nname = \"Ada Lovelace\"\nemail = \"ada@example.com\"\n",
    )
    .expect("write the global config");
    let run = |args: &[&str]| {
        let output = nexus_in(&work, &global, args);
        assert!(
            output.status.success(),
            "`nexus {}` failed: {}",
            args.join(" "),
            stderr(&output)
        );
        output.stdout
    };
    let old: &[u8] = b"caf\xe9\r\nesc \x1b[31mred\x1b[0m\r\nlast\r\n";
    let new: &[u8] = b"caf\xe9\r\nesc \x1b[31mred\x1b[0m\r\nLAST\r\n";
    run(&["init"]);
    std::fs::write(work.join("f.txt"), old).expect("write a file");
    run(&["add", "."]);
    run(&["commit", "-m", "base"]);
    std::fs::write(work.join("f.txt"), new).expect("write a file");
    let patch = run(&["diff"]);
    let has = |needle: &[u8]| patch.windows(needle.len()).any(|window| window == needle);
    assert!(
        has(b"\n caf\xe9\r\n esc \x1b[31mred\x1b[0m\r\n-last\r\n+LAST\r\n"),
        "{patch:?}"
    );
    assert!(
        patch.starts_with(b"diff --git a/f.txt b/f.txt\n"),
        "{patch:?}"
    );

    // `git apply` turns the old file into the new one.
    if let Ok(probe) = Command::new("git").arg("--version").output()
        && probe.status.success()
    {
        let check = dir.path().join("check");
        std::fs::create_dir(&check).expect("create a directory");
        std::fs::write(check.join("f.txt"), old).expect("write a file");
        std::fs::write(dir.path().join("p.patch"), &patch).expect("write the patch");
        let applied = Command::new("git")
            .args([
                "-c",
                "core.autocrlf=false",
                "apply",
                "--unsafe-paths",
                "--directory=.",
            ])
            .arg(dir.path().join("p.patch"))
            .current_dir(&check)
            .output()
            .expect("git runs");
        assert!(
            applied.status.success(),
            "{}",
            String::from_utf8_lossy(&applied.stderr)
        );
        assert_eq!(std::fs::read(check.join("f.txt")).expect("read"), new);
    }

    // A detached HEAD's line is part of status's output, not a warning.
    let log = String::from_utf8(run(&["log", "--oneline"])).expect("UTF-8");
    let id = log.split(' ').next().expect("an ID").to_owned();
    run(&["restore", "f.txt"]);
    run(&["checkout", &id]);
    let status = String::from_utf8(run(&["status"])).expect("UTF-8");
    assert!(
        status.starts_with(&format!("HEAD detached at {id}")),
        "{status}"
    );
}
