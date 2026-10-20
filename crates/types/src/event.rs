//! Normalized run events (the common schema every harness adapter maps into,
//! PRD §7.8) and the server → client WebSocket events (PRD §7.11).

use crate::ids::*;
use crate::memory::MemoryItem;
use crate::model::*;
use crate::status::WorkerRole;
use serde::{Deserialize, Serialize};

/// Log levels used in log panes (design/docs/tokens.md "Log levels").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum LogLevel {
    Info,
    Step,
    Pass,
    Warn,
    Fail,
    /// Capture (screenshot/video taken).
    Capt,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Info => "INFO",
            LogLevel::Step => "STEP",
            LogLevel::Pass => "PASS",
            LogLevel::Warn => "WARN",
            LogLevel::Fail => "FAIL",
            LogLevel::Capt => "CAPT",
        }
    }
}

/// One normalized event from a run. Sequence numbers are assigned by the
/// runner, monotonic per run, and make spool/stream reconciliation idempotent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunEvent {
    pub run_id: RunId,
    pub seq: u64,
    pub ts: Timestamp,
    #[serde(flatten)]
    pub kind: RunEventKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEventKind {
    /// Free-form log line.
    Log {
        level: LogLevel,
        message: String,
    },
    /// A named step started ("boot stack", "run unit tests").
    Step {
        title: String,
    },
    /// Assistant prose.
    Assistant {
        text: String,
    },
    /// Model "thinking"/reasoning summary, if the harness exposes it.
    Reasoning {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        id: String,
        ok: bool,
        output: String,
    },
    Command {
        command: String,
        exit_code: Option<i32>,
        output: String,
    },
    FileChange {
        path: String,
        change: String,
    },
    Progress {
        percent: u8,
        label: Option<String>,
    },
    Usage {
        tokens_in: u64,
        tokens_out: u64,
        cost_usd: f64,
    },
    Artifact {
        path: String,
        kind: ArtifactKind,
        caption: Option<String>,
    },
    Error {
        message: String,
    },
    /// Harness finished. The runner sends `Complete` separately with the outcome.
    Done {
        success: bool,
        summary: String,
    },
}

impl RunEventKind {
    /// Render as a log-pane line (level + message).
    pub fn log_line(&self) -> (LogLevel, String) {
        match self {
            RunEventKind::Log { level, message } => (*level, message.clone()),
            RunEventKind::Step { title } => (LogLevel::Step, title.clone()),
            RunEventKind::Assistant { text } => (LogLevel::Info, first_line(text)),
            RunEventKind::Reasoning { text } => (LogLevel::Info, first_line(text)),
            RunEventKind::ToolCall { name, input, .. } => {
                (LogLevel::Info, format!("{name} {}", summarize_json(input)))
            }
            RunEventKind::ToolResult { ok, output, .. } => (
                if *ok { LogLevel::Info } else { LogLevel::Warn },
                first_line(output),
            ),
            RunEventKind::Command {
                command, exit_code, ..
            } => match exit_code {
                Some(0) | None => (LogLevel::Info, format!("$ {command}")),
                Some(c) => (LogLevel::Fail, format!("$ {command} → exit {c}")),
            },
            RunEventKind::FileChange { path, change } => {
                (LogLevel::Info, format!("{change} {path}"))
            }
            RunEventKind::Progress { percent, label } => (
                LogLevel::Info,
                format!("{percent}% {}", label.clone().unwrap_or_default()),
            ),
            RunEventKind::Usage {
                tokens_in,
                tokens_out,
                cost_usd,
            } => (
                LogLevel::Info,
                format!("usage in={tokens_in} out={tokens_out} ${cost_usd:.4}"),
            ),
            RunEventKind::Artifact { path, caption, .. } => (
                LogLevel::Capt,
                caption.clone().unwrap_or_else(|| path.clone()),
            ),
            RunEventKind::Error { message } => (LogLevel::Fail, message.clone()),
            RunEventKind::Done { success, summary } => (
                if *success {
                    LogLevel::Pass
                } else {
                    LogLevel::Fail
                },
                summary.clone(),
            ),
        }
    }
}

fn first_line(s: &str) -> String {
    let line = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() > 240 {
        line.chars().take(240).collect::<String>() + "…"
    } else {
        line.to_string()
    }
}

fn summarize_json(v: &serde_json::Value) -> String {
    let s = match v {
        serde_json::Value::Object(m) => m
            .iter()
            .find(|(k, _)| {
                matches!(
                    k.as_str(),
                    "command" | "file_path" | "path" | "pattern" | "url" | "query"
                )
            })
            .map(|(_, v)| {
                v.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string())
            })
            .unwrap_or_else(|| v.to_string()),
        other => other.to_string(),
    };
    first_line(&s)
}

/// A log line as served to clients.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogLine {
    pub run_id: RunId,
    pub seq: u64,
    pub ts: Timestamp,
    pub role: WorkerRole,
    pub level: LogLevel,
    pub message: String,
}

/// Server → client events over `/api/v1/ws`. Filtered per user's permissions
/// before fan-out.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    Hello {
        version: String,
        user: UserSummary,
    },
    TaskUpserted {
        task: Task,
    },
    TaskRemoved {
        task_id: TaskId,
    },
    RunUpserted {
        run: Run,
    },
    Log {
        task_id: Option<TaskId>,
        thread_id: Option<ThreadId>,
        line: LogLine,
    },
    PlanUpserted {
        plan: Plan,
    },
    ThreadUpserted {
        thread: Thread,
    },
    Message {
        message: Message,
    },
    /// Planner is working ("reading company brain · drafting changes").
    PlannerStatus {
        thread_id: ThreadId,
        pending: bool,
        label: String,
    },
    Notification {
        notification: Notification,
    },
    MemoryUpserted {
        item: MemoryItem,
    },
    Machine {
        stats: MachineStats,
    },
    /// Something the client should show as a toast/flash.
    Toast {
        message: String,
        ok: bool,
    },
}

impl ServerEvent {
    /// The user this event is scoped to, if any. `None` means broadcast to
    /// everyone (subject to the per-variant filter in the daemon).
    pub fn owner(&self) -> Option<UserId> {
        match self {
            ServerEvent::TaskUpserted { task } => Some(task.owner_id),
            ServerEvent::RunUpserted { run } => Some(run.owner_id),
            ServerEvent::PlanUpserted { plan } => Some(plan.owner_id),
            ServerEvent::ThreadUpserted { thread } => Some(thread.owner_id),
            ServerEvent::Notification { notification } => Some(notification.user_id),
            ServerEvent::MemoryUpserted { item } => Some(item.owner_id),
            _ => None,
        }
    }
}

/// Client → server messages over the WebSocket.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Subscribe to the live log of a task (all roles) or a run.
    FollowTask {
        task_id: TaskId,
    },
    UnfollowTask {
        task_id: TaskId,
    },
    FollowThread {
        thread_id: ThreadId,
    },
    Ping,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn run_event_wire_format() {
        let e = RunEvent {
            run_id: RunId::nil(),
            seq: 3,
            ts: Utc::now(),
            kind: RunEventKind::Log {
                level: LogLevel::Fail,
                message: "boom".into(),
            },
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], "log");
        assert_eq!(v["level"], "FAIL");
        let back: RunEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back, e);
    }
}
