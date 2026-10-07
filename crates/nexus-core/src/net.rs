//! The only module allowed to use network sockets or DNS (spec §5.5).
//!
//! `clippy.toml` forbids the std, tokio, and ureq connect, bind, and lookup
//! APIs everywhere else, so `cargo xtask ci` fails if networking code appears
//! outside this module. That includes the local server's listener, which must
//! come from here so it can only bind `127.0.0.1` (spec §5.3).
//!
//! Anything added here must honor `[network] offline = true` and connect only to
//! endpoints the user configured or explicitly triggered. No telemetry, ever.
//!
//! Nothing needs the network yet. The first users are the local server in
//! Phase 7 and the AI client in Phase 15.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
