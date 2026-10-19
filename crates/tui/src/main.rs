//! `kiln` binary: `kiln [--server URL] [--demo] [login|logout|version]`.

use clap::{Parser, Subcommand};
use kiln_api_client::Client;
use kiln_tui::app::App;
use kiln_tui::demo::DemoSource;
use kiln_tui::live::LiveSource;
use kiln_tui::session::{self, Session};
use kiln_tui::source::DataSource;
use kiln_tui::{DEFAULT_SERVER, runtime};

#[derive(Parser, Debug)]
#[command(name = "kiln", version, about = "Kiln terminal client — keyboard-first agentic development")]
struct Cli {
    /// Daemon URL, e.g. https://mac-mini.tailnet.ts.net (saved with the session).
    #[arg(long, env = "KILN_SERVER", global = true)]
    server: Option<String>,
    /// Run against an in-process simulated daemon (no server needed).
    #[arg(long, global = true)]
    demo: bool,
    /// With --demo: start in the first-run setup wizard.
    #[arg(long, requires = "demo")]
    first_run: bool,
    #[command(subcommand)]
    cmd: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Open the TUI on the sign-in screen, even if a session is saved.
    Login,
    /// Sign out: revoke the saved session token and remove it.
    Logout,
    /// Print the client version.
    Version,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if matches!(cli.cmd, Some(Command::Version)) {
        println!("kiln {}", kiln_types::VERSION);
        return Ok(());
    }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> anyhow::Result<()> {
    let path = session::default_path();
    let saved = Session::load(&path);
    let server = cli.server.clone().or(saved.server.clone()).unwrap_or_else(|| DEFAULT_SERVER.to_string());

    if matches!(cli.cmd, Some(Command::Logout)) {
        if cli.demo {
            println!("demo mode has no saved session");
            return Ok(());
        }
        let client = Client::new(server.clone());
        if let Some(t) = &saved.token {
            client.set_token(Some(t.clone()));
            if let Err(e) = client.logout().await {
                eprintln!("warning: could not revoke the token on {server}: {e}");
            }
        }
        let mut s = saved;
        s.token = None;
        s.save(&path)?;
        println!("signed out · session token removed from {}", session::display_path(&path));
        return Ok(());
    }

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(cli.demo);
    app.session_path = session::display_path(&path);
    let source: Box<dyn DataSource> = if cli.demo {
        app.login.email = "alex@company.dev".into();
        app.run_key = Some("FP-142".into());
        app.dsel = 2;
        app.creds.csel = 3;
        Box::new(DemoSource::start(tx, cli.first_run))
    } else {
        app.login.email = saved.email.clone().unwrap_or_default();
        if app.login.email.is_empty() {
            app.login.field = 0;
        }
        let client = Client::new(server.clone());
        if saved.server.as_deref() == Some(server.as_str())
            && let Some(t) = &saved.token
        {
            client.set_token(Some(t.clone()));
        }
        if saved.server.as_deref() != Some(server.as_str()) {
            // Remember the server even before signing in.
            let s = Session { server: Some(server.clone()), token: None, email: saved.email.clone() };
            let _ = s.save(&path);
        }
        let force_login = matches!(cli.cmd, Some(Command::Login));
        Box::new(LiveSource::start(client, tx, path, force_login))
    };
    runtime::run(app, source, rx).await
}
