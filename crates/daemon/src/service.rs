//! `kilnd install`, `kilnd doctor` and `kilnd admin reset-password`.

use crate::config::{Config, GithubMode, RuntimeKind};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// launchd label / systemd unit name.
pub const LAUNCHD_LABEL: &str = "dev.kiln.kilnd";
pub const SYSTEMD_UNIT: &str = "kilnd.service";

/// Service manager to target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceKind {
    Launchd,
    Systemd,
}

impl ServiceKind {
    pub fn for_host() -> Self {
        if cfg!(target_os = "macos") {
            ServiceKind::Launchd
        } else {
            ServiceKind::Systemd
        }
    }
}

fn xml(s: &str) -> String {
    crate::web::esc(s)
}

/// PATH for the service: Homebrew, OrbStack/Docker and system dirs.
const SERVICE_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// launchd plist: RunAtLoad + KeepAlive, logs under `<data>/logs/`.
pub fn launchd_plist(exe: &Path, config: Option<&Path>, data_dir: &Path) -> String {
    let mut args = vec![exe.display().to_string(), "serve".to_string()];
    if let Some(c) = config {
        args.push("--config".into());
        args.push(c.display().to_string());
    }
    let args_xml: String = args
        .iter()
        .map(|a| format!("\n      <string>{}</string>", xml(a)))
        .collect();
    let logs = data_dir.join("logs");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
  <dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>{args_xml}
    </array>
    <key>EnvironmentVariables</key>
    <dict>
      <key>KILN_DATA_DIR</key>
      <string>{data}</string>
      <key>PATH</key>
      <string>{path}</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>ProcessType</key>
    <string>Standard</string>
    <key>StandardOutPath</key>
    <string>{out}</string>
    <key>StandardErrorPath</key>
    <string>{err}</string>
  </dict>
</plist>
"#,
        label = LAUNCHD_LABEL,
        data = xml(&data_dir.display().to_string()),
        path = SERVICE_PATH,
        out = xml(&logs.join("kilnd.log").display().to_string()),
        err = xml(&logs.join("kilnd.err.log").display().to_string()),
    )
}

fn sd_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-:=".contains(c))
    {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// systemd `--user` unit with restart-always.
pub fn systemd_unit(exe: &Path, config: Option<&Path>, data_dir: &Path) -> String {
    let mut exec = format!("{} serve", sd_quote(&exe.display().to_string()));
    if let Some(c) = config {
        exec.push_str(&format!(" --config {}", sd_quote(&c.display().to_string())));
    }
    format!(
        "[Unit]\nDescription=Kiln daemon (kilnd)\nWants=network-online.target\nAfter=network-online.target tailscaled.service docker.service\n\n\
         [Service]\nType=simple\nExecStart={exec}\nEnvironment={env}\nEnvironment=PATH={path}\nRestart=always\nRestartSec=5\n\
         TimeoutStopSec=30\nKillMode=process\nNoNewPrivileges=true\nPrivateTmp=true\n\n[Install]\nWantedBy=default.target\n",
        env = sd_quote(&format!("KILN_DATA_DIR={}", data_dir.display())),
        path = SERVICE_PATH,
    )
}

/// Where the service file goes.
pub fn service_path(kind: ServiceKind) -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("$HOME is not set")?;
    Ok(match kind {
        ServiceKind::Launchd => home
            .join("Library/LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist")),
        ServiceKind::Systemd => {
            let base = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"));
            base.join("systemd/user").join(SYSTEMD_UNIT)
        }
    })
}

/// Host-prep tips printed after install.
pub fn host_tips(kind: ServiceKind, port: u16) -> String {
    let mut s = String::from("\nHost prep:\n");
    if kind == ServiceKind::Launchd {
        s.push_str("  • Never sleep, restart after power loss:  sudo pmset -a sleep 0 disksleep 0 autorestart 1\n");
        s.push_str("  • Run kilnd as a dedicated non-admin user (System Settings → Users & Groups) and enable auto-login for it.\n");
        s.push_str("  • Keep Docker Desktop / OrbStack set to start at login.\n");
    } else {
        s.push_str("  • Keep the user service running without a login session:  loginctl enable-linger $USER\n");
        s.push_str("  • Run kilnd as a dedicated unprivileged user in the docker group.\n");
    }
    s.push_str(
        "  • Join the tailnet (`tailscale up`); kilnd binds only to the Tailscale address.\n",
    );
    s.push_str(&format!(
        "  • HTTPS for clients:  tailscale serve --bg --https=443 http://$(tailscale ip -4):{port}\n    then set server.public_url to the https://<host>.<tailnet>.ts.net URL.\n"
    ));
    s.push_str("  • Back up <data_dir>/kiln.db with Litestream; snapshot <data_dir>/index and <data_dir>/l0 nightly.\n");
    s.push_str("  • Check everything with:  kilnd doctor\n");
    s
}

/// `kilnd install`.
pub fn install(
    cfg: &Config,
    config_path: Option<&Path>,
    kind: ServiceKind,
    print_only: bool,
    force: bool,
) -> Result<String> {
    let exe = std::fs::canonicalize(std::env::current_exe()?)?;
    let cfg_abs = config_path.map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()));
    let content = match kind {
        ServiceKind::Launchd => launchd_plist(&exe, cfg_abs.as_deref(), &cfg.data_dir),
        ServiceKind::Systemd => systemd_unit(&exe, cfg_abs.as_deref(), &cfg.data_dir),
    };
    if print_only {
        return Ok(content);
    }
    let path = service_path(kind)?;
    if path.exists() && !force {
        bail!("{} exists; pass --force to overwrite", path.display());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::create_dir_all(cfg.data_dir.join("logs"))?;
    std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    let mut out = format!("Wrote {}\n\nStart it:\n", path.display());
    match kind {
        ServiceKind::Launchd => out.push_str(&format!(
            "  launchctl bootstrap gui/$(id -u) {p}\n  launchctl kickstart -k gui/$(id -u)/{LAUNCHD_LABEL}\nLogs: {}/logs/kilnd.log\n",
            cfg.data_dir.display(),
            p = path.display()
        )),
        ServiceKind::Systemd => out.push_str(
            "  systemctl --user daemon-reload\n  systemctl --user enable --now kilnd\nLogs: journalctl --user -u kilnd -f\n",
        ),
    }
    out.push_str(&host_tips(kind, cfg.server.port));
    Ok(out)
}

// ---------------------------------------------------------------- doctor

/// One doctor line.
#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub ok: Option<bool>, // Some(true) ✓, Some(false) ✕, None = warning
    pub name: &'static str,
    pub detail: String,
}

impl Check {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            ok: Some(true),
            name,
            detail: detail.into(),
        }
    }
    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            ok: Some(false),
            name,
            detail: detail.into(),
        }
    }
    fn warn(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            ok: None,
            name,
            detail: detail.into(),
        }
    }
    pub fn line(&self) -> String {
        let g = match self.ok {
            Some(true) => "✓",
            Some(false) => "✕",
            None => "!",
        };
        format!("{g} {:<14} {}", self.name, self.detail)
    }
}

/// Free / total bytes of the filesystem holding `p`.
pub fn disk_space(p: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `s` a valid out-pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let frsize = s.f_frsize as u64;
    Some((s.f_bavail as u64 * frsize, s.f_blocks as u64 * frsize))
}

async fn cmd_ok(program: &str, args: &[&str]) -> Option<String> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new(program)
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run every check.
pub async fn doctor(cfg: &Config, dev: bool) -> Vec<Check> {
    let mut out = Vec::new();
    match cfg.validate(dev) {
        Ok(()) => out.push(Check::pass("config", "valid")),
        Err(e) => out.push(Check::fail("config", e.to_string())),
    }

    // Data dir.
    let data = &cfg.data_dir;
    match std::fs::metadata(data) {
        Ok(md) => {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let mode = md.permissions().mode() & 0o777;
            let mine = md.uid() == unsafe { libc::geteuid() };
            if !mine {
                out.push(Check::fail(
                    "data dir",
                    format!("{} is owned by another user", data.display()),
                ));
            } else if mode & 0o077 != 0 {
                out.push(Check::warn(
                    "data dir",
                    format!("{} is mode {mode:o}; run chmod 700", data.display()),
                ));
            } else {
                out.push(Check::pass(
                    "data dir",
                    format!("{} (mode {mode:o})", data.display()),
                ));
            }
        }
        Err(_) => out.push(Check::warn(
            "data dir",
            format!(
                "{} does not exist yet (created on first start)",
                data.display()
            ),
        )),
    }

    // Unix socket path length (sun_path is 104 bytes on macOS).
    let n = cfg.run_socket_path_len();
    if n > crate::config::MAX_SOCKET_PATH {
        out.push(Check::fail(
            "socket path",
            format!(
                "run sockets would be {n} bytes (> {}); use a shorter data_dir such as ~/.kiln",
                crate::config::MAX_SOCKET_PATH
            ),
        ));
    } else if n > 90 {
        out.push(Check::warn("socket path", format!("run sockets are {n} bytes, close to the {}-byte limit; consider a shorter data_dir", crate::config::MAX_SOCKET_PATH)));
    } else {
        out.push(Check::pass("socket path", format!("{n} bytes")));
    }

    // Disk.
    let probe = if data.exists() {
        data.clone()
    } else {
        data.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| "/".into())
    };
    match disk_space(&probe) {
        Some((free, total)) if total > 0 => {
            let pct = free as f64 * 100.0 / total as f64;
            let d = format!(
                "{:.0} GB free of {:.0} GB ({pct:.0}%)",
                free as f64 / 1e9,
                total as f64 / 1e9
            );
            out.push(if pct < 10.0 {
                Check::fail("disk", d)
            } else {
                Check::pass("disk", d)
            });
        }
        _ => out.push(Check::warn("disk", "could not read free space")),
    }

    // Master key / keychain.
    let db = data.join("kiln.db");
    if db.exists() {
        let opts = kiln_vault::KeyOptions {
            env: true,
            keychain: cfg.vault.keychain && kiln_vault::KeyOptions::default().keychain,
        };
        match kiln_vault::MasterKey::load_or_create_with(data, opts) {
            Ok((key, src)) => match kiln_store::Store::open(&db).await {
                Ok(store) => {
                    let v = kiln_vault::Vault::new(key, store.clone());
                    match v.verify_key().await {
                        Ok(()) => out.push(Check::pass(
                            "master key",
                            format!("{src:?}, matches the database"),
                        )),
                        Err(e) => out.push(Check::fail("master key", e.to_string())),
                    }
                    store.close().await;
                }
                Err(e) => out.push(Check::fail("database", e.to_string())),
            },
            Err(e) => out.push(Check::fail("master key", e.to_string())),
        }
    } else {
        out.push(Check::warn(
            "master key",
            "no database yet; created on first start",
        ));
    }

    // Tailscale.
    match crate::bind::tailscale_ip().await {
        Some(ip) => {
            let name = crate::bind::tailscale_dns_name().await.unwrap_or_default();
            out.push(Check::pass(
                "tailscale",
                format!("{ip} {name}").trim().to_string(),
            ));
        }
        None if dev => out.push(Check::warn(
            "tailscale",
            "not connected (ok with --dev: binds 127.0.0.1)",
        )),
        None if cfg.server.bind != "tailscale" => out.push(Check::warn(
            "tailscale",
            format!("not connected; binding {} as configured", cfg.server.bind),
        )),
        None => out.push(Check::fail(
            "tailscale",
            "no Tailscale address; run `tailscale up`",
        )),
    }

    // Runtime + image.
    match cfg.runtime.kind {
        RuntimeKind::Process => {
            out.push(Check::warn(
                "runtime",
                "process runtime: workers run on the host with NO isolation (dev only)",
            ));
            match crate::daemon::find_runner_bin(cfg.runtime.runner_bin.as_deref()) {
                Some(p) => out.push(Check::pass("kiln-runner", p.display().to_string())),
                None => out.push(Check::fail(
                    "kiln-runner",
                    "not found; set runtime.runner_bin",
                )),
            }
        }
        RuntimeKind::Docker => {
            let rt = match &cfg.runtime.docker_socket {
                Some(s) => kiln_sandbox::DockerRuntime::with_socket(s),
                None => kiln_sandbox::DockerRuntime::connect().await,
            };
            match rt {
                Ok(rt) => {
                    use kiln_sandbox::Runtime;
                    match rt.ping().await {
                        Ok(info) => {
                            out.push(Check::pass(
                                "runtime",
                                format!("{} {} ({})", info.name, info.version, info.endpoint),
                            ));
                            match rt.docker().inspect_image(&cfg.runtime.worker_image).await {
                                Ok(_) => out.push(Check::pass(
                                    "worker image",
                                    cfg.runtime.worker_image.clone(),
                                )),
                                Err(_) => out.push(Check::fail(
                                    "worker image",
                                    format!(
                                        "{} not present; docker pull it (or build images/worker)",
                                        cfg.runtime.worker_image
                                    ),
                                )),
                            }
                        }
                        Err(e) => out.push(Check::fail("runtime", e.to_string())),
                    }
                }
                Err(e) => out.push(Check::fail("runtime", e.to_string())),
            }
        }
    }

    // git.
    match cmd_ok("git", &["--version"]).await {
        Some(v) => out.push(Check::pass("git", v)),
        None => out.push(Check::fail("git", "git not found on PATH")),
    }

    // GitHub.
    let gh = &cfg.github;
    match gh.mode {
        GithubMode::Local => out.push(Check::pass("github", "local PRs (no GitHub)")),
        GithubMode::Token => {
            if std::env::var(&gh.token_env).is_ok_and(|t| !t.trim().is_empty()) {
                out.push(Check::pass(
                    "github",
                    format!("token from ${}", gh.token_env),
                ));
            } else if gh.use_gh_cli
                && cmd_ok("gh", &["auth", "token"])
                    .await
                    .is_some_and(|t| !t.is_empty())
            {
                out.push(Check::pass("github", "token from `gh auth token`"));
            } else {
                out.push(Check::fail(
                    "github",
                    format!(
                        "token mode: ${} unset and `gh auth token` unavailable",
                        gh.token_env
                    ),
                ));
            }
        }
        GithubMode::App => match (gh.app_id, &gh.private_key_path) {
            (Some(id), Some(p)) => match std::fs::read(p) {
                Ok(pem) => match kiln_gitops::GitHubApp::new(id, &pem).and_then(|a| a.app_jwt()) {
                    Ok(_) => out.push(Check::pass("github", format!("GitHub App {id}, key ok"))),
                    Err(e) => out.push(Check::fail("github", format!("GitHub App key: {e}"))),
                },
                Err(e) => out.push(Check::fail(
                    "github",
                    format!("cannot read {}: {e}", p.display()),
                )),
            },
            _ => out.push(Check::fail(
                "github",
                "app mode needs github.app_id and github.private_key_path",
            )),
        },
    }

    // Web build.
    match crate::web::find_web_dir(&cfg.effective_web_dir(), dev) {
        Some(d) => out.push(Check::pass("web app", d.display().to_string())),
        None => out.push(Check::warn(
            "web app",
            format!("no index.html in {}", cfg.effective_web_dir().display()),
        )),
    }
    out
}

// ---------------------------------------------------------------- reset password

/// A random 20-character password from an unambiguous alphabet (CSPRNG).
pub fn random_password() -> String {
    use rand::RngExt;
    const A: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789";
    let mut rng = rand::rng();
    (0..20)
        .map(|_| A[rng.random_range(0..A.len())] as char)
        .collect()
}

/// Set a new password for `email` directly in the store and revoke sessions.
pub async fn reset_password(cfg: &Config, email: &str, password: Option<String>) -> Result<String> {
    let db = cfg.data_dir.join("kiln.db");
    if !db.exists() {
        bail!("no database at {}", db.display());
    }
    let store = kiln_store::Store::open(&db).await?;
    let user = store
        .get_user_by_email(email)
        .await?
        .with_context(|| format!("no user {email}"))?;
    let pw = password.unwrap_or_else(random_password);
    kiln_auth::check_password_policy(&pw).map_err(|e| anyhow::anyhow!("{e}"))?;
    let hash = kiln_auth::hash_password(&pw).map_err(|e| anyhow::anyhow!("{e}"))?;
    store
        .update_user(
            user.id,
            kiln_store::UserPatch {
                password_hash: Some(hash),
                ..Default::default()
            },
        )
        .await?;
    let n = store.delete_user_sessions(user.id).await?;
    store.close().await;
    tracing::info!(user = %user.id, sessions = n, "password reset from the command line");
    Ok(pw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_and_unit() {
        let p = launchd_plist(
            Path::new("/usr/local/bin/kilnd"),
            Some(Path::new("/etc/kiln & co.toml")),
            Path::new("/data"),
        );
        assert!(p.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(p.contains("<key>KeepAlive</key>\n    <true/>"));
        assert!(p.contains("/data/logs/kilnd.log"));
        assert!(p.contains("kiln &amp; co.toml"));
        assert!(p.contains(LAUNCHD_LABEL));
        let u = systemd_unit(
            Path::new("/opt/kiln/kilnd"),
            None,
            Path::new("/home/k/my data"),
        );
        assert!(u.contains("ExecStart=/opt/kiln/kilnd serve\n"));
        assert!(u.contains("Restart=always"));
        assert!(u.contains("Environment=\"KILN_DATA_DIR=/home/k/my data\""));
    }

    #[test]
    fn passwords_are_policy_compliant() {
        let a = random_password();
        assert_eq!(a.len(), 20);
        kiln_auth::check_password_policy(&a).unwrap();
        assert_ne!(a, random_password());
    }

    #[test]
    fn check_lines() {
        assert!(Check::pass("git", "2.4").line().starts_with("✓ git"));
        assert!(Check::fail("disk", "full").line().starts_with("✕ disk"));
    }
}
