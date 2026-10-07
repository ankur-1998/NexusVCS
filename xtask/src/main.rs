//! Project automation. Run `cargo xtask help` for the list of tasks.
//!
//! Every task is plain Rust, so it behaves the same on Linux, macOS, and
//! Windows, with no bash, PowerShell, or Makefiles involved (spec §1, rule 5).

mod bench;
mod build;
mod ci;
mod util;
mod web;

use std::process::ExitCode;

const HELP: &str = "\
Usage: cargo xtask <task>

Tasks:
  ci      Run every check CI runs: formatting, lints, tests, the CLI-only
          build, web checks and tests, the web build, and the bundle budget
  build   Build the web UI and the release binary
  bench   Measure the release binary against the spec's performance budgets
          on synthetic repositories (cached in the temp directory)
  help    Show this message

Later phases add: dev, demo, dist.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let task = match args.as_slice() {
        [task] => task.as_str(),
        [] => {
            eprint!("{HELP}");
            return ExitCode::FAILURE;
        }
        [_, extra, ..] => {
            eprintln!("unexpected argument `{extra}`\n\n{HELP}");
            return ExitCode::FAILURE;
        }
    };
    let result = match task {
        "ci" => ci::run(),
        "build" => build::run(),
        "bench" => bench::run(),
        "help" | "-h" | "--help" => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        other => {
            eprintln!("unknown task `{other}`\n\n{HELP}");
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("\nerror: {err}");
            ExitCode::FAILURE
        }
    }
}
