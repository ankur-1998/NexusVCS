//! `cargo xtask ci`: every check CI runs, ordered so the fastest failures come first.

use std::io::IsTerminal as _;
use std::time::Instant;

use crate::util::{self, Result, step};
use crate::web;

/// What clippy prints, as a warning that `-D warnings` doesn't turn into an
/// error, when a `clippy.toml` path doesn't resolve. Failing on it means a
/// mistyped network rule can't silently switch itself off.
const UNRESOLVED_RULE: &str = "does not refer to a reachable";

pub fn run() -> Result {
    let started = Instant::now();

    step("rustfmt", || {
        util::run(util::cargo().args(["fmt", "--all", "--", "--check"]))
    })?;
    step("clippy (includes the network rules in clippy.toml)", || {
        clippy(&["--workspace", "--all-targets", "--all-features"])
    })?;
    step("cargo test", || {
        util::run(util::cargo().args(["test", "--workspace", "--all-features", "--locked"]))
    })?;
    step(
        "clippy on the CLI-only build (--no-default-features)",
        || {
            clippy(&[
                "--package",
                "nexus",
                "--all-targets",
                "--no-default-features",
            ])
        },
    )?;
    step("pnpm install", web::install)?;
    step("svelte-check", || web::pnpm(&["run", "check"]))?;
    step("prettier", || web::pnpm(&["run", "format:check"]))?;
    step("vitest", || web::pnpm(&["run", "test"]))?;
    step("web build", || web::pnpm(&["run", "build"]))?;
    step("bundle budget", web::check_bundle_budget)?;

    println!(
        "\nAll checks passed in {:.1}s.",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Runs clippy with `--locked` and `-D warnings`, and also fails if any
/// `clippy.toml` rule doesn't resolve.
fn clippy(args: &[&str]) -> Result {
    let mut cmd = util::cargo();
    cmd.arg("clippy");
    // Clippy's stderr is piped so it can be scanned, so keep its colors when
    // a person is watching.
    if std::io::stderr().is_terminal() {
        cmd.arg("--color=always");
    }
    cmd.args(args).args(["--locked", "--", "-D", "warnings"]);
    let stderr = util::run_capturing_stderr(&mut cmd)?;
    if stderr.contains(UNRESOLVED_RULE) {
        return Err(format!(
            "a clippy.toml rule doesn't resolve (clippy says \"{UNRESOLVED_RULE} ...\" above); fix the path"
        )
        .into());
    }
    Ok(())
}
