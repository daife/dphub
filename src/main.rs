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

    let upstream = state
        .client
        .post(&state.config.deepseek.endpoint)
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", state.config.deepseek.api_key),
        )
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
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
