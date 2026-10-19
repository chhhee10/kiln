//! Android navigation model (pure, unit tested natively): three tabs, a
//! stack of pushed screens, one bottom sheet, the voice overlay and the
//! heads-up notification. The system back gesture pops them in the order
//! the design specifies: heads-up → voice → sheet → pushed screen → tab root
//! (Planner). Deep links (push notifications, `kiln://` URLs, `#/task/…`
//! hashes) are parsed here too.

use super::feed::CardKey;
use kiln_types::ids::*;
use kiln_types::{Notification, NotificationKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Planner,
    Runs,
    You,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Planner, Tab::Runs, Tab::You];
    pub fn label(self) -> &'static str {
        match self {
            Tab::Planner => "Planner",
            Tab::Runs => "Runs",
            Tab::You => "You",
        }
    }
    pub fn glyph(self) -> &'static str {
        match self {
            Tab::Planner => "◆",
            Tab::Runs => "▥",
            Tab::You => "●",
        }
    }
}

/// Screens pushed over a tab. They hide the navigation bar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pushed {
    /// Task detail by display key ("FP-142").
    Task(String),
    /// Full-screen plan review dialog.
    PlanReview(PlanId),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Sheet {
    /// Feedback / Decide for a task.
    Feedback(TaskId),
    /// ⋮ More: stop, open PR, view logs.
    More(TaskId),
    /// A QA capture preview.
    Shot { title: String, url: Option<String> },
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct NavState {
    pub tab: Tab,
    pub stack: Vec<Pushed>,
    pub sheet: Option<Sheet>,
    pub voice: bool,
    pub heads_up: bool,
}

impl NavState {
    /// The screen on top: the last pushed screen, else the tab root.
    pub fn top(&self) -> Option<&Pushed> {
        self.stack.last()
    }

    pub fn show_nav_bar(&self) -> bool {
        self.stack.is_empty()
    }

    /// Nothing left to pop: Planner tab, no overlays.
    pub fn is_root(&self) -> bool {
        self.tab == Tab::Planner
            && self.stack.is_empty()
            && self.sheet.is_none()
            && !self.voice
            && !self.heads_up
    }

    /// One system-back step. Returns false when there was nothing to pop
    /// (the app should let the OS handle back, i.e. leave).
    pub fn back(&mut self) -> bool {
        if self.heads_up {
            self.heads_up = false;
        } else if self.voice {
            self.voice = false;
        } else if self.sheet.is_some() {
            self.sheet = None;
        } else if self.stack.pop().is_some() {
        } else if self.tab != Tab::Planner {
            self.tab = Tab::Planner;
        } else {
            return false;
        }
        true
    }

    /// Tapping a tab resets it to its root.
    pub fn tab_to(&mut self, t: Tab) {
        self.tab = t;
        self.stack.clear();
        self.sheet = None;
        self.voice = false;
    }

    pub fn push(&mut self, p: Pushed) {
        if self.stack.last() != Some(&p) {
            self.stack.push(p);
        }
        self.sheet = None;
        self.heads_up = false;
    }

    pub fn open_sheet(&mut self, s: Sheet) {
        self.sheet = Some(s);
    }

    /// Apply a deep link (prototype `deep()`): closes overlays, switches tab
    /// or pushes the screen. Returns the chat card to scroll to and
    /// highlight, if any.
    pub fn apply(&mut self, link: &DeepLink, feedback_task: Option<TaskId>) -> Option<CardKey> {
        self.heads_up = false;
        self.voice = false;
        self.sheet = None;
        match link {
            DeepLink::Chat(card) => {
                self.tab = Tab::Planner;
                self.stack.clear();
                card.clone()
            }
            DeepLink::Runs => {
                self.tab_to(Tab::Runs);
                None
            }
            DeepLink::You => {
                self.tab_to(Tab::You);
                None
            }
            DeepLink::Task { key, feedback } => {
                self.tab = Tab::Planner;
                self.stack = vec![Pushed::Task(key.clone())];
                if *feedback && let Some(t) = feedback_task {
                    self.sheet = Some(Sheet::Feedback(t));
                }
                None
            }
            DeepLink::PlanReview(p) => {
                self.tab = Tab::Planner;
                self.stack.clear();
                match p {
                    Some(p) => {
                        self.stack.push(Pushed::PlanReview(*p));
                        None
                    }
                    None => Some(CardKey::AnyPlan),
                }
            }
            DeepLink::Pair { .. } => None,
        }
    }
}

// ---------------------------------------------------------------- deep links

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeepLink {
    /// The Planner chat, optionally scrolled to (and highlighting) a card.
    Chat(Option<CardKey>),
    Task {
        key: String,
        feedback: bool,
    },
    /// `None` = the current plan card in chat.
    PlanReview(Option<PlanId>),
    Runs,
    You,
    /// `kiln://pair?url=<server>&code=<code>` from the desktop/TUI QR.
    Pair {
        url: String,
        code: String,
    },
}

fn query_value(q: &str, name: &str) -> Option<String> {
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (k == name).then(|| percent_decode(v))
    })
}

/// Minimal `application/x-www-form-urlencoded` decoding (UTF-8 aware).
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse a deep link. Accepts the daemon's notification routes
/// (`task/FP-139`, `plan/<id>`, `memory/<id>`, `runs`, `brain`), app hashes
/// (`#/task/FP-139`), `kiln://…` URLs and the pairing link.
pub fn parse_deep_link(raw: &str) -> Option<DeepLink> {
    let s = raw.trim();
    let s = s
        .strip_prefix("kiln://")
        .or_else(|| s.strip_prefix("kiln:"))
        .unwrap_or(s);
    let s = s.trim_start_matches('#').trim_start_matches('/');
    let (path, query) = s.split_once('?').unwrap_or((s, ""));
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    let head = parts.next().unwrap_or("").to_ascii_lowercase();
    let second = parts.next();
    let third = parts.next().map(|p| p.to_ascii_lowercase());
    let sheet = query_value(query, "sheet").map(|v| v.to_ascii_lowercase());
    let wants_feedback = |x: Option<&String>| {
        x.is_some_and(|v| matches!(v.as_str(), "feedback" | "reply" | "decide"))
    };
    let link = match head.as_str() {
        "pair" => DeepLink::Pair {
            url: query_value(query, "url")?,
            code: query_value(query, "code")?,
        },
        "task" | "tasks" => DeepLink::Task {
            key: second?.to_string(),
            feedback: wants_feedback(third.as_ref()) || wants_feedback(sheet.as_ref()),
        },
        "plan" | "plans" => {
            let id = second.and_then(|p| p.parse::<PlanId>().ok());
            match (id, third.as_deref()) {
                (Some(id), Some("review")) => DeepLink::PlanReview(Some(id)),
                (Some(id), _) => DeepLink::Chat(Some(CardKey::Plan(id))),
                (None, _) if second == Some("review") => DeepLink::PlanReview(None),
                (None, _) => DeepLink::Chat(Some(CardKey::AnyPlan)),
            }
        }
        "memory" | "brain" => DeepLink::Chat(Some(
            second
                .and_then(|m| m.parse::<MemoryId>().ok())
                .map(CardKey::Memory)
                .unwrap_or(CardKey::AnyMemory),
        )),
        "prs" | "pr" | "ready" => DeepLink::Chat(Some(CardKey::Prs)),
        "runs" | "board" => DeepLink::Runs,
        "you" | "settings" | "me" => DeepLink::You,
        "chat" | "planner" | "" => match second.map(|x| x.to_ascii_lowercase()).as_deref() {
            Some("plan") => DeepLink::Chat(Some(CardKey::AnyPlan)),
            Some("prs") | Some("pr") => DeepLink::Chat(Some(CardKey::Prs)),
            Some("live") => DeepLink::Chat(Some(CardKey::Live)),
            Some("memory") | Some("brain") => DeepLink::Chat(Some(CardKey::AnyMemory)),
            _ => DeepLink::Chat(None),
        },
        _ => return None,
    };
    Some(link)
}

/// Where tapping a notification goes (android.md "Notifications" table).
/// Escalations open the feedback sheet; plan cards highlight in chat; the
/// ongoing workers card opens Runs.
pub fn link_for_notification(n: &Notification) -> DeepLink {
    let parsed = parse_deep_link(&n.deep_link);
    match n.kind {
        NotificationKind::WorkersLive => DeepLink::Runs,
        NotificationKind::TaskEscalated => match parsed {
            Some(DeepLink::Task { key, .. }) => DeepLink::Task {
                key,
                feedback: true,
            },
            Some(other) => other,
            None => DeepLink::Chat(n.task_id.map(CardKey::AnyTask)),
        },
        NotificationKind::PlanReady => match (parsed, n.plan_id) {
            (Some(DeepLink::PlanReview(p)), _) => DeepLink::PlanReview(p),
            (_, Some(p)) => DeepLink::Chat(Some(CardKey::Plan(p))),
            (Some(l), None) => l,
            (None, None) => DeepLink::Chat(Some(CardKey::AnyPlan)),
        },
        NotificationKind::MemoryApproval => DeepLink::Chat(Some(
            n.memory_id
                .map(CardKey::Memory)
                .unwrap_or(CardKey::AnyMemory),
        )),
        NotificationKind::TaskReady => match parsed {
            Some(DeepLink::Task { .. }) | None => DeepLink::Chat(Some(CardKey::Prs)),
            Some(l) => l,
        },
        NotificationKind::TaskFailed | NotificationKind::SystemAlert => {
            parsed.unwrap_or(DeepLink::Chat(None))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn back_stack_order() {
        let mut n = NavState::default();
        assert!(n.is_root());
        assert!(!n.back(), "nothing to pop at the Planner root");
        n.tab_to(Tab::Runs);
        n.push(Pushed::Task("FP-142".into()));
        n.open_sheet(Sheet::More(TaskId::nil()));
        n.voice = true;
        n.heads_up = true;
        assert!(!n.show_nav_bar());
        assert!(n.back());
        assert!(!n.heads_up && n.voice, "heads-up first");
        assert!(n.back());
        assert!(!n.voice && n.sheet.is_some(), "then voice");
        assert!(n.back());
        assert!(n.sheet.is_none() && !n.stack.is_empty(), "then the sheet");
        assert!(n.back());
        assert!(
            n.stack.is_empty() && n.tab == Tab::Runs,
            "then the pushed screen"
        );
        assert!(n.show_nav_bar());
        assert!(n.back());
        assert_eq!(n.tab, Tab::Planner, "then the tab root → Planner");
        assert!(n.is_root());
        assert!(!n.back());
    }

    #[test]
    fn tab_resets_to_root() {
        let mut n = NavState::default();
        n.push(Pushed::Task("FP-1".into()));
        n.push(Pushed::Task("FP-1".into()));
        assert_eq!(n.stack.len(), 1, "same screen is not pushed twice");
        n.open_sheet(Sheet::Feedback(TaskId::nil()));
        n.voice = true;
        n.tab_to(Tab::You);
        assert_eq!(
            n,
            NavState {
                tab: Tab::You,
                ..Default::default()
            }
        );
    }

    #[test]
    fn parse_links() {
        assert_eq!(
            parse_deep_link("task/FP-139"),
            Some(DeepLink::Task {
                key: "FP-139".into(),
                feedback: false
            })
        );
        assert_eq!(
            parse_deep_link("#/task/FP-136/feedback"),
            Some(DeepLink::Task {
                key: "FP-136".into(),
                feedback: true
            })
        );
        assert_eq!(
            parse_deep_link("kiln://task/FP-136?sheet=reply"),
            Some(DeepLink::Task {
                key: "FP-136".into(),
                feedback: true
            })
        );
        let p = PlanId::new();
        assert_eq!(
            parse_deep_link(&format!("plan/{p}")),
            Some(DeepLink::Chat(Some(CardKey::Plan(p))))
        );
        assert_eq!(
            parse_deep_link(&format!("plan/{p}/review")),
            Some(DeepLink::PlanReview(Some(p)))
        );
        assert_eq!(
            parse_deep_link("chat/plan"),
            Some(DeepLink::Chat(Some(CardKey::AnyPlan)))
        );
        assert_eq!(
            parse_deep_link("chat/prs"),
            Some(DeepLink::Chat(Some(CardKey::Prs)))
        );
        assert_eq!(parse_deep_link("runs"), Some(DeepLink::Runs));
        assert_eq!(
            parse_deep_link("brain"),
            Some(DeepLink::Chat(Some(CardKey::AnyMemory)))
        );
        assert_eq!(parse_deep_link("/planner"), Some(DeepLink::Chat(None)));
        assert_eq!(parse_deep_link("settings/devices"), Some(DeepLink::You));
        assert_eq!(parse_deep_link("task"), None);
        assert_eq!(parse_deep_link("nope/x"), None);
        assert_eq!(
            parse_deep_link("kiln://pair?url=https%3A%2F%2Fmac-mini.tailnet.ts.net&code=K7Q2-9XMD"),
            Some(DeepLink::Pair {
                url: "https://mac-mini.tailnet.ts.net".into(),
                code: "K7Q2-9XMD".into()
            })
        );
        assert_eq!(parse_deep_link("kiln://pair?code=x"), None);
        assert_eq!(percent_decode("a+b%20c%E2%9C%93%zz"), "a b c✓%zz");
    }

    fn notif(kind: NotificationKind, link: &str) -> Notification {
        Notification {
            id: NotificationId::new(),
            user_id: UserId::nil(),
            kind,
            title: String::new(),
            body: String::new(),
            deep_link: link.into(),
            actions: vec![],
            task_id: None,
            plan_id: None,
            memory_id: None,
            read: false,
            resolved: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn notification_links_follow_the_table() {
        assert_eq!(
            link_for_notification(&notif(NotificationKind::TaskEscalated, "task/FP-136")),
            DeepLink::Task {
                key: "FP-136".into(),
                feedback: true
            }
        );
        assert_eq!(
            link_for_notification(&notif(NotificationKind::TaskFailed, "task/FP-139")),
            DeepLink::Task {
                key: "FP-139".into(),
                feedback: false
            }
        );
        assert_eq!(
            link_for_notification(&notif(NotificationKind::TaskReady, "task/FP-133")),
            DeepLink::Chat(Some(CardKey::Prs))
        );
        assert_eq!(
            link_for_notification(&notif(NotificationKind::WorkersLive, "runs")),
            DeepLink::Runs
        );
        let mut n = notif(NotificationKind::PlanReady, "");
        let p = PlanId::new();
        n.plan_id = Some(p);
        assert_eq!(
            link_for_notification(&n),
            DeepLink::Chat(Some(CardKey::Plan(p)))
        );
        assert_eq!(
            link_for_notification(&notif(NotificationKind::MemoryApproval, "brain")),
            DeepLink::Chat(Some(CardKey::AnyMemory))
        );
    }

    #[test]
    fn applying_links() {
        let mut n = NavState::default();
        n.tab_to(Tab::You);
        n.voice = true;
        let t = TaskId::new();
        let hl = n.apply(
            &DeepLink::Task {
                key: "FP-136".into(),
                feedback: true,
            },
            Some(t),
        );
        assert_eq!(hl, None);
        assert_eq!(n.tab, Tab::Planner);
        assert_eq!(n.stack, vec![Pushed::Task("FP-136".into())]);
        assert_eq!(n.sheet, Some(Sheet::Feedback(t)));
        assert!(!n.voice);
        let hl = n.apply(&DeepLink::Chat(Some(CardKey::Prs)), None);
        assert_eq!(hl, Some(CardKey::Prs));
        assert!(n.stack.is_empty() && n.sheet.is_none());
        n.apply(&DeepLink::Runs, None);
        assert_eq!(n.tab, Tab::Runs);
    }
}
