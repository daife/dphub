mod config;
mod quota;

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use config::Config;
use quota::{
    extract_total_tokens, parse_authorization, AdminUserFilters, AdminUserSort, QuotaError,
    QuotaStore,
};
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
        .route("/admin", get(admin_page))
        .route("/admin/api/overview", get(admin_overview))
        .route("/admin/api/pool/grant", post(admin_grant_pool))
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

async fn admin_page() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        ADMIN_HTML,
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
struct AdminQuery {
    token: Option<String>,
    q: Option<String>,
    min_pool: Option<i64>,
    max_pool: Option<i64>,
    min_used: Option<i64>,
    max_used: Option<i64>,
    over_daily_limit: Option<bool>,
    limit: Option<usize>,
    offset: Option<usize>,
    sort: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AdminGrantPoolRequest {
    phone: String,
    amount: i64,
}

async fn admin_overview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers, query.token.as_deref()) {
        return response;
    }

    let filters = AdminUserFilters {
        query: query.q,
        min_pool: query.min_pool,
        max_pool: query.max_pool,
        min_used: query.min_used,
        max_used: query.max_used,
        over_daily_limit: query.over_daily_limit,
        limit: query.limit.unwrap_or(50),
        offset: query.offset.unwrap_or(0),
        sort: parse_admin_sort(query.sort.as_deref()),
    };

    match state
        .quota
        .admin_overview(&state.config.quota, filters)
        .await
    {
        Ok(overview) => Json(overview).into_response(),
        Err(err) => quota_error_response(err),
    }
}

async fn admin_grant_pool(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
    Json(request): Json<AdminGrantPoolRequest>,
) -> Response {
    if let Err(response) = require_admin(&state, &headers, query.token.as_deref()) {
        return response;
    }

    match state
        .quota
        .admin_grant_phone_pool(&request.phone, request.amount)
        .await
    {
        Ok(result) => Json(result).into_response(),
        Err(err) => quota_error_response(err),
    }
}

fn require_admin(
    state: &AppState,
    headers: &HeaderMap,
    query_token: Option<&str>,
) -> std::result::Result<(), Response> {
    let expected = state.config.admin.token.trim();
    let header_token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .or_else(|| {
            headers
                .get("x-admin-token")
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
        });

    let supplied = header_token.or_else(|| query_token.map(str::trim));
    if supplied.is_some_and(|token| token == expected) {
        return Ok(());
    }

    Err((
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({"error": "admin authorization required"})),
    )
        .into_response())
}

fn parse_admin_sort(value: Option<&str>) -> AdminUserSort {
    match value.unwrap_or_default() {
        "phone_desc" => AdminUserSort::PhoneDesc,
        "used_desc" => AdminUserSort::UsedDesc,
        "used_asc" => AdminUserSort::UsedAsc,
        "pool_desc" => AdminUserSort::PoolDesc,
        "pool_asc" => AdminUserSort::PoolAsc,
        _ => AdminUserSort::PhoneAsc,
    }
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
        QuotaError::PhoneNotFound => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "phone account does not exist"})),
        )
            .into_response(),
        QuotaError::InvalidAdminRequest => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid admin request"})),
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

const ADMIN_HTML: &str = r#"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>dphub 管理后台</title>
  <style>
    :root { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; background: #f4f6f8; color: #17202a; }
    * { box-sizing: border-box; }
    body { margin: 0; }
    button, input, select { font: inherit; }
    button { border: 0; border-radius: 6px; background: #2563eb; color: #fff; cursor: pointer; font-weight: 650; padding: 10px 14px; }
    button.secondary { background: #fff; border: 1px solid #cfd6df; color: #1f2937; }
    button.danger { background: #b91c1c; }
    button:disabled { cursor: not-allowed; opacity: .55; }
    input, select { width: 100%; border: 1px solid #cfd6df; border-radius: 6px; color: #111827; padding: 9px 10px; background: #fff; }
    label { display: grid; gap: 6px; color: #5b6472; font-size: 13px; }
    header { background: #111827; color: #fff; }
    .topbar { max-width: 1180px; margin: 0 auto; padding: 16px 22px; display: flex; align-items: center; justify-content: space-between; gap: 12px; }
    h1 { font-size: 20px; margin: 0; letter-spacing: 0; }
    h2 { font-size: 16px; margin: 0 0 14px; letter-spacing: 0; }
    main { max-width: 1180px; margin: 0 auto; padding: 22px; }
    .login-shell { min-height: calc(100vh - 58px); display: grid; place-items: center; padding: 24px; }
    .login-card { width: min(420px, 100%); background: #fff; border: 1px solid #dfe4ec; border-radius: 8px; padding: 22px; box-shadow: 0 10px 32px rgba(17, 24, 39, .08); }
    .login-card p { color: #5b6472; line-height: 1.5; margin: 4px 0 18px; }
    .panel { background: #fff; border: 1px solid #dfe4ec; border-radius: 8px; padding: 18px; margin-bottom: 16px; }
    .stats { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 12px; }
    .stat { border: 1px solid #e5e9f0; border-radius: 8px; padding: 14px; background: #fbfcfd; color: #5b6472; }
    .stat strong { display: block; color: #111827; font-size: 24px; margin-top: 6px; }
    .toolbar { display: grid; grid-template-columns: 2fr repeat(5, 1fr); gap: 12px; align-items: end; }
    .actions { display: flex; justify-content: space-between; gap: 10px; flex-wrap: wrap; margin-top: 12px; }
    .grant { display: grid; grid-template-columns: 1fr 1fr auto; gap: 12px; align-items: end; }
    .table-wrap { overflow-x: auto; }
    table { width: 100%; border-collapse: collapse; font-size: 14px; }
    th, td { border-bottom: 1px solid #edf0f4; padding: 11px 9px; text-align: left; white-space: nowrap; }
    th { background: #f9fafb; color: #5b6472; font-size: 12px; }
    .status { min-height: 22px; color: #5b6472; margin-top: 10px; }
    .error { color: #b91c1c; }
    .ok { color: #047857; }
    .hidden { display: none !important; }
    @media (max-width: 960px) { .stats { grid-template-columns: repeat(2, minmax(0, 1fr)); } .toolbar { grid-template-columns: repeat(2, minmax(0, 1fr)); } .grant { grid-template-columns: 1fr; } }
    @media (max-width: 560px) { main { padding: 14px; } .topbar { padding: 14px; } .stats, .toolbar { grid-template-columns: 1fr; } }
  </style>
</head>
<body>
  <header>
    <div class="topbar">
      <h1>dphub 管理后台</h1>
      <button id="logout" class="danger hidden">退出</button>
    </div>
  </header>

  <div id="loginView" class="login-shell">
    <section class="login-card">
      <h2>管理员登录</h2>
      <p>输入 `config.toml` 中配置的 `admin.token`。验证通过前不会展示任何用户和额度数据。</p>
      <label>管理员 Token
        <input id="token" type="password" autocomplete="current-password" placeholder="admin.token">
      </label>
      <div class="actions">
        <button id="login">登录</button>
      </div>
      <div id="loginStatus" class="status"></div>
    </section>
  </div>

  <main id="appView" class="hidden">
    <section class="stats">
      <div class="stat">注册手机号数<strong id="registered">0</strong></div>
      <div class="stat">今日手机号总消耗<strong id="used">0</strong></div>
      <div class="stat">可存池总余额<strong id="pool">0</strong></div>
      <div class="stat">达到日额度人数<strong id="overLimit">0</strong></div>
    </section>

    <section class="panel">
      <h2>筛选用户</h2>
      <div class="toolbar">
        <label>搜索
          <input id="q" placeholder="手机号 / 邀请码 / user_id">
        </label>
        <label>最小可存池
          <input id="minPool" type="number" min="0">
        </label>
        <label>最大可存池
          <input id="maxPool" type="number" min="0">
        </label>
        <label>最小今日用量
          <input id="minUsed" type="number" min="0">
        </label>
        <label>最大今日用量
          <input id="maxUsed" type="number" min="0">
        </label>
        <label>日额度状态
          <select id="overDailyLimit">
            <option value="">全部</option>
            <option value="true">已达到</option>
            <option value="false">未达到</option>
          </select>
        </label>
        <label>排序
          <select id="sort">
            <option value="phone_asc">手机号升序</option>
            <option value="phone_desc">手机号降序</option>
            <option value="used_desc">今日用量从高到低</option>
            <option value="used_asc">今日用量从低到高</option>
            <option value="pool_desc">可存池从高到低</option>
            <option value="pool_asc">可存池从低到高</option>
          </select>
        </label>
        <label>每页
          <select id="limit"><option>25</option><option selected>50</option><option>100</option><option>200</option></select>
        </label>
      </div>
      <div class="actions">
        <div>
          <button id="apply">应用筛选</button>
          <button id="refresh" class="secondary">刷新</button>
        </div>
        <div>
          <button id="prev" class="secondary">上一页</button>
          <button id="next" class="secondary">下一页</button>
        </div>
      </div>
    </section>

    <section class="panel">
      <h2>发放可存池额度</h2>
      <div class="grant">
        <label>手机号
          <input id="grantPhone" placeholder="13800000000">
        </label>
        <label>发放 token 数
          <input id="grantAmount" type="number" min="1" placeholder="250000">
        </label>
        <button id="grant">发放</button>
      </div>
      <div id="status" class="status"></div>
    </section>

    <section class="panel">
      <h2>用户额度</h2>
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>手机号</th>
              <th>今日用量</th>
              <th>日额度</th>
              <th>可存池余额</th>
              <th>邀请码</th>
              <th>user_id</th>
              <th>状态</th>
            </tr>
          </thead>
          <tbody id="users"></tbody>
        </table>
      </div>
      <div id="pager" class="status"></div>
    </section>
  </main>

  <script>
    const $ = id => document.getElementById(id);
    let offset = 0;
    let adminToken = "";

    function number(v) { return Number(v || 0).toLocaleString("zh-CN"); }
    function escapeHtml(v) {
      return String(v).replace(/[&<>"']/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
    }
    function setText(id, text) { $(id).textContent = text; }
    function setStatus(id, text, cls) { $(id).textContent = text || ""; $(id).className = "status " + (cls || ""); }
    function authHeaders() { return { "Authorization": "Bearer " + adminToken }; }
    function optional(params, key, value) { if (value !== undefined && value !== null && String(value).trim() !== "") params.set(key, value); }
    function showApp() { $("loginView").classList.add("hidden"); $("appView").classList.remove("hidden"); $("logout").classList.remove("hidden"); }
    function showLogin() { $("appView").classList.add("hidden"); $("logout").classList.add("hidden"); $("loginView").classList.remove("hidden"); }

    async function fetchOverview(custom = {}) {
      const params = new URLSearchParams();
      optional(params, "q", $("q").value);
      optional(params, "min_pool", $("minPool").value);
      optional(params, "max_pool", $("maxPool").value);
      optional(params, "min_used", $("minUsed").value);
      optional(params, "max_used", $("maxUsed").value);
      optional(params, "sort", $("sort").value);
      optional(params, "over_daily_limit", $("overDailyLimit").value);
      params.set("limit", custom.limit || $("limit").value);
      params.set("offset", custom.offset ?? offset);
      const res = await fetch("/admin/api/overview?" + params.toString(), { headers: authHeaders() });
      const data = await res.json();
      if (!res.ok) throw new Error(data.error || "加载失败");
      return data;
    }

    function render(data) {
      setText("registered", number(data.totals.registered_phone_count));
      setText("used", number(data.totals.today_phone_used_tokens));
      setText("pool", number(data.totals.total_pool_balance));
      setText("overLimit", number(data.totals.over_daily_limit_count));
      $("users").innerHTML = data.users.map(user => `
        <tr>
          <td>${escapeHtml(user.phone)}</td>
          <td>${number(user.today_used_tokens)}</td>
          <td>${number(user.daily_limit)}</td>
          <td>${number(user.pool_balance)}</td>
          <td>${escapeHtml(user.invite_code)}</td>
          <td>${escapeHtml(user.user_id)}</td>
          <td>${user.over_daily_limit ? "已达到日额度" : "正常"}</td>
        </tr>
      `).join("");
      const from = data.filtered_count === 0 ? 0 : data.offset + 1;
      const to = Math.min(data.offset + data.users.length, data.filtered_count);
      setText("pager", `筛选结果 ${number(data.filtered_count)} 条，当前 ${from}-${to}`);
      $("prev").disabled = offset === 0;
      $("next").disabled = offset + data.limit >= data.filtered_count;
    }

    async function load() {
      setStatus("status", "加载中...");
      const data = await fetchOverview();
      render(data);
      setStatus("status", "已更新", "ok");
    }

    async function login() {
      const token = $("token").value.trim();
      if (!token) {
        setStatus("loginStatus", "请输入管理员 Token", "error");
        return;
      }
      adminToken = token;
      setStatus("loginStatus", "验证中...");
      try {
        const data = await fetchOverview({ limit: 1, offset: 0 });
        localStorage.setItem("dphub_admin_token", adminToken);
        showApp();
        render(data);
        setStatus("status", "已登录", "ok");
        setStatus("loginStatus", "");
      } catch (err) {
        adminToken = "";
        localStorage.removeItem("dphub_admin_token");
        setStatus("loginStatus", err.message || "登录失败", "error");
      }
    }

    async function grant() {
      const phone = $("grantPhone").value.trim();
      const amount = Number($("grantAmount").value);
      if (!phone || !Number.isFinite(amount) || amount <= 0) {
        setStatus("status", "请填写手机号和正数额度", "error");
        return;
      }
      const res = await fetch("/admin/api/pool/grant", {
        method: "POST",
        headers: { ...authHeaders(), "Content-Type": "application/json" },
        body: JSON.stringify({ phone, amount })
      });
      const data = await res.json();
      if (!res.ok) throw new Error(data.error || "发放失败");
      setStatus("status", `${data.phone} 当前可存池余额 ${number(data.pool_balance)}`, "ok");
      await load();
    }

    $("login").onclick = () => login();
    $("token").onkeydown = event => { if (event.key === "Enter") login(); };
    $("logout").onclick = () => { adminToken = ""; localStorage.removeItem("dphub_admin_token"); $("token").value = ""; showLogin(); };
    $("refresh").onclick = () => load().catch(e => setStatus("status", e.message, "error"));
    $("apply").onclick = () => { offset = 0; load().catch(e => setStatus("status", e.message, "error")); };
    $("prev").onclick = () => { offset = Math.max(0, offset - Number($("limit").value)); load().catch(e => setStatus("status", e.message, "error")); };
    $("next").onclick = () => { offset += Number($("limit").value); load().catch(e => setStatus("status", e.message, "error")); };
    $("grant").onclick = () => grant().catch(e => setStatus("status", e.message, "error"));

    const savedToken = localStorage.getItem("dphub_admin_token");
    if (savedToken) {
      $("token").value = savedToken;
      login();
    }
  </script>
</body>
</html>"#;

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
