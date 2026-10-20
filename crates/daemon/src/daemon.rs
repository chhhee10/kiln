//! Startup wiring and graceful shutdown.
//!
//! [`Daemon::start`] opens every component in dependency order (store →
//! vault → auth → bus → notifier → memory → gitops + PR provider → harness
//! registry → runtime → scheduler), binds the listener, seeds demo data if
//! asked, starts the scheduler (crash recovery first) and serves HTTP.
//! [`Daemon::shutdown`] stops accepting HTTP, closes WebSockets, publishes
//! `Event::Shutdown` and stops the scheduler. Worker containers keep running
//! so the next daemon re-attaches to them.

use crate::bind;
use crate::config::{Config, GithubMode, RuntimeKind};
use crate::state::{AppState, Inner, RateLimiter};
use anyhow::{Context, Result, anyhow, bail};
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderName, HeaderValue, Request, StatusCode, header};
use kiln_auth::{Auth, PasswordCost};
use kiln_events::{Bus, Event};
use kiln_gitops::{GitHubApp, GitHubToken, GitOps, LocalPrs, PrProvider};
use kiln_memory::Memory;
use kiln_notifier::Notifier;
use kiln_sandbox::{DockerRuntime, ProcessRuntime, Runtime};
use kiln_scheduler::{Capacity, RoleTimeouts, Scheduler, SchedulerConfig, SchedulerDeps};
use kiln_store::Store;
use kiln_types::api::API_PREFIX;
use kiln_vault::{KeyOptions, MasterKey, Vault};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

/// A hook that edits the scheduler config before it is created (tests).
pub type SchedulerTweak = Box<dyn FnOnce(&mut SchedulerConfig) + Send>;

/// How to start the daemon.
pub struct ServeOptions {
    pub config: Config,
    /// Development mode: 127.0.0.1 without Tailscale, mock harness, process
    /// runtime fallback, non-`Secure` cookies.
    pub dev: bool,
    /// Seed the demo company (dev only).
    pub demo_seed: bool,
    /// Inject a runtime (tests share one `ProcessRuntime` across restarts).
    pub runtime: Option<Arc<dyn Runtime>>,
    /// Inject a pre-bound listener (tests use an ephemeral port).
    pub listener: Option<TcpListener>,
    /// Password hashing cost (tests use `PasswordCost::INSECURE_FAST`).
    pub password_cost: Option<PasswordCost>,
    /// Scheduler overrides applied after the config mapping (tests).
    pub tweak_scheduler: Option<SchedulerTweak>,
    /// Print the demo credentials banner.
    pub print_banner: bool,
}

impl ServeOptions {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            dev: false,
            demo_seed: false,
            runtime: None,
            listener: None,
            password_cost: None,
            tweak_scheduler: None,
            print_banner: true,
        }
    }
}

/// A running daemon.
pub struct Daemon {
    pub addr: SocketAddr,
    pub state: AppState,
    server: JoinHandle<()>,
    memory: Weak<Memory>,
}

/// Create the data dir layout with private permissions.
pub fn prepare_data_dir(data: &Path) -> Result<()> {
    for sub in ["", "runs", "repos", "worktrees", "logs", "downloads", "web"] {
        let d = data.join(sub);
        std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(data)?.permissions();
        if perm.mode() & 0o077 != 0 {
            perm.set_mode(0o700);
            std::fs::set_permissions(data, perm)
                .context("restricting data dir permissions to 0700")?;
        }
    }
    std::fs::create_dir_all(data.join("downloads").join(kiln_types::VERSION))?;
    Ok(())
}

/// `kiln-runner` for the process runtime: config, next to this executable,
/// then `PATH`.
pub fn find_runner_bin(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = configured {
        return Some(p.to_path_buf());
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let p = dir.join("kiln-runner");
        if p.is_file() {
            return Some(p);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("kiln-runner"))
        .find(|p| p.is_file())
}

/// The PR provider for `[github].mode`.
pub async fn pr_provider(cfg: &Config) -> Result<Arc<dyn PrProvider>> {
    let gh = &cfg.github;
    Ok(match gh.mode {
        GithubMode::Local => Arc::new(LocalPrs::new(&cfg.data_dir)),
        GithubMode::Token => {
            let token = std::env::var(&gh.token_env)
                .ok()
                .filter(|t| !t.trim().is_empty());
            let mut p = match token {
                Some(t) => GitHubToken::new(t.trim()),
                None if gh.use_gh_cli => GitHubToken::from_gh_cli().await.map_err(|e| {
                    anyhow!(
                        "github.mode = \"token\": ${} is unset and `gh auth token` failed: {e}",
                        gh.token_env
                    )
                })?,
                None => bail!("github.mode = \"token\": ${} is unset", gh.token_env),
            };
            if let Some(b) = &gh.api_base {
                p = p.with_api_base(b);
            }
            Arc::new(p)
        }
        GithubMode::App => {
            let (Some(id), Some(key)) = (gh.app_id, gh.private_key_path.as_ref()) else {
                bail!("github.mode = \"app\" needs github.app_id and github.private_key_path");
            };
            let pem = std::fs::read(key).with_context(|| format!("reading {}", key.display()))?;
            let mut p = GitHubApp::new(id, &pem).map_err(|e| anyhow!("GitHub App key: {e}"))?;
            if let Some(b) = &gh.api_base {
                p = p.with_api_base(b);
            }
            Arc::new(p)
        }
    })
}

/// Connect the configured container runtime.
pub async fn connect_runtime(cfg: &Config, dev: bool) -> Result<(Arc<dyn Runtime>, bool)> {
    match cfg.runtime.kind {
        RuntimeKind::Process => Ok((Arc::new(ProcessRuntime::new()), true)),
        RuntimeKind::Docker => {
            let docker = match &cfg.runtime.docker_socket {
                Some(s) => DockerRuntime::with_socket(s).map_err(|e| anyhow!("{e}")),
                None => DockerRuntime::connect().await.map_err(|e| anyhow!("{e}")),
            };
            let docker = match docker {
                Ok(d) => match d.ping().await {
                    Ok(_) => Ok(d),
                    Err(e) => Err(anyhow!("{e}")),
                },
                Err(e) => Err(e),
            };
            match docker {
                Ok(d) if dev && !d.has_image(&cfg.runtime.worker_image).await => {
                    tracing::warn!(
                        image = %cfg.runtime.worker_image,
                        "worker image not built; --dev falls back to the process runtime (NO isolation). \
                         Build it with: docker build -f images/worker/Dockerfile -t {} .",
                        cfg.runtime.worker_image
                    );
                    Ok((Arc::new(ProcessRuntime::new()), true))
                }
                Ok(d) => Ok((Arc::new(d), false)),
                Err(e) if dev => {
                    tracing::warn!(error = %e, "container runtime unreachable; --dev falls back to the process runtime (NO isolation)");
                    Ok((Arc::new(ProcessRuntime::new()), true))
                }
                Err(e) => Err(e.context(
                    "container runtime unreachable (start Docker/OrbStack, or set runtime.kind)",
                )),
            }
        }
    }
}

fn scheduler_config(
    cfg: &Config,
    process: bool,
    public_url: &str,
    defaults: &std::collections::BTreeMap<kiln_types::WorkerRole, kiln_types::plan::WorkerChoice>,
) -> Result<SchedulerConfig> {
    let runner_bin = if process {
        let b = find_runner_bin(cfg.runtime.runner_bin.as_deref())
            .ok_or_else(|| anyhow!("the process runtime needs kiln-runner: set runtime.runner_bin or put it next to kilnd"))?;
        Some(b)
    } else {
        None
    };
    let c = &cfg.capacity;
    let t = &cfg.timeouts;
    Ok(SchedulerConfig {
        worker_image: cfg.runtime.worker_image.clone(),
        runner_bin,
        capacity: Capacity {
            ram_mb: c.ram_mb,
            reserve_mb: c.reserve_mb,
            max_containers: c.max_containers,
            max_tasks: c.max_tasks,
        },
        timeouts: RoleTimeouts {
            planner: t.planner,
            builder: t.builder,
            qa: t.qa,
            reviewer: t.reviewer,
            distiller: t.distiller,
        },
        llm_distill: cfg.memory.llm_distill,
        harness_options: cfg.harness_options(),
        prices: cfg.prices.clone(),
        default_workers: defaults.clone(),
        public_url: Some(public_url.to_string()),
        l0_retention_days: cfg.memory.retention_days,
        ..SchedulerConfig::new(cfg.data_dir.clone())
    })
}

/// Origins of the Tauri apps' WebViews (`tauri://localhost` on macOS/Linux,
/// `http(s)://tauri.localhost` on Android/Windows). Browsers served by the
/// daemon itself are same-origin and need no CORS.
pub const APP_ORIGINS: [&str; 3] = [
    "tauri://localhost",
    "http://tauri.localhost",
    "https://tauri.localhost",
];

/// CORS for the native apps only. The web client sends requests with
/// `credentials: include`, which requires `Allow-Credentials`; that is safe
/// because the origin list is exact (never a wildcard).
fn app_cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(
            APP_ORIGINS.iter().map(|o| HeaderValue::from_static(o)),
        ))
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::PUT,
            axum::http::Method::PATCH,
            axum::http::Method::DELETE,
        ])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE, header::ACCEPT])
        .allow_credentials(true)
        .max_age(Duration::from_secs(3600))
}

/// Middleware stack shared by every route.
fn layered(router: Router<AppState>, state: AppState) -> Router {
    let cfg = &state.cfg.server;
    let max_body = cfg.max_body_kb * 1024;
    let timeout = Duration::from_secs(cfg.request_timeout_secs);
    router
        .layer(DefaultBodyLimit::max(max_body))
        .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, timeout))
        .layer(SetResponseHeaderLayer::if_not_present(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")))
        .layer(SetResponseHeaderLayer::if_not_present(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY")))
        .layer(SetResponseHeaderLayer::if_not_present(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer")))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("cross-origin-opener-policy"),
            HeaderValue::from_static("same-origin"),
        ))
        // Spans carry method + path only: query strings may hold `?token=`.
        .layer(app_cors())
        .layer(TraceLayer::new_for_http().make_span_with(|req: &Request<_>| {
            tracing::info_span!("http", method = %req.method(), path = %req.uri().path())
        }))
        .with_state(state)
}

impl Daemon {
    /// Wire everything and start serving.
    pub async fn start(mut opts: ServeOptions) -> Result<Daemon> {
        let mut cfg = opts.config.clone();
        let dev = opts.dev;
        if opts.demo_seed && cfg.github.mode != GithubMode::Local {
            tracing::info!("--demo-seed uses local PRs");
            cfg.github.mode = GithubMode::Local;
        }
        if dev
            && cfg.github.mode == GithubMode::App
            && (cfg.github.app_id.is_none() || cfg.github.private_key_path.is_none())
        {
            tracing::warn!(
                "--dev without GitHub App settings: PRs are recorded locally (github.mode = \"local\")"
            );
            cfg.github.mode = GithubMode::Local;
        }
        if opts.demo_seed && !dev {
            bail!("--demo-seed is only allowed with --dev");
        }
        cfg.validate(dev).map_err(|e| anyhow!("{e}"))?;
        cfg.check_socket_path().map_err(|e| anyhow!("{e}"))?;
        let data = cfg.data_dir.clone();
        prepare_data_dir(&data)?;

        // Listener first: the public URL feeds the scheduler (PR links).
        let listener = match opts.listener.take() {
            Some(l) => l,
            None => {
                let addr = bind::resolve_bind(&cfg.server.bind, cfg.server.port, dev).await?;
                TcpListener::bind(addr)
                    .await
                    .with_context(|| format!("binding {addr}"))?
            }
        };
        let addr = listener.local_addr()?;
        bind::check_addr(addr.ip(), dev)?;
        let public_url = cfg
            .server
            .public_url
            .clone()
            .unwrap_or_else(|| format!("http://{addr}"));

        let prs = pr_provider(&cfg).await?;
        let store = Store::open(&data.join("kiln.db"))
            .await
            .context("opening the control store")?;
        let keychain = cfg.vault.keychain && KeyOptions::default().keychain;
        let (key, source) = MasterKey::load_or_create_with(
            &data,
            KeyOptions {
                env: true,
                keychain,
            },
        )
        .map_err(|e| anyhow!("loading the vault master key: {e}"))?;
        tracing::info!(source = ?source, "vault master key loaded");
        let vault = Arc::new(Vault::new(key, store.clone()));
        vault.verify_key().await.map_err(|e| anyhow!("{e}"))?;
        let auth = Arc::new(match opts.password_cost {
            Some(c) => Auth::with_cost(store.clone(), c),
            None => Auth::new(store.clone()),
        });
        let bus = Bus::new(8192);
        let notifier = Arc::new(Notifier::new(store.clone(), bus.clone()));
        let memory = Arc::new(
            Memory::open(store.pool().clone(), &data, cfg.memory.tuning.clone())
                .await
                .map_err(|e| anyhow!("opening memory: {e}"))?,
        );
        let gitops = Arc::new(GitOps::new(data.clone()));
        let harnesses = Arc::new(kiln_harness::registry());
        let (runtime, process) = match opts.runtime.take() {
            Some(r) => (r, true),
            None => connect_runtime(&cfg, dev).await?,
        };
        let catalog = cfg.harness_catalog(dev).map_err(|e| anyhow!("{e}"))?;
        let default_workers = cfg.defaults.resolve().map_err(|e| anyhow!("{e}"))?;
        let mut sched_cfg = scheduler_config(&cfg, process, &public_url, &default_workers)?;
        if let Some(f) = opts.tweak_scheduler.take() {
            f(&mut sched_cfg);
        }
        let scheduler = Scheduler::new(SchedulerDeps {
            store: store.clone(),
            bus: bus.clone(),
            runtime,
            gitops: gitops.clone(),
            prs,
            vault: vault.clone(),
            memory: memory.clone(),
            notifier: notifier.clone(),
            harnesses,
            config: sched_cfg,
        })
        .await
        .map_err(|e| anyhow!("creating the scheduler: {e}"))?;

        let hostname = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "kiln".into());
        let tailnet_host = match cfg.server.public_url.as_deref().and_then(host_of) {
            Some(h) if h.ends_with(".ts.net") => Some(h),
            _ => bind::tailscale_dns_name().await,
        };
        let web_dir = crate::web::find_web_dir(&cfg.effective_web_dir(), dev);
        if web_dir.is_none() {
            tracing::info!(dir = %cfg.effective_web_dir().display(), "no web build found; /app shows install instructions");
        }
        let shutdown = CancellationToken::new();
        let state = AppState(Arc::new(Inner {
            limiter: RateLimiter::new(cfg.server.auth_rate_per_minute),
            cfg: cfg.clone(),
            dev,
            store: store.clone(),
            auth: auth.clone(),
            vault,
            bus: bus.clone(),
            notifier,
            memory: memory.clone(),
            scheduler: scheduler.clone(),
            gitops,
            catalog,
            default_workers,
            public_url: public_url.clone(),
            hostname,
            tailnet_host,
            web_dir,
            shutdown: shutdown.clone(),
            started: Instant::now(),
        }));

        if opts.demo_seed {
            let settings = crate::seed::mock_settings(state.default_settings());
            let seeded = crate::seed::seed(&store, &auth, &data, settings).await?;
            if opts.print_banner {
                println!("{}", crate::seed::banner(&public_url, &seeded));
            }
        }

        scheduler
            .start()
            .await
            .map_err(|e| anyhow!("starting the scheduler: {e}"))?;
        if opts.demo_seed {
            for p in store.list_projects().await? {
                if let Err(e) = scheduler.refresh_project(p.id).await {
                    tracing::warn!(project = %p.key, error = %e, "seed project refresh failed");
                }
            }
        }

        let app = Router::new()
            .nest(API_PREFIX, crate::routes::api_router())
            .merge(crate::web::routes(&state));
        let app = layered(app, state.clone());
        let token = shutdown.clone();
        let server = tokio::spawn(async move {
            let svc = app.into_make_service_with_connect_info::<SocketAddr>();
            if let Err(e) = axum::serve(listener, svc)
                .with_graceful_shutdown(token.cancelled_owned())
                .await
            {
                tracing::error!(error = %e, "HTTP server failed");
            }
        });
        tracing::info!(%addr, url = %public_url, dev, "kilnd listening");
        Ok(Daemon {
            addr,
            state,
            server,
            memory: Arc::downgrade(&memory),
        })
    }

    /// `http://<addr>`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Stop accepting HTTP, close sockets, stop the scheduler. Containers
    /// keep running for re-attach. Waits until the memory indexes are
    /// released so a new daemon can open the same data dir.
    pub async fn shutdown(self) {
        tracing::info!("shutting down");
        let st = self.state.clone();
        st.shutdown.cancel();
        st.bus.publish(Event::Shutdown);
        st.scheduler.shutdown().await;
        if tokio::time::timeout(Duration::from_secs(10), self.server)
            .await
            .is_err()
        {
            tracing::warn!("HTTP server did not stop within 10 s");
        }
        let store = st.store.clone();
        drop(st);
        drop(self.state);
        let deadline = Instant::now() + Duration::from_secs(15);
        while self.memory.upgrade().is_some() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if self.memory.upgrade().is_some() {
            tracing::warn!("memory still referenced after shutdown");
        }
        store.close().await;
        tracing::info!("stopped");
    }
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let host = rest.split(['/', ':']).next()?;
    (!host.is_empty()).then(|| host.to_string())
}

/// Wait for SIGINT or SIGTERM.
pub async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// `kilnd serve`: start, run until a signal, shut down.
pub async fn serve(opts: ServeOptions) -> Result<()> {
    let d = Daemon::start(opts).await?;
    wait_for_signal().await;
    d.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_parsing() {
        assert_eq!(
            host_of("https://mini.tail-abc.ts.net/x").as_deref(),
            Some("mini.tail-abc.ts.net")
        );
        assert_eq!(
            host_of("http://100.64.0.1:7420").as_deref(),
            Some("100.64.0.1")
        );
        assert_eq!(host_of("nope"), None);
    }

    #[test]
    fn data_dir_is_private() {
        let d = tempfile::tempdir().unwrap();
        let data = d.path().join("data");
        prepare_data_dir(&data).unwrap();
        assert!(data.join("runs").is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&data).unwrap().permissions().mode() & 0o077,
                0
            );
        }
    }
}
