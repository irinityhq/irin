// Cache route handlers (moved from main.rs).

use axum::{extract::Json, response::IntoResponse};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tracing::debug;

use crate::cache;
use crate::AppState;

#[derive(Deserialize)]
pub(super) struct CacheCheckRequest {
    alias: String,
    raw_body: String,
    /// Lua's translator_version. The check returns hit=false if the cached
    /// entry's version doesn't match — cheap insurance against silent
    /// translator drift between cache writes and reads.
    #[serde(default = "default_translator_version")]
    expected_translator_version: u32,
}

fn default_translator_version() -> u32 {
    0
}

#[derive(Serialize)]
pub(super) struct CacheCheckResponse {
    hit: bool,
    response: Option<serde_json::Value>,
    /// Only present on hit. The provider that produced `response`; the Lua
    /// caller uses this to drive translate_response on the cached native
    /// shape before emitting to the client.
    provider: Option<String>,
    latency_ms: u64,
}

#[derive(Deserialize)]
pub(super) struct CacheStoreRequest {
    alias: String,
    raw_body: String,
    /// The NATIVE upstream response shape. The Lua caller MUST pass the
    /// pre-translation body (gw_response_buf_native) — passing a normalized
    /// body would defeat the cache-shape invariant.
    response: serde_json::Value,
    provider: String,
    translator_version: u32,
    ttl_secs: Option<u64>,
}

#[derive(Serialize)]
pub(super) struct CacheStoreResponse {
    stored: bool,
    latency_ms: u64,
}

pub(super) async fn cache_check(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<CacheCheckRequest>,
) -> impl IntoResponse {
    let t0 = Instant::now();
    let key = cache::GatewayCache::generate_cache_key(&req.alias, &req.raw_body);

    match state.cache.get(&key).await {
        Some(entry) => {
            // Translator-version invalidation: stale entries are reported
            // as misses. The Lua caller will fetch fresh from the upstream
            // and write a current-version entry, naturally aging the cache.
            if entry.translator_version != req.expected_translator_version {
                debug!(
                    cached = entry.translator_version,
                    expected = req.expected_translator_version,
                    "cache entry skipped: translator version mismatch"
                );
                return Json(CacheCheckResponse {
                    hit: false,
                    response: None,
                    provider: None,
                    latency_ms: t0.elapsed().as_millis() as u64,
                });
            }
            Json(CacheCheckResponse {
                hit: true,
                response: Some(entry.response),
                provider: Some(entry.provider),
                latency_ms: t0.elapsed().as_millis() as u64,
            })
        }
        None => Json(CacheCheckResponse {
            hit: false,
            response: None,
            provider: None,
            latency_ms: t0.elapsed().as_millis() as u64,
        }),
    }
}

pub(super) async fn cache_store(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<CacheStoreRequest>,
) -> impl IntoResponse {
    let t0 = Instant::now();
    let key = cache::GatewayCache::generate_cache_key(&req.alias, &req.raw_body);

    // Default TTL is 24 hours if not specified
    let ttl = req.ttl_secs.unwrap_or(86400);

    state
        .cache
        .set(
            &key,
            req.response,
            req.provider,
            req.translator_version,
            ttl,
        )
        .await;

    Json(CacheStoreResponse {
        stored: true,
        latency_ms: t0.elapsed().as_millis() as u64,
    })
}
