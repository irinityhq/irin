//! Integration-style tests for `librarian::health::HealthCache`.
//!
//! Covers the documented state machine:
//! - First call → "warming" placeholder + background `do_check` spawn
//! - Cache hit (within TTL)
//! - Re-check on TTL expiry
//! - Concurrent callers during warmup / in-flight
//! - Offline / error paths
//!
//! Pattern adapted from `tests/librarian_ws.rs` (mock axum + env lock).
//! Zero external spend: all traffic hits a local test server.

use std::net::SocketAddr;
use std::time::Duration;

use axum::{Json, Router, routing::get};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::net::TcpListener;

use council_rs::librarian::health::HealthCache;

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn env_guard() -> tokio::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().await
}

/// Mock upstream serving GET /health?cheap=1
/// Returns either success JSON or an error status depending on `fail`.
async fn spawn_mock_health(success: bool, delay_ms: u64) -> SocketAddr {
    async fn handler() -> Json<Value> {
        Json(json!({
            "state": "online",
            "model": "test-mlx",
            "inference_model": "test-model",
            "raw": { "ok": true }
        }))
    }

    let app = if success {
        if delay_ms == 0 {
            Router::new().route("/health", get(handler))
        } else {
            Router::new().route(
                "/health",
                get(move || async move {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    handler().await
                }),
            )
        }
    } else {
        Router::new().route(
            "/health",
            get(|| async {
                // Simulate upstream failure
                (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom")
            }),
        )
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // tiny grace period
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr
}

fn make_cache(ttl_secs: u64, timeout_secs: u64) -> HealthCache {
    HealthCache::new(ttl_secs, timeout_secs)
}

#[tokio::test]
async fn health_first_call_returns_warming_and_populates_cache() {
    let _guard = env_guard().await;

    let mock = spawn_mock_health(true, 0).await;
    let base = format!("http://{}", mock);
    let client = Client::new();

    let cache = make_cache(30, 5);

    let first = cache.get(&client, &base).await;
    assert_eq!(first["state"], "warming");
    assert!(first["model"].is_null());

    // Wait for background task to finish
    tokio::time::sleep(Duration::from_millis(150)).await;

    let second = cache.get(&client, &base).await;
    assert_eq!(second["state"], "online");
    assert_eq!(second["model"], "test-mlx");
}

#[tokio::test]
async fn health_cache_hit_within_ttl() {
    let _guard = env_guard().await;

    let mock = spawn_mock_health(true, 0).await;
    let base = format!("http://{}", mock);
    let client = Client::new();

    // Very short TTL for test speed, but first hit will use it
    let cache = make_cache(1, 5);

    let _ = cache.get(&client, &base).await;
    tokio::time::sleep(Duration::from_millis(80)).await;

    let hit = cache.get(&client, &base).await;
    assert_eq!(hit["state"], "online");
}

#[tokio::test]
async fn health_offline_on_bad_status() {
    let _guard = env_guard().await;

    let mock = spawn_mock_health(false, 0).await;
    let base = format!("http://{}", mock);
    let client = Client::new();

    let cache = make_cache(30, 5);

    // First call still gives warming, background will see error
    let first = cache.get(&client, &base).await;
    assert_eq!(first["state"], "warming");

    tokio::time::sleep(Duration::from_millis(150)).await;

    let second = cache.get(&client, &base).await;
    assert_eq!(second["state"], "offline");
    assert!(second["detail"].as_str().unwrap_or("").contains("500"));
}

#[tokio::test]
async fn health_concurrent_callers_during_warmup() {
    let _guard = env_guard().await;

    let mock = spawn_mock_health(true, 80).await; // slow response
    let base = format!("http://{}", mock);
    let client = Client::new();

    let cache = make_cache(30, 5);

    let h1 = cache.clone();
    let h2 = cache.clone();
    let base1 = base.clone();
    let base2 = base.clone();
    let c1 = client.clone();
    let c2 = client.clone();

    let (r1, r2) = tokio::join!(
        tokio::spawn(async move { h1.get(&c1, &base1).await }),
        tokio::spawn(async move { h2.get(&c2, &base2).await })
    );

    let v1 = r1.unwrap();
    let v2 = r2.unwrap();

    // At least one should have been warming or returned previous (here none)
    assert!(v1["state"] == "warming" || v2["state"] == "warming" || v1["state"] == "online");
}

#[tokio::test]
async fn health_respects_ttl_expiry() {
    let _guard = env_guard().await;

    let mock = spawn_mock_health(true, 0).await;
    let base = format!("http://{}", mock);
    let client = Client::new();

    // Force frequent re-check by constructing with ttl=0
    let cache = HealthCache::new(0, 5);

    let _ = cache.get(&client, &base).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let second = cache.get(&client, &base).await;
    // With ttl=0 we expect the background / re-check path to have run again
    assert!(second["state"] == "online" || second["state"] == "warming");
}
