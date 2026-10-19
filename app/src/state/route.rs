//! Hash-based routing (`#/board`, `#/task/FP-142`, …). Hash routes work the
//! same at `/app/` on the daemon, under `trunk serve`, and inside the Tauri
//! WebView, with no server-side fallback needed.

use kiln_types::ids::ThreadId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Screen {
    Board,
    Task,
    Planner,
    Brain,
    Settings,
    Admin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    Credentials,
    Defaults,
    Memory,
    Devices,
    Appearance,
}

impl SettingsTab {
    pub const ALL: [SettingsTab; 5] = [
        SettingsTab::Credentials,
        SettingsTab::Defaults,
        SettingsTab::Memory,
        SettingsTab::Devices,
        SettingsTab::Appearance,
    ];
    pub fn label(self) -> &'static str {
        match self {
            SettingsTab::Credentials => "Credentials",
            SettingsTab::Defaults => "Worker defaults",
            SettingsTab::Memory => "Memory sharing",
            SettingsTab::Devices => "Devices",
            SettingsTab::Appearance => "Appearance",
        }
    }
    pub fn slug(self) -> &'static str {
        match self {
            SettingsTab::Credentials => "credentials",
            SettingsTab::Defaults => "defaults",
            SettingsTab::Memory => "memory",
            SettingsTab::Devices => "devices",
            SettingsTab::Appearance => "appearance",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Board,
    /// Task detail by display key ("FP-142").
    Task(String),
    /// Planner, optionally on a specific thread. `None` = most recent thread.
    Planner(Option<ThreadId>),
    /// A fresh, not-yet-created planner thread ("+ New idea").
    NewIdea,
    Brain,
    Settings(SettingsTab),
    Admin,
}

impl Route {
    pub fn screen(&self) -> Screen {
        match self {
            Route::Board => Screen::Board,
            Route::Task(_) => Screen::Task,
            Route::Planner(_) | Route::NewIdea => Screen::Planner,
            Route::Brain => Screen::Brain,
            Route::Settings(_) => Screen::Settings,
            Route::Admin => Screen::Admin,
        }
    }

    pub fn to_hash(&self) -> String {
        match self {
            Route::Board => "/board".into(),
            Route::Task(k) => format!("/task/{k}"),
            Route::Planner(None) => "/planner".into(),
            Route::Planner(Some(t)) => format!("/planner/{t}"),
            Route::NewIdea => "/planner/new".into(),
            Route::Brain => "/brain".into(),
            Route::Settings(t) => format!("/settings/{}", t.slug()),
            Route::Admin => "/admin".into(),
        }
    }

    pub fn parse(hash: &str) -> Option<Route> {
        let h = hash.trim_start_matches('#').trim_start_matches('/');
        let mut parts = h.split('/').filter(|s| !s.is_empty());
        let r = match parts.next()? {
            "board" => Route::Board,
            "task" => Route::Task(parts.next()?.to_string()),
            "planner" => match parts.next() {
                None => Route::Planner(None),
                Some("new") => Route::NewIdea,
                Some(id) => Route::Planner(id.parse().ok()),
            },
            "brain" => Route::Brain,
            "settings" => {
                let tab = parts
                    .next()
                    .and_then(|s| SettingsTab::ALL.into_iter().find(|t| t.slug() == s))
                    .unwrap_or_default();
                Route::Settings(tab)
            }
            "admin" => Route::Admin,
            _ => return None,
        };
        Some(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let t = ThreadId::new();
        for r in [
            Route::Board,
            Route::Task("FP-142".into()),
            Route::Planner(None),
            Route::Planner(Some(t)),
            Route::NewIdea,
            Route::Brain,
            Route::Settings(SettingsTab::Devices),
            Route::Admin,
        ] {
            assert_eq!(Route::parse(&r.to_hash()), Some(r.clone()), "{r:?}");
        }
        assert_eq!(
            Route::parse("#/settings"),
            Some(Route::Settings(SettingsTab::Credentials))
        );
        assert_eq!(Route::parse(""), None);
        assert_eq!(Route::parse("/nope"), None);
        assert_eq!(Route::Task("x".into()).screen(), Screen::Task);
    }
}
