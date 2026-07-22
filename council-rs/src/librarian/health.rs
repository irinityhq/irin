//! Single-flight `/health` cache.
//!
//! State machine:
//!   first call  → returns `{state: "warming", model: null}` immediately,
//!                 spawns background warmup task.
//!   subsequent  → returns cached `{state: online|offline, ...}` until TTL
//!                 expires, then re-checks.

use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::Mutex;

pub const DEFAULT_HEALTH_TTL_SECS: u64 = 30;
pub const DEFAULT_HEALTH_TIMEOUT_SECS: u64 = 60;

struct Inner {
    last_value: Option<Value>,
    last_ts: Option<Instant>,
    warmed: bool,
    in_flight: bool,
}

#[derive(Clone)]
pub struct HealthCache {
    inner: Arc<Mutex<Inner>>,
    pub ttl: Duration,
    pub timeout: Duration,
}

impl HealthCache {
    pub fn new(ttl_secs: u64, timeout_secs: u64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                last_value: None,
                last_ts: None,
                warmed: false,
                in_flight: false,
            })),
            ttl: Duration::from_secs(ttl_secs),
            timeout: Duration::from_secs(timeout_secs),
        }
    }

    pub fn from_env() -> Self {
        let ttl = std::env::var("LIBRARIAN_HEALTH_TTL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_HEALTH_TTL_SECS);
        let timeout = std::env::var("LIBRARIAN_HEALTH_TIMEOUT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_HEALTH_TIMEOUT_SECS);
        Self::new(ttl, timeout)
    }

    pub async fn get(&self, client: &Client, base_url: &str) -> Value {
        // Fast path: fresh cache.
        {
            let g = self.inner.lock().await;
            if let (Some(val), Some(ts)) = (&g.last_value, g.last_ts)
                && ts.elapsed() < self.ttl
            {
                return with_check_ts(val.clone(), ts);
            }
        }

        // Slow path: either kick off warmup (first ever call) or re-check.
        let mut g = self.inner.lock().await;

        if !g.warmed {
            g.warmed = true;
            let placeholder = json!({"state":"warming","model":Value::Null});
            // spawn background warmup
            let cache = self.clone();
            let client_c = client.clone();
            let base = base_url.to_string();
            tokio::spawn(async move {
                let v = do_check(&client_c, &base, cache.timeout).await;
                let mut gi = cache.inner.lock().await;
                gi.last_value = Some(v);
                gi.last_ts = Some(Instant::now());
            });
            return placeholder;
        }

        if g.in_flight {
            // Another caller is doing the check; return previous value (if any),
            // otherwise warming.
            let val = g
                .last_value
                .clone()
                .unwrap_or_else(|| json!({"state":"warming","model":Value::Null}));
            let ts = g.last_ts.unwrap_or_else(Instant::now);
            return with_check_ts(val, ts);
        }
        g.in_flight = true;
        drop(g);

        let value = do_check(client, base_url, self.timeout).await;
        let mut gi = self.inner.lock().await;
        gi.last_value = Some(value.clone());
        gi.last_ts = Some(Instant::now());
        gi.in_flight = false;
        with_check_ts(value, gi.last_ts.unwrap())
    }
}

fn with_check_ts(mut v: Value, ts: Instant) -> Value {
    if let Some(o) = v.as_object_mut() {
        // Use elapsed-ago seconds — Instant has no epoch. Frontend just shows
        // "last checked" relative to now, so a numeric seconds-ago is fine.
        // We expose monotonic seconds since startup-ish via Instant::elapsed.
        // To keep wire shape Python-parity, emit seconds since UNIX epoch.
        let secs = ts.elapsed().as_secs_f64();
        o.insert("last_check_ts".into(), json!(seconds_since_epoch() - secs));
    }
    v
}

fn seconds_since_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

async fn do_check(client: &Client, base_url: &str, timeout: Duration) -> Value {
    // Cheap probe: skips MLX 1-token smoke, only verifies LanceDB + socket.
    // The sidebar polls every 15s; the heavy variant would tie up the 24 GB
    // GPU semaphore on every TTL miss for nothing.
    let url = format!("{base_url}/health?cheap=1");
    let resp = match client.get(&url).timeout(timeout).send().await {
        Ok(r) => r,
        Err(e) => {
            return json!({"state":"offline","model":Value::Null,"detail":e.to_string()});
        }
    };
    if !resp.status().is_success() {
        return json!({
            "state":"offline","model":Value::Null,
            "detail":format!("http {}", resp.status().as_u16())
        });
    }
    let body: Value = match resp.json().await {
        Ok(b) => b,
        Err(e) => {
            return json!({"state":"offline","model":Value::Null,"detail":e.to_string()});
        }
    };
    let model = body
        .get("model")
        .or_else(|| body.get("inference_model"))
        .cloned()
        .unwrap_or(Value::Null);
    json!({
        "state": "online",
        "model": model,
        "raw": body,
    })
}
