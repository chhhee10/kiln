//! `kiln` — the Kiln terminal client (Ratatui).
//!
//! Architecture (Elm-style):
//! - [`app::App`] holds all UI state; [`app::App::update`] maps a [`app::Msg`]
//!   (key, paste, tick, resize, data event) to new state plus [`model::Cmd`]s.
//! - [`view::render`] is a pure function of `&App` drawing into a Ratatui
//!   frame; every screen ports the coordinates of `design/designs/TUI.dc.html`.
//! - A [`source::DataSource`] executes commands and streams
//!   [`model::DataEvent`]s back: [`live::LiveSource`] (daemon over HTTP + WS)
//!   or [`demo::DemoSource`] (in-process simulation).

pub mod app;
pub mod canvas;
pub mod demo;
pub mod live;
pub mod model;
pub mod palette;
pub mod qr;
pub mod runtime;
pub mod session;
pub mod source;
pub mod testing;
pub mod theme;
pub mod util;
pub mod view;

/// Daemon URL used when neither `--server`, `KILN_SERVER` nor a saved
/// session names one.
pub const DEFAULT_SERVER: &str = "http://127.0.0.1:7420";
