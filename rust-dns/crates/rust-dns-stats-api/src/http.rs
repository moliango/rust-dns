// SPDX-License-Identifier: GPL-3.0-only
//! Axum HTTP surface for `/api/v1/*`.
//!
//! Binding is the caller's responsibility: [`serve`] takes an address and a
//! shutdown signal, and the v1 service only listens on loopback by policy.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::rejection::QueryRejection;
use axum::extract::{DefaultBodyLimit, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use crate::collector::{
    DEFAULT_HISTORY_POINTS, DEFAULT_LOGS_LIMIT, DEFAULT_TOP_LIMIT, LogFilter, LogQuery,
    StatsCollector,
};
use crate::types::{
    ErrorResponse, HealthResponse, HistoryResponse, LogsResponse, MAX_SEARCH_CHARS, StatsResponse,
    SuccessResponse, TopResponse, VersionResponse,
};

/// Maximum accepted POST body size in bytes (design §6).
pub const MAX_REQUEST_BODY: usize = 1024;
/// Per-request timeout (design §6).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum concurrent in-flight requests (design §6).
pub const MAX_IN_FLIGHT: usize = 64;

/// Cache plugin handle used by `POST /api/v1/cache/flush`.
#[async_trait]
pub trait CacheFlush: Send + Sync {
    /// Flush the cache. An error string is surfaced as a 500 response.
    async fn flush(&self) -> Result<(), String>;
}

#[derive(Clone)]
struct AppState {
    collector: Arc<StatsCollector>,
    cache: Option<Arc<dyn CacheFlush>>,
    semaphore: Arc<Semaphore>,
}

/// API error rendered as `{"error":"..."}` with an HTTP status code.
#[derive(Debug, Clone)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    /// 400 Bad Request with a contract-specific message.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    /// 404 Not Found.
    pub fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: "not found".to_string(),
        }
    }

    /// 405 Method Not Allowed.
    pub fn method_not_allowed() -> Self {
        Self {
            status: StatusCode::METHOD_NOT_ALLOWED,
            message: "method not allowed".to_string(),
        }
    }

    /// 413 Payload Too Large.
    pub fn payload_too_large() -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: "payload too large".to_string(),
        }
    }

    /// 503 Service Unavailable for concurrency saturation.
    pub fn service_unavailable() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "server busy".to_string(),
        }
    }

    /// 500 Internal Server Error; details are never leaked to clients.
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }

    /// HTTP status of this error.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// Client-facing message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response();
        apply_common_headers(&mut response);
        response
    }
}

fn apply_common_headers(response: &mut Response) {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // v1 disables keep-alive to bound slow-client impact.
    headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
}

/// Build the `/api/v1` router.
pub fn router(collector: Arc<StatsCollector>, cache: Option<Arc<dyn CacheFlush>>) -> Router {
    let state = AppState {
        collector,
        cache,
        semaphore: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
    };

    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/version", get(version))
        .route("/api/v1/stats", get(stats))
        .route("/api/v1/history", get(history))
        .route("/api/v1/top", get(top))
        .route("/api/v1/logs", get(logs))
        .route("/api/v1/logs/clear", post(clear_logs))
        .route("/api/v1/cache/flush", post(flush_cache))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// Serve on an already bound listener until `shutdown` resolves.
pub async fn serve_with_listener<F>(
    listener: TcpListener,
    collector: Arc<StatsCollector>,
    cache: Option<Arc<dyn CacheFlush>>,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, router(collector, cache))
        .with_graceful_shutdown(shutdown)
        .await
}

/// Bind `addr` and serve until `shutdown` resolves.
pub async fn serve<F>(
    addr: SocketAddr,
    collector: Arc<StatsCollector>,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    serve_with_cache(addr, collector, None, shutdown).await
}

/// Bind `addr` and serve with a cache flush callback.
pub async fn serve_with_cache<F>(
    addr: SocketAddr,
    collector: Arc<StatsCollector>,
    cache: Option<Arc<dyn CacheFlush>>,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind(addr).await?;
    serve_with_listener(listener, collector, cache, shutdown).await
}

/// Whether an address is a loopback address (v1 policy helper).
pub fn is_loopback_addr(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(state.collector.health())
}

async fn version() -> Json<VersionResponse> {
    Json(VersionResponse {
        version: crate::crate_version().to_string(),
    })
}

async fn stats(State(state): State<AppState>) -> Json<StatsResponse> {
    Json(state.collector.stats())
}

async fn history(
    State(state): State<AppState>,
    query: Result<Query<HashMap<String, String>>, QueryRejection>,
) -> Result<Json<HistoryResponse>, ApiError> {
    let params = parse_params(query)?;
    let max = state.collector.config().history_max;
    let points = match params.get("points") {
        None => DEFAULT_HISTORY_POINTS.min(max),
        Some(value) => parse_bounded(value, 1, max, format!("invalid points: expected 1..={max}"))?,
    };
    Ok(Json(HistoryResponse {
        points: state.collector.history(points),
    }))
}

async fn top(
    State(state): State<AppState>,
    query: Result<Query<HashMap<String, String>>, QueryRejection>,
) -> Result<Json<TopResponse>, ApiError> {
    let params = parse_params(query)?;
    let max = state.collector.config().top_max;
    let limit = match params.get("limit") {
        None => DEFAULT_TOP_LIMIT.min(max),
        Some(value) => parse_bounded(value, 1, max, format!("invalid limit: expected 1..={max}"))?,
    };
    Ok(Json(state.collector.top(limit)))
}

async fn logs(
    State(state): State<AppState>,
    query: Result<Query<HashMap<String, String>>, QueryRejection>,
) -> Result<Json<LogsResponse>, ApiError> {
    let params = parse_params(query)?;
    let max = state.collector.config().logs_max;

    let limit = match params.get("limit") {
        None => DEFAULT_LOGS_LIMIT.min(max),
        Some(value) => parse_bounded(value, 1, max, format!("invalid limit: expected 1..={max}"))?,
    };

    let offset = match params.get("offset") {
        None => 0,
        Some(value) => value
            .parse::<usize>()
            .map_err(|_| ApiError::bad_request("invalid offset"))?,
    };

    let filter = match params.get("filter") {
        None => LogFilter::All,
        Some(value) => LogFilter::parse(value)
            .ok_or_else(|| ApiError::bad_request("invalid filter: expected all|blocked|cached"))?,
    };

    let search = params.get("search").cloned().unwrap_or_default();
    if search.chars().count() > MAX_SEARCH_CHARS {
        return Err(ApiError::bad_request("search too long"));
    }

    Ok(Json(state.collector.logs(&LogQuery {
        limit,
        offset,
        search,
        filter,
    })))
}

async fn clear_logs(State(state): State<AppState>) -> Json<SuccessResponse> {
    state.collector.clear_logs();
    Json(SuccessResponse::ok())
}

async fn flush_cache(State(state): State<AppState>) -> Result<Json<SuccessResponse>, ApiError> {
    if let Some(cache) = &state.cache {
        cache
            .flush()
            .await
            .map_err(|error| ApiError::internal(format!("cache flush failed: {error}")))?;
    }
    Ok(Json(SuccessResponse::ok()))
}

async fn not_found() -> ApiError {
    ApiError::not_found()
}

async fn method_not_allowed() -> ApiError {
    ApiError::method_not_allowed()
}

fn parse_params(
    query: Result<Query<HashMap<String, String>>, QueryRejection>,
) -> Result<HashMap<String, String>, ApiError> {
    query
        .map(|Query(params)| params)
        .map_err(|_| ApiError::bad_request("invalid query"))
}

fn parse_bounded(value: &str, min: usize, max: usize, message: String) -> Result<usize, ApiError> {
    match value.parse::<usize>() {
        Ok(parsed) if (min..=max).contains(&parsed) => Ok(parsed),
        _ => Err(ApiError::bad_request(message)),
    }
}

async fn guard(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if let Some(length) = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        && length > MAX_REQUEST_BODY
    {
        return ApiError::payload_too_large().into_response();
    }

    let permit = match state.semaphore.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return ApiError::service_unavailable().into_response(),
    };

    let mut response = match tokio::time::timeout(REQUEST_TIMEOUT, async {
        // Buffer the body so the limit also covers chunked uploads.
        let (parts, body) = request.into_parts();
        let body = match axum::body::to_bytes(body, MAX_REQUEST_BODY).await {
            Ok(bytes) => Body::from(bytes),
            Err(_) => return ApiError::payload_too_large().into_response(),
        };
        next.run(Request::from_parts(parts, body)).await
    })
    .await
    {
        Ok(response) => response,
        Err(_) => ApiError::internal("internal error").into_response(),
    };
    drop(permit);
    apply_common_headers(&mut response);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bounded_values() {
        assert_eq!(parse_bounded("24", 1, 168, "bad".to_string()).unwrap(), 24);
        assert!(parse_bounded("0", 1, 168, "bad".to_string()).is_err());
        assert!(parse_bounded("abc", 1, 168, "bad".to_string()).is_err());
    }

    #[test]
    fn loopback_helper() {
        assert!(is_loopback_addr(&"127.0.0.1:9091".parse().unwrap()));
        assert!(!is_loopback_addr(&"192.168.1.1:9091".parse().unwrap()));
    }
}
