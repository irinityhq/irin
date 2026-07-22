//! Phase 2 queue-depth-watch sentinel — generic HTTP+jsonpath threshold poller.
//!
//! Pattern: GET a JSON endpoint, extract a numeric value via a JSONPath
//! expression, compare against a configured threshold. Fires if
//! `value > threshold`.
//!
//! Use case: poll gateway's `/council/stats` for `$.active_total`, fire
//! when in-flight council slots exceed capacity. Reusable for any
//! "expose a counter as JSON, alert when it crosses a line" pattern.

use crate::watch::{
    EscalateError, Escalation, ObserveError, Sentinel, SentinelState, Tier, Urgency,
};
use async_trait::async_trait;
use jsonpath_rust::JsonPath;
use std::time::Duration;

const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_millis(40);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

pub struct QueueDepthSentinel {
    name: String,
    tenant: String,
    url: String,
    jsonpath: String,
    threshold: i64,
    cooldown: Duration,
    timeout: Duration,
}

impl QueueDepthSentinel {
    pub fn new(name: &str, tenant: &str, url: &str, jsonpath: &str, threshold: i64) -> Self {
        Self {
            name: name.into(),
            tenant: tenant.into(),
            url: url.into(),
            jsonpath: jsonpath.into(),
            threshold,
            cooldown: Duration::from_secs(30),
            timeout: DEFAULT_HTTP_TIMEOUT,
        }
    }

    pub fn with_cooldown(mut self, d: Duration) -> Self {
        self.cooldown = d;
        self
    }
}

#[async_trait]
impl Sentinel for QueueDepthSentinel {
    fn name(&self) -> &str {
        &self.name
    }
    fn tenant(&self) -> &str {
        &self.tenant
    }
    fn tier(&self) -> Tier {
        Tier::Polling
    }
    fn cooldown(&self) -> Duration {
        self.cooldown
    }

    async fn observe(&self) -> Result<SentinelState, ObserveError> {
        let client = reqwest::Client::new();
        let resp = client
            .get(&self.url)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| ObserveError::TransientUpstream(format!("get {}: {e}", self.url)))?;

        if !resp.status().is_success() {
            return Err(ObserveError::TransientUpstream(format!(
                "non-2xx from {}: {}",
                self.url,
                resp.status()
            )));
        }

        if let Some(len) = resp.content_length() {
            if len > MAX_RESPONSE_BYTES as u64 {
                return Err(ObserveError::Fatal(format!(
                    "response from {} too large: {} bytes > {}",
                    self.url, len, MAX_RESPONSE_BYTES
                )));
            }
        }

        let mut body = Vec::new();
        let mut resp = resp;
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| ObserveError::TransientUpstream(format!("read body: {e}")))?
        {
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(ObserveError::Fatal(format!(
                    "response from {} exceeded {} bytes",
                    self.url, MAX_RESPONSE_BYTES
                )));
            }
            body.extend_from_slice(&chunk);
        }

        // JSONPath extraction. A parser error on the path is a config
        // bug → Fatal (operator must fix). A missing path on a
        // syntactically-valid expression is also Fatal — the endpoint
        // shape disagrees with the configured path, which is again an
        // operator-level mismatch.
        let doc: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| ObserveError::Fatal(format!("json decode from {}: {e}", self.url)))?;

        let matches = doc
            .query(&self.jsonpath)
            .map_err(|e| ObserveError::Fatal(format!("jsonpath '{}' parse: {e}", self.jsonpath)))?;

        let first = matches.first().ok_or_else(|| {
            ObserveError::Fatal(format!(
                "jsonpath '{}' resolved to no value against {}",
                self.jsonpath, self.url
            ))
        })?;

        let value = first
            .as_i64()
            .or_else(|| first.as_f64().map(|f| f as i64))
            .ok_or_else(|| {
                ObserveError::Fatal(format!(
                    "jsonpath '{}' resolved to non-numeric: {}",
                    self.jsonpath, first
                ))
            })?;

        let observed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        Ok(SentinelState {
            tenant: self.tenant.clone(),
            sentinel: self.name.clone(),
            observed_at,
            payload: serde_json::json!({
                "value": value,
                "threshold": self.threshold,
                "url": self.url,
                "jsonpath": self.jsonpath,
            }),
        })
    }

    fn interesting(&self, state: &SentinelState) -> Option<String> {
        let value = state.payload["value"].as_i64()?;
        let threshold = state.payload["threshold"].as_i64()?;
        if value > threshold {
            Some(format!("queue depth {value} exceeds threshold {threshold}"))
        } else {
            None
        }
    }

    async fn escalate(
        &self,
        state: SentinelState,
        reason: String,
    ) -> Result<Escalation, EscalateError> {
        Ok(Escalation {
            state,
            reason,
            urgency: Urgency::Medium,
        })
    }
}
