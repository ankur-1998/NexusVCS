//! The `nexus` binary. It parses arguments with the definitions shared in
//! `nexus-core`, runs the command there, and renders the result for the
//! terminal (spec §5.2).

use std::ffi::OsString;
use std::io::Write as _;
use std::process::ExitCode;

use anstyle::AnsiColor;
use clap::Parser as _;
use nexus_core::cli::Cli;
use nexus_core::commands::{self, CommandResult, Ctx, Style};

/// The exit status after Ctrl+C, as shells report for SIGINT.
const INTERRUPTED: i32 = 130;

fn main() -> ExitCode {
    // Ctrl+C or a termination signal ends the process without running
    // destructors, which would leave `.nexus/lock` behind. Remove it first.
    // Everything already written stays consistent (writes are atomic, and the
    // next command records a `recovered` op if needed).
    let _ = ctrlc::set_handler(|| {
        nexus_core::oplog::remove_held_locks();
        std::process::exit(INTERRUPTED);
    });

    let argv: Vec<OsString> = std::env::args_os().collect();
    // Parsing here, rather than through `commands::run`, lets clap print help
    // and usage errors in color itself.
    let cli = Cli::parse_from(&argv);
    let ctx = match Ctx::from_process() {
        Ok(ctx) => ctx,
        Err(message) => {
            let _ = writeln!(anstream::stderr(), "error: {message}");
            return ExitCode::FAILURE;
        }
    };
    let result = commands::execute(cli, &argv, &ctx);
    render(&result);
    ExitCode::from(u8::try_from(result.exit_code).unwrap_or(1))
}

/// Errors and warnings go to stderr, everything else to stdout. Colors are
/// dropped automatically when the output isn't a terminal or `NO_COLOR` is
/// set. Raw output (file content) is written to stdout byte for byte, and so
/// are lines of file content when stdout isn't a terminal, so that
/// `nexus diff > change.patch` saves exactly what the files hold.
fn render(result: &CommandResult) {
    let mut stdout = anstream::stdout().lock();
    let mut stderr = anstream::stderr().lock();
    let exact = !stdout.is_terminal();
    let color = stdout.current_choice() != anstream::ColorChoice::Never;
    for line in &result.lines {
        let style = terminal_style(line.style);
        let text = &line.text;
        let written = if matches!(line.style, Some(Style::Error | Style::Warning)) {
            writeln!(stderr, "{style}{text}{style:#}")
        } else if let (true, Some(bytes)) = (exact, &line.bytes) {
            // Past anstream, which would strip escape sequences that are part
            // of the file. Colors only if they were forced on.
            let mut out = std::io::stdout().lock();
            let (start, end) = if color {
                (style.render().to_string(), style.render_reset().to_string())
            } else {
                (String::new(), String::new())
            };
            out.write_all(start.as_bytes())
                .and_then(|()| out.write_all(bytes))
                .and_then(|()| out.write_all(end.as_bytes()))
                .and_then(|()| out.write_all(b"\n"))
        } else {
            writeln!(stdout, "{style}{text}{style:#}")
        };
        // A closed pipe (`nexus log | head`) isn't an error worth reporting.
        if written.is_err() {
            return;
        }
    }
    drop(stdout);
    if let Some(raw) = &result.raw {
        // Not through anstream, which would strip escape sequences that are
        // part of the file.
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(raw).and_then(|()| out.flush());
    }
}

fn terminal_style(style: Option<Style>) -> anstyle::Style {
    let color = |color: AnsiColor| anstyle::Style::new().fg_color(Some(color.into()));
    match style {
        None => anstyle::Style::new(),
        Some(Style::Added) => color(AnsiColor::Green),
        Some(Style::Removed) => color(AnsiColor::Red),
        Some(Style::Hash) => color(AnsiColor::Yellow),
        Some(Style::Heading) => anstyle::Style::new().bold(),
        Some(Style::Warning) => color(AnsiColor::Yellow).bold(),
        Some(Style::Error) => color(AnsiColor::Red).bold(),
        Some(Style::Muted) => anstyle::Style::new().dimmed(),
    }
}
