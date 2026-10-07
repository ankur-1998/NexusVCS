# NexusVCS — Master Build Spec

You are the lead engineer building NexusVCS with me, one phase at a time. This spec is the source of truth for scope, on-disk formats, architecture, privacy rules, and performance targets. When I say **"Execute Phase N"**, build that phase only, following the working agreement below.

---

## 1. Working agreement

1. **One phase at a time.** Build only the phase I ask for. Don't build ahead for later phases beyond what the architecture in this spec already requires.
2. **Plan first, briefly.** Before writing code, list the files you'll create or change and any decision this spec leaves open. If a question truly blocks you, ask. Otherwise state your assumption and proceed.
3. **Tests are part of the deliverable.** Engine code is written test-first. A phase isn't done until `cargo xtask ci` passes.
4. **Formats are a contract.** Don't change anything in §4 without calling it out explicitly, because every later phase depends on it.
5. **Linux, macOS, and Windows are equal targets.** Keep OS-specific code in one `platform` module. All automation is written in Rust under `cargo xtask`, never in bash, PowerShell, or Makefiles, so it runs the same everywhere. CI runs every test on all three operating systems.
6. **Editor-agnostic.** Nothing may depend on a particular editor or IDE. Every workflow runs from a plain terminal through `cargo` and `cargo xtask`. Any editor with rust-analyzer and the Svelte language server gets full support. Shared settings go in `.editorconfig`, `rustfmt.toml`, and `rust-toolchain.toml`.
7. **Lightweight by default.**
   - Justify each new dependency in one line.
   - Dependencies must be pure Rust, or C code that builds with the platform's standard compiler through the `cc` crate (`zstd` and `ring` qualify).
   - Nothing that needs CMake, NASM, pkg-config, or system libraries, so no OpenSSL.
   - Performance budgets (§6) are requirements, not aspirations.
8. **Privacy is a build rule, not a feature.** Follow §5.5 from Phase 0 onward.
9. **One engine, several views.** Every feature is implemented once in `nexus-core` and exposed through the CLI first. The TUI and the web UI are views over the same structured `data`.
10. **End-of-phase report:** what you built, the exact commands I should run to verify it by hand (they must work in bash, zsh, and PowerShell), benchmark numbers against the §6 budgets where relevant, known limitations, and updates to `docs/ARCHITECTURE.md` and `docs/DECISIONS.md` (one entry per non-obvious decision, with the reason).

---

## 2. What we're building

NexusVCS is a personal, self-hosted version control system written from scratch: no `git` binary, no Git libraries. It uses Git's proven model (content-addressed objects, trees, commits, refs, a staging index) with its own on-disk format. It ships as **one small native binary per OS**, with no runtime to install. Issues, boards, code reviews, CI, and AI all run inside that binary on your own machine, and work fully offline.

### Principles

1. **Local-first and private.** Everything runs on your machine and works with no network. Nothing is sent anywhere unless you configure it. No telemetry, ever.
2. **Fast.** Every view reads local data, so nothing waits on a network round trip. The concrete targets are in §6.
3. **Large files are first-class.** There are no size limits. Big files are streamed, split into chunks, and deduplicated automatically, with no separate LFS setup.
4. **AI is local and optional.** It uses a model server on your machine and stays off until you turn it on.

### Interfaces

| Interface | What it is |
|---|---|
| `nexus` CLI | A native command-line tool for any terminal on any OS. |
| `nexus tui` | A keyboard-driven terminal dashboard for status, staging, the history graph, diffs, issues, reviews, and CI. It works over SSH. |
| Web dashboard | A GitHub-style browser served by `nexus ui` and opened in any browser: file tree, viewer, history, graph, diffs, time travel, issues, boards, reviews, CI. |
| Web terminal | A terminal panel inside the dashboard that runs the same commands as the CLI, with identical output. |

### Features beyond a plain Git host

- **Time-travel scrubber.** Drag a slider through history and watch the file tree and the open file change in place.
- **Interactive commit graph** and **diff inspector**, including previews and diffs for images, audio, and video.
- **Issues and kanban boards** stored inside the repo, so they travel and sync with the code.
- **Local code review** of branches, with inline comment threads.
- **Local CI runner** that tests every commit on your machine.
- **Search** across the working tree or any past commit at ripgrep speed.
- **Local AI:** commit messages, "ask the repo" questions, commit explanations, AI review comments, and semantic search.
- **Playground.** Run snippets (JS/TS/Python) and preview HTML/CSS/SVG/Markdown in a sandbox.

### Non-goals for v1

- Compatibility with Git's on-disk format or wire protocol. The model is Git's; the bytes are ours.
- Multiple user accounts, auth, and permissions. Issues and reviews record an author, but there's no login.
- Compatibility with GitHub Actions YAML. The CI runner has its own small format.
- Pack files and delta compression, partial (hunk) staging, rename detection, submodules, empty directories.
- Symlinks. Skip them with a warning, because creating them on Windows needs special privileges.
- Non-UTF-8 file names. Skip them with a warning.
- A real system shell in the browser (see §5.4).
- Running AI models inside the binary. Nexus talks to a separate local model server instead.

---

## 3. Stack (decided)

Each language goes where it's strongest. Rust handles everything that touches the filesystem, and the browser gets the smallest UI stack that does the job.

**Why Rust for the engine, CLI, TUI, and server:**
- It compiles to a single static binary per OS with no runtime, a small footprint, and no garbage-collector pauses.
- The hot paths of a VCS are walking directories, hashing, and searching. Rust gets those from ripgrep's own crates (`ignore`, `grep`).
- `ratatui` is the standard for terminal UIs.
- Precedent: Jujutsu (jj) and gitoxide are VCS tools written in Rust.
- Go was the runner-up. It loses on raw speed and memory, and it doesn't have ripgrep's crates.

**Why Svelte for the UI:** it compiles to small vanilla JS with almost no runtime, which keeps the dashboard light. React's runtime alone is larger than our entire initial-load budget. The UI is served by the binary itself and opens in any browser on any OS, so there's nothing extra to install. A desktop wrapper (Tauri) remains an option later, and it's a natural fit because the backend is already Rust.

### Rust crates

| Concern | Crate |
|---|---|
| CLI parsing | `clap` (derive) |
| Terminal colors | `anstream` + `anstyle` (honors `NO_COLOR`, handles Windows consoles, strips colors when piped) |
| Terminal UI | `ratatui` + `crossterm` |
| Hashing | `sha2` (uses the CPU's SHA instructions when available) |
| Compression | `zstd` (level 3) |
| Large-file chunking | `fastcdc` (FastCDC 2020) |
| Directory walk + ignore rules | `ignore` (ripgrep's parallel walker, gitignore semantics) |
| Parallelism | `rayon` |
| Diff | `imara-diff` (histogram algorithm by default, Myers available) |
| Search | `grep-regex` + `grep-searcher` (ripgrep's engine) |
| Markdown (issues, reviews) | `pulldown-cmark`, sanitized with `ammonia` |
| Atomic writes | `tempfile` (`NamedTempFile::persist`) |
| Config | `serde` + `toml` |
| HTTP + WebSocket | `axum` (with `ws`) on `tokio` |
| Embedded UI | `rust-embed` (the built web UI is compiled into the binary) |
| File watching | `notify` + `notify-debouncer-full` |
| HTTP client (AI) | `ureq` with `rustls` using the `ring` provider |
| Rust-to-TypeScript types | `ts-rs` |
| Errors | `thiserror` in libraries, `anyhow` in the binary |
| Tests | built-in tests plus `tempfile`, `proptest`, `insta` |

Optional external tools are detected at runtime and never required: Docker or Podman for containerized CI jobs, and an OpenAI-compatible model server for AI features.

### Web stack

| Concern | Choice |
|---|---|
| Framework | Svelte 5 + TypeScript, built with Vite |
| Styling | Svelte scoped CSS + CSS custom properties for themes. No CSS framework. |
| Code viewer | CodeMirror 6 (modular, virtualized rendering, small) |
| Terminal | xterm.js (`@xterm/xterm`, `@xterm/addon-fit`) |
| Commit graph | Hand-written SVG, no graph library |
| Package manager | pnpm |
| Checks | `svelte-check`, Prettier, Vitest for pure TS logic |

Node and pnpm are needed only to build the UI. End users need nothing but the `nexus` binary.

### Repository layout

```text
Cargo.toml              workspace; release profile: lto = "thin", codegen-units = 1, strip = true
rust-toolchain.toml     pinned stable toolchain
clippy.toml             includes the network rules from §5.5
crates/
  nexus-core/           Engine, command layer, watcher, metadata op-logs, CI runner, AI client, `net` module.
                        Synchronous, no async runtime, never prints.
  nexus-tui/            ratatui terminal dashboard.
  nexus-server/         axum server and embedded UI.
  nexus/                The binary: CLI rendering, interactive prompts, `nexus tui`, `nexus ui`.
xtask/                  Automation: dev, build, ci, bench, demo, dist.
web/                    Svelte app. Its build output is embedded into the binary.
```

Cargo features on the `nexus` binary: `ui` (server + web) and `tui`, both on by default. `cargo build --no-default-features` produces a CLI-only binary for anyone who wants the smallest possible tool.

### `cargo xtask` commands

| Command | Does |
|---|---|
| `dev` | Runs the Vite dev server and the Rust server together, with `/api` proxied. |
| `build` | Builds the web UI, then the release binary with the UI embedded. |
| `ci` | `cargo fmt --check`, `cargo clippy -D warnings` (including the network rules), `cargo test`, `svelte-check`, Prettier check, web build, bundle-size check, and a stale-check on the `ts-rs` output. |
| `bench` | Generates synthetic repos and measures them against §6. Compares with `git` and `rg` when they're installed. |
| `demo` | Builds a demo repo with branches, merges, a resolved conflict, large files, issues, and a review. |
| `dist` | Builds release archives for the current target (Phase 13). |

---

## 4. Repository format (the contract)

```text
<repo>/
  .nexusignore             gitignore syntax; written with sensible defaults by `nexus init`
  nexus-ci.toml            optional CI job definitions, versioned with the code (Phase 11)
  .nexus/
    HEAD                   "ref: refs/heads/main\n"  or  "<hash>\n" when detached
    config.toml            [user], [ai], [network], ...
    index                  the staging area (binary, see below)
    lock                   exists only while a mutating command runs
    MERGE_STATE            exists only during an unresolved merge (TOML: merge_head, conflicts)
    refs/heads/<branch>    "<hash>\n"
    refs/meta/issues       issue and board history; never checked out
    refs/meta/reviews      code review history; never checked out
    objects/ab/cdef...     objects, named by the SHA-256 of their uncompressed bytes
    ci/                    local CI run records and logs; not versioned
    cache/                 rebuildable data (search, embeddings, derived metadata); safe to delete
    scratch/               playground files; never committed
```

Global config lives in the OS's standard config directory: `~/.config/nexus/` on Linux, `~/Library/Application Support/nexus/` on macOS, and `%APPDATA%\nexus\` on Windows.

### Objects

The canonical bytes of every object are `<type> <bodyByteLength>\0<body>`. The object's hash is the lowercase hex SHA-256 of those canonical bytes.

**Storage.** An object is stored at `objects/<first 2 hex chars>/<remaining 62>` as one encoding byte followed by the payload:
- `0x01`: the payload is the canonical bytes, zstd-compressed.
- `0x00`: the payload is the raw canonical bytes. Use this when zstd saves less than 5%, as it does for already-compressed media, so no CPU is wasted.

**Types**
- **blob:** the body is the file's bytes, verbatim. Never convert line endings.
- **chunked:** used for every file of **8 MiB or more**.
  - The body is a first line `size <total bytes>\n` followed by one line per chunk, `<blob hash> <chunk length>\n`, in file order. Each chunk is stored as an ordinary blob.
  - Chunk boundaries come from FastCDC 2020 with min 256 KiB, average 1 MiB, and max 4 MiB.
  - The 8 MiB threshold and these parameters are part of the contract. Pin the `fastcdc` version, and keep a golden test proving that a fixed 20 MiB pseudo-random input produces a known list of boundaries.
- **tree:** the body is one line per entry, `<file|exec|tree> <hash> <name>\n`, sorted by name in byte order (UTF-8).
  - A `file` or `exec` entry points at a blob or a chunked object; `exec` marks an executable file.
  - A name is a single path segment. Reject names containing `/`, `\n`, or `\0`.
- **commit:**

  ```text
  tree <hash>
  parent <hash>
  author Ada Lovelace <ada@example.com> 1759737600 +0530

  <message>
  ```

  There are zero `parent` lines for a root commit, one normally, and two for a merge. The first parent is the branch you were on.

**Streaming.** All reading, hashing, compression, and checkout stream data, so memory use stays constant whatever the file size. The blob header's length comes from the file's size on disk. If the file changes size while being read, retry once, then fail with a clear message.

### Index (binary, little-endian)

```text
header   magic "NXIX" | version u32 = 1 | entry_count u32
entry    path_len u16 | path (UTF-8, '/'-separated) | kind u8 (0 = file, 1 = exec)
         | hash [32 bytes] | size u64 | mtime_ns i64
trailer  SHA-256 of all preceding bytes [32 bytes]
```

Entries are sorted by path bytes. Paths are relative to the repo root. Loading fails on a bad checksum. `nexus debug index` prints the index as human-readable text.

### Metadata: issues and reviews

Issues and reviews are stored as **operation logs** on the `refs/meta/*` refs, using the same trees, commits, and refs as code. This means they sync with remotes (Phase 14) and get a full audit trail for free.

```text
refs/meta/issues  -> commit -> tree
  _boards/<board-name>.toml        board definition: ordered columns, each mapped to a status
  <issue-id>/<op-hash>.toml        one immutable file per operation
refs/meta/reviews -> commit -> tree
  <review-id>/<op-hash>.toml
```

An operation file:

```toml
op = "set"                                  # create | comment | set | edit-comment | ...
lamport = 17                                # logical clock: highest value seen in the log + 1
author = "Ada Lovelace <ada@example.com>"
time = 1759737600
[fields]
status = "in-progress"
labels = ["bug", "ui"]
```

**Identity**
- An issue's or review's ID is the first 16 hex characters of its `create` operation's hash. Display the shortest unique prefix, at least 6 characters.
- There are no sequential numbers, because two machines can't agree on the next number without coordination.

**Merging**
- Operation files are immutable and named by their own hash, so combining two diverged histories of a meta ref is a union of files and can never conflict.

**Current state**
- To compute an item's current state, replay its operations ordered by `(lamport, op hash)`.
- For `set`, the last writer wins per field.
- Comments are append-only. Editing a comment is a new operation that references it.

**Writes and caching**
- Every write creates a new commit on the meta ref.
- Derived state is cached in `.nexus/cache/meta/`, keyed by the meta ref's commit hash.

### Invariants

**Data integrity**
- The same content produces the same hash on every OS. A determinism test runs in CI on all three.
- Objects are immutable. Write one only if it doesn't already exist. On read, verify that the hash matches and report corruption otherwise.
- Writes to `index`, refs, `HEAD`, and `MERGE_STATE` are atomic: write a temp file in the same directory, then rename it. On Windows, retry the rename on sharing violations with a short backoff, because antivirus and indexers briefly lock files.
- Mutating commands take `.nexus/lock`, created with `create_new`. If it's held, fail with a clear message. The CLI, the TUI, and the UI server will run at the same time.

**Paths across operating systems**
- Store paths with `/` and convert to the OS separator only at the filesystem boundary.
- Normalize stored paths to Unicode NFC, because macOS can return decomposed (NFD) file names.
- Reject any path that resolves outside the repo root.
- Warn when two tracked paths differ only by case, because the Windows and macOS defaults are case-insensitive.
- Warn when a tracked path is invalid on Windows: reserved names such as `CON` or `NUL`, characters such as `<>:"|?*`, or a trailing dot or space.

**Executable bit and ignore rules**
- Read the executable bit from the filesystem on Unix.
- Windows has no executable bit, so keep the kind already recorded in the index. `nexus add --exec <path>` marks a file executable from any OS.
- `.nexus/` is always ignored, whatever `.nexusignore` says.

---

## 5. Architecture

### 5.1 One engine, four interfaces

```text
                         nexus-core  (engine + command layer, synchronous)
                  /            |                 \
     nexus (CLI)          nexus-tui          nexus-server  <-- HTTP / WebSocket -->  web/ (dashboard + web terminal)
     any terminal         any terminal       (axum)                                 any browser
```

### 5.2 Command layer

Implement each `nexus` command exactly once, in `nexus-core`. The clap definitions live there too, so the CLI and the web terminal parse arguments identically.

```rust
pub fn run(argv: &[String], ctx: &Ctx) -> CommandResult;   // Ctx { repo_root, cwd }

pub struct CommandResult {
    pub exit_code: i32,
    pub lines: Vec<Line>,                    // Line { text, style: Option<Style> }
    pub data: Option<serde_json::Value>,     // typed payload per command, for the TUI and web UI
}
// Style: Added | Removed | Hash | Heading | Warning | Error | Muted
```

- **Core never prints and never prompts.** Prompts live in the CLI binary, the TUI, and the web UI. In the web terminal, a command that needs confirmation prints the exact command to run next.
- **Rendering.** The CLI renders `lines` with `anstyle`. The web terminal renders the same `lines` as ANSI in xterm.js. The TUI and the dashboard use `data`.
- **Concurrency.** The server calls core through `tokio::task::spawn_blocking`, and core uses `rayon` for parallel work.
- **Watching.** The file watcher lives in core so the TUI and the server share it.

### 5.3 Server

- **Security.**
  - Bind to `127.0.0.1` only.
  - On startup, generate a random token. `nexus ui` opens `http://127.0.0.1:<port>/?token=...`. The page exchanges the token for an `HttpOnly`, `SameSite=Strict` cookie and removes it from the address bar.
  - Every HTTP request and WebSocket connection must be authenticated.
  - Reject any request whose `Host` or `Origin` isn't localhost. This blocks DNS-rebinding attacks from websites open in the same browser.
- **Reads** are JSON endpoints: repo summary, branches, paginated commit list, commit detail, tree (at a commit or of the working tree), blob (raw bytes with HTTP Range support), diff hunks, status, graph layout, search, issues, reviews, CI runs.
- **Writes** go only through the command layer, over the WebSocket: `{ exec: argv, cwd }` comes back as `{ result }`. Convenience actions, such as the dashboard's checkout button, send the same commands.
- **API types** are defined once in Rust and exported to TypeScript with `ts-rs`. CI fails if the generated file is stale.
- **Live updates.**
  - Watch the working tree and `.nexus/`, debounced about 150 ms and filtered through the ignore rules.
  - Push `{ type: "worktree" | "index" | "refs" | "meta" | "ci" }` events over the WebSocket, and the UI refetches whatever changed.
  - On Linux, if the watch fails because of the inotify watch limit, fall back to polling and show a notice in the UI.

### 5.4 The web terminal is a virtual shell

The browser terminal is not a real system shell. It's a restricted shell confined to the repo root.

- Built-ins: `ls`, `cd`, `pwd`, `cat`, `tree`, `clear`, `help`, `history`. Anything starting with `nexus` goes to the command layer.
- The current directory is client state, sent with each command. The server resolves it and rejects paths outside the repo.
- xterm.js has no line editing, so implement it: cursor movement, backspace, history with up/down, Ctrl+C, and Tab completion for paths and `nexus` subcommands.

Why not a real shell: exposing one over a local socket is a remote-code-execution hole, and its behavior differs across operating systems.

### 5.5 Privacy and network policy

**What may use the network.** The binary opens an outbound connection only in these cases, and each one only after the user configures or triggers it:
- the configured AI endpoint (Phase 12)
- configured remotes (Phase 14)
- the one-time Python runtime download for the playground (Phase 8)
- container image pulls by the CI runner (Phase 11)

Nothing else uses the network. There is no telemetry, analytics, crash reporting, or update check, ever.

**Offline mode.** Setting `[network] offline = true`, globally or per repo, blocks every case above. Features that need the network say why they're unavailable. The CI runner passes `--pull=never` to Docker or Podman.

**Enforced in code**
- All outbound connections go through one `net` module in core.
- `clippy.toml` uses `disallowed-types` and `disallowed-methods` to forbid `ureq`, `std::net::TcpStream::connect`, and similar APIs everywhere else, so `cargo xtask ci` fails on violations.

**The dashboard**
- The server sends a strict Content-Security-Policy that allows only same-origin resources, plus what WebAssembly and workers need.
- The UI loads no CDN scripts, web fonts, or external images.

**AI**
- Before the first send to any endpoint that isn't on localhost, show what will be sent and where, and ask for confirmation.
- Never send or embed files matching secret patterns (`.env*`, `*.pem`, `*.key`, `*secret*`, plus any listed in config).

**Air-gapped install.** Copying the binary is enough. The Python runtime can be installed from a downloaded archive with `nexus playground install-python <file>`.

---

## 6. Performance budgets

`cargo xtask bench` measures these with a warm filesystem cache. The synthetic repo has 10,000 files and 1,000 commits unless stated otherwise. Where Git or ripgrep is the reference, the comparison runs on the same tree.

| Operation | Budget |
|---|---|
| `nexus status`, nothing changed | ≤ 2× `git status`, and under 100 ms |
| `nexus add .`, fresh repo | ≤ 2× `git add .` (hash files in parallel) |
| `nexus commit` after one changed file | under 50 ms |
| `nexus log -n 100` | under 20 ms |
| `nexus find <literal>` over the working tree | ≤ 2× `rg` on the same tree |
| `nexus add` of a 1 GiB file | at least 200 MB/s on an SSD; peak memory under 100 MB |
| Re-adding that 1 GiB file after changing 1 MiB in the middle | stores under 10 MiB of new objects |
| `nexus --version`, cold start | under 20 ms |
| `nexus tui`, first screen | under 100 ms |
| `nexus issue list` with 5,000 issues | under 100 ms with a warm cache |
| `nexus ui` ready to serve | under 500 ms; idle memory under 30 MB |
| Ask-the-repo retrieval over 50,000 chunks, excluding model time | under 50 ms |
| Release binary, UI and TUI included | under 15 MB |
| Dashboard initial JS | under 150 KB gzipped. Load the terminal, editor, graph, boards, and playground lazily. `cargo xtask ci` enforces this. |
| Time-travel step, warm cache | under 50 ms |

If a budget can't be met, say so in the phase report with the measurements. Don't quietly relax it.

---

## 7. Phases

Each phase lists what to **build** and the **done when** check I'll run myself.

| # | Phase | Depends on |
|---|---|---|
| 0 | Scaffold | none |
| 1 | Object store, large files, staging, commits | 0 |
| 2 | Working tree: status, diff, checkout, branches | 1 |
| 3 | Merging, graph layout, storage maintenance | 2 |
| 4 | Terminal UI | 3 |
| 5 | Local server and `nexus ui` | 3 |
| 6 | Dashboard and web terminal | 5 |
| 7 | Time travel, commit graph, diff inspector, media previews | 6 |
| 8 | Search and playground | 6 |
| 9 | Issues and boards | 3 (views need 4 and 6) |
| 10 | Local code review | 9, 7 |
| 11 | Local CI runner | 3 (views need 4 and 6) |
| 12 | Local AI | 8 (12d needs 10) |
| 13 | Release builds | 6; can be done any time after it |
| 14 | Stretch goals | as noted |

### Phase 0: Scaffold

**Build**
- The Cargo workspace and crates from §3, plus `xtask` with at least `ci` and `build`.
- The `web/` Svelte + Vite + TypeScript skeleton.
- `rust-toolchain.toml`, `rustfmt.toml`, `.editorconfig`, Prettier config, workspace-wide clippy lints in `Cargo.toml`, and the §5.5 network rules in `clippy.toml`, with an empty `net` module.
- A GitHub Actions workflow that runs `cargo xtask ci` on `ubuntu-latest`, `macos-latest`, and `windows-latest`.
- `docs/ARCHITECTURE.md` (summarize §4 and §5), `docs/DECISIONS.md`.
- `nexus --version` prints the version.

**Done when** `cargo xtask ci` passes on my machine, `cargo run -- --version` prints a version, a deliberate `TcpStream::connect` outside `net` fails the lint, and the CI workflow is ready to run on all three operating systems.

### Phase 1: Object store, large files, staging, commits

**Build**
- **Object store:** write, read, and existence check per §4, including the encoding byte, streaming, and chunked objects for files of 8 MiB or more. Compress chunks in parallel with `rayon`.
- **Inspection commands:** `nexus hash-object <file>`, `nexus cat-file -p <hash>` (pretty-prints any object type; a chunked object prints its chunk list), and `nexus debug index`.
- **`nexus init`:** refuses if already inside a repo. Creates the layout from §4 with `HEAD` pointing at `refs/heads/main`, which stays unborn until the first commit. Writes a default `.nexusignore` (`node_modules/`, `target/`, `dist/`, `build/`, `.env`, `*.log`).
- **Repo discovery:** walk up from the current directory to find `.nexus`, so commands work from subdirectories.
- **`nexus config [--global] user.name|user.email <value>`.**
- **`nexus add <path...>`:** accepts files, directories, and `.`, and also takes `--exec`. It walks with `ignore` and hashes in parallel with `rayon`. Adding a path that no longer exists on disk removes it from the index.
- **`nexus commit -m "<msg>"`:**
  - Builds trees bottom-up from the index, writes the commit, and advances the current branch (or `HEAD` when detached).
  - Refuses a commit whose tree is identical to its parent's unless `--allow-empty` is passed.
  - Fails with a helpful message if no author identity is configured.
- **`nexus log [-n N] [--oneline]`:** first-parent history from `HEAD`. Prints "no commits yet" on an unborn branch.

**Tests**
- A three-commit pipeline: init, then three rounds of edits (modify, add, delete, nested directories), each committed. Assert the log order and parent links, and assert that every commit's tree rebuilds the exact file bytes from the object store.
- Determinism: the same files created in a different order produce the same tree hash, on every OS in CI.
- Byte-exact round trips for a binary file, a CRLF file, a file with a non-ASCII (NFD) name, and a 50 MiB chunked file.
- The golden FastCDC boundary test.
- Changing 1 MiB in the middle of a 64 MiB file and re-adding it creates only a handful of new chunks.
- Already-compressed data is stored with encoding `0x00`.
- Ignore rules, a commit on an unborn branch, corrupt-object detection, and an index checksum failure.

**Done when** this works in a scratch folder in any shell:
```text
nexus init
echo hello > a.txt
nexus add .
nexus commit -m "first"
nexus log
nexus cat-file -p <hash from log>
```
It also works with a multi-gigabyte file in the folder, without memory use growing.

### Phase 2: Working tree (status, diff, checkout, branches)

**Build**
- **`nexus status`:**
  - Compares HEAD's tree, the index, and the working tree, then reports staged changes (added/modified/deleted), unstaged changes (modified/deleted), and untracked files.
  - Walks the tree in parallel.
  - Fast path: if size and `mtime_ns` match the index entry, assume the file is unchanged. Rehash any file whose mtime is not older than the index file's own mtime (the racy-timestamp problem).
- **Diff** through a `diff` module that wraps `imara-diff`:
  - Defaults to the histogram algorithm, with `--diff-algorithm=myers` available. Returns hunks with 3 lines of context.
  - Files with a NUL byte in their first 8 KB are treated as binary and reported as "Binary files differ".
  - Chunked files are reported as a summary: old and new size, and how many chunks changed.
  - The CLI prints unified diff format with colors. The API returns the same hunks as JSON.
- **`nexus diff`** compares the working tree to the index. `--staged` compares the index to HEAD. `nexus diff <a> <b>` compares two commits. All three accept optional path filters.
- **`nexus show <commit>`:** commit header plus the diff against its first parent.
- **`nexus branch`** lists branches and marks the current one. `nexus branch <name> [<start>]` creates one. `nexus branch -d <name>` deletes one, refusing if it isn't merged into the current branch; `-D` forces it.
- **`nexus checkout <branch|commit>`:**
  - Refuses, listing the files, if it would overwrite uncommitted changes.
  - Otherwise applies the tree difference: write changed files, create added ones, delete removed ones, and set the executable bit on Unix. Files are streamed to a temp file and renamed into place.
  - Never touches untracked or ignored files and never wipes the directory.
  - Rewrites the index and updates `HEAD`, printing a warning when that leaves HEAD detached.
- **`nexus restore <path...> [--source <commit>]`:** discards changes to the named files. This is the one deliberately destructive command, so it requires explicit paths.
- **`nexus export <commit> <dir>`:** writes a commit's tree into an empty directory. The CI runner uses it in Phase 11.
- **Abbreviated hashes:** accept any unique prefix of 4 or more characters. An ambiguous prefix is an error that lists the candidates.
- **Graph helpers** in core: `walk_commits(from, first_parent)`, `is_ancestor(a, b)`, and a children map.
- **`cargo xtask bench`**, covering the `status`, `add`, `commit`, `log`, and large-file rows of §6.

**Tests**
- Diff on the classic cases: empty, identical, completely different, insert at the start or end, repeated lines.
- A `proptest` property: applying the diff of `a` to `b` back onto `a` reproduces `b`.
- Checking out each of three commits restores the exact bytes, including chunked files, and the executable bit on Unix.
- A checkout with dirty files refuses and changes nothing.
- Untracked files survive a checkout.

**Done when** you can create a branch, make diverging commits, switch back and forth, `status` and `diff` report what Git would report in the same situation, and the benchmark rows above are within budget.

### Phase 3: Merging, graph layout, storage maintenance

**Build**
- **Merge base:** the lowest common ancestor, found with a BFS over both histories. For criss-cross histories with several candidates, pick one and document the choice.
- **`nexus merge <branch>`** refuses to start with uncommitted changes, which keeps `--abort` simple and safe. It handles three outcomes: already up to date, fast-forward, or three-way.
- **Three-way merge per file:**
  - If only one side changed, take that side.
  - If both changed, run a line-level diff3 built on the diff module, and write `<<<<<<< ours` / `=======` / `>>>>>>> theirs` markers where hunks overlap.
  - A delete on one side with a modify on the other is a conflict that keeps the modified file.
  - A binary or chunked file changed on both sides is a conflict that keeps ours.
- **Conflict state:**
  - Write `MERGE_STATE`, and have `status` list conflicted files.
  - `nexus commit` refuses while any conflicted file hasn't been re-added or still contains markers.
  - Committing after resolution creates a two-parent commit.
  - `nexus merge --abort` resets tracked files to HEAD and removes `MERGE_STATE`.
- **Graph lane layout** in core: `layout_graph(commits) -> { nodes: [{ hash, row, lane }], edges }`. The first parent continues the lane, and freed lanes are reused. It powers `nexus log --graph` now, and the TUI and the SVG graph later.
- **`nexus gc`:** while holding the lock, deletes objects unreachable from any ref (including `refs/meta/*`) that are more than one hour old.
- **`nexus du`:** reports storage use: total, the largest files, and space saved by deduplication.
- **`cargo xtask demo`:** builds a repo with about 60 commits, several branches, merges, one resolved conflict, and a few large files. Later phases extend it.

**Tests**
- A fast-forward merge, a clean three-way merge, and a conflicting three-way merge with the exact marker output.
- A delete/modify conflict, `--abort`, and refusal to start with a dirty tree.
- `gc` keeps everything reachable and removes a deleted branch's objects.
- `insta` snapshots of the layout and the `log --graph` output for linear history, a single merge, and many parallel branches.

**Done when** the demo repo's `nexus log --graph` output reads correctly, every merge scenario above behaves as described, and `nexus gc` followed by a checkout of every commit still restores every file.

### Phase 4: Terminal UI

**Build**
- **`nexus tui`** with tabs:
  - **Status:** stage and unstage with Space, discard with confirmation, and commit with `c`, which opens a message editor inside the TUI.
  - **Log:** the commit graph from `layout_graph`. Enter opens the commit's diff.
  - **Diff:** scrolling, jump between hunks, toggle the staged view.
  - **Branches:** check out, create, delete.
  - Later phases add **Issues**, **Reviews**, and **CI** tabs.
- **Interaction:**
  - Keyboard-first, with a `?` help overlay, vim-style and arrow keys, and optional mouse support.
  - Handles terminal resize.
  - Works in Windows Terminal, the macOS and Linux terminals, and over SSH.
  - Respects `NO_COLOR` and degrades gracefully on 16-color terminals.
- **Live refresh** through the core watcher.

**Tests**
- Screen snapshots with ratatui's `TestBackend` and `insta`.
- Key-driven flows: stage, commit, and checkout.

**Done when** a full daily loop works without leaving the TUI (stage, commit, branch, inspect history and diffs), the first screen appears within its §6 budget, and it works over an SSH session.

### Phase 5: Local server and `nexus ui`

**Build**
- The server from §5.3, with the built UI embedded through `rust-embed`. In debug builds, serve the files from disk so UI changes don't need a Rust rebuild.
- Blob responses support HTTP Range requests. For chunked files, seek by chunk instead of reading from the start.
- **`nexus ui [path] [--port 8080]`:**
  - If the path has no repo, show how many files would be tracked (after ignore rules) and ask before running `init`.
  - If the port is taken, try the next one.
  - Open the default browser, and shut down cleanly on Ctrl+C.

**Tests**
- Integration tests for every endpoint, driving the axum router directly with `tower::ServiceExt::oneshot`.
- Unauthenticated requests get 401. Requests with a non-localhost `Origin` or `Host` are rejected. `exec` rejects paths that escape the repo.
- The CSP header is present on every page.
- Range requests return the correct bytes from a chunked file.
- The watcher emits an event when a file changes on disk.

**Done when** `nexus ui` inside a repo opens a placeholder page whose repo summary updates live while I edit a file in any editor, and the `nexus ui` row of §6 is within budget.

### Phase 6: Dashboard and web terminal

**Build**
- **Layout:** a top bar (repo name, HEAD, dirty indicator), a left sidebar (branch switcher, file tree), a main viewer, and a resizable bottom terminal panel toggled with Ctrl+`. It follows the OS light or dark preference and has a manual toggle.
- **File tree:** lazy-loads directories and has a source toggle between **Working tree** and **a commit**. In working-tree mode, files show status badges (M/A/D/U).
- **File viewer:** CodeMirror 6, read-only, with the language loaded lazily based on the file extension. Binary files and files over 1 MB show a placeholder instead of loading.
- **History:** a commit list (short hash, message, author, relative time) with virtual scrolling, and a commit detail view with its changed files and diff.
- **Web terminal** per §5.4, loaded lazily the first time the panel opens, rendering `CommandResult.lines` as ANSI, with history kept for the session.

**Done when** every Phase 1–3 command works in the web terminal with output identical to the CLI, a commit made in the web terminal, the TUI, or any other terminal shows up in History immediately, and the bundle-size check passes.

### Phase 7: Time travel, commit graph, diff inspector, media previews

**Build**
- **Time-travel scrubber.** A slider over the current branch's first-parent history, oldest to newest, plus a final **Working tree** stop. Use the arrow keys to step.
  - It is read-only: scrubbing changes what the explorer and viewer show and never touches files on disk. A **Check out this version** button runs `nexus checkout <hash>`, with the usual dirty check.
  - Keep the open file and its scroll position when that file exists at the selected commit. Otherwise show "this file doesn't exist at this point".
  - Performance: blobs and trees are immutable, so serve them with `Cache-Control: immutable` and cache them by hash on the client. Prefetch neighboring commits.
  - Why first-parent: with merges, history is a graph, but a slider needs a line. First-parent history is "this branch as it looked over time".
- **Commit graph.** SVG drawn from the layout endpoint, with branch labels at the tips, a HEAD marker, and curved merge edges. Virtualize rows for long histories. Clicking a node opens the commit detail, and hovering shows the message.
- **Diff inspector.** Unified and side-by-side modes rendered from the server's hunks, so the CLI and the UI always agree. Syntax highlighting reuses CodeMirror's language parsers. It collapses unchanged regions, lists files with +/− counts, and can compare any two commits picked from the graph.
- **Media previews and diffs:**
  - Images (PNG, JPEG, WebP, GIF, SVG) are shown side by side, with swipe and onion-skin comparison modes.
  - Audio and video play in native HTML5 players, streamed with Range requests.
  - Other binaries show size, hash, and a chunk-change summary.

**Done when** scrubbing the demo repo meets the time-travel budget in §6, the SVG graph matches `nexus log --graph`, every diff matches `nexus diff`, and a large video in the demo repo starts playing without being downloaded in full.

### Phase 8: Search and playground

**8a. Search**
- `nexus find <pattern> [--regex] [--at <commit>] [--path <glob>]`, built on `grep-searcher` and `grep-regex` over the `ignore` walker. Skip binary and chunked files.
- With `--at`, search the blobs in that commit's tree directly from the object store.
- Add a UI search panel on Ctrl+Shift+F and a TUI search prompt. Results are grouped by file with line previews, and selecting one opens the file at that line.
- No persistent index unless a benchmark proves the scan misses the §6 budget.

**8b. Playground**
- A scratchpad panel next to the explorer, saved under `.nexus/scratch/`, with a **Send selection to playground** action in the file viewer. All of it is lazily loaded.
- JS runs in a Web Worker. TS is stripped with `sucrase` inside the worker first. Capture console output, and kill the worker after a 5-second timeout. The worker gets no DOM access and no access to the app.
- Python runs on Pyodide in a worker:
  - `nexus playground install-python [<archive>]` downloads the runtime once into the global data directory, or installs it from a local archive for air-gapped machines.
  - The server then serves it locally. It is never loaded from a CDN or embedded in the binary.
- HTML, CSS, SVG, and Markdown files get a live preview in an `<iframe sandbox="allow-scripts">`, without `allow-same-origin`.

### Phase 9: Issues and boards

**Build**
- **The metadata op-log engine** from §4: append an operation, replay, merge by union, and the derived-state cache. Write it generically, because Phase 10 reuses it.
- **CLI:**
  - `nexus issue new|list|show|comment|edit|close|reopen|label`, with filters by status, label, and text.
  - `nexus board [name]` prints the board's columns.
- **Linking to commits:**
  - A commit message mentioning `#<issue-id>` links the commit to the issue.
  - A commit containing `Fixes #<issue-id>` closes the issue when it lands on the default branch.
- **Web:** an issue list with filters, an issue detail page with Markdown rendered server-side (`pulldown-cmark` + `ammonia`), and a kanban board with drag-and-drop between columns.
- **TUI:** an Issues tab with a list, detail view, and quick status changes.

**Tests**
- Replay determinism.
- Two diverged histories of `refs/meta/issues` merge by union into identical state, whatever the merge order.
- Concurrent `set` operations resolve by `(lamport, op hash)`.
- `Fixes #id` closes the issue.
- The `issue list` budget is met with 5,000 generated issues.

**Done when** I can create issues, triage them on a board, and close them from the CLI, the TUI, and the web UI, all with networking disabled.

### Phase 10: Local code review

**Build**
- **CLI:**
  - `nexus review start <branch> [--base main]` and `nexus review list|show|comment|resolve|approve|request-changes|close`.
  - `nexus review merge` runs `nexus merge` into the base branch and marks the review as merged.
- **Comments** are anchored to `(commit, path, side, line)`, with threads and resolution.
- **When the branch gets new commits:**
  - Re-anchor each comment by mapping its line through the diff between the old and new commit.
  - If the anchored line itself changed, mark the comment **outdated** and keep showing it against the version it was written on.
- **Web:** a review page built on the Phase 7 diff inspector, with inline comment threads and a per-file "viewed" checkbox.
- **TUI:** a Reviews tab.
- On your own, this is a self-review checklist plus AI review comments (Phase 12d). With remotes (Phase 14), it becomes review between machines and people.

**Tests**
- Re-anchoring when lines are moved, edited, or deleted, and when the file is deleted.
- Review state transitions.
- Merging from a review.

**Done when** I can review a branch, leave and resolve comments, push new commits that keep comments correctly anchored or mark them outdated, and merge from the review.

### Phase 11: Local CI runner

**Build**
- **`nexus-ci.toml`:**

  ```toml
  [[job]]
  name = "test"
  on = ["commit:main", "manual"]   # commit:<branch-glob> | manual
  run = ["cargo test"]
  timeout = "10m"
  image = "rust:1"                 # optional: run inside Docker or Podman
  cache = ["target"]               # directories kept between runs
  ```

- **Running jobs:**
  - Every run happens in a clean export of the commit (`nexus export`), never in my working directory.
  - Without `image`, steps run on the host as my user. With `image`, they run in a container through the `docker` or `podman` CLI, whichever is installed.
  - Output is streamed to a log, with timeouts, cancellation, and one run at a time per repo (others queue).
- **Trust:** a repo's jobs run only after `nexus ci trust`, which records the hash of `nexus-ci.toml`. If the file changes, the runner refuses until the repo is trusted again. This is the same idea as direnv's `allow`, and it matters because CI config is code that runs on your machine.
- **Triggers:**
  - `nexus ci run [job] [--commit <hash>]` runs in the foreground.
  - Automatic triggers fire on ref changes while `nexus ui` or `nexus ci watch` is running.
- **Results** are stored under `.nexus/ci/runs/<run-id>/` (`run.toml` and `log.txt`). Each commit shows ✓, ✗, or a pending marker in `nexus log`, the TUI, History, and the graph.
- **Views:** a CI page with live-streamed logs in the dashboard, and a CI tab in the TUI.

**Tests**
- A job passes, fails, times out, and is cancelled.
- The runner refuses an untrusted or changed config.
- The run happens in an export, not the working directory.
- Offline mode adds `--pull=never`.
- Container tests run only when Docker or Podman is available.

**Done when** committing to `main` while `nexus ui` is running triggers the job, its log streams live into the dashboard, and the commit shows its result everywhere history is displayed.

### Phase 12: Local AI

**Setup**
- `[ai]` config: `endpoint`, `chat_model`, `embedding_model`, and `enabled`, which is false by default per repo.
- It works with any OpenAI-compatible server, using `/v1/chat/completions` and `/v1/embeddings`: Ollama, llama.cpp's `llama-server`, LM Studio, or vLLM.
- The default endpoint is Ollama at `http://localhost:11434/v1`. A remote endpoint requires explicit config, and its API key comes from an environment variable, never `config.toml`.
- `nexus ai status` shows the endpoint, whether it's local, and the models in use. The dashboard shows an indicator whenever content is being sent.
- All §5.5 AI rules apply.

**12a. Commit messages.** `nexus commit` without `-m`: build a prompt from the staged diff and get a Conventional Commits suggestion.
- The CLI asks me to accept, edit, or reject it. It never commits on its own.
- Truncate large diffs to a token budget and fall back to per-file stats.
- The dashboard and the TUI commit views get a **Suggest message** action.

**12b. Ask the repo.** `nexus ask "<question>" [--at <commit>]`, plus a chat panel in the dashboard.
- **Indexing:**
  - Split text files into windows of about 60 lines that overlap, and embed each window.
  - Cache embeddings in `.nexus/cache/embeddings/` keyed by `(blob hash, model)`. Content is immutable, so unchanged files are never re-embedded, and questions about old commits reuse existing vectors.
  - Indexing runs in the background with progress and can be cancelled.
- **Retrieval** is hybrid: brute-force cosine similarity (no vector database) combined with keyword hits from Phase 8 search.
- **Answers** cite `path:line`, and each citation opens in the viewer.

**12c. Explain.** "Explain this commit" and "Explain this file's history" actions in the dashboard and the TUI.

**12d. AI review** (needs Phase 10). `nexus review ai <review-id>` posts suggestions as review comments, clearly marked as AI-generated.

**12e. Semantic search.** A semantic mode in the search panel, reusing the 12b index.

**Tests**
- Run against a mock OpenAI-compatible server inside the test suite, so CI never needs a real model.
- Secret-pattern files never appear in any request.
- The first-send confirmation appears for non-localhost endpoints.
- The embedding cache is reused across commits.
- The retrieval budget is met.

**Done when**, with Ollama running locally and networking otherwise disabled, I can generate a commit message, ask a question about the codebase and get answers with working citations, and run an AI review on a branch.

### Phase 13: Release builds

**Build**
- A GitHub Actions release workflow, triggered by a version tag, that builds these targets:
  - `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` (static, so they run on any distro)
  - `x86_64-apple-darwin` and `aarch64-apple-darwin`
  - `x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc`
- `cargo xtask dist`: packages each binary as `.tar.gz` (or `.zip` on Windows) with SHA-256 checksums.
- One-line install scripts: `install.sh` for Linux and macOS, `install.ps1` for Windows.
- A README section on installing, upgrading, air-gapped installs, and the CLI-only build.

**Done when** a freshly downloaded binary runs `nexus init`, `nexus tui`, and `nexus ui` on a clean machine of each OS with nothing else installed, and the binary-size budget in §6 is met.

### Phase 14: Stretch goals (only if I ask)

- **Remotes:** `nexus serve` exposes a repo over HTTP, and `nexus clone`, `nexus push`, and `nexus pull <url>` exchange missing objects by walking from refs.
  - Push is fast-forward only for branches.
  - `refs/meta/*` merges by union, so issues and reviews sync without conflicts.
  - Large files transfer only their missing chunks, which makes interrupted transfers resumable.
- **On-demand large files:** clone without large-file contents and fetch each file when it's checked out or opened. A fully transparent version needs a virtual filesystem on each OS (FUSE, ProjFS, FSKit), which is much harder.
- **Pack files:** pack loose objects with delta compression for long histories.
- **Desktop app:** a Tauri v2 wrapper that links `nexus-core` directly.
- **Reflog and `nexus undo`.**
