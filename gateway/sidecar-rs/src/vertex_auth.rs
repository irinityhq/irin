// ==========================================================================
// vertex_auth.rs — Vertex AI ADC token provider.
//
// Wraps `gcp_auth` to automatically discover, cache, and refresh GCP
// access tokens for Vertex AI API calls. The provider tries:
//   1. Service account JSON via GOOGLE_APPLICATION_CREDENTIALS env var
//   2. User credentials at ~/.config/gcloud/application_default_credentials.json
//   3. GCE/GKE metadata server (when running on Google Cloud)
//
// Tokens are cached by gcp_auth and refreshed ~5 minutes before expiry.
// A static VERTEX_ADC_TOKEN env var serves as fallback when ADC is
// unavailable (e.g., CI/testing without GCP credentials).
//
// Exposed via GET /vertex/token for the Lua router to fetch per-request.
// ==========================================================================

use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Scope required for Vertex AI generateContent / streamGenerateContent.
/// cloud-platform is required — Vertex AI doesn't have a narrower scope
/// that covers all model endpoints (generateContent, predict, etc.).
const VERTEX_SCOPES: &[&str] = &["https://www.googleapis.com/auth/cloud-platform"];

/// Source of the token for observability.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenSource {
    /// Token from GCP Application Default Credentials (auto-refreshing).
    Adc,
    /// Static token from VERTEX_ADC_TOKEN env var (no refresh).
    Env,
    /// No token available.
    #[allow(dead_code)]
    None,
}

/// Thread-safe Vertex AI token provider.
pub struct VertexTokenProvider {
    /// gcp_auth provider (None if ADC init failed and we use static fallback)
    provider: Option<Arc<dyn gcp_auth::TokenProvider>>,
    /// Static fallback token from VERTEX_ADC_TOKEN env var
    static_token: Option<String>,
    /// Consecutive ADC failure count for circuit-breaker observability
    adc_failures: Arc<RwLock<u64>>,
}

impl VertexTokenProvider {
    /// Create a new VertexTokenProvider.
    ///
    /// Attempts to initialize GCP ADC. If that fails, falls back to the
    /// VERTEX_ADC_TOKEN env var. If neither is available, token requests
    /// will return an error.
    pub async fn new() -> Self {
        let static_token = std::env::var("VERTEX_ADC_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());

        // Try ADC initialization
        match gcp_auth::provider().await {
            Ok(provider) => {
                info!("vertex_auth: ADC provider initialized successfully");
                Self {
                    provider: Some(provider),
                    static_token,
                    adc_failures: Arc::new(RwLock::new(0)),
                }
            }
            Err(e) => {
                if static_token.is_some() {
                    warn!(
                        "vertex_auth: ADC init failed ({}), using static VERTEX_ADC_TOKEN fallback",
                        e
                    );
                } else {
                    warn!(
                        "vertex_auth: ADC init failed ({}) and no VERTEX_ADC_TOKEN set — \
                         Vertex AI requests will fail",
                        e
                    );
                }
                Self {
                    provider: None,
                    static_token,
                    adc_failures: Arc::new(RwLock::new(0)),
                }
            }
        }
    }

    /// Get a fresh access token for Vertex AI.
    ///
    /// Returns the token string and its source. The gcp_auth provider
    /// handles caching internally — repeated calls within the token
    /// lifetime (~1h) return the cached value without a network request.
    pub async fn get_token(&self) -> Result<(String, TokenSource), String> {
        // Try ADC first
        if let Some(ref provider) = self.provider {
            match provider.token(VERTEX_SCOPES).await {
                Ok(token) => {
                    // Reset failure counter on success
                    let mut failures = self.adc_failures.write().await;
                    *failures = 0;
                    return Ok((token.as_str().to_string(), TokenSource::Adc));
                }
                Err(e) => {
                    let mut failures = self.adc_failures.write().await;
                    *failures += 1;
                    let count = *failures;
                    warn!(
                        "vertex_auth: ADC token fetch failed (attempt #{}): {}",
                        count, e
                    );

                    // Fall through to static token
                }
            }
        }

        // Fallback to static token
        if let Some(ref token) = self.static_token {
            return Ok((token.clone(), TokenSource::Env));
        }

        Err("no Vertex AI credentials available (ADC failed, no VERTEX_ADC_TOKEN)".to_string())
    }

    /// Get the current consecutive ADC failure count (for metrics).
    #[allow(dead_code)]
    pub async fn adc_failure_count(&self) -> u64 {
        *self.adc_failures.read().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_no_credentials_returns_error() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        // In CI/test, neither ADC nor VERTEX_ADC_TOKEN should be set
        // This test verifies graceful degradation
        std::env::remove_var("VERTEX_ADC_TOKEN");
        std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS");

        let provider = VertexTokenProvider::new().await;

        // If no ADC and no env var, should error
        if provider.provider.is_none() && provider.static_token.is_none() {
            let result = provider.get_token().await;
            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .contains("no Vertex AI credentials available"));
        }
        // If ADC happens to work (e.g., running on a dev machine with gcloud),
        // that's fine too — the test just verifies no panic.
    }

    #[tokio::test]
    async fn test_static_token_fallback() {
        std::env::set_var("VERTEX_ADC_TOKEN", "test-token-12345");
        // Force no ADC by removing credentials env
        std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS");

        let provider = VertexTokenProvider {
            provider: None, // simulate ADC failure
            static_token: Some("test-token-12345".to_string()),
            adc_failures: Arc::new(RwLock::new(0)),
        };

        let (token, source) = provider.get_token().await.unwrap();
        assert_eq!(token, "test-token-12345");
        assert_eq!(source, TokenSource::Env);

        std::env::remove_var("VERTEX_ADC_TOKEN");
    }

    #[tokio::test]
    async fn test_failure_counter() {
        let provider = VertexTokenProvider {
            provider: None,
            static_token: None,
            adc_failures: Arc::new(RwLock::new(5)),
        };

        assert_eq!(provider.adc_failure_count().await, 5);
    }
}
