//! Extractors whose rejections are `ApiError` JSON instead of axum's plain
//! text, so every client sees the same error shape.

use crate::error::AppError;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

/// JSON request body.
pub struct Body<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Body<T> {
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(v)) => Ok(Body(v)),
            Err(e) => Err(json_rejection(e)),
        }
    }
}

fn json_rejection(e: JsonRejection) -> AppError {
    match e {
        JsonRejection::MissingJsonContentType(_) => AppError::new(
            axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid",
            "expected Content-Type: application/json",
        ),
        JsonRejection::BytesRejection(b)
            if b.status() == axum::http::StatusCode::PAYLOAD_TOO_LARGE =>
        {
            AppError::new(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                "invalid",
                "request body too large",
            )
        }
        other => AppError::invalid(other.body_text()),
    }
}

/// Query string.
pub struct Q<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for Q<T> {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Query::<T>::from_request_parts(parts, state)
            .await
            .map(|q| Q(q.0))
            .map_err(|e: QueryRejection| AppError::invalid(e.body_text()))
    }
}

/// Path parameters. Malformed ids are a 404 (the resource can't exist).
pub struct P<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for P<T> {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Path::<T>::from_request_parts(parts, state)
            .await
            .map(|p| P(p.0))
            .map_err(|_: PathRejection| AppError::not_found("resource"))
    }
}

/// The client's IP for rate limiting. `X-Forwarded-For` is honoured only
/// when the TCP peer is loopback (i.e. `tailscale serve` proxying for us).
pub struct ClientIp(pub std::net::IpAddr);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|c| c.0.ip())
            .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        if peer.is_loopback()
            && let Some(fwd) = parts
                .headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
            && let Some(ip) = fwd.split(',').next().and_then(|s| s.trim().parse().ok())
        {
            return Ok(ClientIp(ip));
        }
        Ok(ClientIp(peer))
    }
}
