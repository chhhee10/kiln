//! Command palette: the command list (prototype `commands()`) and the fuzzy
//! filter.

use crate::app::{App, Screen, TABS};

#[derive(Clone, Debug, PartialEq)]
pub enum PalAction {
    Go(Screen),
    Approve,
    Retry,
    Stop,
    Feedback,
    Escalate,
    ToggleFollow,
    TestCred(String),
    OpenRun(String),
    OpenPlan(String),
    OpenThread(String),
    NewIdea,
    Pair,
    SignOut,
    Quit,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Command {
    pub group: &'static str,
    pub label: String,
    pub key: &'static str,
    pub action: PalAction,
}

/// Maximum rows the palette shows.
pub const MAX_ROWS: usize = 12;

fn all_commands(app: &App) -> Vec<Command> {
    let c = |group, label: String, key, action| Command { group, label, key, action };
    let mut out: Vec<Command> = TABS
        .iter()
        .enumerate()
        .map(|(i, (s, l))| c("go to", l.to_string(), ["1", "2", "3", "4", "5", "6"][i], PalAction::Go(*s)))
        .collect();
    if let Some(p) = &app.data.plan {
        out.push(c("plan", format!("Approve & dispatch “{}”", p.title), "a", PalAction::Approve));
    }
    if let Some(t) = app.cur() {
        let id = t.key.clone();
        out.push(c("task", format!("Retry {id}"), "r", PalAction::Retry));
        out.push(c("task", format!("Stop {id}"), "s", PalAction::Stop));
        out.push(c("task", format!("Send feedback to Builder · {id}"), "f", PalAction::Feedback));
        out.push(c("task", format!("Escalate {id}"), "e", PalAction::Escalate));
    }
    out.push(c("logs", "Toggle follow".into(), "f", PalAction::ToggleFollow));
    let cred = app
        .data
        .creds
        .iter()
        .find(|c| c.name.to_lowercase().contains("litellm") && !c.is_new)
        .or_else(|| app.data.creds.iter().find(|c| !c.is_new));
    if let Some(cr) = cred {
        let name = cr.name.strip_suffix(" proxy").unwrap_or(&cr.name);
        out.push(c("creds", format!("Test {name} connection"), "t", PalAction::TestCred(cr.key.clone())));
    }
    for w in app.data.workers() {
        out.push(c("open", format!("{}  {}", w.key, w.title), "", PalAction::OpenRun(w.key.clone())));
    }
    out.push(c("account", "Sign out".into(), "q", PalAction::SignOut));
    out.push(c("account", "Pair a phone".into(), "", PalAction::Pair));
    for p in &app.data.plans {
        out.push(c("plan", format!("Open plan “{}”", p.title), "", PalAction::OpenPlan(p.key.clone())));
    }
    for t in &app.data.threads {
        out.push(c("planner", format!("Open thread “{}”", t.title), "", PalAction::OpenThread(t.key.clone())));
    }
    out.push(c("planner", "New idea → planner".into(), "n", PalAction::NewIdea));
    for t in &app.data.tasks {
        if !t.is_worker() {
            out.push(c("open", format!("{}  {}", t.key, t.title), "", PalAction::OpenRun(t.key.clone())));
        }
    }
    out.push(c("app", "Quit".into(), "ctrl-c", PalAction::Quit));
    out
}

/// Score `q` against `hay` (both compared case-insensitively). Contiguous
/// substring matches beat scattered subsequence matches; earlier and
/// word-start matches beat later ones. `None` = no match.
pub fn fuzzy_score(hay: &str, q: &str) -> Option<i32> {
    let q = q.trim().to_lowercase();
    if q.is_empty() {
        return Some(0);
    }
    let h = hay.to_lowercase();
    if let Some(pos) = h.find(&q) {
        let word_start = pos == 0 || !h[..pos].chars().last().is_some_and(|c| c.is_alphanumeric());
        return Some(10_000 - pos as i32 + if word_start { 500 } else { 0 });
    }
    // Subsequence match.
    let hc: Vec<char> = h.chars().collect();
    let mut hi = 0usize;
    let mut first = None;
    let mut gaps = 0i32;
    let mut last: Option<usize> = None;
    for qc in q.chars() {
        if qc == ' ' {
            continue;
        }
        let mut found = false;
        while hi < hc.len() {
            if hc[hi] == qc {
                first.get_or_insert(hi);
                if let Some(l) = last {
                    gaps += (hi - l - 1) as i32;
                }
                last = Some(hi);
                hi += 1;
                found = true;
                break;
            }
            hi += 1;
        }
        if !found {
            return None;
        }
    }
    Some(5_000 - gaps * 10 - first.unwrap_or(0) as i32)
}

/// Commands matching `q`, best first (stable for ties), at most [`MAX_ROWS`].
pub fn commands(app: &App, q: &str) -> Vec<Command> {
    filter(all_commands(app), q)
}

pub fn filter(cmds: Vec<Command>, q: &str) -> Vec<Command> {
    let mut scored: Vec<(i32, usize, Command)> = cmds
        .into_iter()
        .enumerate()
        .filter_map(|(i, c)| fuzzy_score(&format!("{} {}", c.group, c.label), q).map(|s| (s, i, c)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(MAX_ROWS).map(|(_, _, c)| c).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(group: &'static str, label: &str) -> Command {
        Command { group, label: label.into(), key: "", action: PalAction::Quit }
    }

    #[test]
    fn empty_query_keeps_order_and_caps() {
        let cmds: Vec<Command> = (0..20).map(|i| cmd("go to", &format!("item {i}"))).collect();
        let out = filter(cmds, "");
        assert_eq!(out.len(), MAX_ROWS);
        assert_eq!(out[0].label, "item 0");
        assert_eq!(out[11].label, "item 11");
    }

    #[test]
    fn substring_beats_subsequence() {
        let cmds = vec![cmd("task", "Retry FP-142"), cmd("go to", "Run"), cmd("logs", "Toggle follow")];
        let out = filter(cmds, "run");
        assert_eq!(out[0].label, "Run");
        // "Retry FP-142" doesn't contain r-u-n in order? r..(no u) → excluded
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn subsequence_matches() {
        assert!(fuzzy_score("go to Credentials", "crd").is_some());
        assert!(fuzzy_score("go to Credentials", "xyz").is_none());
        assert!(fuzzy_score("task Escalate FP-142", "esc 142").is_some());
        let tight = fuzzy_score("plan Approve", "apr").unwrap();
        let loose = fuzzy_score("account Pair a phone", "apr").unwrap();
        assert!(tight > loose);
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(fuzzy_score("go to Logs", "LOGS"), fuzzy_score("go to Logs", "logs"));
    }
}
