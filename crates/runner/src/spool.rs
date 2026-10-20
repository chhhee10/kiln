//! Crash-safe event spool (`<run_dir>/spool.jsonl`).
//!
//! Every event is appended (one JSON line, written straight to the file
//! without user-space buffering) **before** it is sent to the daemon, so a
//! runner or connection crash loses nothing: on reconnect the runner resends
//! everything after the last ack and the daemon dedupes on `(run_id, seq)`.
//! Re-opening an existing spool continues the sequence after its highest seq.

use kiln_types::event::RunEvent;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub struct Spool {
    path: PathBuf,
    file: File,
    next_seq: u64,
}

impl Spool {
    /// Open (or create) the spool and recover the next sequence number.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)?;
        let mut existing = Vec::new();
        file.read_to_end(&mut existing)?;
        let max = Self::parse(&existing)
            .iter()
            .map(|e| e.seq)
            .max()
            .unwrap_or(0);
        // A torn last line (crash mid-write) must not swallow the next event.
        if !existing.is_empty() && existing.last() != Some(&b'\n') {
            file.write_all(b"\n")?;
        }
        Ok(Self {
            path: path.to_path_buf(),
            file,
            next_seq: max + 1,
        })
    }

    fn parse(bytes: &[u8]) -> Vec<RunEvent> {
        String::from_utf8_lossy(bytes)
            .lines()
            .filter_map(|l| serde_json::from_str::<RunEvent>(l).ok())
            .collect()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Sequence number the next appended event must carry.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Highest sequence number written so far (0 = none).
    pub fn last_seq(&self) -> u64 {
        self.next_seq - 1
    }

    /// Append one event. Its `seq` must be [`Spool::next_seq`].
    pub fn append(&mut self, ev: &RunEvent) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(ev).map_err(std::io::Error::other)?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.next_seq = self.next_seq.max(ev.seq + 1);
        Ok(())
    }

    /// Flush file contents to stable storage.
    pub fn sync(&self) -> std::io::Result<()> {
        self.file.sync_data()
    }

    /// Every spooled event with `seq > after`, in order.
    pub fn read_after(path: &Path, after: u64) -> Vec<RunEvent> {
        let Ok(bytes) = std::fs::read(path) else {
            return vec![];
        };
        let mut v: Vec<RunEvent> = Self::parse(&bytes)
            .into_iter()
            .filter(|e| e.seq > after)
            .collect();
        v.sort_by_key(|e| e.seq);
        v.dedup_by_key(|e| e.seq);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_types::RunId;
    use kiln_types::event::{LogLevel, RunEventKind};

    fn ev(seq: u64) -> RunEvent {
        RunEvent {
            run_id: RunId::nil(),
            seq,
            ts: chrono::Utc::now(),
            kind: RunEventKind::Log {
                level: LogLevel::Info,
                message: format!("m{seq}"),
            },
        }
    }

    #[test]
    fn append_recover_and_read_after() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("spool.jsonl");
        {
            let mut s = Spool::open(&p).unwrap();
            assert_eq!(s.next_seq(), 1);
            for i in 1..=3 {
                s.append(&ev(i)).unwrap();
            }
            assert_eq!(s.last_seq(), 3);
        }
        // Torn trailing write.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&p)
            .unwrap()
            .write_all(b"{\"run_id\":")
            .unwrap();
        let mut s = Spool::open(&p).unwrap();
        assert_eq!(s.next_seq(), 4);
        s.append(&ev(4)).unwrap();
        let after = Spool::read_after(&p, 1);
        assert_eq!(
            after.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert!(Spool::read_after(&d.path().join("missing"), 0).is_empty());
    }
}
