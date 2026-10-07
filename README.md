# NexusVCS
<<<<<<< HEAD
Personal Version Control System
=======

A personal, local-first version control system written from scratch. It ships as one native binary with a CLI, a terminal UI, and a web dashboard. See [Draft 6 for VCS.md](Draft%206%20for%20VCS.md) for the full spec and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for what's built so far.

**Status:** Phase 0 (scaffold). `nexus --version` is the only command so far.

## Prerequisites

| Tool | Needed for |
| --- | --- |
| [Rust via rustup](https://rustup.rs) | Everything. The pinned toolchain (`rust-toolchain.toml`) installs itself on first use. |
| Node.js 22.12+ or 24+, and pnpm 10 | Building the web UI. |
| A C toolchain | Linking on every OS, and the C dependencies in later phases. |

C toolchain by OS:

- **Windows:** Visual Studio Build Tools with "Desktop development with C++", plus a Windows 10 or 11 SDK.
- **macOS:** `xcode-select --install`.
- **Linux:** `gcc` or `clang` from your distribution.

## Commands

All of these run from the repository root, in any shell, on any OS.

```text
cargo xtask ci           # every check CI runs
cargo xtask build        # the web UI, then the release binary
cargo run -- --version   # run the nexus binary from source
cargo xtask help         # list all tasks
```
>>>>>>> 45b0444 (Initial commit)
