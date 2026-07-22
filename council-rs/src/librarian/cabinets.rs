//! Proxy + lifetime cache for upstream `/cabinets`.
//!

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::RwLock;

pub const DEFAULT_LIBRARIAN_BASE_URL: &str = "http://127.0.0.1:11435";
pub const DEFAULT_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
}

#[derive(Debug, Default, Clone)]
pub struct Cache {
    inner: Arc<RwLock<Option<Vec<Value>>>>,
}

impl Cache {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn list_cabinets(
        &self,
        client: &Client,
        base_url: &str,
        refresh: bool,
    ) -> Result<Vec<Value>, ProxyError> {
        if !refresh && let Some(c) = self.inner.read().await.clone() {
            return Ok(c);
        }
        let url = format!("{base_url}/cabinets");
        let resp = client
            .get(&url)
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .send()
            .await?
            .error_for_status()?;
        let body: Value = resp.json().await?;
        let cabs = body
            .get("cabinets")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        *self.inner.write().await = Some(cabs.clone());
        Ok(cabs)
    }

    pub async fn cabinet_names(&self) -> HashSet<String> {
        match self.inner.read().await.as_ref() {
            None => HashSet::new(),
            Some(cabs) => cabs
                .iter()
                .filter_map(|c| c.get("name").and_then(|v| v.as_str()).map(String::from))
                .collect(),
        }
    }
}
