//! Optional backends (features `lance`, `fastembed`) behind the same
//! gateway guarantees. fastembed downloads models, so its test is ignored
//! unless `KILN_TEST_FASTEMBED=1`.

#![cfg(any(feature = "lance", feature = "fastembed"))]

mod common;

#[allow(unused_imports)]
use common::*;
#[allow(unused_imports)]
use kiln_memory::MemoryConfig;
#[allow(unused_imports)]
use kiln_types::api::{ApproveMemoryRequest, MemoryDecision, UpdateMemoryRequest};
#[allow(unused_imports)]
use kiln_types::memory::{MemoryQuery, MemoryType, Visibility};

#[cfg(feature = "lance")]
#[tokio::test]
async fn lance_backend_searches_and_respects_visibility() {
    let cfg = MemoryConfig {
        vector_backend: kiln_memory::VectorBackend::Lance,
        ..Default::default()
    };
    let env = env_with(cfg.clone()).await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let b = add_user(&env.pool, "Ben Ito").await;
    let item = env
        .mem
        .propose(
            a,
            None,
            prop(
                MemoryType::Convention,
                "FP",
                "deploys_with",
                "fly deploy --remote-only",
                Visibility::Company,
            ),
        )
        .await
        .unwrap();
    env.mem
        .decide(
            a,
            item.id,
            ApproveMemoryRequest {
                decision: MemoryDecision::Approve,
                edit: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(env.mem.item_vector_index().name(), "lance");
    assert_eq!(env.mem.item_vector_index().count().await.unwrap(), 1);
    let q = MemoryQuery {
        q: "how does FP deploy".into(),
        ..Default::default()
    };
    assert_eq!(env.mem.search(b, &q).await.unwrap().len(), 1);
    env.mem
        .update(
            a,
            item.id,
            UpdateMemoryRequest {
                visibility: Some(Visibility::Private),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(env.mem.search(b, &q).await.unwrap().is_empty());
    assert_eq!(env.mem.search(a, &q).await.unwrap().len(), 1);
    // Reopen: the Lance table persists and stays in sync.
    let env = env.reopen(cfg).await;
    assert_eq!(env.mem.item_vector_index().count().await.unwrap(), 1);
    assert_eq!(env.mem.search(a, &q).await.unwrap().len(), 1);
}

#[cfg(feature = "fastembed")]
#[tokio::test]
async fn fastembed_backend_end_to_end() {
    if std::env::var("KILN_TEST_FASTEMBED").as_deref() != Ok("1") {
        eprintln!("skipped: set KILN_TEST_FASTEMBED=1 (downloads ONNX models)");
        return;
    }
    let cfg = MemoryConfig {
        embedder: kiln_memory::EmbedderChoice::Fastembed { model: None },
        reranker: kiln_memory::RerankerChoice::Fastembed { model: None },
        ..Default::default()
    };
    let env = env_with(cfg).await;
    let a = add_user(&env.pool, "Ada Rao").await;
    env.mem
        .propose(
            a,
            None,
            prop(
                MemoryType::Fact,
                "uploads",
                "retry_with",
                "exponential backoff",
                Visibility::Private,
            ),
        )
        .await
        .unwrap();
    let hits = env
        .mem
        .search(
            a,
            &MemoryQuery {
                q: "what happens when an upload fails".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(env.mem.embedder().id().starts_with("fastembed:"));
}
