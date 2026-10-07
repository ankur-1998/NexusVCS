# Decisions

One entry per non-obvious decision, newest phase first. Each entry says what was decided and why. The spec ([Draft 6 for VCS.md](../Draft%206%20for%20VCS.md)) records the product-level decisions. This file records the ones made while building.

## Phase 2 — Working tree: status, diff, checkout, branches, tags (2026-10-06)

### Diff

- **imara-diff 0.2, histogram by default.** It's the algorithm behind Git's `--histogram`, and the crate is pure Rust. Lines are compared as bytes including their `\n`, so a file that isn't valid UTF-8 still diffs, and a missing final newline counts as a change.
- **Git's indent heuristic decides where a slidable change goes.** An inserted paragraph between blank lines, for example, could sit in several places. Git picks one with `diff.indentHeuristic`, which has been on by default since 2.14. imara-diff 0.2.0 ships a version, but it miscounts the blank lines at the end of a file, so `diff.rs` implements Git's `measure_split` and `score_add_split`, with the same constants. On about 1,900 random edits, our hunks differ from Git's in 0.7% of cases with histogram and 1.3% with Myers. Some of the remaining differences are imara's choices between equally short diffs. The others come from the order in which imara slides changes: insertions first, while Git slides deletions first. Porting Git's whole compaction step would remove those, and is left for later.
- **Hunks merge like Git's.** Changes at most twice the context apart (6 lines by default) share a hunk. A side with no lines starts at the line before (`@@ -0,0 +1 @@`). The context arithmetic saturates, so `-U 4294967295` gives one hunk instead of overflowing.
- **Binary detection** looks for a NUL in the first 8,000 bytes of either side, the same bytes Git checks.
- **Large files** (stored as chunks) are never loaded to diff. The summary compares the chunk lists: both sizes, and how many of the new side's chunks are new. A working-tree file of 8 MiB or more is chunked on the fly, hashing only.
- **Git's headers:** `diff --git`, `new file mode 100644`, `deleted file mode`, `old mode`/`new mode`, and `index <old>..<new> <mode>`. Names are C-quoted the way Git quotes them. As in Git, `---`/`+++` appear only when hunks follow, so an empty file added or deleted gets no hunk header, and they end in a tab when the name has a space, so `patch` can tell where the name ends. IDs are abbreviated to 12 characters.
- **Hunk headers name the enclosing function**, as Git's default does: the nearest line above the hunk in the old file that starts with a letter, `_`, or `$`, cut to 80 bytes. Language-specific patterns (Git's `diff=<driver>` attributes) aren't supported.
- **Saved diffs are byte-exact.** A diff line carries the file's exact bytes alongside its display text (`Line::bytes`). When stdout isn't a terminal, the CLI writes those bytes, so `nexus diff > fix.patch` keeps CRs, invalid UTF-8, and escape bytes, and `git apply` accepts the result. A terminal gets the display text instead: invalid UTF-8 replaced, a final CR dropped, and other control characters shown in caret notation (`^[`), so a file can't send escape sequences to the terminal.
- **A CRLF-only change is called out** at the end of the first hunk header (`@@ -1,2 +1,2 @@ (only the line endings changed: CRLF and LF)`), where `git apply` ignores text.
- **Nothing printed can carry escape sequences.** Every line's display text has its control characters other than tabs shown in caret notation (`^[`). That covers warnings, errors, and commit messages, as well as file content and names. Only diff content lines also carry their exact bytes, for output that isn't a terminal.
- **Kind changes** (executable bit only) print `old mode`/`new mode` with no hunks.
- **Argument parsing follows Git.** Positional arguments are commits until one isn't; that one and the rest are paths. An argument that is a commit and also a file on disk is an error that suggests `--`, as in Git. A deleted tracked file doesn't count, so a branch can share its name. An argument that is neither keeps the reason it isn't a commit (an ambiguous prefix lists its candidates) and adds the `--` hint.
- **Only paths in scope are checked.** `nexus diff -- a.txt` stats and hashes only `a.txt`, so a large or locked file elsewhere neither slows it down nor fails it. Files whose size showed they changed are hashed in parallel.
- **Against a commit, the index says what's tracked**, as in `git diff <commit>`. A path the commit has but the index doesn't is shown as deleted, even if an untracked file of that name exists.
- **JSON hunks.** The spec's "the API returns the same hunks as JSON" refers to the Phase 7 server (§5.3, with types exported by ts-rs). `Hunk`, `DiffLine`, and `FileDiff` are plain data, and serialization arrives with that API.

### Status

- **Git's layout and wording**, with `nexus` in the hints. The head line comes first, each section follows with a blank line between them, and entries are indented with a tab. An empty unborn repository says "nothing to commit (create/copy files and use "nexus add" to track)". An untracked directory that holds no tracked files is listed once as `dir/`. `HEAD detached at <id>` is shown in red on stdout, as Git shows it, not as a warning on stderr.
- **Paths are relative to the current directory**, as Git shows them (`../a.txt` from a subdirectory), so they can be pasted into the hinted commands. Names with control characters, `"`, or `\` are C-quoted, so a file name can't inject terminal escape sequences.
- **The fast path is Phase 1's:** size and modification time must match, and the entry must be older than the index file. A cached time of 0 means unknown, so it never matches, even a file dated 1970. A different size counts as modified without reading the file, as in Git, unless the entry has no cached time (`restore --staged` leaves none). Anything else is hashed in parallel, without storing.
- **Every index write keeps the racy rule.** When a transaction saves the index, entries carried over unchanged that weren't older than the index file it loaded have their cached time cleared. Otherwise the newer index file would make them look trusted. Phase 1 did this only for `add`, so a status refresh, restore, checkout, or rm could make a same-size edit made in the same timestamp tick invisible, and a later checkout could overwrite it. Entries the transaction just checked keep their times.
- **A file that can't be read counts as modified**, as in Git: one held open by another program, or still being written. A file deleted while status runs counts as deleted. One locked file doesn't stop the command.
- **Staged changes come from comparing trees, not files.** The index's trees are built in memory (hashing only) and compared with HEAD's tree by ID, so only the HEAD trees whose IDs differ are read. When nothing is staged, nothing is read. On the benchmark repository, reading HEAD's whole tree (about 1,100 tree objects) took about 350 ms on this machine; comparing by ID takes about 10 ms.
- **The cached size and time are refreshed** for files that were hashed and found unchanged, so the next status takes the fast path. This is best effort: it runs in a normal transaction, gives up silently if another command holds the lock, and records no op, because the index's content (its tree) didn't change.

### Checkout

- **Git's two-way switch rules.** For each path, compare HEAD (`H`), the index (`I`), and the target (`T`):
  - `H == T`: the switch doesn't touch it. The index entry and working file keep whatever they have, staged or not.
  - `I == T`: the index already matches the target, so nothing is written, and any unstaged change stays.
  - `I != H`: a staged change the switch would overwrite. Refused.
  - Otherwise the working file must match the index or be missing; a deleted file's content is in the index, so nothing is lost. A modified file is refused.
- **Untracked and ignored files are never overwritten.** The switch is refused if any of these is in the way:
  - something at the path of a file the target adds, or anything the filesystem treats as the same name (other letter case on Windows and macOS, or a Windows 8.3 short name such as `VERYLO~1.TXT`)
  - an untracked directory there, even one holding only ignored files
  - a file, symlink, or junction where a directory must go

  Whether the filesystem ignores case is probed by looking up `.nexus` as `.NEXUS`. Where it does, matching resolves every component, not just the first. Where it doesn't, names must match exactly, as in Git. The 8.3 check asks the filesystem directly, because directory listings show only long names. Git lets checkout overwrite ignored files; we don't, because the spec says checkout never touches untracked or ignored files.
- **A tree holding names that differ only in case** (`README` and `readme`) is refused on a filesystem that ignores case, before anything changes. Git checks out both and one overwrites the other.
- **Directory case renames.** When a branch renames `MyPkg/` to `mypkg/` and something untracked keeps the old directory alive, the directory is renamed to the tree's spelling before files are written into it. The index and the disk then agree; otherwise status would report the files as deleted and the directory as untracked. This only happens where the filesystem ignores case, so both spellings are one directory and the untracked files only change letter case.
- **Nothing in `.nexus` is written or deleted.** A path with a component that would name the repository directory can't be checked out, restored, exported, staged by `restore --staged`, or deleted by checkout, restore, or rm. That covers `.nexus` in any case, with the trailing dots or spaces Windows ignores, and its 8.3 name `NEXUS~1`. This holds whether or not the file already exists. Git guards `.git` the same way (CVE-2014-9390).
- **A refusal changes nothing.** It lists every file with the reason, up to 20 and then a count.
- **Writes** go through a temp file and a rename in the same directory, then set the mode. Deletions run first and prune the directories they empty, so a file can replace a directory and the reverse. A directory left holding only empty directories is removed, as Git removes it. Directory listings are cached for speed, and the paths a checkout has just created are remembered, so a stale listing is never mistaken for an 8.3 alias. Deleting retries brief Windows locks, as renaming does.
- **No writing or deleting through links.** Files are found on disk with Phase 1's exact lookup (real directories only, exact NFC names), so a file stored in NFD on disk is overwritten in place rather than duplicated. A tracked file is deleted only if it's reached through real directories. A symlinked or junctioned directory in the working tree therefore can't make checkout or restore touch anything outside the repository.
- **Read-only files are replaced.** Windows refuses to rename over a read-only file, so the attribute is cleared first, as Git for Windows does.
- **The `saved` tree** records every tracked file the switch overwrites or deletes. They're all clean, so this costs no hashing, and it makes the op self-describing for undo.
- **A failure partway still records an op.** If a write or delete fails after some files have changed (a file another program holds open, a full disk), the transaction records an op for the state as it was, plus `saved`. Content already overwritten therefore stays reachable for undo and safe from gc, and the error says so. This applies to checkout, restore, and rm. A failure before anything changed records nothing.
- **A branch name wins.** `nexus checkout X` switches to branch `X` if it exists. Anything else (a tag, an ID, `HEAD`) detaches HEAD, with a warning that says how to keep new commits. Checking out the current branch prints "Already on" and records nothing.

### Restore and rm

- **`restore` without `--source`** writes the index version. With `--source`, it writes the commit's version, deletes tracked files in scope that the commit doesn't have (Git's default "no overlay" mode), and leaves the index alone, as `git restore --source` does.
- **Files that already match are skipped**, and so are deletions of files already gone. Neither is rewritten or saved, so `saved` holds exactly what was discarded. A restore that writes or deletes anything records an op, even when there was nothing to save (recreating deleted files). One that changes nothing records none.
- **Restore refuses** when something untracked is where it must write a file, under the same rules as checkout. An untracked file at the path itself (possible with `--source`) is saved and then overwritten, as Git overwrites it.
- **`restore --staged`** resets index entries to HEAD (or `--source`) and removes entries the source doesn't have. The reset entries get no cached size or time, so the next status hashes those files.
- **`rm` follows Git's safety rules.** Without `--cached`, a file is refused if its working copy differs from the index, or its index entry differs from HEAD. With `--cached`, it's refused only if the staged content matches neither the file nor HEAD, since it would then exist nowhere but the op log. The spec lists no `-f`, so there is none: restore first, or commit. Deleted files go into the op's `saved` tree.
- **`rm` takes directories without `-r`**, the same as `add`.
- **`add --exec` sets execute bits only after every file was read**, so a refused add changes no modes.

### Revisions, branches, and tags

- **Resolution order:**
  1. `HEAD`
  2. a complete 64-character ID of an existing object, which wins over a ref of the same name, as in Git
  3. a full ref name (`refs/heads/x`)
  4. a branch or tag name, which is an error if it's both, naming the two full names
  5. an ID prefix of at least 4 hex characters, in either case
- **Ambiguous prefixes** list each candidate with its type. Where a commit is expected, a prefix matching exactly one commit among other objects resolves to it, as in Git. Prefix lookup reads one fan-out directory.
- **No `HEAD~1` or `^` syntax.** It isn't in the spec, so it's a candidate for §8.
- **Ref names follow Git's rules, plus Windows' file-name rules on every OS.** Git's rules forbid:
  - an empty name, `HEAD`, or a leading `-`
  - `..`, `@{` or `//`
  - a trailing `/` or `.`, or a `.lock` suffix in any case
  - control characters, spaces, and `\ : ? * [ ~ ^`
  - a component starting with `.`

  Windows adds device names (`CON`, `nul`, `com1`, `aux.txt`), a trailing dot or space in any component, and `< > " |`. A repository made on Linux therefore works on Windows, and Windows can't quietly map one name onto another file (`feature.` onto `feature`). Names are stored in NFC, and a full 64-character hex ID isn't a valid name.
- **Ref lookups match exactly.** Each component of a ref must appear in its directory's listing, spelled the same (after NFC). On Windows and macOS, opening `refs/heads/MAIN` would otherwise find `main`, so `branch -d MAIN` could delete the current branch. Names given on the command line are NFC-normalized before any lookup.
- **No refs that collide by case or shape.** On any OS, a ref can't be created if any component differs from an existing ref's only in letter case (`feature/x` when `Feature/y` exists: Windows would put it in `Feature/`). The same goes for a ref that would need one path to be both a file and a directory. Deleting a ref prunes the directories it leaves empty.
- **`branch -d`** refuses unless the branch is an ancestor of the current HEAD. `-D` skips that check. Neither deletes the current branch.
- **Tags** are lightweight refs under `refs/tags/`. `log` and `show` label commits `tag: v1` after the branch labels.
- **`export`** writes only into an empty or new directory, and refuses a directory inside `.nexus`. It skips, with a warning, names this OS can't use, and names that would land on a file it already exported or need a directory where one is (other letter case, an 8.3 name). It takes no lock, because it doesn't change the repository.

### Files on disk

- **Atomic writes use `std::fs` for both the temp file and the rename.** On Windows, std adds the `\\?\` prefix that paths longer than 260 characters need. `tempfile`'s rename doesn't, so a repository in a deep directory (root of about 180 characters or more) couldn't store objects. The temp file gets a unique name from the pid, the time, and a counter, and is created with `create_new`.
- **Objects are no longer written with `persist_noclobber`.** An object that already exists has exactly the content being written, so if it appears between the check and the rename, replacing it is harmless.
- **Modes respect the umask.** New files are created with the umask applied (`0666 & ~umask`), and making a file executable adds an execute bit wherever it has a read bit, as Git does. Files checked out under `umask 077` are therefore `0600`, not `0644`.

### Benchmarks (`cargo xtask bench`)

- **Real processes, wall time.** Each row times the release binary as a user would run it, so process start is included. The table also shows each time minus process start, measured as the median of `nexus --version`. On this Windows machine process start is about 60 ms, which no Rust code can win back.
- **Medians** of 10 runs after a warm-up. `add .` is the median of 3 fresh repositories. The 1 GiB add is the median of 3 runs after reading the file once to warm the cache (§6), and reports the highest peak memory of the three.
- **Cached repositories in the temp directory**, not under `target/`, because `nexus init` refuses to create a repository inside another one and this workspace may be one. `NEXUS_BENCH_DIR` overrides the location. A `.nexus` or `.git` moved aside for the `add .` row is put back even if a run fails, or at the start of the next run. The Git reference repository must be clean before its status is timed.
- **Peak memory.** On Windows it's read exactly from the exited child's handle with `K32GetProcessMemoryInfo`. That is the first `unsafe` in the workspace, allowed only in that small xtask module, with a SAFETY comment. On Linux, the kernel's high-water mark (`VmHWM`) is polled every 10 ms. On macOS, `ps` is polled, which slightly underestimates the peak.
- **Git** runs with the user's own configuration, as the user would see it, plus an identity for the commit.
- **Phase 2's numbers** on the development machine (Windows 11 with real-time antivirus; process start 54 ms). "In-process" means `commands::run` called from a small release-built harness, without process start:

  | Row | Measured | Budget |
  |---|---|---|
  | `status`, nothing changed | 150 ms; 85 ms in-process. Git: 152 ms | ≤ 2× Git and < 100 ms |
  | `add .`, fresh, 10,000 files | 6.7 s. Git: 53 s | ≤ 2× Git |
  | `commit` after one change | 110 ms; 42 ms in-process | < 50 ms |
  | `log -n 100` | 101 ms; 37 ms in-process | < 20 ms |
  | add a 1 GiB file | 526 MB/s, peak 73 MB | ≥ 200 MB/s, < 100 MB |
  | re-add after changing 1 MiB | 2.05 MiB of new objects | < 10 MiB |

  Process start alone breaks the 100 ms, 50 ms, and 20 ms budgets on this machine. Without it, status and commit fit. `log` doesn't: it reads 100 loose objects, which costs about 0.35 ms each here, and pack files (§8) are the fix (Phase 1 recorded the same).

### Code

- **`content::store_files`** is shared by staging, status, checkout, and restore, and returns each file's result separately, so status can treat one unreadable file as modified. Small files run in parallel within the 16 MiB budget. Large files run afterwards on the calling thread: each one already spreads its chunks over the thread pool, and a pool worker blocked on the budget could otherwise deadlock it. A file that grew past 8 MiB between the walk and the read is sent to that later pass, for the same reason.
- **`graph`** holds `read_commit`, `walk_commits` / `walk_from` (breadth-first from one or several tips, each commit read once), `is_ancestor`, and a children map, for Phase 3's merge base and graph layout.

### Known limitations

- **Trees holding names that differ only in case** can't be checked out on Windows or macOS (see above). `add` warns about such pairs, and `export` skips the second one with a warning.
- **The diff algorithm** occasionally groups the same changes differently from Git, in about 1% of random edits (see Diff). Both outputs are correct and minimal, and either applies with `git apply`.
- **Merge commits** show their diff against the first parent only. Phase 3 adds merges.
- **HFS+ ignorable code points.** macOS's old HFS+ ignores some invisible Unicode characters in names, so `.nex\u{200c}us` would reach `.nexus` there. APFS, the default since 2017, doesn't do this, and the guard doesn't check for it.

## Phase 1 — Object store, large files, staging, commits, op recording (2026-10-06)

### The command layer's shape

- **`Ctx` is `{ cwd, global_config, clock }`.** Spec §5.2 sketches `Ctx { repo_root, cwd }`, but `nexus init` has no repository yet, and every other command must find its repository from the current directory anyway (it may be a subdirectory). So commands discover the repo from `cwd`.
- **`CommandResult` is `{ exit_code, lines, raw }`.** The `data` payload in §5.2 arrives with its first consumer, the TUI in Phase 6. `raw` holds file content (`cat-file -p`), which must reach stdout byte for byte: not split into lines, not re-encoded, and not stripped of escape sequences by the color layer.
- **The binary parses arguments itself**, so clap prints help and usage errors in color, and then calls `commands::execute`. `commands::run` (parse, then execute) is what the web terminal will use. `bin_name = "nexus"` keeps help text identical on every OS.
- **Output streams.** Errors and warnings go to stderr, everything else to stdout.
- **Two environment variables, for tests and scripts:** `NEXUS_DATE` pins the clock, and `NEXUS_CONFIG_GLOBAL` replaces the global config path. Phase 3's demo builder needs the first one.

### Formats

- **Decoders are strict, so `encode(decode(x)) == x`.** Only `\n` ends a line, so a `\r` at the end of a file name is data. Tree names must be NFC, sorted, and unique. Numbers must be canonical. `-0000` is rejected. Op refs must be sorted and valid. A chunk list must be one that chunking could produce: at least 8 MiB, with chunks within the FastCDC bounds.
- **Golden values pin the format on every OS:** the IDs of `blob "hello\n"`, the empty blob, and the empty tree (computed independently with .NET), the FastCDC boundaries of a fixed 20 MiB input, and a tree and commit built through `nexus add`/`commit`.
- **Compression.** zstd level 3 over the whole canonical bytes. Raw is stored when `compressed * 100 > raw * 95`.
- **Reading is bounded.** Reads decode the header first and reject declared sizes over a per-type limit (blobs under 8 MiB, everything else under 256 MiB), so a 64 KB hostile object can't make a command allocate gigabytes.
- **FastCDC.** `fastcdc` is pinned to `=5.0.0`, with normalization level 1 and seed 0 (its defaults).
- **The index refuses file/directory conflicts** when it's loaded, naming the path and how to rebuild the index.
- **Recorded command lines.** The op's `command` line is `nexus` plus the arguments, each quoted when it contains whitespace, quotes, backslashes, or control characters, with `\n`, `\r`, `\t`, `\"`, `\\` escapes. It always fits on one line.
- **The stash.** An op's `stash` line is always present. With no stash, it's the empty blob.
- **No-op commands.** A command that changes nothing records no op. A crash-recovery op's command is `recovered`.
- **No fsync.** Writes are atomic renames without fsync, like Git's defaults. A power cut can lose the last command, and the hash check on read reports any damaged object.

### Transactions

- **The index is loaded once per transaction.** A command that doesn't change the index reuses the tree the newest op recorded, with no hashing or stat calls. When the index does change, only trees that weren't in the previous index are written, in parallel. This is what keeps `nexus commit` and one-file `nexus add` from touching every directory's tree.
- **The lock on Windows** is opened delete-on-close. The system removes it even if the process is killed, and nothing else can hold it open in a way that blocks removal.
- **Interrupts.** The CLI installs a Ctrl+C and termination handler (the `ctrlc` crate, pure Rust) that removes held locks before exiting. Lock files record the holder's pid and command for the error message. Nothing takes a lock over based on its pid, because pids get reused and repositories on shared drives can be used from two machines.
- **Config writes take a lock**: `.nexus/lock` for the repository's config, and a sibling `config.toml.lock` for the global one. Config isn't op state, but its read-modify-write must not lose concurrent updates.

### Staging (`nexus add`)

- **Ignore rules.** `.nexusignore` files apply at every level, through the `ignore` crate's custom ignore file. The standard filters are off: `.gitignore` doesn't apply, and hidden files are included. A file that's already tracked stays tracked when a rule starts matching it, as in Git.
- **Finding tracked files on disk.** For a tracked path the walk didn't return, every name must match exactly (after NFC normalization), through real directories. A case-only rename on Windows or macOS therefore replaces the old name rather than keeping both. A directory replaced by a symlink or junction is never read through. A decomposed (NFD) on-disk name still counts as its stored NFC name.
- **Named paths that match nothing.** A path that doesn't exist is an error, reported before anything changes, with a "did you mean" when only the letter case differs. Paths that exist but stage nothing get a warning saying why: ignored, an empty directory, a symlink, or inside a `.nexus` directory.
- **NFC twins.** When two on-disk names normalize to the same path (possible on Linux and Windows), the one already in NFC is kept, deterministically, with a warning.
- **Walk errors.** An I/O error during the walk aborts the command, so an unreadable directory never looks like its files were deleted. Unparsable ignore rules only warn.
- **Racy timestamps.**
  - An entry's cached size and modification time are trusted only if it's older than the index file.
  - Entries not checked in this run and modified no earlier than the last index write are smudged (modification time set to 0).
  - Saving the index smudges every entry modified within the last 2 seconds.
  - Smudged entries are rehashed next time. Without this, a same-size edit within the filesystem's timestamp granularity could go unnoticed forever.
- **The executable bit.**
  - On Unix, the filesystem's bit decides, unless `nexus init` found the bit meaningless (FAT, or Windows drives under WSL) and wrote `[core] filemode = false`.
  - On Windows, and with `filemode = false`, the index keeps the kind it already had.
  - `--exec` marks files executable everywhere, and runs `chmod +x` where the bit means something.
- **Unusable names.** A tracked name that can't be used on this OS (on Windows: `..\x`, `C:x`, `CON`, ...) is never turned into a filesystem path, so it can't escape the root, open an alternate data stream, or open a device.
- **Absolute paths** that reach the repository by another route (a symlinked directory, a junction, different case on Windows, a `\\?\` prefix) are resolved by canonicalizing.
- **Memory.** Small files are hashed in parallel within a 16 MiB budget. Large files are chunked on one thread while the previous 16 MiB batch is hashed and compressed on others.
- **The default `.nexusignore`** also lists `.git/`, so a folder tracked by both Git and NexusVCS doesn't commit Git's internals.

### Commits, log, and inspection

- **Message cleanup.** Messages are cleaned the way `git commit -m` does: trailing whitespace stripped, blank-line runs collapsed, and leading and trailing blank lines dropped. `-m` accepts values that start with `-`. An empty root commit needs `--allow-empty`, like any other empty commit.
- **Identity checks.** The identity is validated when it's read, not only when it's set, so a hand-edited config can't produce a commit its own decoder would reject.
- **`nexus log`** prints full hashes (abbreviated-hash lookup arrived in Phase 2). `--oneline` uses 12 characters. Branch labels appear as `(HEAD -> main)`. Dates show in the commit's own offset.
- **`cat-file -p`** prints every object's body exactly as stored.
- **`hash-object`** works outside a repository.

### Config

- **`toml_edit` instead of `toml`**, so writing a value keeps the user's comments and layout, and works with inline `user = { ... }` tables. Comments at the end of a file stay above a newly added table. Repository settings override global ones.
- **Global config location** comes from environment variables per OS (`APPDATA`, `HOME`, `XDG_CONFIG_HOME`), not the `dirs` crate.

### Dependencies

Each is justified in `Cargo.toml`: `sha2`, `zstd` (`default-features = false`), `fastcdc`, `ignore`, `rayon`, `tempfile`, `thiserror`, `chrono` (clock only), `toml_edit`, `unicode-normalization`, `anstream`/`anstyle`, `ctrlc`, plus `proptest` for tests. Dependencies are built with `opt-level = 3` even in dev and test builds, because hashing and compression at `-O0` made the large-file tests many times slower.

### Known limitations

- **Ignore matching and NFC.** Ignore rules are matched against names as the filesystem reports them. A non-ASCII pattern written in NFC doesn't match a file whose on-disk name is NFD. That's rare, and only possible on Linux and Windows. Fixing it means matching rules ourselves on NFC paths, which the walker in a later phase can do.
- **Stale locks after `kill -9`.** On Unix, a SIGKILL still leaves `.nexus/lock`, and the error message says to delete it. Windows removes it automatically.
- **Case-sensitive pathspecs.** `nexus add A.TXT` doesn't stage `a.txt`; it explains the difference and suggests the right spelling.
- **`log` speed on Windows.** `nexus log -n 100` reads 100 loose objects, so on Windows, where opening a file is slow, it's bound by file opens. Pack files (§8) are the eventual fix.

## Phase 0 — Scaffold (2026-10-06)

### Rust 1.99.0, pinned, with `rust-version = "1.99"`

1.99.0 was the current stable release (2026-10-01). Pinning it in `rust-toolchain.toml` means every machine and CI runner uses the same compiler and the same clippy lints, so a new Rust release can't break CI overnight. The minimum supported Rust version equals the pinned one: we don't promise to build on older compilers. Upgrades are deliberate, one-line changes.

### `default-members = ["crates/nexus"]`

This makes `cargo run -- --version` run the `nexus` binary without `-p`, as the Phase 0 check requires. Workspace-wide tasks pass `--workspace` explicitly, so nothing is skipped.

### `cargo xtask` runs with `--locked`

The alias in `.cargo/config.toml` is `run --locked --package xtask --`. Without `--locked`, the build that starts xtask would quietly rewrite a stale `Cargo.lock`, and every `--locked` step inside `cargo xtask ci` would then pass. A dependency change now has to come with its lockfile update, the same rule pnpm's `--frozen-lockfile` enforces for the web side.

### The clap definitions live in `nexus-core` from day one

Spec §5.2 puts argument parsing in core, so the CLI and the web terminal parse identically. Starting there avoids moving it in Phase 1. Core only defines the parser; printing help and version output (`Cli::parse`) happens in the binary.

### Core denies `print_stdout` and `print_stderr`

"Core never prints" (spec §5.2) is enforced by a clippy lint, not by convention. A plain `cargo build` doesn't run clippy, but `cargo xtask ci` does, and fails.

### Lint levels

`clippy::all` and `clippy::pedantic` are warnings, and CI denies all warnings. `missing_errors_doc`, `missing_panics_doc`, and `must_use_candidate` are allowed, because they add noise without catching bugs at this scale. `unwrap_used` is on for production code (tests are exempt through `clippy.toml`), and `unsafe_code` is denied workspace-wide. A crate that someday needs `unsafe`, such as the `platform` module for Windows APIs, must opt in explicitly.

### How the network rule is enforced

- **What's forbidden.** `clippy.toml` uses `disallowed-methods` and `disallowed-types` to forbid socket and DNS APIs outside `nexus_core::net`:
  - std: `TcpStream::connect` and `connect_timeout`, `TcpListener::bind`, `UdpSocket::bind`, `ToSocketAddrs::to_socket_addrs`
  - tokio: `TcpStream::connect`, `TcpListener::bind`, `UdpSocket::bind`, `lookup_host`, the `TcpSocket` type
  - ureq: the request functions and the `Agent` type
- **Rules for absent crates.** The tokio and ureq entries are already in place. Clippy ignores paths into crates that aren't dependencies yet, so they start working the moment the crate is added, without anyone having to remember them.
- **Typos fail CI.** A path that doesn't resolve in a crate that is present makes clippy print "does not refer to a reachable ...". That's only a warning that `-D warnings` doesn't upgrade, so `cargo xtask ci` searches clippy's output for it and fails, and a typo can't silently disable a rule.
- **Listeners count too.** Binding a listener is forbidden alongside connecting, because `bind` resolves hostnames (DNS) and can bind every interface. The Phase 7 server gets its listener from `nexus_core::net`, which can only bind `127.0.0.1` (spec §5.3).
- **What isn't covered.** The rule covers in-process sockets. It can't stop a future change from shelling out to a tool like `curl`, so that stays a code-review rule.
- **One config file.** Clippy reads the nearest `clippy.toml`, so a crate-level one would drop these rules for that crate. All clippy configuration stays in the root file.

### The CLI-only build is linted, not just compiled

`cargo xtask ci` runs clippy with `-D warnings` on `nexus --no-default-features`, as well as on the all-features workspace. Code that exists only in the CLI-only binary, such as `#[cfg(not(feature = "ui"))]` fallbacks, is therefore held to the same network rule and lints as everything else.

### xtask dependencies: `flate2` and `serde_json`

Arguments are matched by hand, because there are only a few tasks and xtask should compile quickly. `flate2` measures gzipped bundle sizes, and its default backend is pure Rust. `serde_json` reads cargo's JSON build messages, so `cargo xtask build` reports the binary's real path, even when `CARGO_TARGET_DIR` or `--target` moves it. Phase 11's `dist` task needs the same thing.

### xtask finds tools through `PATH` and `PATHEXT`

`std::process::Command` only finds `.exe` files by bare name on Windows. pnpm, installed through npm, is a `.cmd` file, so xtask looks up each `PATHEXT` extension itself. It runs cargo through `$CARGO`, so the pinned toolchain is the one used.

### `pnpm install --frozen-lockfile` in both `ci` and `build`

Local runs install exactly what CI installs. To change dependencies, use `pnpm add` or `pnpm remove`, which update the lockfile.

### What the bundle budget measures

`cargo xtask ci` reads `web/dist/index.html` and sums the gzipped size (default level) of every `<script type="module" src>` and `<link rel="modulepreload" href>`. That is the JavaScript a browser must fetch before the app starts. CSS isn't counted, because spec §6 budgets initial JS. The spec writes budgets in decimal units, so the limit is 150 KB = 150,000 bytes. Phase 0 uses about 9.4 KB.

### The ts-rs stale-check is deferred to Phase 7

No API types exist yet, so there is nothing to export or compare. The check lands with the first exported type, when the server API starts in Phase 7.

### TypeScript 6, not 7

TypeScript 7.0 (the native Go port) is the latest release, but `svelte-check` only supports TypeScript 5 and 6, because it relies on the JavaScript compiler API. Revisit when svelte-check supports 7.

### Svelte runes mode is forced

`web/svelte.config.js` compiles every component of ours in runes mode, so deprecated Svelte 4 syntax (`export let`, `$:`) is a compile error instead of passing silently. Packages in `node_modules` keep their own mode. The config carries a JSDoc type, so `svelte-check` type-checks it too.

### Web dependencies

All are dev dependencies; nothing ships except the compiled bundle.

- `svelte`, `vite`, `vitest`, `typescript`, `prettier`, `svelte-check`: the web stack and checks named in spec §3.
- `@sveltejs/vite-plugin-svelte`: Vite's official plugin for compiling `.svelte` files.
- `prettier-plugin-svelte`: lets Prettier format `.svelte` files, which the Prettier check needs.

### Node 22.12+ or 24+, and CI uses Node 24 LTS

Vitest 5 is the strictest dependency: it supports Node `^22.12.0 || ^24.0.0 || >=26.0.0`. Vite 8 alone would accept Node 20.19. `engines` in `web/package.json` mirrors Vitest's range. CI tests on the current LTS, Node 24.

### GitHub Actions versions

The workflow uses `actions/checkout@v7`, `actions/setup-node@v7`, `pnpm/action-setup@v6`, and `Swatinem/rust-cache@v2`, the latest major of each at the time of writing. All run on Node 24, which GitHub runners require now that Node 20 was removed in September 2026. `rustup toolchain install` with no arguments installs whatever `rust-toolchain.toml` pins.

### CI runs on pushes to `main` and `master`

The repository isn't under Git yet, and `git init` on this machine defaults to `master`. Triggering on both names means CI runs whichever name the first push uses.

### LF line endings everywhere

`.gitattributes` (`* text=auto eol=lf`), `.editorconfig`, rustfmt's `newline_style = "Unix"`, and Prettier's default all agree. Without `.gitattributes`, Git on Windows, including GitHub's Windows runners, checks files out with CRLF, and the formatting checks fail only on Windows.

### CLI tests turn color off explicitly

The integration tests run the binary with `NO_COLOR=1` and without `CLICOLOR_FORCE`, so a developer who forces color globally doesn't get failing assertions about plain-text output.

### Prettier covers `web/` only

Rust is formatted by rustfmt. Prettier is installed in `web/`, and pulling root-level Markdown, TOML, and YAML into it would require a second Node project at the root. Those files are kept tidy by `.editorconfig`.

### Vitest runs with `--passWithNoTests`

The runner is wired into `cargo xtask ci` now, so the first TypeScript logic that gets tests is checked automatically. Until then, an empty suite isn't a failure.

### No `@tsconfig/svelte` base config

The handful of compiler options it provides are set directly in `web/tsconfig.json`, which saves a dependency and keeps every option visible.

### Windows development needs the MSVC Build Tools and a Windows SDK

Rust's MSVC toolchain links against the Windows SDK. The C dependencies in later phases (`zstd`, `ring`) also compile with MSVC, as spec §1 rule 7 requires. The README lists the prerequisites for each OS.
