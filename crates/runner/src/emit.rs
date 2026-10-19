//! Event emission: assign seq → append to spool → hand to the uplink.

use crate::spool::Spool;
use crate::uplink::Uplink;
use kiln_types::RunId;
use kiln_types::event::{LogLevel, RunEvent, RunEventKind};
use kiln_types::protocol::Usage;

pub struct Emitter {
    run_id: RunId,
    spool: Spool,
    uplink: Uplink,
    /// Running totals the run loop needs for the outcome.
    pub usage: Usage,
    pub last_assistant: Option<String>,
    pub errors: Vec<String>,
}

impl Emitter {
    pub fn new(run_id: RunId, spool: Spool, uplink: Uplink) -> Self {
        Self {
            run_id,
            spool,
            uplink,
            usage: Usage::default(),
            last_assistant: None,
            errors: Vec::new(),
        }
    }

    pub fn uplink(&self) -> &Uplink {
        &self.uplink
    }

    pub fn last_seq(&self) -> u64 {
        self.spool.last_seq()
    }

    /// Record and send one event; returns its seq.
    pub fn emit(&mut self, kind: RunEventKind) -> u64 {
        match &kind {
            RunEventKind::Usage {
                tokens_in,
                tokens_out,
                cost_usd,
            } => {
                self.usage.tokens_in += tokens_in;
                self.usage.tokens_out += tokens_out;
                self.usage.cost_usd += cost_usd;
            }
            RunEventKind::Assistant { text } => self.last_assistant = Some(text.clone()),
            RunEventKind::Error { message } if self.errors.len() < 50 => {
                self.errors.push(message.clone())
            }
            _ => {}
        }
        let ev = RunEvent {
            run_id: self.run_id,
            seq: self.spool.next_seq(),
            ts: chrono::Utc::now(),
            kind,
        };
        let seq = ev.seq;
        if let Err(e) = self.spool.append(&ev) {
            // Still try to deliver it live; the daemon dedupes anyway.
            tracing::error!(error = %e, seq, "failed to append to spool");
        }
        self.uplink.send_event(ev);
        seq
    }

    pub fn log(&mut self, level: LogLevel, message: impl Into<String>) -> u64 {
        self.emit(RunEventKind::Log {
            level,
            message: message.into(),
        })
    }

    pub fn sync(&self) {
        if let Err(e) = self.spool.sync() {
            tracing::warn!(error = %e, "spool fsync failed");
        }
    }
}
