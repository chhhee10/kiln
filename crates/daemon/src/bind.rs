//! Listener address selection (Tailscale only, never public).
//!
//! `server.bind = "tailscale"` resolves the host's Tailscale IPv4 address via
//! `tailscale ip -4`, falling back to scanning network interfaces named
//! `tailscale*` / `utun*` for an address in the CGNAT range 100.64.0.0/10.
//! Without one, kilnd refuses to start unless `--dev`, which binds
//! 127.0.0.1. Unspecified addresses (`0.0.0.0`, `::`) are always rejected.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

/// Errors choosing a listen address.
#[derive(Debug, thiserror::Error)]
pub enum BindError {
    #[error(
        "no Tailscale address found (is tailscale up?). kilnd only listens on the tailnet; \
         use --dev for 127.0.0.1 or set server.bind to an explicit tailnet IP"
    )]
    NoTailscale,
    #[error("refusing to bind {0}: kilnd never listens on all interfaces")]
    Unspecified(IpAddr),
    #[error("invalid server.bind {0:?}: expected \"tailscale\", an IP, or IP:port")]
    Invalid(String),
}

/// True for 100.64.0.0/10 (Tailscale's CGNAT range).
pub fn is_tailscale_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (o[1] & 0xC0) == 64
}

/// `tailscale ip -4`, if the CLI exists and answers within 3 s.
pub async fn tailscale_cli_ip() -> Option<Ipv4Addr> {
    let out = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::process::Command::new(tailscale_bin())
            .args(["ip", "-4"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.trim().parse::<Ipv4Addr>().ok())
}

/// The Tailscale CLI path (macOS app bundles it outside `PATH`).
pub fn tailscale_bin() -> String {
    const MAC_APP: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";
    if cfg!(target_os = "macos") && std::path::Path::new(MAC_APP).exists() {
        return MAC_APP.into();
    }
    "tailscale".into()
}

/// Scan interfaces `tailscale*` / `utun*` for a 100.64.0.0/10 address.
pub fn tailscale_interface_ip() -> Option<Ipv4Addr> {
    let ifaces = if_addrs::get_if_addrs().ok()?;
    ifaces.into_iter().find_map(|i| {
        let named = i.name.starts_with("tailscale") || i.name.starts_with("utun");
        match i.ip() {
            IpAddr::V4(v4) if named && is_tailscale_v4(v4) => Some(v4),
            _ => None,
        }
    })
}

/// Resolve the Tailscale IPv4 address (CLI first, then interfaces).
pub async fn tailscale_ip() -> Option<Ipv4Addr> {
    match tailscale_cli_ip().await {
        Some(ip) => Some(ip),
        None => tailscale_interface_ip(),
    }
}

/// MagicDNS name of this node (`tailscale status --json` → `Self.DNSName`).
pub async fn tailscale_dns_name() -> Option<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::process::Command::new(tailscale_bin())
            .args(["status", "--json"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let name = v
        .get("Self")?
        .get("DNSName")?
        .as_str()?
        .trim_end_matches('.');
    (!name.is_empty()).then(|| name.to_string())
}

/// Pick the socket address to listen on.
pub async fn resolve_bind(bind: &str, port: u16, dev: bool) -> Result<SocketAddr, BindError> {
    let bind = bind.trim();
    let addr = if bind.eq_ignore_ascii_case("tailscale") {
        match tailscale_ip().await {
            Some(ip) => SocketAddr::new(IpAddr::V4(ip), port),
            None if dev => {
                tracing::warn!("no Tailscale address; --dev binds 127.0.0.1");
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
            }
            None => return Err(BindError::NoTailscale),
        }
    } else if let Ok(sa) = bind.parse::<SocketAddr>() {
        sa
    } else if let Ok(ip) = bind.parse::<IpAddr>() {
        SocketAddr::new(ip, port)
    } else {
        return Err(BindError::Invalid(bind.to_string()));
    };
    check_addr(addr.ip(), dev)?;
    Ok(addr)
}

/// Reject unspecified addresses always; warn about non-tailnet, non-loopback
/// addresses outside dev.
pub fn check_addr(ip: IpAddr, dev: bool) -> Result<(), BindError> {
    if ip.is_unspecified() {
        return Err(BindError::Unspecified(ip));
    }
    let tailnet = match ip {
        IpAddr::V4(v4) => is_tailscale_v4(v4),
        // Tailscale's IPv6 ULA prefix fd7a:115c:a1e0::/48.
        IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    };
    if !tailnet && !ip.is_loopback() && !dev {
        tracing::warn!(%ip, "server.bind is not a Tailscale address; make sure this interface is private");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgnat_range() {
        assert!(is_tailscale_v4("100.64.0.1".parse().unwrap()));
        assert!(is_tailscale_v4("100.127.255.254".parse().unwrap()));
        assert!(!is_tailscale_v4("100.128.0.1".parse().unwrap()));
        assert!(!is_tailscale_v4("192.168.1.2".parse().unwrap()));
    }

    #[tokio::test]
    async fn explicit_and_refused() {
        let a = resolve_bind("127.0.0.1", 7420, false).await.unwrap();
        assert_eq!(a.to_string(), "127.0.0.1:7420");
        let a = resolve_bind("127.0.0.1:9", 7420, false).await.unwrap();
        assert_eq!(a.port(), 9);
        assert!(matches!(
            resolve_bind("0.0.0.0", 7420, true).await,
            Err(BindError::Unspecified(_))
        ));
        assert!(matches!(
            resolve_bind("[::]:80", 7420, true).await,
            Err(BindError::Unspecified(_))
        ));
        assert!(matches!(
            resolve_bind("nope", 7420, true).await,
            Err(BindError::Invalid(_))
        ));
    }
}
