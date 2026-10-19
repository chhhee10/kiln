//! Request authentication: `Authorization: Bearer <token>` or the
//! `kiln_session` HTTP-only cookie; the WebSocket route also accepts
//! `?token=`. Session cookies are `HttpOnly; SameSite=Strict; Path=/`, plus
//! `Secure` outside `--dev`.

use crate::error::AppError;
use crate::state::AppState;
use axum::extract::FromRequestParts;
use axum::http::header::{AUTHORIZATION, COOKIE, SET_COOKIE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue};
use kiln_types::User;
use kiln_types::api::SESSION_COOKIE;

/// The authenticated caller.
pub struct CurrentUser {
    pub user: User,
    /// The presented session token (for logout).
    pub token: String,
}

/// Admin-only extractor: 403 for members.
pub struct AdminUser(pub CurrentUser);

/// Pull a session token out of headers (bearer first, then cookie).
pub fn token_from_headers(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        if let Some(t) = v
            .strip_prefix("Bearer ")
            .or_else(|| v.strip_prefix("bearer "))
        {
            let t = t.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    for v in headers.get_all(COOKIE) {
        let Ok(s) = v.to_str() else { continue };
        for pair in s.split(';') {
            if let Some((k, val)) = pair.trim().split_once('=')
                && k.trim() == SESSION_COOKIE
                && !val.trim().is_empty()
            {
                return Some(val.trim().to_string());
            }
        }
    }
    None
}

/// `token=` from a query string.
pub fn token_from_query(query: Option<&str>) -> Option<String> {
    query?.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == "token" && !v.is_empty()).then(|| v.to_string())
    })
}

/// Resolve a token to an active user.
pub async fn authenticate(
    state: &AppState,
    token: Option<String>,
) -> Result<CurrentUser, AppError> {
    let token = token.ok_or_else(AppError::unauthorized)?;
    match state.auth.authenticate(&token).await {
        Ok(user) => Ok(CurrentUser { user, token }),
        Err(kiln_auth::AuthError::Unauthorized) => Err(AppError::unauthorized()),
        Err(e) => Err(e.into()),
    }
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        authenticate(state, token_from_headers(&parts.headers)).await
    }
}

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let u = CurrentUser::from_request_parts(parts, state).await?;
        if !u.user.is_admin() {
            return Err(AppError::forbidden("admin only"));
        }
        Ok(AdminUser(u))
    }
}

/// `Set-Cookie` for a new session.
pub fn session_cookie(token: &str, secure: bool) -> HeaderValue {
    let max_age = kiln_auth::SESSION_TTL_DAYS * 24 * 3600;
    let s = format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{}",
        if secure { "; Secure" } else { "" }
    );
    HeaderValue::from_str(&s).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// `Set-Cookie` that deletes the session cookie.
pub fn clear_cookie(secure: bool) -> HeaderValue {
    let s = format!(
        "{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{}",
        if secure { "; Secure" } else { "" }
    );
    HeaderValue::from_str(&s).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// Headers carrying a session cookie.
pub fn cookie_headers(token: &str, secure: bool) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(SET_COOKIE, session_cookie(token, secure));
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_bearer_then_cookie() {
        let mut h = HeaderMap::new();
        h.insert(
            COOKIE,
            HeaderValue::from_static("a=1; kiln_session=cookie-tok; b=2"),
        );
        assert_eq!(token_from_headers(&h).as_deref(), Some("cookie-tok"));
        h.insert(AUTHORIZATION, HeaderValue::from_static("Bearer bearer-tok"));
        assert_eq!(token_from_headers(&h).as_deref(), Some("bearer-tok"));
        assert_eq!(
            token_from_query(Some("x=1&token=abc")).as_deref(),
            Some("abc")
        );
        assert_eq!(token_from_query(Some("token=")), None);
    }

    #[test]
    fn cookie_flags() {
        let c = session_cookie("t", true);
        let s = c.to_str().unwrap();
        assert!(s.contains("HttpOnly") && s.contains("SameSite=Strict") && s.contains("Secure"));
        assert!(
            !session_cookie("t", false)
                .to_str()
                .unwrap()
                .contains("Secure")
        );
    }
}
