//! axum router for `/api/librarian/*`.
//!
//! Route table:
//!   GET    /api/librarian/health
//!   GET    /api/librarian/cabinets?refresh=bool
//!   GET    /api/librarian/chats
//!   POST   /api/librarian/chats                  body: {cabinet}
//!   GET    /api/librarian/chats/{chat_id}
//!   PATCH  /api/librarian/chats/{chat_id}        header: If-Match
//!                                                body: {title?, cabinet?}
//!   DELETE /api/librarian/chats/{chat_id}        204 No Content
//!   POST   /api/librarian/chats/{chat_id}/asks   body: {client_msg_id, content}

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use chrono::{SecondsFormat, Utc};
use reqwest::{Client, header::CONTENT_TYPE};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Semaphore};

use super::{
    adapter::{self, ShapeError},
    cabinets::{self, Cache as CabinetsCache, ProxyError},
    health::HealthCache,
    idempotency::Lru,
    redaction,
    storage::{ChatSummary, Conversation, StorageError, Store},
    title,
};

pub const ASSISTANT_CAP: usize = 64 * 1024;
pub const SNIPPET_CAP: usize = 1024;
pub const MAX_SOURCES: usize = 20;
pub const USER_CONTENT_MAX: usize = 8192;
pub const TITLE_MAX: usize = 120;
pub const CABINET_NAME_MAX: usize = 64;
pub const CLIENT_MSG_ID_MAX: usize = 64;
pub const DEFAULT_ASK_TIMEOUT_SECS: u64 = 300;
pub const DEFAULT_MAX_INFLIGHT: usize = 1;

/// Shared state for `/api/librarian/*`.
#[derive(Clone)]
pub struct LibrarianState {
    pub store: Store,
    pub http: Client,
    pub base_url: String,
    pub ask_timeout: Duration,
    pub semaphore: Arc<Semaphore>,
    pub cabinets: CabinetsCache,
    pub health: HealthCache,
    pub idem: Arc<Mutex<Lru<Value>>>,
}

impl LibrarianState {
    pub fn from_env() -> Self {
        let base_url = std::env::var("LIBRARIAN_BASE_URL")
            .unwrap_or_else(|_| cabinets::DEFAULT_LIBRARIAN_BASE_URL.into());
        let ask_timeout = std::env::var("LIBRARIAN_ASK_TIMEOUT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_ASK_TIMEOUT_SECS);
        let max_inflight = std::env::var("LIBRARIAN_MAX_INFLIGHT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_INFLIGHT);
        Self {
            store: Store::from_env(),
            http: Client::new(),
            base_url,
            ask_timeout: Duration::from_secs(ask_timeout),
            semaphore: Arc::new(Semaphore::new(max_inflight)),
            cabinets: CabinetsCache::new(),
            health: HealthCache::from_env(),
            idem: Arc::new(Mutex::new(Lru::with_defaults())),
        }
    }
}

pub fn router(state: LibrarianState) -> Router {
    Router::new()
        .route("/health", get(get_health))
        .route("/cabinets", get(get_cabinets))
        .route("/chats", get(list_chats).post(create_chat))
        .route(
            "/chats/{chat_id}",
            get(get_chat).patch(patch_chat).delete(delete_chat),
        )
        .route("/chats/{chat_id}/asks", post(post_ask))
        .route("/context/{tenant}", get(get_context))
        .route("/commits", post(post_commit))
        .with_state(state)
}

// ── Request bodies ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateChatBody {
    cabinet: String,
}

#[derive(Deserialize)]
struct PatchChatBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    cabinet: Option<String>,
}

#[derive(Deserialize)]
struct AskBody {
    client_msg_id: String,
    content: String,
}

#[derive(Deserialize)]
struct CabinetsQuery {
    #[serde(default)]
    refresh: bool,
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn err_json(code: StatusCode, msg: impl Into<String>) -> axum::response::Response {
    (code, Json(json!({"detail": msg.into()}))).into_response()
}

fn err_json_with_headers(
    code: StatusCode,
    msg: impl Into<String>,
    headers: Vec<(&'static str, &'static str)>,
) -> axum::response::Response {
    let mut resp = (code, Json(json!({"detail": msg.into()}))).into_response();
    for (k, v) in headers {
        if let Ok(v) = v.parse() {
            resp.headers_mut().insert(k, v);
        }
    }
    resp
}

fn map_storage_err(e: StorageError) -> axum::response::Response {
    match e {
        StorageError::InvalidChatId(_) => err_json(StatusCode::BAD_REQUEST, "invalid chat id"),
        StorageError::NotFound(_) => err_json(StatusCode::NOT_FOUND, "chat not found"),
        StorageError::UnsupportedSchema(m) => err_json(StatusCode::UNSUPPORTED_MEDIA_TYPE, m),
        StorageError::ChatTooLarge(_) => err_json(
            StatusCode::PAYLOAD_TOO_LARGE,
            "chat too large; start a new conversation",
        ),
        StorageError::Io(e) => {
            err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("io error: {e}"))
        }
        StorageError::Json(e) => err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("json error: {e}"),
        ),
    }
}

#[allow(clippy::result_large_err)]
async fn ensure_cabinet_ok(
    state: &LibrarianState,
    name: &str,
) -> Result<(), axum::response::Response> {
    let mut names = state.cabinets.cabinet_names().await;
    if names.is_empty() {
        let _ = state
            .cabinets
            .list_cabinets(&state.http, &state.base_url, true)
            .await;
        names = state.cabinets.cabinet_names().await;
    }
    if !names.contains(name) {
        return Err(err_json(
            StatusCode::BAD_REQUEST,
            format!("unknown cabinet: {name}"),
        ));
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn validate_payload_lengths(
    cabinet: Option<&str>,
    content: Option<&str>,
    client_msg_id: Option<&str>,
    title: Option<&str>,
) -> Result<(), axum::response::Response> {
    if let Some(c) = cabinet
        && (c.is_empty() || c.len() > CABINET_NAME_MAX)
    {
        return Err(err_json(StatusCode::BAD_REQUEST, "cabinet length"));
    }
    if let Some(c) = content
        && (c.is_empty() || c.len() > USER_CONTENT_MAX)
    {
        return Err(err_json(StatusCode::BAD_REQUEST, "content length"));
    }
    if let Some(id) = client_msg_id
        && (id.is_empty() || id.len() > CLIENT_MSG_ID_MAX)
    {
        return Err(err_json(StatusCode::BAD_REQUEST, "client_msg_id length"));
    }
    if let Some(t) = title
        && t.len() > TITLE_MAX
    {
        return Err(err_json(StatusCode::BAD_REQUEST, "title length"));
    }
    Ok(())
}

// ── Handlers ────────────────────────────────────────────────────────────

async fn get_health(State(s): State<LibrarianState>) -> impl IntoResponse {
    Json(s.health.get(&s.http, &s.base_url).await)
}

async fn get_cabinets(
    State(s): State<LibrarianState>,
    Query(q): Query<CabinetsQuery>,
) -> impl IntoResponse {
    match s
        .cabinets
        .list_cabinets(&s.http, &s.base_url, q.refresh)
        .await
    {
        Ok(c) => Json(json!({"cabinets": c})).into_response(),
        Err(ProxyError::Http(_)) => Json(json!({"cabinets": Vec::<Value>::new()})).into_response(),
    }
}

async fn list_chats(State(s): State<LibrarianState>) -> impl IntoResponse {
    let chats: Vec<ChatSummary> = s.store.list_chats();
    Json(json!({"chats": chats}))
}

async fn create_chat(
    State(s): State<LibrarianState>,
    Json(body): Json<CreateChatBody>,
) -> axum::response::Response {
    if let Err(r) = validate_payload_lengths(Some(&body.cabinet), None, None, None) {
        return r;
    }
    if let Err(r) = ensure_cabinet_ok(&s, &body.cabinet).await {
        return r;
    }
    match s.store.create_chat(&body.cabinet) {
        Ok(id) => Json(json!({"id": id})).into_response(),
        Err(e) => map_storage_err(e),
    }
}

async fn get_chat(
    State(s): State<LibrarianState>,
    Path(chat_id): Path<String>,
) -> axum::response::Response {
    match s.store.load_chat(&chat_id) {
        Ok(c) => Json(c).into_response(),
        Err(e) => map_storage_err(e),
    }
}

async fn patch_chat(
    State(s): State<LibrarianState>,
    Path(chat_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<PatchChatBody>,
) -> axum::response::Response {
    if let Err(r) =
        validate_payload_lengths(body.cabinet.as_deref(), None, None, body.title.as_deref())
    {
        return r;
    }
    let convo: Conversation = match s.store.load_chat(&chat_id) {
        Ok(c) => c,
        Err(e) => return map_storage_err(e),
    };
    let if_match = headers
        .get("if-match")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string());
    if if_match.as_deref() != Some(convo.updated_at.as_str()) {
        return err_json(StatusCode::CONFLICT, "stale etag");
    }
    let mut update = serde_json::Map::new();
    update.insert("type".into(), json!("meta_update"));
    update.insert("ts".into(), json!(now_iso()));
    if let Some(t) = body.title.as_deref() {
        update.insert("title".into(), json!(t));
    }
    if let Some(c) = body.cabinet.as_deref() {
        if let Err(r) = ensure_cabinet_ok(&s, c).await {
            return r;
        }
        update.insert("cabinet".into(), json!(c));
    }
    if update.len() == 2 {
        return Json(convo).into_response();
    }
    if let Err(e) = s.store.append_event(&chat_id, &Value::Object(update)) {
        return map_storage_err(e);
    }
    match s.store.load_chat(&chat_id) {
        Ok(c) => Json(c).into_response(),
        Err(e) => map_storage_err(e),
    }
}

async fn delete_chat(
    State(s): State<LibrarianState>,
    Path(chat_id): Path<String>,
) -> axum::response::Response {
    match s.store.delete_chat(&chat_id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => map_storage_err(e),
    }
}

/// Result of [`LibrarianState::run_ask`] — the single source of truth shared by
/// `POST /ask` (mapped to an HTTP response) and the `/ws/librarian` WS handler
/// (R20, mapped to an event sequence).
pub enum AskOutcome {
    /// Cached idempotent replay — the prior `{user_turn, assistant_turn}` result.
    Cached(Value),
    /// Upstream busy (semaphore exhausted) — 503 / WS error.
    Busy,
    /// A storage/validation/upstream error mapped to (status, message).
    Failed(StatusCode, String),
    /// Successful turn. `result` is `{user_turn, assistant_turn}`; the
    /// `assistant_turn`'s `content`/`sources`/`model` are also broken out for
    /// the WS handler's `ask_complete` / `sources` frames.
    Ok {
        result: Value,
        user_turn: Value,
        assistant_turn: Value,
    },
}

impl LibrarianState {
    /// Run one librarian ask turn, end to end.
    ///
    /// Cancel-safety (feature contract, reused by R20): if the caller's future is dropped
    /// mid-ask (HTTP client disconnect, or WS close = cancel), cancellation can
    /// only land at `.await` points. JSONL store appends are synchronous fs ops
    /// (never torn); the owned semaphore permit is bound to this future and
    /// drops with it (no wedged in-flight slot); the upstream reqwest aborts on
    /// drop. Worst-case residue: a dangling user turn without an assistant turn,
    /// which loads fine. The idempotency cache is populated on success only, so
    /// an aborted ask retried with the SAME client_msg_id appends a duplicate
    /// user turn — callers (HTTP Stop flow + WS Stop) mint a fresh
    /// client_msg_id per send.
    ///
    /// Streaming note (R20): upstream `/ask` may return buffered JSON or SSE.
    /// SSE is currently buffered into one assistant turn; the WS handler still
    /// emits `ask_started` then `ask_complete` with zero or more future chunks.
    pub async fn run_ask(&self, chat_id: &str, content: &str, client_msg_id: &str) -> AskOutcome {
        let s = self;
        let convo: Conversation = match s.store.load_chat(chat_id) {
            Ok(c) => c,
            Err(e) => return ask_outcome_from_storage_err(e),
        };

        // Idempotency check
        {
            let mut idem = s.idem.lock().await;
            if let Some(cached) = idem.get(chat_id, client_msg_id) {
                return AskOutcome::Cached(cached);
            }
        }

        // Try to acquire semaphore — non-blocking.
        let permit = match s.semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => return AskOutcome::Busy,
        };

        let ts = now_iso();
        let user_id = format!("u_{}", Utc::now().timestamp_millis());
        let user_turn = json!({
            "type": "user",
            "id": user_id,
            "content": content,
            "ts": ts,
            "client_msg_id": client_msg_id,
        });
        if let Err(e) = s.store.append_event(chat_id, &user_turn) {
            drop(permit);
            return ask_outcome_from_storage_err(e);
        }

        // Title-on-receipt
        if convo.title.is_empty() {
            let new_title = title::from_first_message(content);
            let _ = s.store.append_event(
                chat_id,
                &json!({"type":"meta_update","title":new_title,"ts":ts}),
            );
        }

        // Call upstream /ask (single buffered POST — no streaming upstream).
        let url = format!("{}/ask", s.base_url);
        let resp = match s
            .http
            .post(&url)
            .timeout(s.ask_timeout)
            .json(&json!({"query": content, "cabinet": convo.cabinet}))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                drop(permit);
                return AskOutcome::Failed(
                    StatusCode::BAD_GATEWAY,
                    format!("librarian unreachable: {e}"),
                );
            }
        };
        let resp = match resp.error_for_status() {
            Ok(r) => r,
            Err(e) => {
                drop(permit);
                return AskOutcome::Failed(
                    StatusCode::BAD_GATEWAY,
                    format!("librarian unreachable: {e}"),
                );
            }
        };
        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let answer = if content_type.starts_with("text/event-stream") {
            let raw = match resp.text().await {
                Ok(v) => v,
                Err(e) => {
                    drop(permit);
                    return AskOutcome::Failed(
                        StatusCode::BAD_GATEWAY,
                        format!("librarian unreachable: {e}"),
                    );
                }
            };
            match adapter::parse_sse(&raw) {
                Ok(a) => a,
                Err(e) => {
                    drop(permit);
                    return AskOutcome::Failed(StatusCode::BAD_GATEWAY, shape_error_message(e));
                }
            }
        } else {
            let raw: Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    drop(permit);
                    return AskOutcome::Failed(
                        StatusCode::BAD_GATEWAY,
                        format!("librarian unreachable: {e}"),
                    );
                }
            };
            match adapter::parse(&raw) {
                Ok(a) => a,
                Err(e) => {
                    drop(permit);
                    return AskOutcome::Failed(StatusCode::BAD_GATEWAY, shape_error_message(e));
                }
            }
        };

        // Cap and redact
        let mut ans_text: String = answer.answer.chars().take(ASSISTANT_CAP).collect();
        let (red, hit_a) = redaction::redact_secrets(&ans_text);
        ans_text = red;
        let mut sources_out: Vec<Value> = Vec::new();
        let mut any_hit = hit_a;
        for s_in in answer.sources.into_iter().take(MAX_SOURCES) {
            let snippet: String = s_in.snippet.chars().take(SNIPPET_CAP).collect();
            let (snip_red, hit_s) = redaction::redact_secrets(&snippet);
            any_hit = any_hit || hit_s;
            let mut entry = json!({
                "path": s_in.path,
                "score": s_in.score,
                "snippet": snip_red,
            });
            if let Some(o) = entry.as_object_mut() {
                if let Some(c) = s_in.corpus {
                    o.insert("corpus".into(), json!(c));
                }
                if let Some(t) = s_in.trust_tier {
                    o.insert("trust_tier".into(), json!(t));
                }
            }
            sources_out.push(entry);
        }

        let assistant_id = format!("a_{}", Utc::now().timestamp_millis());
        let mut assistant_turn = json!({
            "type": "assistant",
            "id": assistant_id,
            "content": ans_text,
            "sources": sources_out,
            "model": answer.model,
            "redacted": any_hit,
            "ts": now_iso(),
        });
        if let Err(StorageError::ChatTooLarge(_)) = s.store.append_event(chat_id, &assistant_turn)
            && let Some(o) = assistant_turn.as_object_mut()
        {
            o.insert("partial".into(), json!(true));
        }

        let result = json!({
            "user_turn": user_turn,
            "assistant_turn": assistant_turn,
        });
        {
            let mut idem = s.idem.lock().await;
            idem.put(chat_id, client_msg_id, result.clone());
        }
        drop(permit);
        AskOutcome::Ok {
            result,
            user_turn,
            assistant_turn,
        }
    }
}

fn shape_error_message(e: ShapeError) -> String {
    match e {
        ShapeError::UpstreamSse(msg) => format!("librarian upstream error: {msg}"),
        ShapeError::SseMissingAnswer => "librarian SSE response had no answer text".to_string(),
        ShapeError::MalformedSse(_) => "librarian SSE shape error".to_string(),
        ShapeError::NotObject(_)
        | ShapeError::MissingTopLevel(_)
        | ShapeError::UnknownTopLevel(_)
        | ShapeError::SourceNotObject(_)
        | ShapeError::SourceMissing(_, _) => "librarian shape error".to_string(),
    }
}

fn ask_outcome_from_storage_err(e: StorageError) -> AskOutcome {
    match e {
        StorageError::InvalidChatId(_) => {
            AskOutcome::Failed(StatusCode::BAD_REQUEST, "invalid chat id".into())
        }
        StorageError::NotFound(_) => {
            AskOutcome::Failed(StatusCode::NOT_FOUND, "chat not found".into())
        }
        StorageError::UnsupportedSchema(m) => {
            AskOutcome::Failed(StatusCode::UNSUPPORTED_MEDIA_TYPE, m)
        }
        StorageError::ChatTooLarge(_) => AskOutcome::Failed(
            StatusCode::PAYLOAD_TOO_LARGE,
            "chat too large; start a new conversation".into(),
        ),
        StorageError::Io(e) => {
            AskOutcome::Failed(StatusCode::INTERNAL_SERVER_ERROR, format!("io error: {e}"))
        }
        StorageError::Json(e) => AskOutcome::Failed(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("json error: {e}"),
        ),
    }
}

async fn post_ask(
    State(s): State<LibrarianState>,
    Path(chat_id): Path<String>,
    Json(body): Json<AskBody>,
) -> axum::response::Response {
    if let Err(r) =
        validate_payload_lengths(None, Some(&body.content), Some(&body.client_msg_id), None)
    {
        return r;
    }

    match s
        .run_ask(&chat_id, &body.content, &body.client_msg_id)
        .await
    {
        AskOutcome::Cached(v) => Json(v).into_response(),
        AskOutcome::Busy => err_json_with_headers(
            StatusCode::SERVICE_UNAVAILABLE,
            "librarian busy",
            vec![("retry-after", "5")],
        ),
        AskOutcome::Failed(code, msg) => err_json(code, msg),
        AskOutcome::Ok { result, .. } => Json(result).into_response(),
    }
}

// ---------------------------------------------------------------------------
// v0.3 Open Surface: Librarian Identity / Memory Context & Commit Proposals
// ---------------------------------------------------------------------------

async fn get_context(
    State(_s): State<LibrarianState>,
    Path(tenant): Path<String>,
) -> axum::response::Response {
    // Basic adapter hook for Librarian context resolution per tenant.
    // Full implementation relies on upstream Librarian (out of scope for v0.3).
    let mut ctx = adapter::LibrarianContext::default();
    ctx.identity.tenant_id = tenant;
    Json(ctx).into_response()
}

async fn post_commit(
    State(_s): State<LibrarianState>,
    Json(proposal): Json<adapter::CommitProposal>,
) -> axum::response::Response {
    // Basic adapter hook for receiving commit proposals from Gateway/Worker.
    // In v0.3, this just ACKs the proposal.
    Json(json!({"status": "received", "tenant_id": proposal.tenant_id, "causal_fire_id": proposal.causal_fire_id})).into_response()
}
