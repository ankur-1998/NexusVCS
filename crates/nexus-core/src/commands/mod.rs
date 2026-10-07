//! The command layer (spec §5.2). Every `nexus` command is implemented once,
//! here. Commands never print and never prompt: they return a
//! [`CommandResult`], which the CLI renders with colors and the web terminal
//! renders in xterm.js.

mod add;
mod branch;
mod checkout;
mod commit;
mod config;
mod diff;
mod export;
mod init;
mod inspect;
mod log;
mod render;
mod restore;
mod rm;
mod show;
mod status;
mod tag;

use std::ffi::OsString;
use std::path::PathBuf;

use clap::Parser as _;

use crate::cli::{Cli, Command, DebugCommand};
use crate::error::Error;
use crate::oplog;
use crate::platform;
use crate::time::{Clock, Timestamp};

/// The environment a command runs in.
#[derive(Clone, Debug)]
pub struct Ctx {
    /// The directory the command was run from. The repository is found by
    /// searching upward from here.
    pub cwd: PathBuf,
    /// The global config file, if there is one.
    pub global_config: Option<PathBuf>,
    pub clock: Clock,
}

impl Ctx {
    /// The context of this process.
    ///
    /// Two environment variables exist for tests and scripts:
    /// `NEXUS_CONFIG_GLOBAL` replaces the global config path (empty disables
    /// it), and `NEXUS_DATE` pins the clock, in the form `1759737600 +0530`.
    pub fn from_process() -> Result<Self, String> {
        let cwd = std::env::current_dir()
            .map_err(|err| format!("can't read the current directory: {err}"))?;
        let global_config = match std::env::var_os("NEXUS_CONFIG_GLOBAL") {
            Some(path) if path.is_empty() => None,
            Some(path) => Some(PathBuf::from(path)),
            None => platform::global_config_file(),
        };
        let clock = match std::env::var("NEXUS_DATE") {
            Ok(text) => Clock::Fixed(
                Timestamp::parse(&text).ok_or("NEXUS_DATE must look like `1759737600 +0530`")?,
            ),
            Err(_) => Clock::System,
        };
        Ok(Self {
            cwd,
            global_config,
            clock,
        })
    }
}

/// What a command produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandResult {
    pub exit_code: i32,
    pub lines: Vec<Line>,
    /// Bytes to write to standard output exactly as they are, after the
    /// lines: file content from `cat-file -p`, which must not be split into
    /// lines, re-encoded, or stripped of control characters.
    pub raw: Option<Vec<u8>>,
}

impl CommandResult {
    /// All lines' text, newline-terminated, then any raw output (decoded
    /// lossily as UTF-8).
    pub fn text(&self) -> String {
        let mut text = String::new();
        for line in &self.lines {
            text.push_str(&line.text);
            text.push('\n');
        }
        if let Some(raw) = &self.raw {
            text.push_str(&String::from_utf8_lossy(raw));
        }
        text
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// What to show. It never holds control characters other than tabs
    /// (see [`displayable`]), so nothing a command prints, from a file name
    /// to a commit message, can send escape sequences to a terminal.
    pub text: String,
    pub style: Option<Style>,
    /// The line's exact bytes, for lines that carry file content (the lines
    /// of a diff), which needn't be UTF-8 or free of control characters.
    /// The CLI writes these when its output isn't a terminal, so a saved
    /// diff is byte for byte what the files hold. `text` is the same line
    /// made safe to show.
    pub bytes: Option<Vec<u8>>,
}

/// How a line should look. Renderers choose the actual colors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Added,
    Removed,
    Hash,
    Heading,
    Warning,
    Error,
    Muted,
}

/// Collects a command's output lines.
#[derive(Default)]
pub(crate) struct Output {
    lines: Vec<Line>,
    raw: Option<Vec<u8>>,
}

impl Output {
    pub fn line(&mut self, text: impl Into<String>) {
        self.lines.push(Line {
            text: displayable(text.into()),
            style: None,
            bytes: None,
        });
    }

    pub fn styled(&mut self, style: Style, text: impl Into<String>) {
        self.lines.push(Line {
            text: displayable(text.into()),
            style: Some(style),
            bytes: None,
        });
    }

    pub fn warning(&mut self, message: impl std::fmt::Display) {
        self.styled(Style::Warning, format!("warning: {message}"));
    }

    /// A line of file content: `text` to display, `bytes` as they are (see
    /// [`Line::bytes`]).
    pub fn content(&mut self, style: Option<Style>, text: String, bytes: Vec<u8>) {
        self.lines.push(Line {
            text: displayable(text),
            style,
            bytes: Some(bytes),
        });
    }

    /// Sets bytes to be written verbatim (see [`CommandResult::raw`]).
    pub fn raw(&mut self, bytes: Vec<u8>) {
        self.raw = Some(bytes);
    }

    /// Adds `text` split into lines, so multi-line messages render properly.
    pub fn text(&mut self, style: Option<Style>, text: &str) {
        for line in text.lines() {
            self.lines.push(Line {
                text: displayable(line.to_owned()),
                style,
                bytes: None,
            });
        }
    }
}

/// `text` with control characters other than tabs shown in caret notation
/// (`^[` for ESC, `^?` for DEL) or, for the rarer C1 controls, as U+FFFD.
pub fn displayable(text: String) -> String {
    if !text.chars().any(|c| c.is_control() && c != '\t') {
        return text;
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '\t' => out.push('\t'),
            '\u{7f}' => out.push_str("^?"),
            c if u32::from(c) < 0x20 => {
                out.push('^');
                out.push(char::from_u32(u32::from(c) + 0x40).unwrap_or('?'));
            }
            c if c.is_control() => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
    out
}

/// Parses `argv` (including the program name) and runs the command.
pub fn run<I, T>(argv: I, ctx: &Ctx) -> CommandResult
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let argv: Vec<OsString> = argv.into_iter().map(Into::into).collect();
    match Cli::try_parse_from(&argv) {
        Ok(cli) => execute(cli, &argv, ctx),
        Err(err) => {
            let mut out = Output::default();
            let style = err.use_stderr().then_some(Style::Error);
            out.text(style, &err.render().to_string());
            CommandResult {
                exit_code: err.exit_code(),
                lines: out.lines,
                raw: None,
            }
        }
    }
}

/// Runs an already-parsed command. `argv` is the raw command line, recorded in
/// the op log.
pub fn execute(cli: Cli, argv: &[OsString], ctx: &Ctx) -> CommandResult {
    let arguments: Vec<String> = argv
        .iter()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let command_line = oplog::command_line(&arguments);
    let mut out = Output::default();
    let result = match cli.command {
        Command::Init { dir } => init::run(dir.as_deref(), ctx, &command_line, &mut out),
        Command::Add { paths, exec } => add::run(&paths, exec, ctx, &command_line, &mut out),
        Command::Commit {
            message,
            allow_empty,
        } => commit::run(&message, allow_empty, ctx, &command_line, &mut out),
        Command::Log { max_count, oneline } => log::run(max_count, oneline, ctx, &mut out),
        Command::Config { global, key, value } => config::run(
            global,
            key.into(),
            value.as_deref(),
            ctx,
            &command_line,
            &mut out,
        ),
        Command::Status => status::run(ctx, &command_line, &mut out),
        Command::Diff {
            staged,
            options,
            revs,
            paths,
        } => diff::run(staged, options, &revs, &paths, ctx, &mut out),
        Command::Show { commit, options } => show::run(&commit, options, ctx, &mut out),
        Command::Branch {
            name,
            start,
            delete,
            force_delete,
        } => branch::run(
            &branch::Args {
                name: name.as_deref(),
                start: start.as_deref(),
                delete,
                force_delete,
            },
            ctx,
            &command_line,
            &mut out,
        ),
        Command::Tag {
            name,
            commit,
            delete,
        } => tag::run(
            name.as_deref(),
            commit.as_deref(),
            delete,
            ctx,
            &command_line,
            &mut out,
        ),
        Command::Checkout { target } => checkout::run(&target, ctx, &command_line, &mut out),
        Command::Restore {
            staged,
            source,
            paths,
        } => restore::run(
            staged,
            source.as_deref(),
            &paths,
            ctx,
            &command_line,
            &mut out,
        ),
        Command::Rm { cached, paths } => rm::run(cached, &paths, ctx, &command_line, &mut out),
        Command::Export { commit, dir } => export::run(&commit, &dir, ctx, &mut out),
        Command::HashObject { file } => inspect::hash_object(&file, ctx, &mut out),
        Command::CatFile { pretty: _, id } => inspect::cat_file(&id, ctx, &mut out),
        Command::Debug {
            what: DebugCommand::Index,
        } => inspect::debug_index(ctx, &mut out),
    };
    let exit_code = match result {
        Ok(()) => 0,
        Err(err) => {
            report_error(&err, &mut out);
            1
        }
    };
    CommandResult {
        exit_code,
        lines: out.lines,
        raw: out.raw,
    }
}

fn report_error(err: &Error, out: &mut Output) {
    let message = err.to_string();
    let mut lines = message.lines();
    if let Some(first) = lines.next() {
        out.styled(Style::Error, format!("error: {first}"));
    }
    for line in lines {
        out.styled(Style::Error, line);
    }
}
