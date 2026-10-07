# NexusVCS architecture

This is a working summary of the build spec, [Draft 6 for VCS.md](../Draft%206%20for%20VCS.md), which remains the source of truth. When the two disagree, the spec wins and this file gets fixed. Each phase updates the status table and adds what it built.

## Status

| Area | State | Phase |
|---|---|---|
| Workspace, crates, `cargo xtask ci` / `build`, CI on 3 OSes | Built | 0 |
| Network rule (`clippy.toml` + `nexus_core::net`) | Enforced for std; tokio and ureq rules activate when those crates arrive | 0 |
| Web skeleton (Svelte 5 + Vite + TypeScript) | Placeholder page | 0 |
| Object store (zstd, chunked large files), index, refs, commits | Built | 1 |
| Transactions, op log, crash recovery, lock | Built (undo itself is Phase 4) | 1 |
| `init`, `add`, `commit`, `log`, `config`, `hash-object`, `cat-file -p`, `debug index` | Built | 1 |
| `status`, `diff`, `show`, `checkout`, `restore`, `rm`, `export` | Built | 2 |
| Branches, tags, abbreviated IDs, commit-graph helpers | Built | 2 |
| `cargo xtask bench` (the `status`, `add`, `commit`, `log`, and large-file rows of §6) | Built | 2 |
| Everything else | Not started | 3–15 |

## Code layout

```text
crates/nexus-core/    Engine and command layer. Synchronous, never prints (enforced by a lint).
  src/cli.rs          clap definitions shared by the CLI and the web terminal.
  src/commands/       One module per command; `run`/`execute` return a CommandResult.
  src/hash.rs         ObjectId (SHA-256).
  src/object.rs       Canonical object formats: blob, chunked, tree, commit, op. Strict decoders.
  src/odb.rs          The object store: encoding byte, zstd, size-limited decoding, hash checks.
  src/content.rs      File content as objects: blob below 8 MiB, else FastCDC chunks (streamed).
  src/index.rs        The binary index, racy-timestamp smudging.
  src/tree.rs         Index to trees (iterative, writes only new trees) and back.
  src/refs.rs         HEAD and refs.
  src/oplog.rs        Lock, Transaction, op recording, crash recovery.
  src/walk.rs         Parallel walk with .nexusignore; exact on-disk lookup of stored paths.
  src/stage.rs        `nexus add`: staging, deletions, racy entries, portability warnings.
  src/worktree.rs     Working tree against index and trees: status, checkout planning, safe writes.
  src/diff.rs         Line diffs (imara-diff), hunks, binary and large-file summaries.
  src/graph.rs        The commit graph: reading commits, walks, ancestry, children.
  src/rev.rs          Naming objects: HEAD, branches, tags, abbreviated IDs.
  src/path.rs         RepoPath (NFC, `/`-separated), Windows-name and case-collision checks.
  src/repo.rs         Discovery, layout, pathspec to RepoPath.
  src/config.rs       config.toml read/write (toml_edit), identity.
  src/fsutil.rs       Atomic writes with Windows retry.
  src/platform.rs     Everything OS-specific.
  src/time.rs         Timestamps with UTC offsets.
  src/net.rs          The only module allowed to use network sockets or DNS.
crates/nexus-tui/     `nexus tui` (Phase 6). Behind the `tui` feature.
crates/nexus-server/  `nexus ui` server and embedded UI (Phase 7). Behind the `ui` feature.
crates/nexus/         The `nexus` binary: parses with core's definitions, renders output.
xtask/                Automation in Rust: `cargo xtask ci`, `build`, `bench`.
web/                  Svelte app, built with Vite and managed with pnpm.
```

`cargo build --no-default-features` builds a CLI-only binary without the TUI and server crates. CI runs clippy on that build too, so it gets the same lints and network rule.

## What Phase 1 built

**Transactions.** A command that changes state calls `Transaction::begin`, which:
- takes `.nexus/lock` (delete-on-close on Windows; a Ctrl+C handler in the CLI removes it elsewhere)
- loads the index once
- checks that HEAD, refs, the index tree, and the stash still match the newest op, recording a `recovered` op first if they don't

`finish` writes the index, refs, and HEAD atomically, then records one op and rewrites `OPLOG`, unless the state is unchanged. The objects an op refers to are always stored: an unchanged index reuses the newest op's tree, and a changed one writes only the trees that the previous index didn't have.

**Staging.** `nexus add` walks the named scopes in parallel with `.nexusignore` at every level, and adds tracked files the walk skipped (ignored, still tracked). It then decides what to rehash:
- An entry is reused only if its size and modification time match and it's older than the index file.
- Racily clean entries are smudged so they get rehashed next time.

A tracked path the walk didn't return counts as present only if every name matches exactly (after NFC) through real directories. So a case-only rename replaces the old name, and nothing is read through a symlink or junction.

Small files are hashed in parallel within a 16 MiB memory budget. Large files are chunked on one thread while the previous 16 MiB batch is compressed and stored on others.

**Reading objects.** `ObjectStore::read` checks the declared size against a per-type limit before decompressing, then verifies the SHA-256.

## What Phase 2 built

**Comparing.** `worktree::Checker` decides whether a tracked file changed. It uses the size-and-time fast path when the entry is older than the index file, treats a different size as a change, and otherwise hashes in parallel without storing. `status` walks the tree once and checks every index entry. It finds staged changes by comparing HEAD's tree with trees built from the index, by ID, so it reads only the subtrees that differ. `diff` builds two sides (stored versions, or working files) and diffs each path that differs: text through imara-diff, with Git's indent heuristic deciding where a slidable change goes, and binary and chunked files as one-line summaries.

**Switching.** `checkout` plans first, and a refused plan changes nothing. The plan follows Git's two-way rules: a path HEAD and the target agree on is left alone, staged or not. It refuses anything that would overwrite uncommitted work, or an untracked or ignored file. That includes one reached under another letter case or a Windows 8.3 short name, and anything inside `.nexus`. Every overwritten or deleted file is recorded in the op's `saved` tree. Applying the plan deletes first, then writes each file through a temp file and a rename, never through a symlink or junction. If applying fails partway, an op is still recorded, so what was saved stays reachable. `restore` and `rm` reuse the same pieces.

**Naming.** `rev` resolves `HEAD`, full IDs, full ref names, branches, tags, and ID prefixes of 4 or more characters, and lists the candidates when a prefix is ambiguous. Ref lookups match the on-disk spelling exactly. Ref names must be valid file names on every OS. `graph` walks history, tests ancestry for `branch -d`, and builds the children map Phase 3's graph layout needs.

**Output.** A diff line carries its exact bytes as well as safe display text. The CLI writes the bytes when stdout isn't a terminal, so saved patches are exact. Paths are quoted the way Git quotes them.

**Benchmarks.** `cargo xtask bench` builds the release binary and generates a 10,000-file tree with 1,000 commits and a 1 GiB file, cached in the temp directory. It then times real `nexus` processes against the §6 budgets, with Git as the reference.

## One engine, four interfaces

```text
                         nexus-core  (engine + command layer, synchronous)
                  /            |                 \
     nexus (CLI)          nexus-tui          nexus-server  <-- HTTP / WebSocket -->  web/
     any terminal         any terminal       (axum)                                 any browser
```

### Command layer (spec §5.2)

- Each command is implemented once in core as `run(argv, ctx) -> CommandResult`, where the result is `{ exit_code, lines, data }`. `lines` are styled text, and `data` is a typed payload for the TUI and web UI.
- The CLI renders `lines` with ANSI colors, and the web terminal renders the same `lines` in xterm.js. The TUI and dashboard read `data`.
- Core never prints and never prompts. Prompts live in the CLI binary, the TUI, and the web UI.
- Every change to refs, `HEAD`, the index, the stash, or working-tree files goes through one `Transaction`. It takes the lock, saves the files it will overwrite or delete, applies the change atomically, and records one operation. This is what makes `nexus undo` complete.
- The server calls core with `spawn_blocking`, and core parallelizes with `rayon`. The file watcher lives in core and is shared by the TUI, the server, and idle snapshots.

### Server (spec §5.3)

- It binds to `127.0.0.1` only. A random startup token is exchanged for an `HttpOnly`, `SameSite=Strict` cookie, every request must be authenticated, and requests with a non-localhost `Host` or `Origin` are rejected (DNS-rebinding defense).
- Reads are JSON endpoints, and blobs support HTTP Range requests.
- Writes go only through the command layer, over the WebSocket.
- API types are defined in Rust and exported to TypeScript with `ts-rs`.
- A debounced watcher pushes change events to the UI. On Linux, when the inotify watch limit is hit, it falls back to polling.

### Web terminal (spec §5.4)

The web terminal is a restricted virtual shell confined to the repo root: `ls`, `cd`, `pwd`, `cat`, `tree`, `clear`, `help`, `history`, and `nexus ...`. It is never a real system shell.

## Repository format (spec §4, the contract)

```text
<repo>/.nexusignore, nexus-ci.toml, nexus-secrets.toml
<repo>/.nexus/
  HEAD  OPLOG  config.toml  index  lock  MERGE_STATE  REWRITE_STATE  snapshots  stash
  refs/heads/*  refs/tags/*  refs/meta/issues  refs/meta/reviews
  objects/ab/cdef...   ci/   cache/   scratch/
```

**Objects**

- Canonical bytes are `<type> <len>\0<body>`, and the hash is the SHA-256 hex of those bytes.
- On disk, an object is one encoding byte (`0x01` zstd, `0x00` raw when zstd saves under 5%) followed by the payload, at `objects/<2>/<62>`.
- Types:
  - `blob`: the file's bytes, verbatim.
  - `chunked`: files of 8 MiB or more, split by FastCDC 2020 (min 256 KiB, average 1 MiB, max 4 MiB) into blob chunks. These parameters are part of the contract.
  - `tree`: lines of `<file|exec|tree> <hash> <name>`, sorted by bytes.
  - `commit`: `tree`, `parent`, and `author` lines, then the message.
  - `op`: one recorded operation.
- All I/O streams, so memory use stays flat for any file size.

**Index**

- A binary file: magic `NXIX`, version, and entry count, then sorted entries (path, kind, hash, size, `mtime_ns`), then a SHA-256 trailer.
- `nexus debug index` prints it as text.

**Operation log**

- Each op object records the state after the operation: its parent op, time, command, `HEAD`, every ref, the index as a tree, and the stash. It can also record `saved`, the files the operation overwrote or deleted, and `undo-of`.
- `OPLOG` points at the latest op. `nexus gc` prunes ops older than `[oplog] keep_days` (default 30).

**Local-history snapshots**

- An append-only text log of `<time> <tree> <trigger>`, thinned over time.
- Local only: snapshots are never synced and never appear in history.

**Stash:** one line per entry: id, time, base commit, tree, and message.

**Issues and reviews**

- Stored as per-item operation logs on `refs/meta/*`, one immutable TOML file per operation.
- Diverged copies merge by union.
- Current state is computed by replaying operations ordered by `(lamport, op hash)`.

**Invariants**

- Hashing is deterministic on every OS, objects are immutable and verified on read, and writes are atomic (temp file plus rename, retried on Windows sharing violations).
- One exclusive lock guards all mutations.
- Paths are stored with `/` and NFC-normalized, and paths escaping the repo are rejected. Tracked paths that differ only by case, or that are invalid on Windows, produce warnings.
- Unix executable bits are tracked. `.nexus/` is always ignored, and no tree path can write into it (in any letter case, or by its Windows 8.3 name).
- Writes to the working tree never go through a symlink or junction, and never overwrite an untracked or ignored file.

## Privacy and network policy (spec §5.5)

- Outbound connections are allowed only for features the user configures or triggers: the AI endpoint, the playground's Python runtime download, CI container pulls, and future remotes. There is no telemetry, ever. `[network] offline = true` blocks all of them.
- **Enforcement today:** `clippy.toml` forbids socket and DNS APIs outside `nexus_core::net`: std `TcpStream::connect`/`connect_timeout`, `TcpListener::bind`, `UdpSocket::bind`, and `to_socket_addrs`, plus the tokio and ureq equivalents, which take effect when those crates are added. `cargo xtask ci` runs clippy with `-D warnings` on the all-features workspace and the CLI-only build, and fails if any rule path doesn't resolve.
- Listeners are covered too: the Phase 7 server gets its listener from `nexus_core::net`, which only binds `127.0.0.1`. Shelling out to network tools isn't caught by the lint and is left to code review.
- The dashboard will ship a strict Content-Security-Policy and load no external resources.
- The op log, snapshots, stash, and CI records never leave the machine.

## Tooling

- **Toolchain:** Rust 1.99.0, pinned in `rust-toolchain.toml`. Edition 2024.
- **Lints:** `clippy::all` and `clippy::pedantic` as warnings, plus `unwrap_used`, `dbg_macro`, and `todo`. CI turns every warning into an error. `unsafe_code` is denied.
- **`cargo xtask bench`:** see "What Phase 2 built". Results also go to `results.json` in the benchmark directory.
- **`cargo xtask ci`:** rustfmt, clippy, `cargo test`, clippy on the CLI-only build, `pnpm install --frozen-lockfile`, svelte-check, Prettier, Vitest, the web build, and the initial-JS budget (150 KB gzipped). Every cargo step runs `--locked`, including the alias that starts xtask.
- **CI:** GitHub Actions runs `cargo xtask ci` on Ubuntu, macOS, and Windows.
- **Line endings:** LF everywhere (`.gitattributes`, `.editorconfig`, rustfmt, Prettier).
