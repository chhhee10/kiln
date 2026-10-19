//! Saved session: `~/.config/kiln/session.toml` (0600).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Daemon origin, e.g. `https://mac-mini.tailnet.ts.net`.
    #[serde(default)]
    pub server: Option<String>,
    /// Bearer token. Absent after `kiln logout`.
    #[serde(default)]
    pub token: Option<String>,
    /// Last signed-in email (prefills the login form).
    #[serde(default)]
    pub email: Option<String>,
}

/// `$XDG_CONFIG_HOME/kiln/session.toml`, falling back to `~/.config/kiln/`.
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("kiln").join("session.toml")
}

/// Path shown in the UI (`~` for the home directory).
pub fn display_path(p: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME")
        && let Ok(rest) = p.strip_prefix(PathBuf::from(home))
    {
        return format!("~/{}", rest.display());
    }
    p.display().to_string()
}

impl Session {
    pub fn load(path: &Path) -> Session {
        std::fs::read_to_string(path).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
    }

    /// Write with 0600 permissions (directory 0700).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
        }
        let body = toml::to_string(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("toml.tmp");
        {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(&tmp)?;
            f.write_all(body.as_bytes())?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_with_private_perms() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("kiln").join("session.toml");
        let s = Session { server: Some("https://x".into()), token: Some("t0k".into()), email: Some("a@b.c".into()) };
        s.save(&p).unwrap();
        assert_eq!(Session::load(&p), s);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert_eq!(Session::load(&dir.path().join("missing.toml")), Session::default());
    }
}
