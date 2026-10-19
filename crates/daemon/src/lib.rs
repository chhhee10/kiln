//! `kilnd` — the Kiln host daemon.
//!
//! Wires every crate together (store, auth, vault, events, notifier, memory,
//! gitops, sandbox, harness, scheduler) and exposes the HTTP/WebSocket API of
//! `kiln_types::api`, the web app at `/app`, client builds at `/download`
//! and `/healthz`. See `docs/SCHEDULER.md` "Daemon (kilnd)" for the spec and
//! `config/kiln.example.toml` for configuration.
//!
//! The library form exists so integration tests can run the full daemon
//! in-process ([`daemon::Daemon::start`]).

pub mod bind;
pub mod config;
pub mod daemon;
pub mod error;
pub mod extract;
pub mod routes;
mod sched_err;
pub mod seed;
pub mod service;
pub mod session;
pub mod state;
pub mod views;
pub mod web;
pub mod ws;

pub use config::Config;
pub use daemon::{Daemon, ServeOptions};
