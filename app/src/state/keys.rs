//! Keyboard-shortcut infrastructure: a pure mapping from a key press (plus UI
//! context) to a `Shortcut`. The desktop surface installs one window keydown
//! listener that calls `map_key` and dispatches the result.
//!
//! Rules:
//! - ⌘K / Ctrl+K toggles the palette from anywhere.
//! - While the palette is open it owns ↑ ↓ ↵ Esc.
//! - Esc closes the top overlay → the side panel → goes back from a task.
//! - ⌘↵ / ⌘⇧↵ in the planner send / approve, even from the composer.
//! - Inputs swallow single-letter keys.
//! - `g` then b/p/m/s/a jumps (900 ms window).

use super::route::Screen;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeyInput {
    pub key: String,
    pub meta: bool,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    /// Focus is in an input, textarea, select or contenteditable.
    pub in_field: bool,
    pub now_ms: f64,
}

impl KeyInput {
    pub fn new(key: &str) -> Self {
        Self {
            key: key.into(),
            ..Default::default()
        }
    }
    fn modk(&self) -> bool {
        self.meta || self.ctrl
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overlay {
    None,
    Palette,
    /// Feedback / create-account / credential modals.
    Modal,
    Lightbox,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyContext {
    pub screen: Screen,
    pub peek_open: bool,
    pub overlay: Overlay,
    /// Login / setup screens disable app shortcuts.
    pub signed_in: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskKey {
    Retry,
    Stop,
    Escalate,
    Feedback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shortcut {
    TogglePalette,
    PaletteUp,
    PaletteDown,
    PaletteRun,
    CloseOverlay,
    ClosePeek,
    BackToBoard,
    Go(Screen),
    NewIdea,
    OpenFull,
    Task(TaskKey),
    PlannerSend,
    PlannerDispatch,
    FocusSearch,
}

#[derive(Clone, Debug, Default)]
pub struct KeyState {
    g_at: Option<f64>,
}

pub const G_WINDOW_MS: f64 = 900.0;

/// Returns the shortcut (if any) and whether to `preventDefault`.
pub fn map_key(ctx: &KeyContext, st: &mut KeyState, k: &KeyInput) -> Option<(Shortcut, bool)> {
    if !ctx.signed_in {
        return None;
    }
    let key = k.key.as_str();
    let lower = key.to_lowercase();
    if k.modk() && lower == "k" {
        return Some((Shortcut::TogglePalette, true));
    }
    match ctx.overlay {
        Overlay::Palette => {
            return match key {
                "Escape" => Some((Shortcut::CloseOverlay, true)),
                "ArrowDown" => Some((Shortcut::PaletteDown, true)),
                "ArrowUp" => Some((Shortcut::PaletteUp, true)),
                "Enter" => Some((Shortcut::PaletteRun, true)),
                _ => None,
            };
        }
        Overlay::Modal | Overlay::Lightbox => {
            return (key == "Escape").then_some((Shortcut::CloseOverlay, true));
        }
        Overlay::None => {}
    }
    if key == "Escape" {
        if ctx.screen == Screen::Board && ctx.peek_open {
            return Some((Shortcut::ClosePeek, true));
        }
        if ctx.screen == Screen::Task && !k.in_field {
            return Some((Shortcut::BackToBoard, true));
        }
        return None;
    }
    if k.modk() && key == "Enter" && ctx.screen == Screen::Planner {
        return Some((
            if k.shift {
                Shortcut::PlannerDispatch
            } else {
                Shortcut::PlannerSend
            },
            true,
        ));
    }
    if k.in_field || k.modk() || k.alt {
        return None;
    }
    if let Some(at) = st.g_at.take()
        && k.now_ms - at <= G_WINDOW_MS
    {
        let to = match lower.as_str() {
            "b" => Some(Screen::Board),
            "p" => Some(Screen::Planner),
            "m" => Some(Screen::Brain),
            "s" => Some(Screen::Settings),
            "a" => Some(Screen::Admin),
            _ => None,
        };
        return to.map(|s| (Shortcut::Go(s), true));
    }
    if lower == "g" && !k.shift {
        st.g_at = Some(k.now_ms);
        return None;
    }
    let task_ctx = ctx.screen == Screen::Task || (ctx.screen == Screen::Board && ctx.peek_open);
    match lower.as_str() {
        "c" if ctx.screen == Screen::Board => Some((Shortcut::NewIdea, true)),
        "o" if ctx.screen == Screen::Board && ctx.peek_open => Some((Shortcut::OpenFull, true)),
        "r" if task_ctx => Some((Shortcut::Task(TaskKey::Retry), true)),
        "s" if task_ctx => Some((Shortcut::Task(TaskKey::Stop), true)),
        "e" if task_ctx => Some((Shortcut::Task(TaskKey::Escalate), true)),
        "f" if task_ctx => Some((Shortcut::Task(TaskKey::Feedback), true)),
        "/" if ctx.screen == Screen::Brain => Some((Shortcut::FocusSearch, true)),
        _ => None,
    }
}

/// Label for the platform modifier in key hints.
pub fn mod_label(mac: bool) -> &'static str {
    if mac { "⌘" } else { "Ctrl+" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(screen: Screen) -> KeyContext {
        KeyContext {
            screen,
            peek_open: false,
            overlay: Overlay::None,
            signed_in: true,
        }
    }
    fn at(key: &str, ms: f64) -> KeyInput {
        KeyInput {
            key: key.into(),
            now_ms: ms,
            ..Default::default()
        }
    }

    #[test]
    fn palette_from_anywhere() {
        let mut st = KeyState::default();
        let k = KeyInput {
            key: "k".into(),
            ctrl: true,
            in_field: true,
            ..Default::default()
        };
        assert_eq!(
            map_key(&ctx(Screen::Brain), &mut st, &k),
            Some((Shortcut::TogglePalette, true))
        );
        let k = KeyInput {
            key: "K".into(),
            meta: true,
            ..Default::default()
        };
        assert_eq!(
            map_key(&ctx(Screen::Board), &mut st, &k).map(|x| x.0),
            Some(Shortcut::TogglePalette)
        );
        let mut c = ctx(Screen::Board);
        c.signed_in = false;
        assert_eq!(map_key(&c, &mut st, &k), None);
    }

    #[test]
    fn palette_owns_navigation_keys() {
        let mut st = KeyState::default();
        let mut c = ctx(Screen::Board);
        c.overlay = Overlay::Palette;
        assert_eq!(
            map_key(&c, &mut st, &KeyInput::new("ArrowDown")).map(|x| x.0),
            Some(Shortcut::PaletteDown)
        );
        assert_eq!(
            map_key(&c, &mut st, &KeyInput::new("Enter")).map(|x| x.0),
            Some(Shortcut::PaletteRun)
        );
        assert_eq!(
            map_key(&c, &mut st, &KeyInput::new("Escape")).map(|x| x.0),
            Some(Shortcut::CloseOverlay)
        );
        assert_eq!(
            map_key(&c, &mut st, &KeyInput::new("c")),
            None,
            "letters type into the palette"
        );
    }

    #[test]
    fn g_sequences() {
        let mut st = KeyState::default();
        let c = ctx(Screen::Board);
        assert_eq!(map_key(&c, &mut st, &at("g", 0.0)), None);
        assert_eq!(
            map_key(&c, &mut st, &at("m", 300.0)).map(|x| x.0),
            Some(Shortcut::Go(Screen::Brain))
        );
        // Window expired → `p` does nothing on the board.
        assert_eq!(map_key(&c, &mut st, &at("g", 1000.0)), None);
        assert_eq!(map_key(&c, &mut st, &at("p", 2500.0)), None);
        for (k, s) in [
            ("b", Screen::Board),
            ("p", Screen::Planner),
            ("s", Screen::Settings),
            ("a", Screen::Admin),
        ] {
            map_key(&c, &mut st, &at("g", 5000.0));
            assert_eq!(
                map_key(&c, &mut st, &at(k, 5100.0)).map(|x| x.0),
                Some(Shortcut::Go(s))
            );
        }
    }

    #[test]
    fn inputs_swallow_letters() {
        let mut st = KeyState::default();
        let mut c = ctx(Screen::Board);
        c.peek_open = true;
        let mut k = KeyInput::new("r");
        k.in_field = true;
        assert_eq!(map_key(&c, &mut st, &k), None);
        k.in_field = false;
        assert_eq!(
            map_key(&c, &mut st, &k).map(|x| x.0),
            Some(Shortcut::Task(TaskKey::Retry))
        );
    }

    #[test]
    fn escape_cascade() {
        let mut st = KeyState::default();
        let mut c = ctx(Screen::Board);
        c.overlay = Overlay::Modal;
        assert_eq!(
            map_key(&c, &mut st, &KeyInput::new("Escape")).map(|x| x.0),
            Some(Shortcut::CloseOverlay)
        );
        assert_eq!(
            map_key(&c, &mut st, &KeyInput::new("r")),
            None,
            "modal blocks task keys"
        );
        c.overlay = Overlay::None;
        c.peek_open = true;
        assert_eq!(
            map_key(&c, &mut st, &KeyInput::new("Escape")).map(|x| x.0),
            Some(Shortcut::ClosePeek)
        );
        let c = ctx(Screen::Task);
        assert_eq!(
            map_key(&c, &mut st, &KeyInput::new("Escape")).map(|x| x.0),
            Some(Shortcut::BackToBoard)
        );
        assert_eq!(
            map_key(&ctx(Screen::Admin), &mut st, &KeyInput::new("Escape")),
            None
        );
    }

    #[test]
    fn screen_specific_keys() {
        let mut st = KeyState::default();
        assert_eq!(
            map_key(&ctx(Screen::Board), &mut st, &KeyInput::new("c")).map(|x| x.0),
            Some(Shortcut::NewIdea)
        );
        assert_eq!(
            map_key(&ctx(Screen::Planner), &mut st, &KeyInput::new("c")),
            None
        );
        assert_eq!(
            map_key(&ctx(Screen::Board), &mut st, &KeyInput::new("o")),
            None,
            "o needs the panel"
        );
        assert_eq!(
            map_key(&ctx(Screen::Task), &mut st, &KeyInput::new("f")).map(|x| x.0),
            Some(Shortcut::Task(TaskKey::Feedback))
        );
        assert_eq!(
            map_key(&ctx(Screen::Brain), &mut st, &KeyInput::new("/")).map(|x| x.0),
            Some(Shortcut::FocusSearch)
        );
        let mut send = KeyInput {
            key: "Enter".into(),
            meta: true,
            in_field: true,
            ..Default::default()
        };
        assert_eq!(
            map_key(&ctx(Screen::Planner), &mut st, &send).map(|x| x.0),
            Some(Shortcut::PlannerSend)
        );
        send.shift = true;
        assert_eq!(
            map_key(&ctx(Screen::Planner), &mut st, &send).map(|x| x.0),
            Some(Shortcut::PlannerDispatch)
        );
        assert_eq!(map_key(&ctx(Screen::Board), &mut st, &send), None);
    }
}
