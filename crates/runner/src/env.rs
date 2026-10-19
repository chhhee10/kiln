//! Runner configuration from the environment.
//!
//! | var                         | default                 | meaning                                   |
//! |-----------------------------|-------------------------|-------------------------------------------|
//! | `KILN_RUN_DIR`              | `/run/kiln`             | run folder (spool, out/, transcripts/)    |
//! | `KILN_WORKSPACE`            | `RunConfig.workdir`     | task worktree                             |
//! | `KILN_SOCKET`               | `<run_dir>/kiln.sock`   | daemon socket                             |
//! | `KILN_RUN_TOKEN`            | — (required)            | per-run token sent in `Hello`             |
//! | `KILN_RUN_ID`               | — (required)            | run id sent in `Hello`                    |
//! | `KILN_RECONNECT_BUDGET_MS`  | 60000                   | give up reconnecting after this long      |
//! | `KILN_STOP_GRACE_MS`        | 10000                   | SIGTERM → SIGKILL grace for the harness   |
//! | `KILN_ACK_WAIT_MS`          | 15000                   | wait for acks before `Complete`           |

use kiln_types::RunId;
use kiln_types::protocol::paths;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone)]
pub struct RunnerEnv {
    pub run_dir: PathBuf,
    pub workspace: Option<PathBuf>,
    pub socket: PathBuf,
    pub token: String,
    pub run_id: RunId,
    pub reconnect_budget: Duration,
    pub stop_grace: Duration,
    pub ack_wait: Duration,
}

impl std::fmt::Debug for RunnerEnv {
    // Never print the token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunnerEnv")
            .field("run_dir", &self.run_dir)
            .field("workspace", &self.workspace)
            .field("socket", &self.socket)
            .field("run_id", &self.run_id)
            .finish_non_exhaustive()
    }
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn ms(name: &str, default: u64) -> Duration {
    Duration::from_millis(var(name).and_then(|v| v.parse().ok()).unwrap_or(default))
}

impl RunnerEnv {
    pub fn from_env() -> Result<Self, String> {
        let run_dir = var("KILN_RUN_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(paths::RUN_DIR));
        let socket = var(paths::ENV_SOCKET)
            .map(PathBuf::from)
            .unwrap_or_else(|| run_dir.join("kiln.sock"));
        let token =
            var(paths::ENV_TOKEN).ok_or_else(|| format!("{} is not set", paths::ENV_TOKEN))?;
        let run_id = var(paths::ENV_RUN_ID)
            .ok_or_else(|| format!("{} is not set", paths::ENV_RUN_ID))?
            .parse()
            .map_err(|e| format!("{}: {e}", paths::ENV_RUN_ID))?;
        Ok(Self {
            run_dir,
            workspace: var("KILN_WORKSPACE").map(PathBuf::from),
            socket,
            token,
            run_id,
            reconnect_budget: ms("KILN_RECONNECT_BUDGET_MS", 60_000),
            stop_grace: ms("KILN_STOP_GRACE_MS", 10_000),
            ack_wait: ms("KILN_ACK_WAIT_MS", 15_000),
        })
    }

    pub fn out_dir(&self) -> PathBuf {
        self.run_dir.join("out")
    }
    pub fn artifacts_dir(&self) -> PathBuf {
        self.out_dir().join("artifacts")
    }
    pub fn spool_path(&self) -> PathBuf {
        self.run_dir.join("spool.jsonl")
    }
    pub fn outcome_path(&self) -> PathBuf {
        self.out_dir().join("outcome.json")
    }
    pub fn feedback_path(&self) -> PathBuf {
        self.run_dir.join("feedback.txt")
    }
}
