//! Info, setup, login/registration, pairing, devices, settings, harnesses, machine.

use crate::error::{ApiResult, AppError};
use crate::extract::{Body, ClientIp};
use crate::session::{CurrentUser, clear_cookie, cookie_headers};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use kiln_store::NewDevice;
use kiln_types::api::*;
use kiln_types::{ClientKind, Device, MachineStats, User, UserSettings};

pub async fn info(State(st): State<AppState>) -> ApiResult<Json<ServerInfo>> {
    Ok(Json(st.server_info().await?))
}

fn rate_limit(st: &AppState, ip: std::net::IpAddr) -> ApiResult<()> {
    if st.limiter.check(ip) {
        Ok(())
    } else {
        tracing::warn!(%ip, "auth rate limit hit");
        Err(AppError::too_many())
    }
}

/// Successful sign-in: JSON body plus the session cookie.
fn signed_in(st: &AppState, user: User, token: String) -> impl IntoResponse + use<> {
    let headers = cookie_headers(&token, st.secure_cookies());
    (headers, Json(LoginResponse { token, user }))
}

pub async fn setup(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    Body(req): Body<SetupRequest>,
) -> ApiResult<impl IntoResponse> {
    rate_limit(&st, ip)?;
    let (user, token) = st.auth.setup(req).await?;
    st.settings_for(user.id).await?;
    tracing::info!(user = %user.id, "first-run setup complete");
    Ok(signed_in(&st, user, token))
}

pub async fn login(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    Body(req): Body<LoginRequest>,
) -> ApiResult<impl IntoResponse> {
    rate_limit(&st, ip)?;
    let (user, token) = st.auth.login(req).await?;
    Ok(signed_in(&st, user, token))
}

pub async fn register(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    Body(req): Body<RegisterRequest>,
) -> ApiResult<impl IntoResponse> {
    rate_limit(&st, ip)?;
    let (user, token) = st.auth.register(req).await?;
    st.settings_for(user.id).await?;
    tracing::info!(user = %user.id, "user self-registered");
    Ok(signed_in(&st, user, token))
}

pub async fn logout(State(st): State<AppState>, me: CurrentUser) -> ApiResult<impl IntoResponse> {
    st.auth.logout(&me.token).await?;
    let mut h = HeaderMap::new();
    h.insert(SET_COOKIE, clear_cookie(st.secure_cookies()));
    Ok((StatusCode::NO_CONTENT, h))
}

pub async fn me(State(st): State<AppState>, me: CurrentUser) -> ApiResult<Json<MeResponse>> {
    let settings = st.settings_for(me.user.id).await?;
    Ok(Json(MeResponse {
        user: me.user,
        settings,
        server: st.server_info().await?,
    }))
}

/// `kiln://pair?url=<server_url>&code=<code>`.
pub fn pair_uri(server_url: &str, code: &str) -> String {
    format!(
        "kiln://pair?url={}&code={}",
        crate::web::pct(server_url),
        crate::web::pct(code)
    )
}

/// SVG QR code of `data`.
pub fn qr_svg(data: &str) -> ApiResult<String> {
    use qrcode::render::svg;
    let code = qrcode::QrCode::with_error_correction_level(data.as_bytes(), qrcode::EcLevel::M)
        .map_err(AppError::internal)?;
    Ok(code
        .render::<svg::Color<'_>>()
        .min_dimensions(240, 240)
        .quiet_zone(true)
        .dark_color(svg::Color("#0f1115"))
        .light_color(svg::Color("#ffffff"))
        .build())
}

pub async fn pair_start(
    State(st): State<AppState>,
    me: CurrentUser,
) -> ApiResult<Json<PairStartResponse>> {
    let (code, expires_at) = st.auth.start_pairing(&me.user).await?;
    let server_url = st.public_url.clone();
    let uri = pair_uri(&server_url, &code);
    let svg = qr_svg(&uri)?;
    tracing::info!(user = %me.user.id, "pairing code issued");
    Ok(Json(PairStartResponse {
        code,
        server_url,
        pair_uri: uri,
        qr_svg: svg,
        expires_at,
    }))
}

pub async fn pair_complete(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    Body(req): Body<PairCompleteRequest>,
) -> ApiResult<impl IntoResponse> {
    rate_limit(&st, ip)?;
    let (user, token) = st.auth.complete_pairing(req).await?;
    tracing::info!(user = %user.id, "device paired");
    Ok(signed_in(&st, user, token))
}

pub async fn register_push(
    State(st): State<AppState>,
    me: CurrentUser,
    Body(req): Body<RegisterPushRequest>,
) -> ApiResult<Json<Device>> {
    let ep = req.endpoint.trim();
    if !(ep.starts_with("https://") || ep.starts_with("http://")) || ep.len() > 2048 {
        return Err(AppError::invalid("push endpoint must be an http(s) URL"));
    }
    let device = st
        .store
        .upsert_device(NewDevice {
            id: None,
            user_id: me.user.id,
            kind: ClientKind::Android,
            name: req.device_name.clone(),
        })
        .await?;
    let keys = match (req.p256dh.as_deref(), req.auth.as_deref()) {
        (Some(p256dh), Some(auth)) => {
            let k = kiln_notifier::SubscriptionKeys {
                p256dh: p256dh.trim().into(),
                auth: auth.trim().into(),
            };
            kiln_notifier::validate_keys(&k).map_err(|e| AppError::invalid(e.to_string()))?;
            Some(k)
        }
        (None, None) => None,
        _ => return Err(AppError::invalid("send both p256dh and auth, or neither")),
    };
    st.store
        .set_push_subscription(
            device.id,
            Some(ep),
            keys.as_ref().map(|k| (k.p256dh.as_str(), k.auth.as_str())),
        )
        .await?;
    Ok(Json(st.store.get_device(device.id).await?))
}

pub async fn get_settings(
    State(st): State<AppState>,
    me: CurrentUser,
) -> ApiResult<Json<UserSettings>> {
    Ok(Json(st.settings_for(me.user.id).await?))
}

/// Notification channel keys accepted in settings.
const CHANNELS: [&str; 4] = ["decisions", "failures", "ready_for_review", "live_workers"];

pub async fn put_settings(
    State(st): State<AppState>,
    me: CurrentUser,
    Body(mut s): Body<UserSettings>,
) -> ApiResult<Json<UserSettings>> {
    for (role, choice) in &s.worker_defaults {
        if !st.catalog.harnesses.iter().any(|h| h.id == choice.harness) {
            return Err(AppError::invalid(format!(
                "{}: unknown harness {:?}",
                role.label(),
                choice.harness
            )));
        }
        if choice.model.trim().is_empty() || choice.model.len() > 200 {
            return Err(AppError::invalid(format!(
                "{}: model is required",
                role.label()
            )));
        }
    }
    if let Some(bad) = s
        .notification_channels
        .keys()
        .find(|k| !CHANNELS.contains(&k.as_str()))
    {
        return Err(AppError::invalid(format!(
            "unknown notification channel {bad:?}"
        )));
    }
    // Roles the user didn't set keep the server defaults.
    for (role, c) in &st.default_workers {
        s.worker_defaults.entry(*role).or_insert_with(|| c.clone());
    }
    st.store.set_user_settings(me.user.id, &s).await?;
    Ok(Json(s))
}

pub async fn harnesses(State(st): State<AppState>, _me: CurrentUser) -> Json<HarnessCatalog> {
    Json(st.catalog.clone())
}

pub async fn machine(State(st): State<AppState>, _me: CurrentUser) -> Json<MachineStats> {
    Json(st.scheduler.machine())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_uri_and_qr() {
        let u = pair_uri("https://mini.ts.net", "ABCD2345");
        assert_eq!(u, "kiln://pair?url=https%3A%2F%2Fmini.ts.net&code=ABCD2345");
        let svg = qr_svg(&u).unwrap();
        assert!(svg.contains("<svg") && svg.contains("</svg>"));
    }
}
