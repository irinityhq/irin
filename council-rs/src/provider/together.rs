//! Together provider client
use crate::types::ProviderResponse;

pub async fn ask(prompt: &str, system: &str, model: &str, max_tokens: u32) -> ProviderResponse {
    super::openai_compat::ask("together", prompt, system, model, max_tokens).await
}
