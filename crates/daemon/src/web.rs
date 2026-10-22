//! Non-API routes: the web app at `/app`, client builds at `/download`,
//! `/healthz`, and the `/` → `/app/` redirect.

use crate::state::AppState;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use std::path::{Path, PathBuf};
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;

/// Pick the web build directory: `web_dir` (default `<data>/web`) when it has
/// an `index.html`, else `web/` next to the executable (release archives);
/// in dev, fall back to the repo's `app/dist`.
pub fn find_web_dir(configured: &Path, dev: bool) -> Option<PathBuf> {
    if configured.join("index.html").is_file() {
        return Some(configured.to_path_buf());
    }
    // Resolve symlinks (e.g. ~/.kiln/bin/kilnd → the unpacked release).
    if let Some(beside) = std::env::current_exe()
        .ok()
        .and_then(|exe| std::fs::canonicalize(exe).ok())
        .and_then(|exe| exe.parent().map(|d| d.join("web")))
        .filter(|d| d.join("index.html").is_file())
    {
        return Some(beside);
    }
    if dev {
        let candidates = [
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../app/dist"),
            PathBuf::from("app/dist"),
        ];
        for c in candidates {
            if c.join("index.html").is_file() {
                return Some(std::fs::canonicalize(&c).unwrap_or(c));
            }
        }
    }
    None
}

/// Routes outside `/api/v1`.
pub fn routes(state: &AppState) -> Router<AppState> {
    let mut r = Router::new()
        .route("/", get(|| async { Redirect::permanent("/app/") }))
        .route("/healthz", get(healthz))
        .route("/download", get(download_page))
        .route("/download/", get(download_page));

    let downloads = state
        .cfg
        .data_dir
        .join("downloads")
        .join(kiln_types::VERSION);
    r = r.nest_service(
        "/download/files",
        tower::ServiceBuilder::new()
            .layer(SetResponseHeaderLayer::overriding(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_static("attachment"),
            ))
            .layer(SetResponseHeaderLayer::overriding(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ))
            .service(ServeDir::new(downloads)),
    );

    if state.web_dir.is_some() {
        r = r
            .route("/app", get(|| async { Redirect::permanent("/app/") }))
            .route("/app/", get(serve_app))
            .route("/app/{*rest}", get(serve_app));
    } else {
        r = r
            .route("/app", get(no_web))
            .route("/app/", get(no_web))
            .route("/app/{*rest}", get(no_web));
    }
    r
}

/// Resolve a request path under `/app/` to a file in the web build.
///
/// Trunk emits relative asset URLs (`./app-<hash>.js`), so a deep link such as
/// `/app/task/FP-1` makes the browser ask for `/app/task/app-<hash>.js`: when
/// the exact path is missing but its file name exists at the build root, the
/// root file is served. Everything else falls back to `index.html` (SPA).
pub fn resolve_app_path(root: &Path, rest: &str) -> PathBuf {
    let parts: Vec<&str> = rest
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts
        .iter()
        .any(|p| *p == ".." || p.contains('\\') || p.contains('\0'))
    {
        return root.join("index.html");
    }
    let exact = parts.iter().fold(root.to_path_buf(), |acc, p| acc.join(p));
    if !parts.is_empty() && exact.is_file() {
        return exact;
    }
    if let Some(last) = parts.last()
        && last.contains('.')
    {
        let flat = root.join(last);
        if flat.is_file() {
            return flat;
        }
    }
    root.join("index.html")
}

async fn serve_app(State(state): State<AppState>, req: axum::extract::Request) -> Response {
    let Some(root) = state.web_dir.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let rest = req
        .uri()
        .path()
        .strip_prefix("/app")
        .unwrap_or("")
        .to_string();
    let file = resolve_app_path(&root, &rest);
    let is_index = file.file_name().is_some_and(|n| n == "index.html");
    match ServeFile::new(&file).oneshot(req).await {
        Ok(resp) => {
            let mut resp = resp.into_response();
            let cache = if is_index {
                "no-cache"
            } else {
                "public, max-age=31536000, immutable"
            };
            resp.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
            resp.headers_mut().insert(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            );
            resp
        }
        Err(e) => crate::error::AppError::internal(e).into_response(),
    }
}

async fn no_web(State(state): State<AppState>) -> Response {
    let body = page(
        "Web app not installed",
        &format!(
            "<p>The web build was not found in <code>{}</code>.</p>\
             <p>Build it with <code>trunk build --release</code> in <code>app/</code> and copy \
             <code>app/dist</code> there, or set <code>web_dir</code> in kiln.toml.</p>",
            esc(&state.cfg.effective_web_dir().display().to_string())
        ),
    );
    (StatusCode::NOT_FOUND, Html(body)).into_response()
}

async fn healthz(State(state): State<AppState>) -> Response {
    let db_ok = sqlx_ping(&state).await;
    let machine = state.scheduler.machine();
    let body = serde_json::json!({
        "ok": db_ok,
        "product": kiln_types::PRODUCT_NAME,
        "version": kiln_types::VERSION,
        "uptime_secs": state.started.elapsed().as_secs(),
        "db": db_ok,
        "runtime": machine.runtime_ok,
        "queue_depth": machine.queue_depth,
        "containers_running": machine.containers_running,
    });
    let status = if db_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body)).into_response()
}

async fn sqlx_ping(state: &AppState) -> bool {
    state.store.count_users().await.is_ok()
}

/// One downloadable file.
#[derive(Debug, Clone, PartialEq)]
pub struct DownloadFile {
    pub name: String,
    pub size: u64,
}

/// Regular files directly under `dir`, sorted by name.
pub fn list_downloads(dir: &Path) -> Vec<DownloadFile> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut out: Vec<DownloadFile> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let md = e.metadata().ok()?;
            let name = e.file_name().into_string().ok()?;
            (md.is_file() && !name.starts_with('.')).then_some(DownloadFile {
                name,
                size: md.len(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Human-readable size ("12.4 MB").
pub fn human_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

fn kind_of(name: &str) -> &'static str {
    let n = name.to_ascii_lowercase();
    if n.ends_with(".apk") || n.ends_with(".aab") {
        "Android"
    } else if n.ends_with(".dmg")
        || n.contains("darwin")
        || n.contains("macos")
        || n.contains("apple")
    {
        "macOS"
    } else if n.ends_with(".appimage")
        || n.ends_with(".deb")
        || n.ends_with(".rpm")
        || n.contains("linux")
    {
        "Linux"
    } else if n.ends_with(".exe") || n.ends_with(".msi") || n.contains("windows") {
        "Windows"
    } else {
        "Other"
    }
}

async fn download_page(State(state): State<AppState>) -> Html<String> {
    let dir = state
        .cfg
        .data_dir
        .join("downloads")
        .join(kiln_types::VERSION);
    let files = list_downloads(&dir);
    let rows = if files.is_empty() {
        format!(
            "<p class=dim>No client builds published for v{} yet. Copy them into <code>{}</code>.</p>",
            kiln_types::VERSION,
            esc(&dir.display().to_string())
        )
    } else {
        let mut s = String::from("<ul>");
        for f in &files {
            s.push_str(&format!(
                "<li><a href=\"/download/files/{href}\">{name}</a><span class=tag>{kind}</span><span class=size>{size}</span></li>",
                href = pct(&f.name),
                name = esc(&f.name),
                kind = kind_of(&f.name),
                size = human_size(f.size)
            ));
        }
        s.push_str("</ul>");
        s
    };
    let body = format!(
        "<p class=dim>Clients must match the daemon's version (v{v}). TUI, desktop and Android builds for this \
         daemon:</p>{rows}<p class=dim>Pair a phone: open Settings → Pair device on the desktop app or run \
         <code>kiln pair</code>, then scan the QR code.</p>",
        v = kiln_types::VERSION
    );
    Html(page(
        &format!(
            "Download {} v{}",
            kiln_types::PRODUCT_NAME,
            kiln_types::VERSION
        ),
        &body,
    ))
}

/// Minimal page shell with the design tokens (dark, lime accent, Geist).
pub fn page(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html><html lang=en><head><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1">
<title>{t}</title>
<link rel=preconnect href="https://fonts.googleapis.com"><link href="https://fonts.googleapis.com/css2?family=Geist:wght@400;500;600&family=Geist+Mono&display=swap" rel=stylesheet>
<style>
:root{{--bg:#0f1115;--panel:#15171c;--line:#2a2d34;--text:#eceef2;--muted:#9a9fa8;--dim:#6b7079;--accent:#cdf04a;--on-accent:#14170a}}
*{{box-sizing:border-box}}body{{margin:0;background:var(--bg);color:var(--text);font:14px/1.5 "Geist",ui-sans-serif,system-ui,sans-serif}}
main{{max-width:720px;margin:56px auto;padding:0 20px}}h1{{font-size:22px;font-weight:600;margin:0 0 6px;display:flex;gap:10px;align-items:center}}
.mark{{width:22px;height:22px;border-radius:6px;background:var(--accent);display:inline-block}}
code{{font-family:"Geist Mono",ui-monospace,monospace;font-size:12.5px;background:var(--panel);border:1px solid var(--line);border-radius:4px;padding:1px 5px}}
.dim{{color:var(--muted)}}ul{{list-style:none;padding:0;margin:20px 0;border:1px solid var(--line);border-radius:10px;background:var(--panel)}}
li{{display:flex;gap:12px;align-items:center;padding:12px 16px;border-top:1px solid var(--line)}}li:first-child{{border-top:0}}
a{{color:var(--accent);text-decoration:none;font-family:"Geist Mono",ui-monospace,monospace;flex:1;overflow:hidden;text-overflow:ellipsis}}a:hover{{text-decoration:underline}}
.tag{{font-size:11px;color:var(--on-accent);background:var(--accent);border-radius:4px;padding:1px 6px}}.size{{color:var(--dim);font-variant-numeric:tabular-nums;min-width:72px;text-align:right}}
</style></head><body><main><h1><span class=mark></span>{t}</h1>{body}</main></body></html>"#,
        t = esc(title)
    )
}

/// HTML-escape.
pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

/// Percent-encode everything but unreserved characters (RFC 3986).
pub fn pct(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_escaping() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(esc("<a href=\"x\">&"), "&lt;a href=&quot;x&quot;&gt;&amp;");
        assert_eq!(pct("a b/c?d=e&f"), "a%20b%2Fc%3Fd%3De%26f");
        assert_eq!(kind_of("kiln-0.1.0.apk"), "Android");
        assert_eq!(kind_of("Kiln_0.1.0_aarch64.dmg"), "macOS");
    }

    #[test]
    fn app_paths() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("index.html"), "i").unwrap();
        std::fs::write(d.path().join("app-1.js"), "j").unwrap();
        let r = d.path();
        assert_eq!(resolve_app_path(r, "/"), r.join("index.html"));
        assert_eq!(resolve_app_path(r, "/app-1.js"), r.join("app-1.js"));
        assert_eq!(
            resolve_app_path(r, "/task/FP-1/app-1.js"),
            r.join("app-1.js")
        );
        assert_eq!(resolve_app_path(r, "/task/FP-1"), r.join("index.html"));
        assert_eq!(
            resolve_app_path(r, "/../../etc/passwd"),
            r.join("index.html")
        );
    }

    #[test]
    fn lists_files() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("b.apk"), b"12345").unwrap();
        std::fs::write(d.path().join("a.dmg"), b"1").unwrap();
        std::fs::write(d.path().join(".hidden"), b"1").unwrap();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        let f = list_downloads(d.path());
        assert_eq!(
            f,
            vec![
                DownloadFile {
                    name: "a.dmg".into(),
                    size: 1
                },
                DownloadFile {
                    name: "b.apk".into(),
                    size: 5
                }
            ]
        );
    }
}
