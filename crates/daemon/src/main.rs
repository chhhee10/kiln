//! `kilnd` command line.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use kiln_daemon::ServeOptions;
use kiln_daemon::config::Config;
use kiln_daemon::service::{self, ServiceKind};
use std::io::Read;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "kilnd",
    version,
    about = "Kiln daemon: multi-tenant agentic development platform host"
)]
struct Cli {
    /// Path of kiln.toml (default: $KILN_CONFIG, then <data_dir>/kiln.toml).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon.
    Serve {
        /// Development mode: binds 127.0.0.1 without Tailscale, enables the
        /// mock harness, falls back to the process runtime without Docker.
        #[arg(long)]
        dev: bool,
        /// Seed the demo company (admin@company.dev / kiln-demo-password). Needs --dev.
        #[arg(long, requires = "dev")]
        demo_seed: bool,
    },
    /// Install kilnd as a launchd agent (macOS) or systemd user service (Linux).
    Install {
        /// Print the service file instead of writing it.
        #[arg(long)]
        print: bool,
        /// Overwrite an existing service file.
        #[arg(long)]
        force: bool,
        /// Force a service manager (default: launchd on macOS, systemd elsewhere).
        #[arg(long, value_parser = ["launchd", "systemd"])]
        kind: Option<String>,
    },
    /// Check the host: runtime, keychain, Tailscale, disk, image, GitHub auth, data dir.
    Doctor {
        /// Evaluate as in --dev (Tailscale optional, mock harness allowed).
        #[arg(long)]
        dev: bool,
    },
    /// Print the version.
    Version,
    /// Local administration (run on the host, as the daemon's user).
    Admin {
        #[command(subcommand)]
        cmd: AdminCmd,
    },
}

#[derive(Subcommand)]
enum AdminCmd {
    /// Set a new password for a user and sign out all their sessions.
    ResetPassword {
        email: String,
        /// Read the new password from stdin instead of generating one.
        #[arg(long)]
        stdin: bool,
    },
}

fn init_tracing(cfg: &Config) {
    let filter =
        EnvFilter::try_from_env("RUST_LOG").unwrap_or_else(|_| EnvFilter::new(&cfg.log.level));
    let b = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    if cfg.log.format == "json" {
        b.json().init();
    } else {
        b.init();
    }
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("kilnd: cannot start the async runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("kilnd: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<std::process::ExitCode> {
    if let Cmd::Version = cli.cmd {
        println!(
            "kilnd {} ({})",
            kiln_types::VERSION,
            kiln_types::PRODUCT_NAME
        );
        return Ok(std::process::ExitCode::SUCCESS);
    }
    let (cfg, path) = Config::load(cli.config.as_deref()).context("loading configuration")?;
    match cli.cmd {
        Cmd::Version => unreachable!(),
        Cmd::Serve { dev, demo_seed } => {
            init_tracing(&cfg);
            match &path {
                Some(p) => {
                    tracing::info!(config = %p.display(), data_dir = %cfg.data_dir.display(), "configuration loaded")
                }
                None => {
                    tracing::info!(data_dir = %cfg.data_dir.display(), "no kiln.toml found; using defaults")
                }
            }
            let mut opts = ServeOptions::new(cfg);
            opts.dev = dev;
            opts.demo_seed = demo_seed;
            kiln_daemon::daemon::serve(opts).await?;
            Ok(std::process::ExitCode::SUCCESS)
        }
        Cmd::Install { print, force, kind } => {
            let kind = match kind.as_deref() {
                Some("launchd") => ServiceKind::Launchd,
                Some("systemd") => ServiceKind::Systemd,
                _ => ServiceKind::for_host(),
            };
            let out = service::install(&cfg, path.as_deref(), kind, print, force)?;
            print!("{out}");
            Ok(std::process::ExitCode::SUCCESS)
        }
        Cmd::Doctor { dev } => {
            let checks = service::doctor(&cfg, dev).await;
            println!(
                "kilnd {} doctor — data dir {}",
                kiln_types::VERSION,
                cfg.data_dir.display()
            );
            for c in &checks {
                println!("{}", c.line());
            }
            let failed = checks.iter().filter(|c| c.ok == Some(false)).count();
            if failed > 0 {
                println!("\n{failed} check(s) failed");
                Ok(std::process::ExitCode::FAILURE)
            } else {
                println!("\nall checks passed");
                Ok(std::process::ExitCode::SUCCESS)
            }
        }
        Cmd::Admin {
            cmd: AdminCmd::ResetPassword { email, stdin },
        } => {
            let pw = if stdin {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s)?;
                Some(s.trim_end_matches(['\r', '\n']).to_string())
            } else {
                None
            };
            let generated = pw.is_none();
            let pw = service::reset_password(&cfg, &email, pw).await?;
            if generated {
                println!("New password for {email}: {pw}");
            } else {
                println!("Password updated for {email}.");
            }
            println!("All of their sessions were signed out.");
            Ok(std::process::ExitCode::SUCCESS)
        }
    }
}
