//! `cargo xtask bench`: the release binary measured against the spec §6
//! budgets that exist so far (the `status`, `add`, `commit`, `log`, and
//! large-file rows), on synthetic repositories, with Git as the reference
//! where a budget names it.
//!
//! Every number is the wall time of a real `nexus` process, so it includes
//! process start. Process start alone (`nexus --version`) is measured too and
//! shown next to each row, because on some machines (Windows with real-time
//! antivirus scanning, for one) it's most of the budget.
//!
//! The repositories are generated once and cached in the system temp
//! directory (`nexusvcs-bench`, or `$NEXUS_BENCH_DIR`). They don't go under
//! `target/` because `nexus init` refuses to create a repository inside
//! another one, and this workspace may itself be one.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{BufWriter, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::build;
use crate::util::{self, Result, step};

/// The synthetic tree: 10,000 files in 1,000 directories (spec §6).
const TOP_DIRS: usize = 100;
const SUB_DIRS: usize = 10;
const FILES_PER_DIR: usize = 10;
/// Commits in the history `log` walks (spec §6).
const COMMITS: usize = 1_000;
const LARGE_LEN: u64 = 1 << 30;
const EDIT_LEN: u64 = 1 << 20;
/// Changes whenever the generated content does, so stale caches are rebuilt.
const CACHE_VERSION: &str = "1";
/// Runs per measurement; the median is reported.
const RUNS: usize = 10;

const MS: f64 = 1000.0;
const MB: f64 = 1_000_000.0;
const MIB: f64 = 1024.0 * 1024.0;

pub fn run() -> Result {
    let mut nexus = PathBuf::new();
    step("release binary", || {
        nexus = build::release_binary()?;
        Ok(())
    })?;
    let dir = bench_dir();
    fs::create_dir_all(&dir)?;
    let tools = Tools::new(nexus, &dir)?;
    println!("\nBenchmark data: {}", dir.display());
    match &tools.git {
        Some(git) => println!("Git reference: {}", git.display()),
        None => println!(
            "Git isn't installed, so rows that compare with it only check their own limits."
        ),
    }

    let mut report = Report::default();
    step("process start (nexus --version)", || {
        report.start = median(RUNS * 2, || time(&tools.nexus(&dir, &["--version"])))?;
        Ok(())
    })?;
    let repo = dir.join("repo");
    let git_repo = dir.join("git-repo");
    step(
        "synthetic repositories (cached after the first run)",
        || prepare(&tools, &dir, &repo, &git_repo),
    )?;
    step("status, nothing changed", || {
        bench_status(&tools, &repo, &git_repo, &mut report)
    })?;
    step("add ., fresh repository", || {
        bench_add(&tools, &dir, &repo, &git_repo, &mut report)
    })?;
    step("commit after one changed file", || {
        bench_commit(&tools, &repo, &mut report)
    })?;
    step("log -n 100", || bench_log(&tools, &repo, &mut report))?;
    step("1 GiB file", || bench_large(&tools, &dir, &mut report))?;

    report.print();
    let results = dir.join("results.json");
    fs::write(&results, serde_json::to_string_pretty(&report.json())?)?;
    println!("\nResults saved to {}", results.display());
    Ok(())
}

fn bench_dir() -> PathBuf {
    std::env::var_os("NEXUS_BENCH_DIR").map_or_else(
        || std::env::temp_dir().join("nexusvcs-bench"),
        PathBuf::from,
    )
}

/// The programs being measured, and the environment they run in.
struct Tools {
    nexus: PathBuf,
    git: Option<PathBuf>,
    global_config: PathBuf,
}

impl Tools {
    fn new(nexus: PathBuf, dir: &Path) -> Result<Self> {
        let global_config = dir.join("global.toml");
        fs::write(
            &global_config,
            "[user]\nname = \"Bench\"\nemail = \"bench@example.com\"\n",
        )?;
        Ok(Self {
            nexus,
            git: util::program("git").ok(),
            global_config,
        })
    }

    fn nexus(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.nexus);
        cmd.args(args)
            .current_dir(cwd)
            .env("NO_COLOR", "1")
            .env("NEXUS_CONFIG_GLOBAL", &self.global_config);
        cmd
    }

    fn git(&self, cwd: &Path, args: &[&str]) -> Option<Command> {
        let mut cmd = Command::new(self.git.as_ref()?);
        cmd.args([
            "-c",
            "user.name=Bench",
            "-c",
            "user.email=bench@example.com",
        ])
        .args(args)
        .current_dir(cwd);
        Some(cmd)
    }
}

/// Runs a command that must succeed and returns its wall time. Its
/// output is discarded.
fn time(cmd: &Command) -> Result<Duration> {
    let mut cmd = rebuild(cmd);
    let started = Instant::now();
    let output = cmd.stdout(Stdio::null()).stderr(Stdio::piped()).output()?;
    let elapsed = started.elapsed();
    if !output.status.success() {
        return Err(failure(&cmd, &output.stderr));
    }
    Ok(elapsed)
}

/// Runs a command that must succeed and returns its stdout.
fn output(cmd: &Command) -> Result<String> {
    let mut cmd = rebuild(cmd);
    let output = cmd.stdin(Stdio::null()).output()?;
    if !output.status.success() {
        return Err(failure(&cmd, &output.stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A copy of `cmd` that can be run again (`Command` can't be cloned).
fn rebuild(cmd: &Command) -> Command {
    let mut copy = Command::new(cmd.get_program());
    copy.args(cmd.get_args());
    if let Some(dir) = cmd.get_current_dir() {
        copy.current_dir(dir);
    }
    for (key, value) in cmd.get_envs() {
        match value {
            Some(value) => copy.env(key, value),
            None => copy.env_remove(key),
        };
    }
    copy
}

fn failure(cmd: &Command, stderr: &[u8]) -> Box<dyn std::error::Error> {
    let args: Vec<String> = cmd
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    format!(
        "`{} {}` failed: {}",
        Path::new(cmd.get_program())
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy(),
        args.join(" "),
        String::from_utf8_lossy(stderr).trim()
    )
    .into()
}

/// The median of `runs` measurements, after one warm-up run.
fn median(runs: usize, mut measure: impl FnMut() -> Result<Duration>) -> Result<Duration> {
    measure()?;
    let mut times = (0..runs).map(|_| measure()).collect::<Result<Vec<_>>>()?;
    times.sort();
    Ok(times[times.len() / 2])
}

/// Builds (or finds in the cache) a Nexus repository holding the synthetic
/// tree with 1,000 commits of history, and a Git repository with the same
/// tree in one commit.
fn prepare(tools: &Tools, dir: &Path, repo: &Path, git_repo: &Path) -> Result {
    restore_kept(&repo.join(".nexus"), &dir.join("kept.nexus"))?;
    restore_kept(&git_repo.join(".git"), &dir.join("kept.git"))?;
    let marker = dir.join(format!("repo.v{CACHE_VERSION}.done"));
    if !marker.exists() {
        remove_dir(repo)?;
        println!("    writing {} files", TOP_DIRS * SUB_DIRS * FILES_PER_DIR);
        write_tree(repo)?;
        run_ok(tools.nexus(repo, &["init"]))?;
        run_ok(tools.nexus(repo, &["add", "."]))?;
        run_ok(tools.nexus(repo, &["commit", "-m", "base"]))?;
        println!(
            "    making {} more commits (a few minutes, once)",
            COMMITS - 1
        );
        for n in 1..COMMITS {
            let file = format!("m{:02}/s{}/f0.txt", n % TOP_DIRS, (n / TOP_DIRS) % SUB_DIRS);
            append(&repo.join(&file), &format!("edit {n}\n"))?;
            run_ok(tools.nexus(repo, &["add", &file]))?;
            run_ok(tools.nexus(repo, &["commit", "-m", &format!("edit {n}")]))?;
        }
        fs::write(&marker, "")?;
    }
    let git_marker = dir.join(format!("git-repo.v{CACHE_VERSION}.done"));
    if tools.git.is_some() && !git_marker.exists() {
        remove_dir(git_repo)?;
        write_tree(git_repo)?;
        for args in [
            &["init", "-q"][..],
            &["add", "."],
            &["commit", "-q", "-m", "base"],
        ] {
            run_ok(tools.git(git_repo, args).ok_or("git disappeared")?)?;
        }
        fs::write(&git_marker, "")?;
    }
    Ok(())
}

/// Runs `work` with `repo`'s metadata directory (`name`) moved to `kept`,
/// then puts it back, also when `work` fails.
fn moved_aside<T>(
    repo: &Path,
    name: &str,
    kept: &Path,
    work: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let original = repo.join(name);
    restore_kept(&original, kept)?;
    fs::rename(&original, kept)?;
    let result = work();
    let restored = restore_kept(&original, kept);
    let value = result?;
    restored?;
    Ok(value)
}

/// Puts a metadata directory moved aside by [`moved_aside`] back in place,
/// replacing whatever is there: also after an interrupted run.
fn restore_kept(original: &Path, kept: &Path) -> Result {
    if kept.exists() {
        remove_dir(original)?;
        fs::rename(kept, original)?;
    }
    Ok(())
}

fn run_ok(mut cmd: Command) -> Result {
    let output = cmd.stdin(Stdio::null()).output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(failure(&cmd, &output.stderr))
    }
}

fn remove_dir(dir: &Path) -> Result {
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("can't remove {}: {err}", dir.display()).into()),
    }
}

fn append(path: &Path, text: &str) -> Result {
    let mut file = fs::OpenOptions::new().append(true).open(path)?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

/// Writes the synthetic tree: source-like text files of 0.5 to 8 KiB.
fn write_tree(root: &Path) -> Result {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    for top in 0..TOP_DIRS {
        for sub in 0..SUB_DIRS {
            let dir = root.join(format!("m{top:02}")).join(format!("s{sub}"));
            fs::create_dir_all(&dir)?;
            for file in 0..FILES_PER_DIR {
                let len = 512 + usize::try_from(next(&mut state) % 7680)?;
                let mut text = String::with_capacity(len + 80);
                let mut line = 0;
                while text.len() < len {
                    line += 1;
                    let _ = writeln!(
                        text,
                        "    let value_{line} = compute({top}, {sub}, {file}, {:#x}); // line {line}",
                        next(&mut state) % 65_536
                    );
                }
                fs::write(dir.join(format!("f{file}.txt")), text)?;
            }
        }
    }
    Ok(())
}

/// xorshift64*: fast, deterministic, and incompressible.
fn next(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_f491_4f6c_dd1d)
}

fn bench_status(tools: &Tools, repo: &Path, git_repo: &Path, report: &mut Report) -> Result {
    let status = tools.nexus(repo, &["status"]);
    let text = output(&status)?;
    if !text.contains("nothing to commit, working tree clean") {
        return Err(format!("the benchmark repository isn't clean:\n{text}").into());
    }
    let measured = median(RUNS, || time(&status))?;
    let git = match tools.git(git_repo, &["status"]) {
        Some(cmd) => {
            let porcelain = output(
                &tools
                    .git(git_repo, &["status", "--porcelain"])
                    .ok_or("git disappeared")?,
            )?;
            if !porcelain.trim().is_empty() {
                return Err(
                    format!("the Git reference repository isn't clean:\n{porcelain}").into(),
                );
            }
            Some(median(RUNS, || time(&cmd))?)
        }
        None => None,
    };
    let limit = Duration::from_millis(100);
    let pass = measured < limit && git.is_none_or(|git| measured <= git * 2);
    report.push(Row {
        operation: "nexus status, nothing changed".to_owned(),
        measured: millis(measured),
        net: Some(net(measured, report.start)),
        budget: format!("≤ 2× git status{}, and < 100 ms", reference(git)),
        pass: Some(pass),
        value: measured.as_secs_f64() * MS,
    });
    Ok(())
}

fn bench_add(
    tools: &Tools,
    dir: &Path,
    repo: &Path,
    git_repo: &Path,
    report: &mut Report,
) -> Result {
    // Each run starts from no repository: the cached one is moved aside.
    let measured = moved_aside(repo, ".nexus", &dir.join("kept.nexus"), || {
        let mut times = Vec::new();
        for _ in 0..3 {
            remove_dir(&repo.join(".nexus"))?;
            run_ok(tools.nexus(repo, &["init"]))?;
            times.push(time(&tools.nexus(repo, &["add", "."]))?);
        }
        times.sort();
        Ok(times[1])
    })?;
    let git = if tools.git.is_some() {
        let git_cmd = |args: &[&str]| tools.git(git_repo, args).ok_or("git disappeared");
        Some(moved_aside(
            git_repo,
            ".git",
            &dir.join("kept.git"),
            || {
                let mut times = Vec::new();
                for _ in 0..3 {
                    remove_dir(&git_repo.join(".git"))?;
                    run_ok(git_cmd(&["init", "-q"])?)?;
                    times.push(time(&git_cmd(&["add", "."])?)?);
                }
                times.sort();
                Ok(times[1])
            },
        )?)
    } else {
        None
    };
    report.push(Row {
        operation: "nexus add ., fresh repository (10,000 files)".to_owned(),
        measured: millis(measured),
        net: None,
        budget: format!("≤ 2× git add .{}", reference(git)),
        pass: git.map(|git| measured <= git * 2),
        value: measured.as_secs_f64() * MS,
    });
    Ok(())
}

fn bench_commit(tools: &Tools, repo: &Path, report: &mut Report) -> Result {
    let file = "m99/s9/f9.txt";
    let mut runs = 0;
    let measured = median(RUNS, || {
        runs += 1;
        append(&repo.join(file), &format!("bench edit {runs}\n"))?;
        run_ok(tools.nexus(repo, &["add", file]))?;
        time(&tools.nexus(repo, &["commit", "-m", &format!("bench edit {runs}")]))
    })?;
    let limit = Duration::from_millis(50);
    report.push(Row {
        operation: "nexus commit after one changed file".to_owned(),
        measured: millis(measured),
        net: Some(net(measured, report.start)),
        budget: "< 50 ms (the secret scan arrives in a later phase)".to_owned(),
        pass: Some(measured < limit),
        value: measured.as_secs_f64() * MS,
    });
    Ok(())
}

fn bench_log(tools: &Tools, repo: &Path, report: &mut Report) -> Result {
    let log = tools.nexus(repo, &["log", "-n", "100"]);
    let shown = output(&log)?.matches("\ncommit ").count() + 1;
    if shown != 100 {
        return Err(format!("nexus log -n 100 showed {shown} commits").into());
    }
    let measured = median(RUNS, || time(&log))?;
    report.push(Row {
        operation: "nexus log -n 100 (1,000+ commits)".to_owned(),
        measured: millis(measured),
        net: Some(net(measured, report.start)),
        budget: "< 20 ms".to_owned(),
        pass: Some(measured < Duration::from_millis(20)),
        value: measured.as_secs_f64() * MS,
    });
    Ok(())
}

fn bench_large(tools: &Tools, dir: &Path, report: &mut Report) -> Result {
    let large = dir.join("large");
    let file = large.join("big.bin");
    if fs::metadata(&file).map(|meta| meta.len()).ok() != Some(LARGE_LEN) {
        println!("    writing a 1 GiB file (once)");
        fs::create_dir_all(&large)?;
        write_random(&file, LARGE_LEN)?;
    }
    // A warm cache, as §6 asks for: read the file once first.
    std::io::copy(&mut File::open(&file)?, &mut std::io::sink())?;
    let mut runs = Vec::new();
    for _ in 0..3 {
        remove_dir(&large.join(".nexus"))?;
        run_ok(tools.nexus(&large, &["init"]))?;
        runs.push(run_with_peak(
            &mut tools.nexus(&large, &["add", "big.bin"]),
        )?);
    }
    runs.sort_by_key(|(elapsed, _)| *elapsed);
    let elapsed = runs[1].0;
    let peak = runs.iter().filter_map(|(_, peak)| *peak).max();
    let rate = to_f64(LARGE_LEN) / elapsed.as_secs_f64() / MB;
    report.push(Row {
        operation: "nexus add of a 1 GiB file: throughput (median of 3)".to_owned(),
        measured: format!("{rate:.0} MB/s ({:.2} s)", elapsed.as_secs_f64()),
        net: None,
        budget: "≥ 200 MB/s".to_owned(),
        pass: Some(rate >= 200.0),
        value: rate,
    });
    report.push(Row {
        operation: "nexus add of a 1 GiB file: peak memory (highest of 3)".to_owned(),
        measured: peak.map_or_else(
            || "unknown on this OS".to_owned(),
            |peak| format!("{:.1} MB", to_f64(peak) / MB),
        ),
        net: None,
        budget: "< 100 MB".to_owned(),
        pass: peak.map(|peak| to_f64(peak) < 100.0 * MB),
        value: peak.map_or(0.0, |peak| to_f64(peak) / MB),
    });

    // Change 1 MiB in the middle, re-add, and count the new object bytes.
    let objects = large.join(".nexus").join("objects");
    let before = dir_size(&objects)?;
    flip(&file, LARGE_LEN / 2, EDIT_LEN)?;
    let result = run_ok(tools.nexus(&large, &["add", "big.bin"]));
    // Flip the bytes back so the cached file stays as generated.
    flip(&file, LARGE_LEN / 2, EDIT_LEN)?;
    result?;
    let added = dir_size(&objects)? - before;
    report.push(Row {
        operation: "re-adding it after changing 1 MiB in the middle".to_owned(),
        measured: format!("{:.2} MiB of new objects", to_f64(added) / MIB),
        net: None,
        budget: "< 10 MiB".to_owned(),
        pass: Some(to_f64(added) < 10.0 * MIB),
        value: to_f64(added) / MIB,
    });
    remove_dir(&large.join(".nexus"))?;
    Ok(())
}

fn write_random(path: &Path, len: u64) -> Result {
    let mut out = BufWriter::with_capacity(1 << 20, File::create(path)?);
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    for _ in 0..len / 8 {
        out.write_all(&next(&mut state).to_le_bytes())?;
    }
    out.flush()?;
    Ok(())
}

/// Inverts `len` bytes at `offset`. Doing it twice restores them.
fn flip(path: &Path, offset: u64, len: u64) -> Result {
    let mut file = fs::OpenOptions::new().read(true).write(true).open(path)?;
    let mut bytes = vec![0; usize::try_from(len)?];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut bytes)?;
    for byte in &mut bytes {
        *byte = !*byte;
    }
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(&bytes)?;
    Ok(())
}

fn dir_size(dir: &Path) -> Result<u64> {
    let mut total = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        total += if meta.is_dir() {
            dir_size(&entry.path())?
        } else {
            meta.len()
        };
    }
    Ok(total)
}

/// Runs `cmd` to completion and returns its wall time and, where the OS can
/// report it, its peak resident memory.
fn run_with_peak(cmd: &mut Command) -> Result<(Duration, Option<u64>)> {
    let started = Instant::now();
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut peak = None;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        peak = peak.max(memory::sample(child.id()));
        std::thread::sleep(Duration::from_millis(10));
    };
    let elapsed = started.elapsed();
    peak = peak.max(memory::peak_after_exit(&child));
    if !status.success() {
        return Err(failure(cmd, b"(see above)"));
    }
    Ok((elapsed, peak))
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod memory {
    //! The exact peak working set, read from the exited process's handle.

    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle as _;
    use std::process::Child;

    /// `PROCESS_MEMORY_COUNTERS`: `cb`, `PageFaultCount`, then eight sizes,
    /// the first being `PeakWorkingSetSize`.
    #[repr(C)]
    struct Counters {
        cb: u32,
        page_fault_count: u32,
        sizes: [usize; 8],
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn K32GetProcessMemoryInfo(process: *mut c_void, counters: *mut Counters, cb: u32) -> i32;
    }

    pub fn sample(_pid: u32) -> Option<u64> {
        None
    }

    pub fn peak_after_exit(child: &Child) -> Option<u64> {
        let cb = u32::try_from(size_of::<Counters>()).ok()?;
        let mut counters = Counters {
            cb,
            page_fault_count: 0,
            sizes: [0; 8],
        };
        // SAFETY: `child` still owns its process handle (an exited process
        // stays queryable while a handle is open), and `counters` is a
        // writable PROCESS_MEMORY_COUNTERS whose size is passed as `cb`.
        let ok = unsafe { K32GetProcessMemoryInfo(child.as_raw_handle(), &raw mut counters, cb) };
        let _ = counters.page_fault_count;
        if ok == 0 {
            None
        } else {
            u64::try_from(counters.sizes[0]).ok()
        }
    }
}

#[cfg(target_os = "linux")]
mod memory {
    //! The kernel's high-water mark (`VmHWM`), polled while the process runs.

    use std::process::Child;

    pub fn sample(pid: u32) -> Option<u64> {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let line = status.lines().find(|line| line.starts_with("VmHWM:"))?;
        let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kib * 1024)
    }

    pub fn peak_after_exit(_child: &Child) -> Option<u64> {
        None
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod memory {
    //! The resident set size from `ps`, polled while the process runs: a
    //! slight underestimate of the true peak.

    use std::process::{Child, Command};

    pub fn sample(pid: u32) -> Option<u64> {
        let output = Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        let kib: u64 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .ok()?;
        Some(kib * 1024)
    }

    pub fn peak_after_exit(_child: &Child) -> Option<u64> {
        None
    }
}

#[derive(Default)]
struct Report {
    /// Process start: the median wall time of `nexus --version`.
    start: Duration,
    rows: Vec<Row>,
}

struct Row {
    operation: String,
    measured: String,
    /// The measurement minus process start, for the rows where that's
    /// most of the time.
    net: Option<String>,
    budget: String,
    /// `None` when there's nothing to compare with.
    pass: Option<bool>,
    value: f64,
}

impl Report {
    fn push(&mut self, row: Row) {
        let verdict = verdict(row.pass);
        println!("    {}: {} ({verdict})", row.operation, row.measured);
        self.rows.push(row);
    }

    fn print(&self) {
        println!("\nProcess start (nexus --version): {}", millis(self.start));
        println!(
            "Budgets are spec §6. Times are medians of real process runs, process start included.\n"
        );
        let header = [
            "Operation",
            "Measured",
            "Without process start",
            "Budget",
            "Result",
        ];
        let cells: Vec<[String; 5]> = self
            .rows
            .iter()
            .map(|row| {
                [
                    row.operation.clone(),
                    row.measured.clone(),
                    row.net.clone().unwrap_or_else(|| "-".to_owned()),
                    row.budget.clone(),
                    verdict(row.pass).to_owned(),
                ]
            })
            .collect();
        let mut widths = header.map(|cell| cell.chars().count());
        for row in &cells {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.chars().count());
            }
        }
        let line = |cells: &[String]| {
            let padded: Vec<String> = cells
                .iter()
                .zip(widths)
                .map(|(cell, width)| format!("{cell:<width$}"))
                .collect();
            println!("| {} |", padded.join(" | "));
        };
        line(&header.map(str::to_owned));
        line(&widths.map(|width| "-".repeat(width)));
        for row in &cells {
            line(row);
        }
    }

    fn json(&self) -> serde_json::Value {
        let rows: Vec<serde_json::Value> = self
            .rows
            .iter()
            .map(|row| {
                json!({
                    "operation": row.operation,
                    "measured": row.measured,
                    "value": row.value,
                    "budget": row.budget,
                    "pass": row.pass,
                })
            })
            .collect();
        json!({
            "os": std::env::consts::OS,
            "process_start_ms": self.start.as_secs_f64() * MS,
            "rows": rows,
        })
    }
}

fn verdict(pass: Option<bool>) -> &'static str {
    match pass {
        Some(true) => "pass",
        Some(false) => "OVER BUDGET",
        None => "no reference",
    }
}

fn millis(duration: Duration) -> String {
    format!("{:.1} ms", duration.as_secs_f64() * MS)
}

fn net(measured: Duration, start: Duration) -> String {
    millis(measured.saturating_sub(start))
}

fn reference(git: Option<Duration>) -> String {
    git.map(|git| format!(" ({})", millis(git)))
        .unwrap_or_default()
}

#[allow(clippy::cast_precision_loss)]
fn to_f64(bytes: u64) -> f64 {
    bytes as f64
}
