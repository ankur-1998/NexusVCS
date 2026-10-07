# NexusVCS — Master Build Spec

You are the lead engineer building NexusVCS with me, one phase at a time. This spec is the source of truth for scope, on-disk formats, architecture, and performance targets. When I say **"Execute Phase N"**, build that phase only, following the working agreement below.

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
8. **End-of-phase report:** what you built, the exact commands I should run to verify it by hand (they must work in bash, zsh, and PowerShell), benchmark numbers against the §6 budgets where relevant, known limitations, and updates to `docs/ARCHITECTURE.md` and `docs/DECISIONS.md` (one entry per non-obvious decision, with the reason).

---

## 2. What we're building

NexusVCS is a personal, self-hosted version control system written from scratch: no `git` binary, no Git libraries. It uses Git's proven model (content-addressed objects, trees, commits, refs, a staging index) with its own on-disk format. It ships as **one small native binary per OS**, with no runtime to install, and puts three interfaces on one engine:

| Interface | What it is |
|---|---|
| `nexus` CLI | A native command-line tool for any terminal on any OS. |
| Web dashboard | A GitHub-style browser served by `nexus ui` and opened in any browser: file tree, file viewer, history, commit graph, diffs, time travel. |
| Web terminal | A terminal panel inside the dashboard that runs the same commands as the CLI, with identical output. |

What sets it apart from a plain Git host:

- **Time-travel scrubber.** Drag a slider through history and watch the file tree and the open file change in place.
- **Interactive commit graph** and **diff inspector.**
- **Search** across the working tree or any past commit at ripgrep speed, from the UI or `nexus find`.
- **AI commit messages** generated from the staged diff, using a local model by default.
- **Playground.** Run snippets (JS/TS/Python) and preview HTML/CSS/SVG/Markdown in a sandbox next to the code.

### Non-goals for v1

- Compatibility with Git's on-disk format or wire protocol. The model is Git's; the bytes are ours.
- Multiple users, accounts, auth, permissions, pull requests, issues.
- Pack files and delta compression, partial (hunk) staging, rename detection, submodules, empty directories.
- Symlinks. Skip them with a warning, because creating them on Windows needs special privileges.
- Non-UTF-8 file names. Skip them with a warning.
- A real system shell in the browser (see §5.4).

---

## 3. Stack (decided)

Each language goes where it's strongest. Rust handles everything that touches the filesystem, and the browser gets the smallest UI stack that does the job.

**Why Rust for the engine, CLI, and server:**
- It compiles to a single static binary per OS with no runtime, a small footprint, and no garbage-collector pauses.
- The hot paths of a VCS are walking directories, hashing, and searching. Rust gets those from ripgrep's own crates (`ignore`, `grep`).
- Precedent: Jujutsu (jj) and gitoxide are VCS tools written in Rust.
- Go was the runner-up. It loses on raw speed and memory, and it doesn't have ripgrep's crates.

**Why Svelte for the UI:** it compiles to small vanilla JS with almost no runtime, which keeps the dashboard light. React's runtime alone is larger than our entire initial-load budget. The UI is served by the binary itself and opens in any browser on any OS, so there's nothing extra to install. A desktop wrapper (Tauri) remains an option later, and it's a natural fit because the backend is already Rust.

### Rust crates

| Concern | Crate |
|---|---|
| CLI parsing | `clap` (derive) |
| Terminal colors | `anstream` + `anstyle` (honors `NO_COLOR`, handles Windows consoles, strips colors when piped) |
| Hashing | `sha2` (uses the CPU's SHA instructions when available) |
| Compression | `zstd` (level 3) |
| Directory walk + ignore rules | `ignore` (ripgrep's parallel walker, gitignore semantics) |
| Parallelism | `rayon` |
| Diff | `imara-diff` (histogram algorithm by default, Myers available) |
| Search | `grep-regex` + `grep-searcher` (ripgrep's engine) |
| Atomic writes | `tempfile` (`NamedTempFile::persist`) |
| Config | `serde` + `toml` |
| HTTP + WebSocket | `axum` (with `ws`) on `tokio` |
| Embedded UI | `rust-embed` (the built web UI is compiled into the binary) |
| File watching | `notify` + `notify-debouncer-full` |
| HTTP client (AI) | `ureq` with `rustls` using the `ring` provider |
| Rust-to-TypeScript types | `ts-rs` |
| Errors | `thiserror` in libraries, `anyhow` in the binary |
| Tests | built-in tests plus `tempfile`, `proptest`, `insta` |

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
crates/
  nexus-core/           Engine + command layer. Synchronous, no async runtime, never prints.
  nexus-server/         axum server, file watcher, embedded UI.
  nexus/                The binary: CLI rendering, interactive prompts, `nexus ui`.
xtask/                  Automation: dev, build, ci, bench, demo, dist.
web/                    Svelte app. Its build output is embedded into the binary.
```

The `nexus` binary has a default `ui` feature. `cargo build --no-default-features` produces a CLI-only binary with no tokio or axum, for anyone who wants the smallest possible tool.

### `cargo xtask` commands

| Command | Does |
|---|---|
| `dev` | Runs the Vite dev server and the Rust server together, with `/api` proxied. |
| `build` | Builds the web UI, then the release binary with the UI embedded. |
| `ci` | `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, `svelte-check`, Prettier check, web build, bundle-size check, and a stale-check on the `ts-rs` output. |
| `bench` | Generates synthetic repos and measures them against §6. Compares with `git` when it's installed. |
| `demo` | Builds a demo repo with branches, merges, and a resolved conflict (Phase 3). |
| `dist` | Builds release archives for the current target (Phase 8). |

---

## 4. Repository format (the contract)

```text
<repo>/
  .nexusignore             gitignore syntax; written with sensible defaults by `nexus init`
  .nexus/
    HEAD                   "ref: refs/heads/main\n"  or  "<hash>\n" when detached
    config.toml            [user] name, email; [ai] endpoint, model, ...
    index                  the staging area (binary, see below)
    lock                   exists only while a mutating command runs
    MERGE_STATE            exists only during an unresolved merge (TOML: merge_head, conflicts)
    refs/heads/<branch>    "<hash>\n"
    objects/ab/cdef...     zstd-compressed objects, named by the SHA-256 of their uncompressed bytes
    cache/                 rebuildable data; safe to delete
    scratch/               playground files; never committed
```

Global config lives in the OS's standard config directory: `~/.config/nexus/` on Linux, `~/Library/Application Support/nexus/` on macOS, and `%APPDATA%\nexus\` on Windows.

### Objects

The uncompressed bytes of every object are `<type> <bodyByteLength>\0<body>`. The object's hash is the lowercase hex SHA-256 of those bytes. It is stored zstd-compressed at `objects/<first 2 hex chars>/<remaining 62>`.

- **blob:** the body is the file's bytes, verbatim. Never convert line endings.
- **tree:** the body is one line per entry, `<file|exec|tree> <hash> <name>\n`, sorted by name in byte order (UTF-8). `exec` marks an executable file. A name is a single path segment. Reject names containing `/`, `\n`, or `\0`.
- **commit:**

  ```text
  tree <hash>
  parent <hash>
  author Ada Lovelace <ada@example.com> 1759737600 +0530

  <message>
  ```

  There are zero `parent` lines for a root commit, one normally, and two for a merge. The first parent is the branch you were on.

### Index (binary, little-endian)

```text
header   magic "NXIX" | version u32 = 1 | entry_count u32
entry    path_len u16 | path (UTF-8, '/'-separated) | kind u8 (0 = file, 1 = exec)
         | hash [32 bytes] | size u64 | mtime_ns i64
trailer  SHA-256 of all preceding bytes [32 bytes]
```

Entries are sorted by path bytes. Paths are relative to the repo root. Loading fails on a bad checksum. `nexus debug index` prints the index as human-readable text.

### Invariants

**Data integrity**
- The same content produces the same hash on every OS. A determinism test runs in CI on all three.
- Objects are immutable. Write one only if it doesn't already exist. On read, verify that the hash matches and report corruption otherwise.
- Writes to `index`, refs, `HEAD`, and `MERGE_STATE` are atomic: write a temp file in the same directory, then rename it. On Windows, retry the rename on sharing violations with a short backoff, because antivirus and indexers briefly lock files.
- Mutating commands take `.nexus/lock`, created with `create_new`. If it's held, fail with a clear message. The CLI and the UI server will run at the same time.

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

### 5.1 One engine, three interfaces

```text
                    nexus-core  (engine + command layer, synchronous)
                   /                \
      nexus (CLI)                     nexus-server  <-- HTTP / WebSocket -->  web/ (dashboard + web terminal)
      any terminal                    (axum, watcher)                         any browser
```

### 5.2 Command layer

Implement each `nexus` command exactly once, in `nexus-core`. The clap definitions live there too, so the CLI and the web terminal parse arguments identically.

```rust
pub fn run(argv: &[String], ctx: &Ctx) -> CommandResult;   // Ctx { repo_root, cwd }

pub struct CommandResult {
    pub exit_code: i32,
    pub lines: Vec<Line>,                    // Line { text, style: Option<Style> }
    pub data: Option<serde_json::Value>,     // typed payload per command, for the UI
}
// Style: Added | Removed | Hash | Heading | Warning | Error | Muted
```

- **Core never prints and never prompts.** Prompts live in the CLI binary and in the web UI. In the web terminal, a command that needs confirmation prints the exact command to run next.
- **Rendering.** The CLI renders `lines` with `anstyle`. The web terminal renders the same `lines` as ANSI in xterm.js, and the dashboard uses `data`. As a result, `nexus status` looks identical in any terminal and in the browser.
- **Concurrency.** The server calls core through `tokio::task::spawn_blocking`, and core uses `rayon` for parallel work.

### 5.3 Server

- **Security.**
  - Bind to `127.0.0.1` only.
  - On startup, generate a random token. `nexus ui` opens `http://127.0.0.1:<port>/?token=...`. The page exchanges the token for an `HttpOnly`, `SameSite=Strict` cookie and removes it from the address bar.
  - Every HTTP request and WebSocket connection must be authenticated.
  - Reject any request whose `Host` or `Origin` isn't localhost. This blocks DNS-rebinding attacks from websites open in the same browser.
- **Reads** are JSON endpoints: repo summary, branches, paginated commit list, commit detail, tree (at a commit or of the working tree), blob (raw bytes), diff hunks, status, graph layout, search.
- **Writes** go only through the command layer, over the WebSocket: `{ exec: argv, cwd }` comes back as `{ result }`. Convenience actions, such as the dashboard's checkout button, send the same commands.
- **API types** are defined once in Rust and exported to TypeScript with `ts-rs`. CI fails if the generated file is stale.
- **Live updates.**
  - Watch the working tree and `.nexus/`, debounced about 150 ms and filtered through the ignore rules.
  - Push `{ type: "worktree" | "index" | "refs" }` events over the WebSocket, and the UI refetches whatever changed.
  - A commit made in any other terminal appears in the dashboard without a page refresh.
  - On Linux, if the watch fails because of the inotify watch limit, fall back to polling and show a notice in the UI.

### 5.4 The web terminal is a virtual shell

The browser terminal is not a real system shell. It's a restricted shell confined to the repo root.

- Built-ins: `ls`, `cd`, `pwd`, `cat`, `tree`, `clear`, `help`, `history`. Anything starting with `nexus` goes to the command layer.
- The current directory is client state, sent with each command. The server resolves it and rejects paths outside the repo.
- xterm.js has no line editing, so implement it: cursor movement, backspace, history with up/down, Ctrl+C, and Tab completion for paths and `nexus` subcommands.

Why not a real shell: exposing one over a local socket is a remote-code-execution hole, and its behavior differs across operating systems.

---

## 6. Performance budgets

`cargo xtask bench` measures these on a synthetic repo of 10,000 files and 1,000 commits, with a warm filesystem cache. Where Git is the reference, the comparison runs on the same tree.

| Operation | Budget |
|---|---|
| `nexus status`, nothing changed | ≤ 2× `git status`, and under 100 ms |
| `nexus add .`, fresh repo | ≤ 2× `git add .` (hash files in parallel) |
| `nexus commit` after one changed file | under 50 ms |
| `nexus log -n 100` | under 20 ms |
| `nexus find <literal>` over the working tree | ≤ 2× `rg` on the same tree |
| `nexus --version`, cold start | under 20 ms |
| `nexus ui` ready to serve | under 500 ms; idle memory under 30 MB |
| Release binary, UI embedded | under 15 MB |
| Dashboard initial JS | under 150 KB gzipped. Load the terminal, editor, graph, and playground lazily. `cargo xtask ci` enforces this. |
| Time-travel step, warm cache | under 50 ms |

If a budget can't be met, say so in the phase report with the measurements. Don't quietly relax it.

---

## 7. Phases

Each phase lists what to **build** and the **done when** check I'll run myself.

### Phase 0: Scaffold

**Build**
- The Cargo workspace and crates from §3, plus `xtask` with at least `ci` and `build`.
- The `web/` Svelte + Vite + TypeScript skeleton.
- `rust-toolchain.toml`, `rustfmt.toml`, workspace-wide clippy lints in `Cargo.toml`, `.editorconfig`, Prettier config.
- A GitHub Actions workflow that runs `cargo xtask ci` on `ubuntu-latest`, `macos-latest`, and `windows-latest`.
- `docs/ARCHITECTURE.md` (summarize §4 and §5), `docs/DECISIONS.md`.
- `nexus --version` prints the version.

**Done when** `cargo xtask ci` passes on my machine, `cargo run -- --version` prints a version, and the CI workflow is ready to run the same command on all three operating systems.

### Phase 1: Object store, staging, commits

**Build**
- Object store: write, read, and existence check per §4, plus `nexus hash-object <file>`, `nexus cat-file -p <hash>` (pretty-prints any object type), and `nexus debug index`.
- `nexus init`: refuses if already inside a repo. Creates the layout from §4 with `HEAD` pointing at `refs/heads/main`, which stays unborn until the first commit. Writes a default `.nexusignore` (`node_modules/`, `target/`, `dist/`, `build/`, `.env`, `*.log`).
- Repo discovery: walk up from the current directory to find `.nexus`, so commands work from subdirectories.
- `nexus config [--global] user.name|user.email <value>`.
- `nexus add <path...>`: accepts files, directories, and `.`, and also takes `--exec`. It walks with `ignore` and hashes in parallel with `rayon`. Adding a path that no longer exists on disk removes it from the index.
- `nexus commit -m "<msg>"`: builds trees bottom-up from the index, writes the commit, and advances the current branch (or `HEAD` when detached). Refuses a commit whose tree is identical to its parent's unless `--allow-empty` is passed. Fails with a helpful message if no author identity is configured.
- `nexus log [-n N] [--oneline]`: first-parent history from `HEAD`. Prints "no commits yet" on an unborn branch.

**Tests**
- A three-commit pipeline: init, then three rounds of edits (modify, add, delete, nested directories), each committed. Assert the log order and parent links, and assert that every commit's tree rebuilds the exact file bytes from the object store.
- Determinism: the same files created in a different order produce the same tree hash, on every OS in CI.
- Byte-exact round trips for a binary file, a CRLF file, and a file with a non-ASCII (NFD) name.
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

### Phase 2: Working tree (status, diff, checkout, branches)

**Build**
- `nexus status`:
  - Compares HEAD's tree, the index, and the working tree, then reports staged changes (added/modified/deleted), unstaged changes (modified/deleted), and untracked files.
  - Walks the tree in parallel.
  - Fast path: if size and `mtime_ns` match the index entry, assume the file is unchanged. Rehash any file whose mtime is not older than the index file's own mtime (the racy-timestamp problem).
- **Diff** through a `diff` module that wraps `imara-diff`:
  - Defaults to the histogram algorithm, with `--diff-algorithm=myers` available. Returns hunks with 3 lines of context.
  - Files with a NUL byte in their first 8 KB are treated as binary and reported as "Binary files differ".
  - The CLI prints unified diff format with colors. The API returns the same hunks as JSON.
- `nexus diff` compares the working tree to the index. `--staged` compares the index to HEAD. `nexus diff <a> <b>` compares two commits. All three accept optional path filters.
- `nexus show <commit>`: commit header plus the diff against its first parent.
- `nexus branch` lists branches and marks the current one. `nexus branch <name> [<start>]` creates one. `nexus branch -d <name>` deletes one, refusing if it isn't merged into the current branch; `-D` forces it.
- `nexus checkout <branch|commit>`:
  - Refuses, listing the files, if it would overwrite uncommitted changes.
  - Otherwise applies the tree difference: write changed files, create added ones, delete removed ones, and set the executable bit on Unix.
  - Never touches untracked or ignored files and never wipes the directory.
  - Rewrites the index and updates `HEAD`, printing a warning when that leaves HEAD detached.
- `nexus restore <path...> [--source <commit>]`: discards changes to the named files. This is the one deliberately destructive command, so it requires explicit paths.
- Abbreviated hashes: accept any unique prefix of 4 or more characters. An ambiguous prefix is an error that lists the candidates.
- Graph helpers in core: `walk_commits(from, first_parent)`, `is_ancestor(a, b)`, and a children map.
- `cargo xtask bench`, covering the `status`, `add`, `commit`, and `log` rows of §6.

**Tests**
- Diff on the classic cases: empty, identical, completely different, insert at the start or end, repeated lines.
- A `proptest` property: applying the diff of `a` to `b` back onto `a` reproduces `b`.
- Checking out each of three commits restores the exact bytes, and the executable bit on Unix.
- A checkout with dirty files refuses and changes nothing.
- Untracked files survive a checkout.

**Done when** you can create a branch, make diverging commits, switch back and forth, `status` and `diff` report what Git would report in the same situation, and the benchmark rows above are within budget.

### Phase 3: Merging and the commit graph layout

**Build**
- Merge base: the lowest common ancestor, found with a BFS over both histories. For criss-cross histories with several candidates, pick one and document the choice.
- `nexus merge <branch>` refuses to start with uncommitted changes, which keeps `--abort` simple and safe. It handles three outcomes: already up to date, fast-forward, or three-way.
- Three-way merge per file. If only one side changed, take that side. If both changed, run a line-level diff3 built on the diff module, and write `<<<<<<< ours` / `=======` / `>>>>>>> theirs` markers where hunks overlap. A delete on one side with a modify on the other is a conflict that keeps the modified file. A binary file changed on both sides is a conflict that keeps ours.
- Conflict state: write `MERGE_STATE`, and have `status` list conflicted files. `nexus commit` refuses while any conflicted file hasn't been re-added or still contains markers. Committing after resolution creates a two-parent commit. `nexus merge --abort` resets tracked files to HEAD and removes `MERGE_STATE`.
- **Graph lane layout** in core: `layout_graph(commits) -> { nodes: [{ hash, row, lane }], edges }`. The first parent continues the lane, and freed lanes are reused. It powers `nexus log --graph` now and the SVG graph in Phase 6, which receives it from the API.
- `cargo xtask demo`: builds a repo with about 60 commits, several branches, merges, and one resolved conflict. Later phases use it for UI work.

**Tests**
- A fast-forward merge, a clean three-way merge, and a conflicting three-way merge with the exact marker output.
- A delete/modify conflict, `--abort`, and refusal to start with a dirty tree.
- `insta` snapshots of the layout and the `log --graph` output for linear history, a single merge, and many parallel branches.

**Done when** the demo repo's `nexus log --graph` output reads correctly and every merge scenario above behaves as described.

### Phase 4: Local server and `nexus ui`

**Build**
- The server from §5.3, with the built UI embedded through `rust-embed`. In debug builds, serve the files from disk so UI changes don't need a Rust rebuild.
- `nexus ui [path] [--port 8080]`:
  - If the path has no repo, show how many files would be tracked (after ignore rules) and ask before running `init`.
  - If the port is taken, try the next one.
  - Open the default browser, and shut down cleanly on Ctrl+C.

**Tests**
- Integration tests for every endpoint, driving the axum router directly with `tower::ServiceExt::oneshot`.
- Unauthenticated requests get 401. Requests with a non-localhost `Origin` or `Host` are rejected. `exec` rejects paths that escape the repo.
- The watcher emits an event when a file changes on disk.

**Done when** `nexus ui` inside a repo opens a placeholder page whose repo summary updates live while I edit a file in any editor, and the `nexus ui` row of §6 is within budget.

### Phase 5: Dashboard and web terminal

**Build**
- Layout: a top bar (repo name, HEAD, dirty indicator), a left sidebar (branch switcher, file tree), a main viewer, and a resizable bottom terminal panel toggled with Ctrl+`. It follows the OS light or dark preference and has a manual toggle.
- File tree: lazy-loads directories and has a source toggle between **Working tree** and **a commit**. In working-tree mode, files show status badges (M/A/D/U).
- File viewer: CodeMirror 6, read-only, with the language loaded lazily based on the file extension. Binary files and files over 1 MB show a placeholder instead of loading.
- History: a commit list (short hash, message, author, relative time) with virtual scrolling, and a commit detail view with its changed files and diff.
- Web terminal per §5.4, loaded lazily the first time the panel opens, rendering `CommandResult.lines` as ANSI, with history kept for the session.

**Done when** every Phase 1–3 command works in the web terminal with output identical to the CLI, a commit made in either the web terminal or any other terminal shows up in History immediately, and the bundle-size check passes.

### Phase 6: Time travel, commit graph, diff inspector

**Build**
- **Time-travel scrubber.** A slider over the current branch's first-parent history, oldest to newest, plus a final **Working tree** stop. Use the arrow keys to step.
  - It is read-only: scrubbing changes what the explorer and viewer show and never touches files on disk. A **Check out this version** button runs `nexus checkout <hash>`, with the usual dirty check.
  - Keep the open file and its scroll position when that file exists at the selected commit. Otherwise show "this file doesn't exist at this point".
  - Performance: blobs and trees are immutable, so serve them with `Cache-Control: immutable` and cache them by hash on the client. Prefetch neighboring commits.
  - Why first-parent: with merges, history is a graph, but a slider needs a line. First-parent history is "this branch as it looked over time".
- **Commit graph.** SVG drawn from the layout endpoint, with branch labels at the tips, a HEAD marker, and curved merge edges. Virtualize rows for long histories. Clicking a node opens the commit detail, and hovering shows the message.
- **Diff inspector.** Unified and side-by-side modes rendered from the server's hunks, so the CLI and the UI always agree. Syntax highlighting reuses CodeMirror's language parsers. It collapses unchanged regions, lists files with +/− counts, and can compare any two commits picked from the graph.

**Done when** scrubbing the demo repo meets the time-travel budget in §6, the SVG graph matches `nexus log --graph`, and every diff matches `nexus diff`.

### Phase 7: Developer extras

These three are independent. I'll request them one at a time, in any order.

**7a. Search**
- `nexus find <pattern> [--regex] [--at <commit>] [--path <glob>]`, built on `grep-searcher` and `grep-regex` over the `ignore` walker. Skip binary files.
- With `--at`, search the blobs in that commit's tree directly from the object store.
- Add a UI search panel on Ctrl+Shift+F. Results are grouped by file with line previews, and clicking one opens the file at that line.
- No persistent index unless a benchmark proves the scan misses the §6 budget.

**7b. AI commit messages**
- `nexus commit` without `-m`, when AI is configured: build a prompt from the staged diff and get a Conventional Commits suggestion from an OpenAI-compatible `/v1/chat/completions` endpoint. The CLI then asks me to accept, edit, or reject it. It never commits on its own.
- Default endpoint: local Ollama at `http://localhost:11434/v1`. A remote endpoint requires explicit config, and its API key comes from an environment variable, never `config.toml`.
- Never send files matching secret patterns (`.env*`, `*.pem`, `*.key`, `*secret*`). Truncate large diffs to a token budget and fall back to per-file stats.
- When AI isn't configured, fail with a message pointing at `-m`. In the UI, add a commit panel with a **Suggest message** button.

**7c. Playground**
- A scratchpad panel next to the explorer, saved under `.nexus/scratch/`, with a **Send selection to playground** action in the file viewer. All of it is lazily loaded.
- JS runs in a Web Worker. TS is stripped with `sucrase` inside the worker first. Capture console output, and kill the worker after a 5-second timeout. The worker gets no DOM access and no access to the app.
- Python runs on Pyodide in a worker, downloaded only on first use and never embedded in the binary.
- HTML, CSS, SVG, and Markdown files get a live preview in an `<iframe sandbox="allow-scripts">`, without `allow-same-origin`.

### Phase 8: Release builds

**Build**
- A GitHub Actions release workflow, triggered by a version tag, that builds these targets:
  - `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` (static, so they run on any distro)
  - `x86_64-apple-darwin` and `aarch64-apple-darwin`
  - `x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc`
- `cargo xtask dist`: packages each binary as `.tar.gz` (or `.zip` on Windows) with SHA-256 checksums.
- One-line install scripts: `install.sh` for Linux and macOS, `install.ps1` for Windows.
- A README section on installing, upgrading, and the CLI-only build.

**Done when** a freshly downloaded binary runs `nexus init` and `nexus ui` on a clean machine of each OS with nothing else installed, and the binary-size budget in §6 is met.

### Phase 9: Stretch goals (only if I ask)

- **Remotes:** `nexus serve` exposes a repo over HTTP, and `nexus clone`, `nexus push`, and `nexus pull <url>` exchange missing objects by walking from refs. Push is fast-forward only. This is what turns it into a self-hosted "GitHub" for my other machines.
- **Pack files:** pack loose objects with delta compression for large histories.
- **Desktop app:** a Tauri v2 wrapper that links `nexus-core` directly.
- **Semantic search** with local embeddings.
- **Reflog and `nexus undo`.**
