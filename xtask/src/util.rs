//! Running external programs the same way on every OS.

use std::env;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Instant;

pub type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The workspace root: the directory that contains `xtask/`.
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives inside the workspace")
        .to_path_buf()
}

/// The `cargo` that is running this task, so the pinned toolchain is used.
pub fn cargo() -> Command {
    let mut cmd = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.current_dir(root());
    cmd
}

/// Finds `name` on `PATH`.
///
/// On Windows this tries each extension in `PATHEXT`, because `Command::new`
/// only finds `.exe` files by bare name, and tools installed through npm, such
/// as pnpm, are `.cmd` files.
pub fn program(name: &str) -> Result<PathBuf> {
    let extensions: Vec<String> = if cfg!(windows) {
        env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
            .split(';')
            .filter(|ext| !ext.is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        vec![String::new()]
    };
    let path = env::var_os("PATH").ok_or("PATH is not set")?;
    for dir in env::split_paths(&path) {
        for ext in &extensions {
            let candidate = dir.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(format!("`{name}` was not found on PATH").into())
}

/// Prints the command, runs it, and fails if it exits unsuccessfully.
pub fn run(cmd: &mut Command) -> Result {
    let shown = announce(cmd);
    let status = cmd
        .status()
        .map_err(|err| format!("could not start `{shown}`: {err}"))?;
    check(&shown, status)
}

/// Like [`run`], but also returns everything the command wrote to stderr.
/// Each line is still shown as it arrives.
pub fn run_capturing_stderr(cmd: &mut Command) -> Result<String> {
    let shown = announce(cmd);
    let mut child = cmd
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("could not start `{shown}`: {err}"))?;
    let mut captured = String::new();
    if let Some(stderr) = child.stderr.take() {
        let mut terminal = std::io::stderr().lock();
        for line in BufReader::new(stderr).split(b'\n') {
            let line = line?;
            terminal.write_all(&line)?;
            terminal.write_all(b"\n")?;
            captured.push_str(&String::from_utf8_lossy(&line));
            captured.push('\n');
        }
    }
    check(&shown, child.wait()?)?;
    Ok(captured)
}

/// Like [`run`], but returns the command's stdout instead of showing it.
/// Its stderr is still shown.
pub fn run_capturing_stdout(cmd: &mut Command) -> Result<String> {
    let shown = announce(cmd);
    let output = cmd
        .stderr(Stdio::inherit())
        .output()
        .map_err(|err| format!("could not start `{shown}`: {err}"))?;
    check(&shown, output.status)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn announce(cmd: &Command) -> String {
    let shown = describe(cmd);
    println!("$ {shown}");
    shown
}

fn check(shown: &str, status: ExitStatus) -> Result {
    if status.success() {
        Ok(())
    } else {
        Err(format!("`{shown}` failed ({status})").into())
    }
}

/// Runs one named step of a task, printing its name and how long it took.
pub fn step(name: &str, work: impl FnOnce() -> Result) -> Result {
    println!("\n==> {name}");
    let started = Instant::now();
    work().map_err(|err| format!("{name}: {err}"))?;
    println!("    ok ({:.1}s)", started.elapsed().as_secs_f64());
    Ok(())
}

/// A short, readable form of `cmd`: the program's file stem, its arguments,
/// and the directory it runs in when that isn't the workspace root.
fn describe(cmd: &Command) -> String {
    let program = Path::new(cmd.get_program())
        .file_stem()
        .map_or_else(
            || cmd.get_program().to_string_lossy(),
            |stem| stem.to_string_lossy(),
        )
        .into_owned();
    let mut shown = std::iter::once(program)
        .chain(cmd.get_args().map(|arg| arg.to_string_lossy().into_owned()))
        .collect::<Vec<_>>()
        .join(" ");
    if let Some(dir) = cmd.get_current_dir()
        && let Ok(relative) = dir.strip_prefix(root())
        && !relative.as_os_str().is_empty()
    {
        shown = format!("{shown}   (in {})", relative.display());
    }
    shown
}
