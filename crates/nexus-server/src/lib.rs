//! The `nexus ui` local web server and embedded dashboard, built in Phase 7.
//!
//! It is a separate crate so the CLI-only build
//! (`cargo build --no-default-features`) can leave out the server and its
//! async runtime entirely.
