//! Ollama provider client (local)
use crate::types::ProviderResponse;

pub async fn ask(prompt: &str, system: &str, model: &str, max_tokens: u32) -> ProviderResponse {
    super::openai_compat::ask("ollama", prompt, system, model, max_tokens).await
}
