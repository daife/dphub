mod config;
mod quota;

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    body::Bytes,
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use config::Config;
use quota::{extract_total_tokens, parse_authorization, QuotaError, QuotaStore};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_cron_scheduler::{Job, JobScheduler};
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    quota: QuotaStore,
    client: Client,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let config_path = std::env::var("DPHUB_CONFIG").unwrap_or_else(|_| "config.toml".to_owned());
    let config = Arc::new(Config::load(config_path)?);
    let quota = QuotaStore::connect(&config.database.path).await?;
    let client = Client::builder()
        .timeout(config.deepseek_timeout())
        .build()
        .context("failed to build reqwest client")?;

    start_midnight_clear_job(quota.clone()).await?;

    let state = AppState {
        config: config.clone(),
        quota,
        client,
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/register", post(register_phone))
        .route("/v1/quota", get(get_quota))
        .route("/v1/invite-code", get(get_invite_code))
        .route("/v1/beta/chat/completions", post(chat_completions))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    info!("listening on {}", config.server.bind);
    let listener = tokio::net::TcpListener::bind(config.server.bind).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Debug, Deserialize)]
struct RegisterRequest {
    phone: String,
    #[serde(default)]
    invite_code: Option<String>,
}

#[derive(Debug, Serialize)]
struct RegisterResponse {
    phone: String,
    invite_code: String,
    user_id: String,
    pool_balance: i64,
}

async fn register_phone(
    State(state): State<AppState>,
    Json(request): Json<RegisterRequest>,
) -> Response {
    let result = state
        .quota
        .register_phone(
            &request.phone,
            request.invite_code.as_deref(),
            &state.config.quota,
        )
        .await;

    match result {
        Ok(result) => (
            StatusCode::CREATED,
            Json(RegisterResponse {
                phone: result.phone,
                invite_code: result.invite_code,
                user_id: result.user_id,
                pool_balance: result.pool_balance,
            }),
        )
            .into_response(),
        Err(err) => quota_error_response(err),
    }
}

#[derive(Debug, Serialize)]
struct QuotaResponse {
    used_tokens: i64,
    daily_limit: i64,
    usage_ratio: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pool_balance: Option<i64>,
}

async fn get_quota(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let principal = match parse_authorization(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
    ) {
        Ok(principal) => principal,
        Err(err) => return quota_error_response(err),
    };

    match state
        .quota
        .quota_status(&principal, &state.config.quota)
        .await
    {
        Ok(status) => Json(QuotaResponse {
            used_tokens: status.used_tokens,
            daily_limit: status.daily_limit,
            usage_ratio: status.usage_ratio,
            pool_balance: status.pool_balance,
        })
        .into_response(),
        Err(err) => quota_error_response(err),
    }
}

#[derive(Debug, Serialize)]
struct InviteCodeResponse {
    phone: String,
    invite_code: String,
}

async fn get_invite_code(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let principal = match parse_authorization(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
    ) {
        Ok(principal) => principal,
        Err(err) => return quota_error_response(err),
    };

    match state.quota.invite_code_for_principal(&principal).await {
        Ok(info) => Json(InviteCodeResponse {
            phone: info.phone,
            invite_code: info.invite_code,
        })
        .into_response(),
        Err(err) => quota_error_response(err),
    }
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let principal = match parse_authorization(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
    ) {
        Ok(principal) => principal,
        Err(err) => return quota_error_response(err),
    };

    if request_wants_stream(&body) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "stream=true is not supported because usage.total_tokens must be recorded"
            })),
        )
            .into_response();
    }

    let charge_target = match state.quota.reserve(&principal, &state.config.quota).await {
        Ok(target) => target,
        Err(err) => return quota_error_response(err),
    };

    let user_id = match state.quota.user_id_for_principal(&principal).await {
        Ok(user_id) => user_id,
        Err(err) => return quota_error_response(err),
    };

    let upstream_body = match attach_user_id(&body, &user_id) {
        Ok(body) => body,
        Err(response) => return response,
    };

    let upstream = state
        .client
        .post(&state.config.deepseek.endpoint)
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", state.config.deepseek.api_key),
        )
        .header(header::CONTENT_TYPE, "application/json")
        .body(upstream_body)
        .send()
        .await;

    let upstream = match upstream {
        Ok(response) => response,
        Err(err) => {
            error!(error = %err, "failed to call deepseek upstream");
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": "failed to call upstream"})),
            )
                .into_response();
        }
    };

    let status = upstream.status();
    let content_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("application/json"));
    let body = match upstream.bytes().await {
        Ok(body) => body,
        Err(err) => {
            error!(error = %err, "failed to read deepseek response body");
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": "failed to read upstream response"})),
            )
                .into_response();
        }
    };

    if status.is_success() {
        if let Some(total_tokens) = extract_total_tokens(&body) {
            if let Err(err) = state
                .quota
                .charge(&principal, charge_target, total_tokens)
                .await
            {
                error!(error = %err, "failed to record token usage");
            }
        } else {
            warn!("upstream response did not contain usage.total_tokens");
        }
    }

    let mut response = (status, body).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type);
    response
}

fn request_wants_stream(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.get("stream").and_then(Value::as_bool))
        .unwrap_or(false)
}

fn attach_user_id(body: &[u8], user_id: &str) -> std::result::Result<Vec<u8>, Response> {
    let mut value = serde_json::from_slice::<Value>(body).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "request body must be valid JSON"})),
        )
            .into_response()
    })?;

    let object = value.as_object_mut().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "request body must be a JSON object"})),
        )
            .into_response()
    })?;

    object.insert("user_id".to_owned(), Value::String(user_id.to_owned()));
    serde_json::to_vec(&value).map_err(|err| {
        error!(error = %err, "failed to encode upstream request body");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "failed to encode upstream request"})),
        )
            .into_response()
    })
}

fn quota_error_response(err: QuotaError) -> Response {
    match err {
        QuotaError::MissingAuthorization | QuotaError::InvalidAuthorization => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response(),
        QuotaError::QuotaExceeded => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({"error": "quota exceeded"})),
        )
            .into_response(),
        QuotaError::PhoneAlreadyRegistered => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "phone already registered"})),
        )
            .into_response(),
        QuotaError::InvalidInviteCode => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invite code does not exist"})),
        )
            .into_response(),
        QuotaError::PhoneRequired => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "phone is required to query invite code"})),
        )
            .into_response(),
        QuotaError::Database(err) => {
            error!(error = %err, "quota database error");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "quota database error"})),
            )
                .into_response()
        }
    }
}

async fn start_midnight_clear_job(quota: QuotaStore) -> Result<()> {
    let scheduler = JobScheduler::new().await?;
    let job = Job::new_async("0 0 0 * * *", move |_uuid, _lock| {
        let quota = quota.clone();
        Box::pin(async move {
            if let Err(err) = quota.clear_today_usage().await {
                error!(error = %err, "failed to clear daily usage");
            } else {
                info!("cleared daily usage");
            }
        })
    })?;
    scheduler.add(job).await?;
    scheduler.start().await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

fn init_tracing() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dphub=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_stream_true() {
        assert!(request_wants_stream(br#"{"stream":true}"#));
        assert!(!request_wants_stream(br#"{"stream":false}"#));
        assert!(!request_wants_stream(br#"{"model":"x"}"#));
        assert!(!request_wants_stream(b"not json"));
    }
}
