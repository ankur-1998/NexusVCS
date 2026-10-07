//! The Svelte app in `web/`: pnpm commands and the bundle budget.

use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::Command;

use flate2::Compression;
use flate2::write::GzEncoder;

use crate::util::{self, Result};

/// Spec §6: the JavaScript the dashboard loads up front must stay under
/// 150 KB once gzipped. Everything else has to be loaded lazily. The spec writes
/// budgets in decimal units (KB, MB), so this is 150,000 bytes.
const INITIAL_JS_BUDGET: usize = 150_000;

fn dir() -> PathBuf {
    util::root().join("web")
}

/// Runs `pnpm <args>` inside `web/`.
pub fn pnpm(args: &[&str]) -> Result {
    let mut cmd = Command::new(util::program("pnpm")?);
    cmd.args(args).current_dir(dir());
    util::run(&mut cmd)
}

/// Installs exactly what the lockfile pins, so local runs match CI.
pub fn install() -> Result {
    pnpm(&["install", "--frozen-lockfile"])
}

/// Fails if the JavaScript that `web/dist/index.html` loads up front is over
/// [`INITIAL_JS_BUDGET`] once gzipped.
pub fn check_bundle_budget() -> Result {
    let dist = dir().join("dist");
    let html = fs::read_to_string(dist.join("index.html")).map_err(|err| {
        format!("could not read web/dist/index.html (run the web build first): {err}")
    })?;
    let scripts = initial_scripts(&html);
    if scripts.is_empty() {
        return Err("web/dist/index.html loads no module scripts".into());
    }

    let mut total = 0;
    for src in &scripts {
        let relative = src.trim_start_matches("./").trim_start_matches('/');
        let bytes = fs::read(dist.join(relative))
            .map_err(|err| format!("could not read {relative} from web/dist: {err}"))?;
        let size = gzipped_len(&bytes)?;
        println!("    {relative}: {} gzipped", kb(size));
        total += size;
    }
    println!(
        "    initial JS: {} of the {} budget",
        kb(total),
        kb(INITIAL_JS_BUDGET)
    );
    if total > INITIAL_JS_BUDGET {
        return Err(format!(
            "initial JS is {} gzipped, over the {} budget; load more of it lazily",
            kb(total),
            kb(INITIAL_JS_BUDGET)
        )
        .into());
    }
    Ok(())
}

/// The scripts a browser fetches before the app starts: every
/// `<script type="module" src>` and every `<link rel="modulepreload" href>`.
fn initial_scripts(html: &str) -> Vec<String> {
    let mut scripts = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('>') else { break };
        let tag = &rest[..end];
        rest = &rest[end + 1..];

        let name = tag.split_whitespace().next().unwrap_or_default();
        let url = if name.eq_ignore_ascii_case("script")
            && attribute(tag, "type").as_deref() == Some("module")
        {
            attribute(tag, "src")
        } else if name.eq_ignore_ascii_case("link")
            && attribute(tag, "rel").as_deref() == Some("modulepreload")
        {
            attribute(tag, "href")
        } else {
            None
        };
        if let Some(url) = url
            && !scripts.contains(&url)
        {
            scripts.push(url);
        }
    }
    scripts
}

/// The value of `name="..."` inside a tag's text. Vite always emits
/// double-quoted attributes.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let mut search = tag;
    loop {
        let found = search.find(&needle)?;
        let preceded_by_space = search[..found]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace);
        let value_start = found + needle.len();
        if preceded_by_space {
            let value = &search[value_start..];
            let value_end = value.find('"')?;
            return Some(value[..value_end].to_owned());
        }
        search = &search[value_start..];
    }
}

fn gzipped_len(bytes: &[u8]) -> Result<usize> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?.len())
}

/// Formats a byte count in decimal kilobytes with one decimal place, rounding
/// down, without floats.
fn kb(bytes: usize) -> String {
    format!("{}.{} KB", bytes / 1000, bytes % 1000 / 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VITE_INDEX: &str = r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <title>NexusVCS</title>
    <script type="module" crossorigin src="/assets/index-Dk3a9.js"></script>
    <link rel="modulepreload" crossorigin href="/assets/vendor-B7x1.js">
    <link rel="stylesheet" crossorigin href="/assets/index-C2f0.css">
  </head>
  <body><div id="app"></div></body>
</html>"#;

    #[test]
    fn finds_module_scripts_and_preloads_but_not_styles() {
        assert_eq!(
            initial_scripts(VITE_INDEX),
            ["/assets/index-Dk3a9.js", "/assets/vendor-B7x1.js"]
        );
    }

    #[test]
    fn ignores_classic_scripts_and_lookalike_attributes() {
        let html = r#"<script src="/legacy.js"></script><script data-src="/x.js" type="module" src="/real.js"></script>"#;
        assert_eq!(initial_scripts(html), ["/real.js"]);
    }

    #[test]
    fn formats_decimal_kb_rounding_down() {
        assert_eq!(kb(0), "0.0 KB");
        assert_eq!(kb(1_599), "1.5 KB");
        assert_eq!(kb(149_999), "149.9 KB");
        assert_eq!(kb(INITIAL_JS_BUDGET), "150.0 KB");
    }

    #[test]
    fn gzip_shrinks_repetitive_input() {
        let input = vec![b'a'; 10_000];
        assert!(gzipped_len(&input).unwrap() < 100);
    }
}
