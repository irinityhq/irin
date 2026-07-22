//! DeepSeek provider client — native API at api.deepseek.com (DEEPSEEK_API_KEY).
//! Operator without a direct key: use `provider: nvidia` + `deepseek-ai/*` (NIM)
//! or `provider: nous` with a compatible `deepseek/*` model.
use crate::types::ProviderResponse;

pub async fn ask(prompt: &str, system: &str, model: &str, max_tokens: u32) -> ProviderResponse {
    super::openai_compat::ask("deepseek", prompt, system, model, max_tokens).await
}
