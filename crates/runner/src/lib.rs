//! `kiln-runner`: the in-container driver for one Kiln worker run
//! (ARCHITECTURE.md "runner").
//!
//! Subcommands:
//! * `run` (default) — connect to the daemon socket, drive the configured
//!   harness, spool and stream events, collect outputs, report `Complete`.
//! * `mcp` — stdio MCP server exposing `search_memory` / `propose_memory`,
//!   proxied to the daemon over a second socket connection.
//! * `mock-harness` — deterministic fake agent used by tests and demos.
//!
//! All container paths can be overridden by env (`KILN_RUN_DIR`,
//! `KILN_WORKSPACE`, `KILN_SOCKET`) so the runner also works under the
//! sandbox crate's `ProcessRuntime`.

pub mod collect;
pub mod emit;
pub mod env;
pub mod lines;
pub mod mcp;
pub mod mock;
pub mod run;
pub mod spool;
pub mod uplink;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "kiln-runner", version, about = "Kiln in-container runner")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Subcommand, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    /// Drive one run (default).
    Run,
    /// Serve the memory MCP server on stdio.
    Mcp,
    /// Deterministic mock agent (reads RunConfig JSON on stdin).
    MockHarness,
}

fn init_tracing(default: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_env("KILN_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
}

/// Binary entry point; returns the process exit code.
pub fn main_with(cli: Cli) -> i32 {
    match cli.command.unwrap_or(Cmd::Run) {
        Cmd::MockHarness => mock::main(),
        Cmd::Mcp => {
            init_tracing("warn");
            match runtime() {
                Ok(rt) => rt.block_on(mcp::serve()),
                Err(e) => {
                    eprintln!("kiln-runner: {e}");
                    1
                }
            }
        }
        Cmd::Run => {
            init_tracing("info");
            let env = match env::RunnerEnv::from_env() {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!("{e}");
                    return run::EXIT_STARTUP;
                }
            };
            match runtime() {
                Ok(rt) => rt.block_on(run::run(env)),
                Err(e) => {
                    tracing::error!(error = %e, "cannot start tokio");
                    run::EXIT_STARTUP
                }
            }
        }
    }
}
