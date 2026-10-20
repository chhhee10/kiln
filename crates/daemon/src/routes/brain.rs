//! Company brain (memory gateway) and the credential vault.
//!
//! Memory: every call goes through `kiln_memory::Memory` with the requesting
//! user, which enforces visibility on every read.
//! Credentials: the vault scopes every call to the caller; secrets are never
//! returned, only masked summaries.

use crate::error::{ApiResult, AppError};
use crate::extract::{Body, P, Q};
use crate::session::CurrentUser;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use kiln_events::Event;
use kiln_types::api::*;
use kiln_types::event::ServerEvent;
use kiln_types::memory::*;
use kiln_types::{CredentialId, CredentialSummary, MemoryId, ProjectId};
use serde::Deserialize;

/// `GET /memory` query; lenient (every field optional) so browsers can omit `q`.
#[derive(Debug, Default, Deserialize)]
pub struct MemoryParams {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
    #[serde(default)]
    pub kind: Option<MemoryType>,
    #[serde(default)]
    pub pending_only: bool,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub token_budget: Option<u32>,
}

impl From<MemoryParams> for MemoryQuery {
    fn from(p: MemoryParams) -> Self {
        MemoryQuery {
            q: p.q,
            project_id: p.project_id,
            visibility: p.visibility,
            kind: p.kind,
            pending_only: p.pending_only,
            limit: p.limit.map(|l| l.min(500)),
            token_budget: p.token_budget,
        }
    }
}

pub async fn list_memory(
    State(st): State<AppState>,
    me: CurrentUser,
    Q(q): Q<MemoryParams>,
) -> ApiResult<Json<MemoryListResponse>> {
    let q: MemoryQuery = q.into();
    Ok(Json(st.memory.list(me.user.id, &q).await?))
}

pub async fn get_memory(
    State(st): State<AppState>,
    me: CurrentUser,
    P(id): P<MemoryId>,
) -> ApiResult<Json<MemoryDetail>> {
    Ok(Json(st.memory.get(me.user.id, id).await?))
}

/// Tell clients (visibility-filtered in the WS fan-out) and the pack builder.
pub fn publish_memory(st: &AppState, item: &MemoryItem) {
    st.bus
        .server(ServerEvent::MemoryUpserted { item: item.clone() });
    st.bus.publish(Event::MemoryChanged {
        owner_id: item.owner_id,
        project_id: item.project_id,
    });
}

pub async fn update_memory(
    State(st): State<AppState>,
    me: CurrentUser,
    P(id): P<MemoryId>,
    Body(req): Body<UpdateMemoryRequest>,
) -> ApiResult<Json<MemoryItem>> {
    let item = st.memory.update(me.user.id, id, req).await?;
    publish_memory(&st, &item);
    Ok(Json(item))
}

pub async fn decide_memory(
    State(st): State<AppState>,
    me: CurrentUser,
    P(id): P<MemoryId>,
    Body(req): Body<ApproveMemoryRequest>,
) -> ApiResult<Json<MemoryItem>> {
    let item = st.memory.decide(me.user.id, id, req).await?;
    publish_memory(&st, &item);
    Ok(Json(item))
}

// ---------------------------------------------------------------- credentials

pub async fn list_credentials(
    State(st): State<AppState>,
    me: CurrentUser,
) -> ApiResult<Json<Vec<CredentialSummary>>> {
    Ok(Json(st.vault.list(me.user.id).await?))
}

pub async fn add_credential(
    State(st): State<AppState>,
    me: CurrentUser,
    Body(req): Body<CreateCredentialRequest>,
) -> ApiResult<(StatusCode, Json<CredentialSummary>)> {
    if req.secret.len() > 16 * 1024 {
        return Err(AppError::invalid("secret is too long"));
    }
    let c = st.vault.add(me.user.id, req).await?;
    tracing::info!(user = %me.user.id, credential = %c.id, kind = ?c.kind, "credential added");
    Ok((StatusCode::CREATED, Json(c)))
}

pub async fn rotate_credential(
    State(st): State<AppState>,
    me: CurrentUser,
    P(id): P<CredentialId>,
    Body(req): Body<RotateCredentialRequest>,
) -> ApiResult<Json<CredentialSummary>> {
    if req.secret.len() > 16 * 1024 {
        return Err(AppError::invalid("secret is too long"));
    }
    let c = st.vault.rotate(me.user.id, id, req).await?;
    tracing::info!(user = %me.user.id, credential = %id, "credential rotated");
    Ok(Json(c))
}

pub async fn revoke_credential(
    State(st): State<AppState>,
    me: CurrentUser,
    P(id): P<CredentialId>,
) -> ApiResult<StatusCode> {
    st.vault.revoke(me.user.id, id).await?;
    tracing::info!(user = %me.user.id, credential = %id, "credential revoked");
    Ok(StatusCode::NO_CONTENT)
}

pub async fn test_credential(
    State(st): State<AppState>,
    me: CurrentUser,
    P(id): P<CredentialId>,
) -> ApiResult<Json<CredentialTestResponse>> {
    Ok(Json(st.vault.test(me.user.id, id).await?))
}
