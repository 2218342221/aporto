//! HTTP adapter. All agent state and operations cross the Core JSON-RPC boundary.
mod events;
use aporto_core_client::{CoreClient, CoreError};
use aporto_protocol as protocol;
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, Request, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use events::{events, events_page};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, sync::Arc, time::Duration};
use subtle::ConstantTimeEq;
use tokio_util::sync::CancellationToken;
use tower_http::cors::CorsLayer;

const BODY_LIMIT: usize = 1024 * 1024;
pub struct ServerConfig {
    pub bearer_token: String,
    pub allowed_origins: Vec<String>,
    pub event_poll_interval: Duration,
    pub request_timeout: Duration,
    pub max_requests: usize,
    pub max_streams: usize,
}
impl ServerConfig {
    pub fn new(bearer_token: String, allowed_origins: Vec<String>) -> Self {
        Self {
            bearer_token,
            allowed_origins,
            event_poll_interval: Duration::from_millis(250),
            request_timeout: Duration::from_secs(30),
            max_requests: 128,
            max_streams: 64,
        }
    }
}
#[derive(Clone)]
struct AppState {
    core: CoreClient,
    token_hash: [u8; 32],
    allowed_origins: BTreeSet<String>,
    poll_interval: Duration,
    shutdown: CancellationToken,
    request_timeout: Duration,
    requests: Arc<tokio::sync::Semaphore>,
    streams: Arc<tokio::sync::Semaphore>,
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: i32,
    message: String,
}
#[derive(Serialize)]
struct ErrorEnvelope {
    error: protocol::RpcError,
}
impl ApiError {
    fn new(status: StatusCode, code: i32, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
    fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, protocol::INVALID_PARAMS, message)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(ErrorEnvelope {
                error: protocol::RpcError::new(self.code, self.message),
            }),
        )
            .into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        if self.status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}
impl From<CoreError> for ApiError {
    fn from(error: CoreError) -> Self {
        match error {
            CoreError::Timeout => Self::new(
                StatusCode::GATEWAY_TIMEOUT,
                protocol::UNAVAILABLE,
                "Core request timed out; operation completion is unknown",
            ),
            CoreError::Overloaded => Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                protocol::OVERLOADED,
                "Core request capacity reached",
            ),
            CoreError::Rpc { code, message, .. } => {
                let status = match code {
                    protocol::NOT_FOUND => StatusCode::NOT_FOUND,
                    protocol::CONFLICT => StatusCode::CONFLICT,
                    protocol::INVALID_REQUEST | protocol::INVALID_PARAMS => StatusCode::BAD_REQUEST,
                    protocol::OVERLOADED => StatusCode::TOO_MANY_REQUESTS,
                    protocol::UNAVAILABLE => StatusCode::SERVICE_UNAVAILABLE,
                    _ => StatusCode::BAD_GATEWAY,
                };
                let public: String = if status == StatusCode::BAD_GATEWAY {
                    "Core request failed".into()
                } else {
                    message.chars().take(512).collect()
                };
                Self::new(status, code, public)
            }
            _ => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                protocol::UNAVAILABLE,
                "Core process is unavailable",
            ),
        }
    }
}

pub fn router(
    core: CoreClient,
    config: ServerConfig,
    shutdown: CancellationToken,
) -> Result<Router, String> {
    if config.bearer_token.len() < 32
        || config.bearer_token.len() > 4096
        || !config
            .bearer_token
            .bytes()
            .all(|byte| byte.is_ascii_graphic())
    {
        return Err(
            "operator bearer token must contain 32 to 4096 visible ASCII characters".into(),
        );
    }
    if config.event_poll_interval < Duration::from_millis(10) {
        return Err("event poll interval must be at least 10 ms".into());
    }
    if config.request_timeout.is_zero() || config.max_requests == 0 || config.max_streams == 0 {
        return Err("request deadline and concurrency limits must be positive".into());
    }
    let mut origins = BTreeSet::new();
    let mut origin_headers = Vec::new();
    for origin in config.allowed_origins {
        let url = url::Url::parse(&origin).map_err(|_| "invalid allowed origin".to_string())?;
        if !matches!(url.scheme(), "http" | "https") || url.origin().ascii_serialization() != origin
        {
            return Err(
                "allowed origins must be exact HTTP(S) origins without paths or wildcards".into(),
            );
        }
        origin_headers
            .push(HeaderValue::from_str(&origin).map_err(|_| "invalid origin header".to_string())?);
        origins.insert(origin);
    }
    let state = Arc::new(AppState {
        core,
        token_hash: Sha256::digest(config.bearer_token.as_bytes()).into(),
        allowed_origins: origins,
        poll_interval: config.event_poll_interval,
        shutdown,
        request_timeout: config.request_timeout,
        requests: Arc::new(tokio::sync::Semaphore::new(config.max_requests)),
        streams: Arc::new(tokio::sync::Semaphore::new(config.max_streams)),
    });
    let api = Router::new()
        .route("/v1/agents", get(agents))
        .route("/v1/agents/{id}/instances", get(runtime_instances))
        .route("/v1/threads", get(threads).post(start_thread))
        .route("/v1/threads/{id}", get(read_thread))
        .route("/v1/threads/{id}/turns", post(start_turn))
        .route("/v1/threads/{id}/turns/{turn}/items", get(items))
        .route(
            "/v1/threads/{id}/turns/{turn}/interrupt",
            post(interrupt_turn),
        )
        .route("/v1/threads/{id}/events", get(events))
        .route("/v1/threads/{id}/events/page", get(events_page))
        .layer(middleware::from_fn_with_state(state.clone(), authorize));
    let cors = CorsLayer::new()
        .allow_origin(origin_headers)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::HeaderName::from_static("last-event-id"),
        ]);
    Ok(Router::new()
        .route("/healthz", get(health))
        .route("/readinessz", get(readiness))
        .merge(api)
        .fallback(|| async {
            ApiError::new(
                StatusCode::NOT_FOUND,
                protocol::NOT_FOUND,
                "route not found",
            )
        })
        .method_not_allowed_fallback(|| async {
            ApiError::new(
                StatusCode::METHOD_NOT_ALLOWED,
                protocol::METHOD_NOT_FOUND,
                "method not allowed",
            )
        })
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(cors)
        .layer(middleware::from_fn_with_state(state.clone(), check_origin))
        .with_state(state))
}

async fn authorize(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let values = request.headers().get_all(header::AUTHORIZATION);
    let mut values = values.iter();
    let candidate = values
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
        .map(|(_, token)| token);
    let authorized = candidate
        .filter(|value| value.len() <= 4096)
        .map(|value| {
            let digest: [u8; 32] = Sha256::digest(value.as_bytes()).into();
            bool::from(digest.ct_eq(&state.token_hash))
        })
        .unwrap_or(false)
        && values.next().is_none();
    if !authorized {
        return ApiError::new(
            StatusCode::UNAUTHORIZED,
            -32001,
            "bearer authentication required",
        )
        .into_response();
    }
    if state.shutdown.is_cancelled() {
        return ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            protocol::UNAVAILABLE,
            "Server is shutting down",
        )
        .into_response();
    }
    let stream = request.method() == Method::GET && request.uri().path().ends_with("/events");
    let _permit = if stream {
        None
    } else {
        match state.requests.try_acquire() {
            Ok(permit) => Some(permit),
            Err(_) => {
                return ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    protocol::OVERLOADED,
                    "HTTP request capacity reached",
                )
                .into_response();
            }
        }
    };
    // Only the handshake is timed for SSE; its response body has no total deadline.
    let mut response = match tokio::time::timeout(state.request_timeout, next.run(request)).await {
        Ok(response) => response,
        Err(_) => {
            return ApiError::new(
                StatusCode::REQUEST_TIMEOUT,
                protocol::UNAVAILABLE,
                "HTTP request timed out; operation completion may be unknown",
            )
            .into_response();
        }
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn check_origin(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let mut origins = request.headers().get_all(header::ORIGIN).iter();
    if let Some(origin) = origins.next()
        && (origins.next().is_some()
            || !origin
                .to_str()
                .is_ok_and(|origin| state.allowed_origins.contains(origin)))
    {
        return ApiError::new(StatusCode::FORBIDDEN, -32003, "origin is not allowed")
            .into_response();
    }
    next.run(request).await
}

async fn health() -> Json<Value> {
    Json(json!({"status":"ok"}))
}
async fn readiness(State(state): State<Arc<AppState>>) -> Response {
    let ready = state.core.is_available() && !state.shutdown.is_cancelled();
    (
        if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(json!({"status":if ready {"ok"}else{"unavailable"}})),
    )
        .into_response()
}
fn query<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    query
        .map(|Query(value)| value)
        .map_err(|_| ApiError::invalid("invalid query parameters"))
}
fn body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    body.map(|Json(value)| value).map_err(|error| {
        if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                protocol::INVALID_PARAMS,
                "request body exceeds 1 MiB",
            )
        } else {
            ApiError::invalid("invalid JSON request body")
        }
    })
}
fn identifier(id: &str) -> Result<(), ApiError> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Err(ApiError::invalid("invalid resource ID"))
    } else {
        Ok(())
    }
}

async fn agents(
    State(state): State<Arc<AppState>>,
) -> Result<Json<protocol::AgentListResult>, ApiError> {
    Ok(Json(state.core.call("agent/list", json!({})).await?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstancesQuery {
    cursor: Option<String>,
    limit: Option<u32>,
}
async fn runtime_instances(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    params: Result<Query<InstancesQuery>, QueryRejection>,
) -> Result<Json<protocol::RuntimeInstanceListResult>, ApiError> {
    identifier(&agent_id)?;
    let params = query(params)?;
    Ok(Json(
        state
            .core
            .call(
                "runtime/instances",
                protocol::RuntimeInstanceListParams {
                    agent_id,
                    cursor: params.cursor,
                    limit: params.limit,
                },
            )
            .await?,
    ))
}
async fn threads(
    State(state): State<Arc<AppState>>,
    params: Result<Query<protocol::ThreadListParams>, QueryRejection>,
) -> Result<Json<protocol::ThreadListResult>, ApiError> {
    Ok(Json(state.core.call("thread/list", query(params)?).await?))
}
async fn start_thread(
    State(state): State<Arc<AppState>>,
    params: Result<Json<protocol::ThreadStartParams>, JsonRejection>,
) -> Result<(StatusCode, Json<protocol::Thread>), ApiError> {
    Ok((
        StatusCode::CREATED,
        Json(state.core.call("thread/start", body(params)?).await?),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadQuery {
    limit: Option<u32>,
    before: Option<String>,
}
async fn read_thread(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    params: Result<Query<ReadQuery>, QueryRejection>,
) -> Result<Json<protocol::ThreadReadResult>, ApiError> {
    identifier(&id)?;
    let params = query(params)?;
    Ok(Json(
        state
            .core
            .call(
                "thread/read",
                protocol::ThreadReadParams {
                    thread_id: id,
                    limit: params.limit,
                    before: params.before,
                },
            )
            .await?,
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TurnBody {
    input: String,
    idempotency_key: String,
}
async fn start_turn(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    params: Result<Json<TurnBody>, JsonRejection>,
) -> Result<(StatusCode, Json<protocol::Turn>), ApiError> {
    identifier(&id)?;
    let params = body(params)?;
    if params.input.trim().is_empty()
        || params.idempotency_key.is_empty()
        || params.idempotency_key.len() > 128
    {
        return Err(ApiError::invalid("input and idempotency_key are required"));
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(
            state
                .core
                .call(
                    "turn/start",
                    protocol::TurnStartParams {
                        thread_id: id,
                        input: params.input,
                        idempotency_key: params.idempotency_key,
                    },
                )
                .await?,
        ),
    ))
}
async fn interrupt_turn(
    State(state): State<Arc<AppState>>,
    Path((id, turn)): Path<(String, String)>,
) -> Result<Json<protocol::Turn>, ApiError> {
    identifier(&id)?;
    identifier(&turn)?;
    Ok(Json(
        state
            .core
            .call(
                "turn/interrupt",
                protocol::TurnInterruptParams {
                    thread_id: id,
                    turn_id: turn,
                },
            )
            .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemQuery {
    after: Option<u64>,
    limit: Option<u32>,
}
async fn items(
    State(state): State<Arc<AppState>>,
    Path((thread_id, turn_id)): Path<(String, String)>,
    params: Result<Query<ItemQuery>, QueryRejection>,
) -> Result<Json<protocol::ItemListResult>, ApiError> {
    identifier(&thread_id)?;
    identifier(&turn_id)?;
    let params = query(params)?;
    Ok(Json(
        state
            .core
            .call(
                "item/list",
                protocol::ItemListParams {
                    thread_id,
                    turn_id,
                    after: params.after,
                    limit: params.limit,
                },
            )
            .await?,
    ))
}
