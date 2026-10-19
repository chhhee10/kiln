//! jobs and notifications.

mod common;

use chrono::{Duration, Utc};
use common::*;
use kiln_store::*;
use kiln_types::*;
use serde_json::json;
use std::collections::HashSet;

#[tokio::test]
async fn claim_order_kinds_owner_and_delay() {
    let s = store().await;
    let a = user(&s, "a@example.com").await;
    let b = user(&s, "b@example.com").await;
    let low = s
        .enqueue_job(NewJob::new("run_role", json!({"n": 1})).owner(a.id))
        .await
        .unwrap();
    let high = s
        .enqueue_job(NewJob {
            priority: 10,
            ..NewJob::new("run_role", json!({"n": 2})).owner(b.id)
        })
        .await
        .unwrap();
    let later = s
        .enqueue_job(NewJob {
            run_after: Some(Utc::now() + Duration::hours(1)),
            ..NewJob::new("run_role", json!({}))
        })
        .await
        .unwrap();
    let other = s
        .enqueue_job(NewJob::new("nightly", json!({})))
        .await
        .unwrap();

    assert_eq!(s.queued_jobs(None).await.unwrap().len(), 4);
    assert_eq!(s.queued_jobs(Some("nightly")).await.unwrap().len(), 1);

    // Owner filter.
    let j = s
        .claim_job(&["run_role"], Some(a.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(j.id, low);
    assert_eq!(j.state, JobState::Running);
    assert_eq!(j.attempts, 1);
    assert!(j.locked_at.is_some());
    assert_eq!(j.payload, json!({"n": 1}));
    assert_eq!(j.owner_id, Some(a.id));
    // Priority wins; delayed job is not claimable yet.
    assert_eq!(
        s.claim_job(&["run_role"], None).await.unwrap().unwrap().id,
        high
    );
    assert!(s.claim_job(&["run_role"], None).await.unwrap().is_none());
    // Empty kinds = any kind.
    assert_eq!(s.claim_job(&[], None).await.unwrap().unwrap().id, other);
    assert!(s.claim_job(&[], None).await.unwrap().is_none());
    assert_eq!(s.get_job(later).await.unwrap().state, JobState::Queued);

    s.complete_job(low).await.unwrap();
    assert_eq!(s.get_job(low).await.unwrap().state, JobState::Done);
    // Completing twice conflicts.
    assert!(s.complete_job(low).await.unwrap_err().is_conflict());
    assert!(
        s.complete_job(JobId::new())
            .await
            .unwrap_err()
            .is_not_found()
    );
    assert_eq!(
        s.jobs_in_state(JobState::Done, None).await.unwrap().len(),
        1
    );
    assert!(matches!(
        s.enqueue_job(NewJob::new(" ", json!({}))).await,
        Err(StoreError::Invalid(_))
    ));
}

#[tokio::test]
async fn fail_retries_until_attempts_exhausted() {
    let s = store().await;
    let id = s
        .enqueue_job(NewJob::new("distill", json!({})))
        .await
        .unwrap(); // max_attempts 2
    let j = s.claim_job(&["distill"], None).await.unwrap().unwrap();
    assert_eq!(j.attempts, 1);
    // Retry with zero delay → claimable again.
    assert_eq!(
        s.fail_job(id, "transient", Some(Duration::zero()))
            .await
            .unwrap(),
        JobState::Queued
    );
    let j2 = s.get_job(id).await.unwrap();
    assert_eq!(j2.last_error.as_deref(), Some("transient"));
    assert!(j2.locked_at.is_none());
    let j = s.claim_job(&["distill"], None).await.unwrap().unwrap();
    assert_eq!(j.attempts, 2);
    // Out of attempts → failed even though a retry was requested.
    assert_eq!(
        s.fail_job(id, "again", Some(Duration::zero()))
            .await
            .unwrap(),
        JobState::Failed
    );
    assert!(s.claim_job(&["distill"], None).await.unwrap().is_none());

    // No retry requested → failed immediately.
    let id2 = s
        .enqueue_job(NewJob {
            max_attempts: 5,
            ..NewJob::new("x", json!({}))
        })
        .await
        .unwrap();
    s.claim_job(&["x"], None).await.unwrap().unwrap();
    assert_eq!(
        s.fail_job(id2, "fatal", None).await.unwrap(),
        JobState::Failed
    );
    // Delayed retry is not immediately claimable.
    let id3 = s
        .enqueue_job(NewJob {
            max_attempts: 5,
            ..NewJob::new("y", json!({}))
        })
        .await
        .unwrap();
    s.claim_job(&["y"], None).await.unwrap().unwrap();
    assert_eq!(
        s.fail_job(id3, "later", Some(Duration::minutes(5)))
            .await
            .unwrap(),
        JobState::Queued
    );
    assert!(s.claim_job(&["y"], None).await.unwrap().is_none());
    // Failing a job that isn't running conflicts.
    assert!(s.fail_job(id3, "x", None).await.unwrap_err().is_conflict());
}

#[tokio::test]
async fn cancel_and_requeue_stale() {
    let s = store().await;
    let t1 = TaskId::new().to_string();
    let t2 = TaskId::new().to_string();
    s.enqueue_job(NewJob::new(
        "run_role",
        json!({"task_id": t1, "role": "builder"}),
    ))
    .await
    .unwrap();
    s.enqueue_job(NewJob::new(
        "run_role",
        json!({"task_id": t1, "role": "qa"}),
    ))
    .await
    .unwrap();
    s.enqueue_job(NewJob::new("distill", json!({"task_id": t1})))
        .await
        .unwrap();
    s.enqueue_job(NewJob::new("run_role", json!({"task_id": t2})))
        .await
        .unwrap();
    assert_eq!(
        s.cancel_jobs_for(Some("run_role"), "task_id", &t1)
            .await
            .unwrap(),
        2
    );
    assert_eq!(s.cancel_jobs_for(None, "task_id", &t1).await.unwrap(), 1);
    assert_eq!(s.queued_jobs(None).await.unwrap().len(), 1);
    assert!(matches!(
        s.cancel_jobs_for(None, "a.b') OR 1=1 --", "x").await,
        Err(StoreError::Invalid(_))
    ));

    let j = s.claim_job(&[], None).await.unwrap().unwrap();
    // Not stale yet.
    assert_eq!(
        s.requeue_stale_jobs(Duration::minutes(10)).await.unwrap(),
        0
    );
    // Everything locked before "now + 1s" is stale.
    assert_eq!(
        s.requeue_stale_jobs(Duration::seconds(-1)).await.unwrap(),
        1
    );
    let back = s.get_job(j.id).await.unwrap();
    assert_eq!(back.state, JobState::Queued);
    assert_eq!(back.last_error.as_deref(), Some("worker lost"));
    // Second loss exhausts attempts (max 2) → failed.
    s.claim_job(&[], None).await.unwrap().unwrap();
    assert_eq!(
        s.requeue_stale_jobs(Duration::seconds(-1)).await.unwrap(),
        1
    );
    assert_eq!(s.get_job(j.id).await.unwrap().state, JobState::Failed);

    let c = s.enqueue_job(NewJob::new("z", json!({}))).await.unwrap();
    assert!(s.cancel_job(c).await.unwrap());
    assert!(!s.cancel_job(c).await.unwrap());
    assert!(
        s.prune_finished_jobs(Utc::now() + Duration::seconds(1))
            .await
            .unwrap()
            >= 4
    );
}

#[tokio::test]
async fn concurrent_claims_never_double_claim() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(&dir.path().join("kiln.db")).await.unwrap();
    for i in 0..20 {
        s.enqueue_job(NewJob::new("work", json!({ "i": i })))
            .await
            .unwrap();
    }
    let mut handles = Vec::new();
    for _ in 0..8 {
        let s = s.clone();
        handles.push(tokio::spawn(async move {
            let mut mine = Vec::new();
            while let Some(j) = s.claim_job(&["work"], None).await.unwrap() {
                mine.push(j.id);
            }
            mine
        }));
    }
    let mut all = Vec::new();
    for h in handles {
        all.extend(h.await.unwrap());
    }
    let unique: HashSet<_> = all.iter().collect();
    assert_eq!(all.len(), 20);
    assert_eq!(unique.len(), 20);
}

#[tokio::test]
async fn notifications_crud() {
    let s = store().await;
    let a = user(&s, "a@example.com").await;
    let b = user(&s, "b@example.com").await;
    let mut n = NewNotification::new(
        a.id,
        NotificationKind::TaskFailed,
        "QA failed",
        "filter resets on refresh",
    );
    n.deep_link = "task/FP-1".into();
    n.actions = vec![NotificationAction {
        id: "retry".into(),
        label: "Retry".into(),
    }];
    n.task_id = Some(TaskId::new());
    let first = s.insert_notification(n).await.unwrap();
    assert!(!first.read);
    assert_eq!(first.actions.len(), 1);
    s.insert_notification(NewNotification::new(
        a.id,
        NotificationKind::TaskReady,
        "Ready",
        "PR open",
    ))
    .await
    .unwrap();
    s.insert_notification(NewNotification::new(
        b.id,
        NotificationKind::PlanReady,
        "Plan",
        "",
    ))
    .await
    .unwrap();

    let list = s.list_notifications(a.id, 10).await.unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].title, "Ready", "newest first");
    assert_eq!(s.list_notifications(a.id, 1).await.unwrap().len(), 1);
    assert_eq!(s.count_unread_notifications(a.id).await.unwrap(), 2);

    // Another user can't mark it read.
    assert!(
        s.mark_read(b.id, first.id)
            .await
            .unwrap_err()
            .is_not_found()
    );
    s.mark_read(a.id, first.id).await.unwrap();
    assert!(s.get_notification(first.id).await.unwrap().read);
    assert_eq!(s.count_unread_notifications(a.id).await.unwrap(), 1);
    assert_eq!(s.mark_all_read(a.id).await.unwrap(), 1);

    let r = s
        .resolve_notification(first.id, "✓ Retried · attempt 2/3 running")
        .await
        .unwrap();
    assert_eq!(
        r.resolved.as_deref(),
        Some("✓ Retried · attempt 2/3 running")
    );
    assert!(
        s.resolve_notification(NotificationId::new(), "x")
            .await
            .unwrap_err()
            .is_not_found()
    );

    assert!(
        s.find_open_notification(a.id, NotificationKind::WorkersLive)
            .await
            .unwrap()
            .is_none()
    );
    let live = s
        .insert_notification(NewNotification::new(
            a.id,
            NotificationKind::WorkersLive,
            "2 live",
            "",
        ))
        .await
        .unwrap();
    assert_eq!(
        s.find_open_notification(a.id, NotificationKind::WorkersLive)
            .await
            .unwrap()
            .unwrap()
            .id,
        live.id
    );
    let upd = s
        .update_notification_content(live.id, "3 live", "avg 40%")
        .await
        .unwrap();
    assert_eq!(upd.title, "3 live");
    s.resolve_notification(live.id, "done").await.unwrap();
    assert!(
        s.find_open_notification(a.id, NotificationKind::WorkersLive)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn foreign_keys_are_enforced() {
    let s = store().await;
    let e = s
        .insert_notification(NewNotification::new(
            UserId::new(),
            NotificationKind::SystemAlert,
            "x",
            "y",
        ))
        .await;
    assert!(matches!(e, Err(StoreError::Db(_))), "{e:?}");
}
