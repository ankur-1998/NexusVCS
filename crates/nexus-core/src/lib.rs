//! The NexusVCS engine and command layer.
//!
//! Every `nexus` command is implemented once, in [`commands`]. The CLI, the
//! TUI, and the web UI only render what core returns (spec §5.2), so core never
//! prints and never prompts. The deny below makes any stray `println!` or
//! `eprintln!` a clippy error, which fails `cargo xtask ci`.

#![deny(clippy::print_stdout, clippy::print_stderr)]

pub mod cli;
pub mod commands;
pub mod config;
pub mod content;
pub mod diff;
pub mod error;
pub mod fsutil;
pub mod graph;
pub mod hash;
pub mod index;
pub mod net;
pub mod object;
pub mod odb;
pub mod oplog;
pub mod path;
pub mod platform;
pub mod refs;
pub mod repo;
pub mod rev;
pub mod stage;
pub mod time;
pub mod tree;
pub mod walk;
pub mod worktree;

pub use error::{Error, Result};
pub use hash::ObjectId;
pub use repo::Repo;
