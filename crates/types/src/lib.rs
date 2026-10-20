//! Shared types for every Kiln component.
//!
//! This crate is the contract between the daemon, the in-container runner and
//! all three clients (TUI, desktop, Android). It is pure data: no I/O, and it
//! compiles to `wasm32-unknown-unknown` for the Leptos app.

pub mod api;
pub mod event;
pub mod ids;
mod lenient;
pub mod memory;
pub mod model;
pub mod plan;
pub mod protocol;
pub mod qa;
pub mod status;

pub use ids::*;
pub use model::*;
pub use status::*;

/// Product name shown in every UI. Provisional; see DECISIONS.md.
pub const PRODUCT_NAME: &str = "Kiln";

/// Version shared by daemon and clients. Clients refuse to talk to a daemon
/// with a different major.minor.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default maximum number of Builder loops before a task escalates.
pub const DEFAULT_LOOP_CAP: u32 = 3;

/// Returns true when a client at `client` may talk to a daemon at `server`.
pub fn versions_compatible(client: &str, server: &str) -> bool {
    fn major_minor(v: &str) -> Option<(u32, u32)> {
        let mut it = v.split('.');
        Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
    }
    matches!((major_minor(client), major_minor(server)), (Some(a), Some(b)) if a == b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_match() {
        assert!(versions_compatible("0.1.0", "0.1.7"));
        assert!(!versions_compatible("0.1.0", "0.2.0"));
        assert!(!versions_compatible("garbage", "0.1.0"));
    }
}
