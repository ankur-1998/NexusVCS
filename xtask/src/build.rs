//! `cargo xtask build`: the web UI, then the release binary.
//!
//! Until Phase 7 embeds the UI with `rust-embed`, the two artifacts are
//! separate: the binary, at the path printed at the end, and the UI in
//! `web/dist/`.

use std::path::PathBuf;

use serde_json::Value;

use crate::util::{self, Result, step};
use crate::web;

pub fn run() -> Result {
    step("pnpm install", web::install)?;
    step("web build", || web::pnpm(&["run", "build"]))?;

    let mut binary = None;
    step("release binary", || {
        binary = Some(release_binary()?);
        Ok(())
    })?;

    if let Some(binary) = binary {
        println!("\nBuilt {}", binary.display());
    }
    Ok(())
}

/// Builds the release binary and returns its path.
pub fn release_binary() -> Result<PathBuf> {
    let messages = util::run_capturing_stdout(util::cargo().args([
        "build",
        "--release",
        "--locked",
        "--package",
        "nexus",
        "--message-format=json-render-diagnostics",
    ]))?;
    Ok(executable(&messages, "nexus").ok_or("cargo didn't report building the nexus binary")?)
}

/// Finds the executable for the binary target `name` in cargo's JSON messages.
/// Asking cargo, instead of assuming `target/release`, keeps the path right when
/// `CARGO_TARGET_DIR`, `build.target-dir`, or `--target` moves the output.
fn executable(messages: &str, name: &str) -> Option<PathBuf> {
    messages
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|message| {
            message["reason"] == "compiler-artifact" && message["target"]["name"] == name
        })
        .find_map(|message| message["executable"].as_str().map(PathBuf::from))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_binary_among_other_messages() {
        let messages = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"clap","kind":["lib"]},"executable":null}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"nexus","kind":["bin"]},"executable":"C:\\work\\target\\release\\nexus.exe"}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
            "\n",
        );
        assert_eq!(
            executable(messages, "nexus"),
            Some(PathBuf::from(r"C:\work\target\release\nexus.exe"))
        );
    }

    #[test]
    fn reports_nothing_when_the_binary_is_missing() {
        let messages = r#"{"reason":"build-finished","success":true}"#;
        assert_eq!(executable(messages, "nexus"), None);
    }
}
