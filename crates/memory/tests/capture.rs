//! L0 capture: idempotent ingest, redaction, restart safety, torn segments,
//! nightly compaction and retention (and DataFusion SQL when enabled).

mod common;

use chrono::{Duration, Utc};
use common::*;
use kiln_memory::{MemoryConfig, NightlyOptions};
use kiln_types::WorkerRole;
use kiln_types::event::{LogLevel, RunEvent, RunEventKind};
use std::io::Read;

const SECRET: &str = "sk-ant-api03-SUPERSECRET\"quoted\\";

fn redactor(s: &str) -> String {
    s.replace(SECRET, "[REDACTED]")
}

fn events(
    run: kiln_types::RunId,
    seqs: std::ops::RangeInclusive<u64>,
    at: chrono::DateTime<Utc>,
) -> Vec<RunEvent> {
    seqs.map(|i| {
        let kind = match i % 4 {
            0 => log(LogLevel::Info, &format!("step {i} token={SECRET}")),
            1 => cmd("cargo test", 0, &format!("ok {SECRET}")),
            2 => RunEventKind::ToolCall {
                id: format!("t{i}"),
                name: "Bash".into(),
                input: serde_json::json!({"command": "echo", "env": {"KEY": SECRET}, "args": [SECRET]}),
            },
            _ => RunEventKind::FileChange { path: format!("src/f{i}.rs"), change: "M".into() },
        };
        ev(run, i, at + Duration::milliseconds(i as i64), kind)
    })
    .collect()
}

fn decompress_all(dir: &std::path::Path) -> String {
    let mut out = String::new();
    for day in std::fs::read_dir(dir.join("l0")).unwrap().flatten() {
        for f in std::fs::read_dir(day.path()).unwrap().flatten() {
            let p = f.path();
            if p.to_string_lossy().ends_with(".jsonl.zst") {
                let mut s = String::new();
                zstd::stream::read::Decoder::new(std::fs::File::open(&p).unwrap())
                    .unwrap()
                    .read_to_string(&mut s)
                    .unwrap();
                out.push_str(&s);
            }
        }
    }
    out
}

#[tokio::test]
async fn ingest_is_idempotent_and_redacted() {
    let env = env().await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let p = add_project(&env.pool, "FP").await;
    let run = add_run(&env.pool, a, None, WorkerRole::Builder).await;
    let now = Utc::now();

    let r1 = env
        .mem
        .ingest_l0(&run, Some(p), &events(run.id, 1..=5, now), &redactor)
        .await
        .unwrap();
    assert_eq!((r1.appended, r1.duplicates), (5, 0));
    let r2 = env
        .mem
        .ingest_l0(&run, Some(p), &events(run.id, 1..=5, now), &redactor)
        .await
        .unwrap();
    assert_eq!((r2.appended, r2.duplicates), (0, 5));
    let r3 = env
        .mem
        .ingest_l0(&run, Some(p), &events(run.id, 3..=8, now), &redactor)
        .await
        .unwrap();
    assert_eq!((r3.appended, r3.duplicates), (3, 3));
    // duplicate seqs inside one batch
    let mut dup = events(run.id, 9..=9, now);
    dup.extend(events(run.id, 9..=9, now));
    assert_eq!(
        env.mem
            .ingest_l0(&run, Some(p), &dup, &redactor)
            .await
            .unwrap()
            .appended,
        1
    );

    let got = env.mem.read_l0(run.id).await.unwrap();
    assert_eq!(
        got.iter().map(|e| e.seq).collect::<Vec<_>>(),
        (1..=9).collect::<Vec<_>>()
    );
    let rows = env.mem.read_l0_rows(run.id).await.unwrap();
    assert!(
        rows.iter()
            .all(|r| r.owner == a && r.project == Some(p) && r.role == WorkerRole::Builder)
    );
    assert!(
        rows.iter()
            .all(|r| r.visibility == kiln_types::memory::Visibility::Private)
    );
    assert_eq!(rows[0].kind, "command");

    let raw = decompress_all(&env.data);
    assert!(!raw.contains("SUPERSECRET"), "secret reached disk");
    assert!(raw.contains("[REDACTED]"));
    let json = serde_json::to_string(&got).unwrap();
    assert!(!json.contains("SUPERSECRET"));

    let (path, format, n): (String, String, i64) =
        sqlx::query_as("SELECT path, format, rows FROM l0_segments")
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert_eq!(format, "jsonl.zst");
    assert_eq!(n, 9);
    assert!(
        path.starts_with("l0/") && path.ends_with(&format!("{}.jsonl.zst", run.id)),
        "{path}"
    );
    assert!(env.mem.l0_span().await.unwrap().is_some());

    // events from another run are refused
    let other = add_run(&env.pool, a, None, WorkerRole::Qa).await;
    let e = env
        .mem
        .ingest_l0(&run, None, &events(other.id, 1..=1, now), &redactor)
        .await
        .unwrap_err();
    assert!(e.is_invalid());
}

#[tokio::test]
async fn ingest_stays_idempotent_across_restarts_and_torn_segments() {
    let env = env().await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let run = add_run(&env.pool, a, None, WorkerRole::Builder).await;
    let now = Utc::now();
    env.mem
        .ingest_l0(&run, None, &events(run.id, 1..=4, now), &no_redact)
        .await
        .unwrap();

    // Crash mid-append: a torn frame at the end of the segment.
    let seg = env
        .data
        .join("l0")
        .join(now.format("%Y-%m-%d").to_string())
        .join(format!("{}.jsonl.zst", run.id));
    let mut bytes = std::fs::read(&seg).unwrap();
    bytes.extend_from_slice(&[0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x13]);
    std::fs::write(&seg, bytes).unwrap();

    let env = env.reopen(MemoryConfig::default()).await;
    let r = env
        .mem
        .ingest_l0(&run, None, &events(run.id, 1..=6, now), &no_redact)
        .await
        .unwrap();
    assert_eq!(
        (r.appended, r.duplicates),
        (2, 4),
        "seen set rebuilt from disk after restart"
    );
    let got = env.mem.read_l0(run.id).await.unwrap();
    assert_eq!(
        got.len(),
        6,
        "repaired segment keeps old and new rows readable"
    );
}

#[tokio::test]
async fn events_are_partitioned_by_day() {
    let env = env().await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let run = add_run(&env.pool, a, None, WorkerRole::Builder).await;
    let d1 = Utc::now() - Duration::days(2);
    let mut evs = events(run.id, 1..=2, d1);
    evs.extend(events(run.id, 3..=4, d1 + Duration::days(1)));
    env.mem
        .ingest_l0(&run, None, &evs, &no_redact)
        .await
        .unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM l0_segments")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(env.mem.read_l0(run.id).await.unwrap().len(), 4);
}

#[cfg(feature = "parquet")]
#[tokio::test]
async fn nightly_compacts_to_parquet_and_keeps_reads_identical() {
    let env = env().await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let run = add_run(&env.pool, a, None, WorkerRole::Builder).await;
    let old = Utc::now() - Duration::days(3);
    env.mem
        .ingest_l0(&run, None, &events(run.id, 1..=6, old), &redactor)
        .await
        .unwrap();
    let fresh_run = add_run(&env.pool, a, None, WorkerRole::Qa).await;
    env.mem
        .ingest_l0(
            &fresh_run,
            None,
            &events(fresh_run.id, 1..=2, Utc::now()),
            &redactor,
        )
        .await
        .unwrap();
    let before = env.mem.read_l0(run.id).await.unwrap();

    let rep = env
        .mem
        .nightly(&NightlyOptions::default(), &|_, _| true)
        .await
        .unwrap();
    assert_eq!(
        (
            rep.segments_compacted,
            rep.parquet_files_written,
            rep.rows_compacted
        ),
        (1, 1, 6)
    );
    let day = env.data.join("l0").join(old.format("%Y-%m-%d").to_string());
    assert!(day.join(format!("{}.parquet", run.id)).exists());
    assert!(!day.join(format!("{}.jsonl.zst", run.id)).exists());
    assert_eq!(
        env.mem.read_l0(run.id).await.unwrap(),
        before,
        "parquet round-trips every event"
    );
    let formats: Vec<String> = sqlx::query_scalar("SELECT format FROM l0_segments ORDER BY format")
        .fetch_all(&env.pool)
        .await
        .unwrap();
    assert_eq!(
        formats,
        vec!["jsonl.zst", "parquet"],
        "fresh segment is left alone"
    );

    // Late events for the compacted day: idempotent against parquet, merged next night.
    let r = env
        .mem
        .ingest_l0(&run, None, &events(run.id, 5..=8, old), &redactor)
        .await
        .unwrap();
    assert_eq!((r.appended, r.duplicates), (2, 2));
    assert_eq!(env.mem.read_l0(run.id).await.unwrap().len(), 8);
    let rep = env
        .mem
        .nightly(&NightlyOptions::default(), &|_, _| true)
        .await
        .unwrap();
    assert_eq!(rep.rows_compacted, 8);
    assert_eq!(env.mem.read_l0(run.id).await.unwrap().len(), 8);
    let rows: i64 = sqlx::query_scalar("SELECT rows FROM l0_segments WHERE format = 'parquet'")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(rows, 8);
}

#[tokio::test]
async fn retention_deletes_old_days_and_is_clamped() {
    let env = env().await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let ancient = add_run(&env.pool, a, None, WorkerRole::Builder).await;
    let month = add_run(&env.pool, a, None, WorkerRole::Builder).await;
    env.mem
        .ingest_l0(
            &ancient,
            None,
            &events(ancient.id, 1..=3, Utc::now() - Duration::days(100)),
            &no_redact,
        )
        .await
        .unwrap();
    env.mem
        .ingest_l0(
            &month,
            None,
            &events(month.id, 1..=3, Utc::now() - Duration::days(30)),
            &no_redact,
        )
        .await
        .unwrap();

    // 10 days is below the PRD floor: clamped to 60, so the 30-day-old run survives.
    let opts = NightlyOptions {
        retention_days: 10,
        ..Default::default()
    };
    let rep = env.mem.nightly(&opts, &|_, _| true).await.unwrap();
    assert_eq!(rep.retention_days, 60);
    assert_eq!(rep.segments_expired, 1);
    assert!(rep.bytes_freed > 0);
    assert!(env.mem.read_l0(ancient.id).await.unwrap().is_empty());
    assert_eq!(env.mem.read_l0(month.id).await.unwrap().len(), 3);
    let paths: Vec<String> = sqlx::query_scalar("SELECT path FROM l0_segments")
        .fetch_all(&env.pool)
        .await
        .unwrap();
    assert_eq!(paths.len(), 1);
    assert!(paths[0].contains(&month.id.to_string()));
}

#[cfg(feature = "datafusion")]
#[tokio::test]
async fn sql_over_l0_is_owner_scoped_and_read_only() {
    let env = env().await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let b = add_user(&env.pool, "Ben Ito").await;
    let ra = add_run(&env.pool, a, None, WorkerRole::Builder).await;
    let rb = add_run(&env.pool, b, None, WorkerRole::Builder).await;
    env.mem
        .ingest_l0(
            &ra,
            None,
            &events(ra.id, 1..=8, Utc::now() - Duration::days(3)),
            &no_redact,
        )
        .await
        .unwrap();
    env.mem
        .ingest_l0(&rb, None, &events(rb.id, 1..=4, Utc::now()), &no_redact)
        .await
        .unwrap();
    env.mem
        .nightly(&NightlyOptions::default(), &|_, _| true)
        .await
        .unwrap(); // ra → parquet

    let all = env
        .mem
        .query_l0(None, "SELECT COUNT(*) AS n FROM l0")
        .await
        .unwrap();
    assert_eq!(all[0]["n"], 12, "parquet + fresh jsonl");
    let mine = env
        .mem
        .query_l0(
            Some(b),
            "SELECT type, COUNT(*) AS n FROM l0 GROUP BY type ORDER BY type",
        )
        .await
        .unwrap();
    let total: i64 = mine.iter().map(|r| r["n"].as_i64().unwrap()).sum();
    assert_eq!(total, 4, "owner scope hides other users' rows");
    let leak = env
        .mem
        .query_l0(Some(b), &format!("SELECT * FROM l0 WHERE owner = '{a}'"))
        .await
        .unwrap();
    assert!(leak.is_empty());
    assert!(
        env.mem
            .query_l0(Some(b), "CREATE TABLE x AS SELECT 1")
            .await
            .is_err()
    );
    assert!(
        env.mem
            .query_l0(Some(b), "SELECT * FROM l0_parquet")
            .await
            .is_err(),
        "raw sources are not reachable"
    );
}
