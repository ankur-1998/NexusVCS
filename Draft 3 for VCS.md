# NexusVCS — Master Build Spec

You are the lead engineer building NexusVCS with me, one phase at a time. This spec is the source of truth for scope, on-disk formats, and architecture. When I say **"Execute Phase N"**, build that phase only, following the working agreement below.

---

## 1. Working agreement

1. **One phase at a time.** Build only the phase I ask for. Don't build ahead for later phases beyond what the architecture in this spec already requires.
2. **Plan first, briefly.** Before writing code, list the files you'll create or change and any decision this spec leaves open. If a question truly blocks you, ask. Otherwise state your assumption and proceed.
3. **Tests are part of the deliverable.** Engine code (`packages/core`, `packages/shared`) is written test-first. A phase isn't done until `npm test`, `npm run typecheck`, and `npm run lint` all pass.
4. **Formats are a contract.** Don't change anything in §4 without calling it out explicitly, because every later phase depends on it.
5. **Windows first.** I develop on Windows 11 in VS Code. Every script and instruction must work in PowerShell. npm runs scripts through `cmd.exe` on Windows, so no sh-only syntax in `package.json` scripts (`rm -rf`, `export`, `VAR=x cmd`, single-quoted arguments).
6. **Few dependencies, no native modules.** Prefer the Node standard library. Justify each new dependency in one line. Never add a dependency that needs native compilation (node-gyp): it breaks Windows installs and single-file packaging.
7. **End-of-phase report:** what you built, the exact PowerShell commands I should run to verify it by hand, known limitations, and updates to `docs/ARCHITECTURE.md` and `docs/DECISIONS.md` (one entry per non-obvious decision, with the reason).

---

## 2. What we're building

NexusVCS is a personal, self-hosted version control system written from scratch: no `git` binary, no Git libraries. It uses Git's proven model (content-addressed objects, trees, commits, refs, a staging index) with its own on-disk format, and puts three interfaces on one engine:

| Interface | What it is |
|---|---|
| `nexus` CLI | A real command-line tool you run in PowerShell. |
| Web dashboard | A GitHub-style browser: file tree, file viewer, history, commit graph, diffs, time travel. |
| Web terminal | A terminal panel inside the dashboard that runs the same commands as the CLI, with identical output. |

What sets it apart from a plain Git host:

- **Time-travel scrubber.** Drag a slider through history and watch the file tree and the open file change in place.
- **Interactive commit graph** and **diff inspector**, built on our own diff engine.
- **Search** across the working tree or any past commit, from the UI or `nexus find`.
- **AI commit messages** generated from the staged diff, using a local model by default.
- **Playground.** Run snippets (JS/TS/Python) and preview HTML/CSS/SVG/Markdown in a sandbox next to the code.

### Non-goals for v1

- Compatibility with Git's on-disk format or wire protocol. The model is Git's; the bytes are ours.
- Multiple users, accounts, auth, permissions, pull requests, issues.
- Pack files and delta compression, partial (hunk) staging, rename detection, submodules, symlinks, file modes or executable bits, empty directories.
- A real system shell in the browser (see §5.4).
- Files are read fully into memory. Warn on files over 50 MB; don't optimize for them.

---

## 3. Stack (decided)

| Concern | Choice |
|---|---|
| Language | TypeScript (strict, ESM) everywhere |
| Runtime | Node.js 24 LTS or newer |
| Repo layout | npm workspaces monorepo |
| Hashing / compression | `node:crypto` SHA-256, `node:zlib` deflate |
| Tests | Vitest |
| Server | Fastify + `@fastify/websocket` + `@fastify/static` |
| File watching | chokidar |
| Ignore rules | `ignore` (gitignore semantics) |
| UI | React + Vite + Tailwind CSS |
| Terminal | xterm.js (`@xterm/xterm`, `@xterm/addon-fit`) |
| Code viewer | Monaco (`@monaco-editor/react`) |
| Commit graph | Hand-written SVG, no graph library |

**Why TypeScript and not Rust or Go:** one language from engine to UI gives the server and the dashboard shared API types, and xterm.js, Monaco, and React are native to it. Node is fast enough for personal-scale repos. The engine lives in its own package with no UI or HTTP code, so it can be ported to Rust later without touching anything else.

### Packages

```text
packages/
  shared/   Pure, browser-safe TS: API types, Myers diff, three-way merge, graph layout. No node:* imports.
  core/     The engine: object store, index, refs, working tree, command layer. Node only. Never prints.
  cli/      The `nexus` binary: argv -> core command -> ANSI-rendered output.
  server/   HTTP + WebSocket API, file watcher, serves the built web UI.
  web/      React dashboard and web terminal.
```

---

## 4. Repository format (the contract)

```text
<repo>/
  .nexusignore             gitignore syntax; written with sensible defaults by `nexus init`
  .nexus/
    HEAD                   "ref: refs/heads/main\n"  or  "<hash>\n" when detached
    config.json            { "user": { "name", "email" }, "ai": { ... } }
    index.json             the staging area
    lock                   exists only while a mutating command runs
    MERGE_HEAD             exists only during an unresolved merge
    refs/heads/<branch>    "<hash>\n"
    objects/ab/cdef...     deflated objects, named by the SHA-256 of their uncompressed bytes
    cache/                 rebuildable data (search index, etc.); safe to delete
    scratch/               playground files; never committed
```

### Objects

The uncompressed bytes of every object are `<type> <bodyByteLength>\0<body>`. The object's hash is the lowercase hex SHA-256 of those bytes. It is stored deflated at `objects/<first 2 hex chars>/<remaining 62>`.

- **blob:** the body is the file's bytes, verbatim. Never convert line endings.
- **tree:** the body is one line per entry, `<blob|tree> <hash> <name>\n`, sorted by name in byte order (UTF-8). A name is a single path segment. Reject names containing `/`, `\n`, or `\0`.
- **commit:**

  ```text
  tree <hash>
  parent <hash>
  author Ada Lovelace <ada@example.com> 1759737600 +0530

  <message>
  ```

  There are zero `parent` lines for a root commit, one normally, and two for a merge. The first parent is the branch you were on.

### Index

```json
{ "version": 1,
  "entries": [ { "path": "src/app.ts", "hash": "<sha256>", "size": 1234, "mtimeMs": 1759737600000 } ] }
```

Entries are sorted by path. Paths are relative to the repo root and always use `/`.

### Invariants

- The same content produces the same hash on any machine and any OS. A determinism test is required.
- Objects are immutable. Write one only if it doesn't already exist. On read, verify that the hash matches and report corruption otherwise.
- Writes to `index.json`, refs, and `HEAD` are atomic: write a temp file in the same directory, then rename. On Windows, retry the rename on `EPERM`/`EBUSY`/`EACCES` with a short backoff, because antivirus and indexers briefly lock files.
- Mutating commands take `.nexus/lock`, created with the exclusive `wx` flag. If it's held, fail with a clear message. The CLI and the UI server will run at the same time.
- Convert paths between `/` and the OS separator only at the filesystem boundary. Reject any path that resolves outside the repo root. Warn when two tracked paths differ only by case, since Windows and macOS filesystems are case-insensitive.
- `.nexus/` is always ignored, whatever `.nexusignore` says.

---

## 5. Architecture

### 5.1 One engine, three interfaces

```text
                    packages/core  (engine + command layer)
                   /                \
      packages/cli                    packages/server  <-- HTTP / WebSocket -->  packages/web
      (PowerShell)                    (API, watcher)                             (dashboard + web terminal)
```

### 5.2 Command layer

Implement each `nexus` command exactly once, in core:

```ts
run(argv: string[], ctx: { repoRoot: string; cwd: string }): Promise<CommandResult>

type CommandResult = {
  exitCode: number;
  lines: { text: string; style?: "added" | "removed" | "hash" | "heading" | "warning" | "error" | "muted" }[];
  data?: unknown; // typed payload per command, for the UI
};
```

Core never writes to stdout. The CLI renders `lines` with ANSI colors. The web terminal renders the same `lines` through xterm.js. The dashboard uses `data`. As a result, `nexus status` looks identical in PowerShell and in the browser.

### 5.3 Server

- **Security.** Bind to `127.0.0.1` only. On startup, generate a random token. `nexus ui` opens `http://127.0.0.1:<port>/?token=...`, and every HTTP request and WebSocket connection must carry that token. Reject any request whose `Host` or `Origin` isn't localhost, which blocks DNS-rebinding attacks from websites open in the same browser.
- **Reads** are JSON endpoints: repo summary, branches, commit list, commit detail, tree (at a commit or of the working tree), blob, diff, status, search.
- **Writes** go through the command layer only: `POST /api/exec { argv, cwd }`. Convenience endpoints, such as the dashboard's checkout button, call the same commands.
- **Live updates.** Watch the working tree and `.nexus/`, debounced about 150 ms and respecting `.nexusignore`. Push `{ type: "worktree" | "index" | "refs" }` events over WebSocket, and the UI refetches whatever changed. A commit made in a separate PowerShell window appears in the dashboard without a page refresh.

### 5.4 The web terminal is a virtual shell

The browser terminal is not a real PowerShell or bash session. It's a restricted shell confined to the repo root.

- Built-ins: `ls`, `cd`, `pwd`, `cat`, `tree`, `clear`, `help`, `history`. Anything starting with `nexus` goes to the command layer.
- The current directory is client state, sent with each command. The server resolves it and rejects paths outside the repo.
- xterm.js has no line editing, so implement it: cursor movement, backspace, history with up/down, Ctrl+C, and Tab completion for paths and `nexus` subcommands.

Why not a real shell: exposing one over a local socket is a remote-code-execution hole, and its behavior differs across operating systems.

---

## 6. Phases

Each phase lists what to **build** and the **done when** check I'll run myself.

### Phase 0: Scaffold

**Build**
- The npm workspaces from §3, strict `tsconfig`, ESLint + Prettier, Vitest.
- Root scripts: `build`, `test`, `typecheck`, `lint`, `dev`.
- A `nexus` bin that prints its version, runnable via `npm link`.
- `docs/ARCHITECTURE.md` (summarize §4 and §5), `docs/DECISIONS.md`.
- `.vscode/launch.json` with configurations to debug the CLI and the tests.

**Done when** `npm install`, `npm test`, and `npm run typecheck` pass in PowerShell, and `nexus --version` prints a version.

### Phase 1: Object store, staging, commits

**Build**
- Object store: write, read, and existence check per §4, plus `nexus hash-object <file>` and `nexus cat-file -p <hash>` (pretty-prints any object type).
- `nexus init`: refuses if already inside a repo. Creates the layout from §4 with `HEAD` pointing at `refs/heads/main`, which stays unborn until the first commit. Writes a default `.nexusignore` (`node_modules/`, `dist/`, `build/`, `.env`, `*.log`).
- Repo discovery: walk up from the current directory to find `.nexus`, so commands work from subdirectories.
- `nexus config user.name|user.email <value> [--global]`. Global config lives at `%USERPROFILE%\.nexusconfig.json`.
- `nexus add <path...>`: accepts files, directories, and `.`, and respects ignore rules. Adding a path that no longer exists on disk removes it from the index.
- `nexus commit -m "<msg>"`: builds trees bottom-up from the index, writes the commit, and advances the current branch (or `HEAD` when detached). Refuses a commit whose tree is identical to its parent's unless `--allow-empty` is passed. Fails with a helpful message if no author identity is configured.
- `nexus log [-n N] [--oneline]`: first-parent history from `HEAD`. Prints "no commits yet" on an unborn branch.

**Tests**
- A three-commit pipeline: init, then three rounds of edits (modify, add, delete, nested directories), each committed. Assert the log order and parent links, and assert that every commit's tree rebuilds the exact file bytes from the object store.
- Determinism: the same files created in a different order produce the same tree hash.
- Byte-exact round trips for a binary file and a CRLF file.
- Ignore rules, a commit on an unborn branch, and corrupt-object detection.

**Done when** this works in a scratch folder:
```powershell
nexus init
Set-Content a.txt "hello"
nexus add .
nexus commit -m "first"
nexus log
nexus cat-file -p <hash from log>
```

### Phase 2: Working tree (status, diff, checkout, branches)

**Build**
- `nexus status`: compares HEAD's tree, the index, and the working tree, then reports staged changes (added/modified/deleted), unstaged changes (modified/deleted), and untracked files. Fast path: if size and mtime match the index entry, assume the file is unchanged. Rehash any file whose mtime is not older than `index.json`'s own mtime (the racy-timestamp problem).
- **Myers diff**, written by hand in `shared`: a pure `diffLines(a, b, { context = 3 }) -> Hunk[]`. Files with a NUL byte in their first 8 KB are treated as binary and reported as "Binary files differ". The CLI prints unified diff format with colors. The API returns the same hunks as JSON.
- `nexus diff` compares the working tree to the index. `--staged` compares the index to HEAD. `nexus diff <a> <b>` compares two commits. All three accept optional path filters.
- `nexus show <commit>`: commit header plus the diff against its first parent.
- `nexus branch` lists branches and marks the current one. `nexus branch <name> [<start>]` creates one. `nexus branch -d <name>` deletes one, refusing if it isn't merged into the current branch; `-D` forces it.
- `nexus checkout <branch|commit>`: refuses, listing the files, if it would overwrite uncommitted changes. Otherwise it applies the tree difference: write changed files, create added ones, delete removed ones. It never touches untracked or ignored files and never wipes the directory. It then rewrites the index and updates `HEAD`, printing a warning when that leaves HEAD detached.
- `nexus restore <path...> [--source <commit>]`: discards changes to the named files. This is the one deliberately destructive command, so it requires explicit paths.
- Abbreviated hashes: accept any unique prefix of 4 or more characters. An ambiguous prefix is an error that lists the candidates.
- Graph helpers in core: `walkCommits(from, { firstParent })`, `isAncestor(a, b)`, and a children map.

**Tests**
- Myers on the classic cases: empty, identical, completely different, insert at the start or end, repeated lines.
- A property test: applying the diff of `a` to `b` back onto `a` reproduces `b`.
- Checking out each of three commits restores the exact bytes.
- A checkout with dirty files refuses and changes nothing.
- Untracked files survive a checkout.

**Done when** you can create a branch, make diverging commits, switch back and forth, and `status` and `diff` report what Git would report in the same situation.

### Phase 3: Merging and the commit graph layout

**Build**
- Merge base: the lowest common ancestor, found with a BFS over both histories. For criss-cross histories with several candidates, pick one and document the choice.
- `nexus merge <branch>` handles three outcomes: already up to date, fast-forward, or three-way.
- Three-way merge per file, in `shared`. If only one side changed, take that side. If both changed, run a line-level diff3 built on the Myers engine, and write `<<<<<<< ours` / `=======` / `>>>>>>> theirs` markers where hunks overlap. A delete on one side with a modify on the other is a conflict that keeps the modified file. A binary file changed on both sides is a conflict that keeps ours.
- Conflict state: write `MERGE_HEAD`, and have `status` list conflicted files. `nexus commit` refuses while any conflicted file hasn't been re-added or still contains markers. Committing after resolution creates a two-parent commit. `nexus merge --abort` restores the pre-merge state.
- **Graph lane layout** in `shared`: a pure `layoutGraph(commits) -> { nodes: { hash, row, lane }[], edges }`. The first parent continues the lane, and freed lanes are reused. It powers `nexus log --graph` now and the SVG graph in Phase 6.
- `scripts/make-demo-repo.ts` (`npm run demo`): builds a repo with about 60 commits, several branches, merges, and one resolved conflict. Later phases use it for UI work.

**Tests**
- A fast-forward merge, a clean three-way merge, and a conflicting three-way merge with the exact marker output.
- A delete/modify conflict, and `--abort`.
- Layout snapshots for linear history, a single merge, and an octopus-like fan of branches.

**Done when** the demo repo's `nexus log --graph` output reads correctly and every merge scenario above behaves as described.

### Phase 4: Local server and `nexus ui`

**Build**
- The server from §5.3, with API request and response types defined in `shared`.
- `nexus ui [path] [--port 8080]`: if the path has no repo, show how many files would be tracked (after ignore rules) and ask before running `init`. If the port is taken, try the next one. Open the default browser, and shut down cleanly on Ctrl+C.
- In production the server serves the built web UI as static files. In development, Vite proxies `/api` to the server.

**Tests**
- Integration tests through `fastify.inject` for every endpoint.
- Requests without the token get 401. Requests with a non-localhost `Origin` are rejected. `exec` rejects paths that escape the repo.
- The watcher emits an event when a file changes on disk.

**Done when** `nexus ui` inside a repo opens a placeholder page whose repo summary updates live while I edit a file in VS Code.

### Phase 5: Dashboard and web terminal

**Build**
- Layout: a top bar (repo name, HEAD, dirty indicator), a left sidebar (branch switcher, file tree), a main viewer, and a resizable bottom terminal panel toggled with Ctrl+`. Dark theme by default, with a light toggle.
- File tree: lazy-loads directories and has a source toggle between **Working tree** and **a commit**. In working-tree mode, files show status badges (M/A/D/U).
- File viewer: Monaco, read-only, with the language picked from the file extension. Binary files and files over 1 MB show a placeholder instead of loading.
- History: a commit list (short hash, message, author, relative time), and a commit detail view with its changed files and diff.
- Web terminal per §5.4, rendering `CommandResult.lines` as ANSI, with history kept for the session.

**Done when** every Phase 1–3 command works in the web terminal with output identical to the CLI, and a commit made in either the web terminal or an external PowerShell window shows up in History immediately.

### Phase 6: Time travel, commit graph, diff inspector

**Build**
- **Time-travel scrubber.** A slider over the current branch's first-parent history, oldest to newest, plus a final **Working tree** stop. Use the arrow keys to step.
  - It is read-only: scrubbing changes what the explorer and viewer show and never touches files on disk. A **Check out this version** button runs `nexus checkout <hash>`, with the usual dirty check.
  - Keep the open file and its scroll position when that file exists at the selected commit. Otherwise show "this file doesn't exist at this point".
  - Performance: cache tree listings per commit and blobs by hash (they're immutable, so cache forever), and prefetch neighboring commits. Target under 100 ms per step on a 1,000-commit repo.
  - Why first-parent: with merges, history is a graph, but a slider needs a line. First-parent history is "this branch as it looked over time".
- **Commit graph.** SVG drawn from the Phase 3 layout, with branch labels at the tips, a HEAD marker, and curved merge edges. Virtualize rows for long histories. Clicking a node opens the commit detail, and hovering shows the message.
- **Diff inspector.** Unified and side-by-side modes rendered from our own hunks rather than Monaco's built-in diff, so the CLI and the UI always agree. It collapses unchanged regions, lists files with +/− counts, and can compare any two commits picked from the graph.

**Done when** scrubbing the demo repo feels instant, the SVG graph matches `nexus log --graph`, and every diff matches `nexus diff`.

### Phase 7: Developer extras

These three are independent. I'll request them one at a time, in any order.

**7a. Search**
- `nexus find <pattern> [--regex] [--at <commit>] [--path <glob>]`, plus a UI search panel on Ctrl+Shift+F. Results are grouped by file with line previews, and clicking one opens the file at that line.
- Start with a straight scan of the tree, skipping ignored and binary files. Since blobs are cached by hash, searching old commits is cheap. Add an index under `.nexus/cache/` only if a measurement shows the scan is too slow.

**7b. AI commit messages**
- `nexus commit` without `-m`, when AI is configured: build a prompt from the staged diff and get a Conventional Commits suggestion from an OpenAI-compatible `/v1/chat/completions` endpoint. I accept, edit, or reject it. It never commits on its own.
- Default endpoint: local Ollama at `http://localhost:11434/v1`. A remote endpoint requires explicit config, and its API key comes from an environment variable, never `config.json`.
- Never send files matching secret patterns (`.env*`, `*.pem`, `*.key`, `*secret*`). Truncate large diffs to a token budget and fall back to per-file stats.
- When AI isn't configured, fail with a message pointing at `-m`. In the UI, add a commit panel with a **Suggest message** button.

**7c. Playground**
- A scratchpad panel next to the explorer, saved under `.nexus/scratch/`, with a **Send selection to playground** action in the file viewer.
- JS and TS run in a Web Worker, with TS transpiled in the browser first. Capture console output, and kill the worker after a 5-second timeout. The worker gets no DOM access and no access to the app.
- Python runs on Pyodide in a worker, loaded only on first use.
- HTML, CSS, SVG, and Markdown files get a live preview in an `<iframe sandbox="allow-scripts">`, without `allow-same-origin`.

### Phase 8: Stretch goals (only if I ask)

- **Remotes:** `nexus serve` exposes a repo over HTTP, and `nexus clone`, `nexus push`, and `nexus pull <url>` exchange missing objects by walking from refs. Push is fast-forward only. This is what turns it into a self-hosted "GitHub" for my other machines.
- **Packaging:** a single Windows executable via Node SEA, which works because there are no native modules.
- **Desktop app:** wrap the web UI in Tauri v2.
- **Semantic search** with local embeddings.
- **Reflog and `nexus undo`.**
