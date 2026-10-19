//! Admin-only routes: overview, users, allowed domain.

use crate::error::ApiResult;
use crate::extract::{Body, P};
use crate::session::AdminUser;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use kiln_types::api::*;
use kiln_types::{User, UserId, UserSpend};

async fn user_rows(st: &AppState) -> ApiResult<Vec<AdminUserRow>> {
    let mut rows = Vec::new();
    for user in st.store.list_users().await? {
        let month_usd = st.store.month_spend(user.id).await?;
        let running = st.store.count_live_runs(Some(user.id)).await? as u32;
        rows.push(AdminUserRow {
            spend: UserSpend {
                user_id: user.id,
                month_usd,
                budget_usd: user.limits.monthly_budget_usd,
            },
            running,
            user,
        });
    }
    Ok(rows)
}

pub async fn overview(
    State(st): State<AppState>,
    AdminUser(_me): AdminUser,
) -> ApiResult<Json<AdminOverview>> {
    Ok(Json(AdminOverview {
        machine: st.scheduler.machine(),
        users: user_rows(&st).await?,
        containers: st.scheduler.running_containers().await?,
        queue: st.scheduler.queue().await?,
        projects: st.store.list_projects().await?,
    }))
}

pub async fn users(
    State(st): State<AppState>,
    AdminUser(_me): AdminUser,
) -> ApiResult<Json<Vec<AdminUserRow>>> {
    Ok(Json(user_rows(&st).await?))
}

pub async fn create_user(
    State(st): State<AppState>,
    AdminUser(me): AdminUser,
    Body(req): Body<CreateUserRequest>,
) -> ApiResult<(StatusCode, Json<User>)> {
    let u = st.auth.create_user(&me.user, req).await?;
    st.settings_for(u.id).await?;
    Ok((StatusCode::CREATED, Json(u)))
}

pub async fn update_user(
    State(st): State<AppState>,
    AdminUser(me): AdminUser,
    P(id): P<UserId>,
    Body(req): Body<UpdateUserRequest>,
) -> ApiResult<Json<User>> {
    let u = st.auth.update_user(&me.user, id, req).await?;
    tracing::info!(by = %me.user.id, user = %id, "user updated by admin");
    // A newly disabled user may have freed capacity; let the dispatcher look.
    st.bus.publish(kiln_events::Event::SchedulerWake);
    Ok(Json(u))
}

pub async fn set_domain(
    State(st): State<AppState>,
    AdminUser(me): AdminUser,
    Body(req): Body<SetDomainRequest>,
) -> ApiResult<Json<ServerInfo>> {
    st.auth
        .set_allowed_domain(&me.user, &req.allowed_domain)
        .await?;
    Ok(Json(st.server_info().await?))
}
