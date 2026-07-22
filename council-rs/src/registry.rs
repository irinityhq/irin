//! Provider Registry — Auto-discovery + User TOML + Compiled-in
//!
//! Discovery merges independent transport identities from user TOML,
//! compiled-in API env keys, supported local CLIs/adapters, and localhost
//! model servers. Same-family transports never overwrite each other.
//!
//! Design from War Room design.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Known API key environment variables → provider mapping.
/// Order matters: first match wins for default seat assignment.
const KNOWN_KEYS: &[(&str, &str, &str, &str)] = &[
    // (env_var, provider_slug, display_name, base_url)
    (
        "XAI_API_KEY",
        "grok_api",
        "Grok — xAI API",
        "https://api.x.ai/v1",
    ),
    (
        "ANTHROPIC_API_KEY",
        "claude_api",
        "Claude — Anthropic API",
        "https://api.anthropic.com/v1",
    ),
    (
        "OPENAI_API_KEY",
        "openai_api",
        "OpenAI API",
        "https://api.openai.com/v1",
    ),
    (
        "NVIDIA_API_KEY",
        "nvidia",
        "NVIDIA NIM",
        "https://integrate.api.nvidia.com/v1",
    ),
    (
        "DEEPSEEK_API_KEY",
        "deepseek",
        "DeepSeek",
        "https://api.deepseek.com/v1",
    ),
    (
        "NOUS_API_KEY",
        "nous",
        "Nous Research",
        "https://inference-api.nousresearch.com/v1",
    ),
    (
        "MISTRAL_API_KEY",
        "mistral",
        "Mistral",
        "https://api.mistral.ai/v1",
    ),
    (
        "GROQ_API_KEY",
        "groq",
        "Groq",
        "https://api.groq.com/openai/v1",
    ),
    (
        "PERPLEXITY_API_KEY",
        "perplexity",
        "Perplexity",
        "https://api.perplexity.ai",
    ),
    (
        "TOGETHER_API_KEY",
        "together",
        "Together AI",
        "https://api.together.xyz/v1",
    ),
    (
        "COHERE_API_KEY",
        "cohere",
        "Cohere",
        "https://api.cohere.com/v2",
    ),
    (
        "FIREWORKS_API_KEY",
        "fireworks",
        "Fireworks AI",
        "https://api.fireworks.ai/inference/v1",
    ),
    (
        "OPENROUTER_API_KEY",
        "openrouter",
        "OpenRouter",
        "https://openrouter.ai/api/v1",
    ),
    (
        "MOONSHOT_API_KEY",
        "kimi",
        "Kimi (Moonshot)",
        "https://api.moonshot.cn/v1",
    ),
    (
        "SAMBANOVA_API_KEY",
        "sambanova",
        "SambaNova",
        "https://api.sambanova.ai/v1",
    ),
    (
        "CEREBRAS_API_KEY",
        "cerebras",
        "Cerebras",
        "https://api.cerebras.ai/v1",
    ),
    // Vertex handled separately (ADC, no API key)
];

/// Localhost endpoints to probe for local models.
const LOCAL_PROBES: &[(&str, &str, u16)] = &[
    ("ollama", "Ollama", 11434),
    ("lmstudio", "LM Studio", 1234),
    ("localai", "LocalAI", 8080),
    ("llamacpp", "llama.cpp", 8081),
];

/// Provider capability flags.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Capabilities {
    /// Maximum context window in tokens.
    pub context_window: u32,
    /// Supports extended thinking / chain-of-thought.
    pub reasoning: bool,
    /// Supports tool/function calling.
    pub tool_use: bool,
    /// Supports vision/image input.
    pub vision: bool,
    /// Cost tier: "free", "low", "medium", "high", "premium"
    pub cost_tier: String,
    /// Whether this provider uses OpenAI-compatible API.
    pub openai_compatible: bool,
}

/// A discovered or configured provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredProvider {
    pub slug: String,
    pub display_name: String,
    pub base_url: String,
    pub auth_type: AuthType,
    /// If from user TOML and uses a custom base_url.
    pub trusted: bool,
    pub source: ProviderSource,
    pub capabilities: Capabilities,
    /// Preferred model for this provider (from /v1/models or config).
    pub default_model: String,
    /// All available models (populated after /v1/models probe).
    pub available_models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthType {
    /// Bearer token from env var.
    BearerToken { env_var: String },
    /// GCP Application Default Credentials (gcloud auth).
    Adc,
    /// No auth (local models).
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderSource {
    /// Compiled into the binary.
    Builtin,
    /// Discovered via env var scan.
    EnvScan,
    /// Found via localhost probe.
    LocalProbe,
    /// User-configured in TOML.
    UserConfig,
}

/// The complete provider registry.
#[derive(Debug, Default)]
pub struct ProviderRegistry {
    pub providers: HashMap<String, DiscoveredProvider>,
    pub discovery_log: Vec<String>,
}

#[derive(Debug, Default)]
struct LocalCliReadiness {
    claude: bool,
    grok_build: bool,
    codex: bool,
    agy: bool,
    gemini: bool,
    hermes: bool,
}

impl LocalCliReadiness {
    fn detect() -> Self {
        Self {
            claude: crate::provider::claude::is_claude_cli_available(),
            grok_build: crate::provider::agent_cli::is_grok_cli_available(),
            codex: crate::provider::agent_cli::is_codex_cli_available(),
            agy: crate::provider::agent_cli::is_agy_cli_available(),
            gemini: std::process::Command::new("gemini")
                .arg("--version")
                .stderr(std::process::Stdio::null())
                .output()
                .is_ok_and(|output| output.status.success()),
            hermes: crate::provider::hermes_cli::is_hermes_seat_available(),
        }
    }
}

/// A `[[providers]]` entry from `~/.config/council/providers.toml`. Only `slug`
/// and `base_url` are required; everything else defaults, so a minimal entry is
/// three lines. Used to add custom/self-hosted OpenAI-compatible endpoints.
#[derive(Debug, Deserialize)]
struct UserProviderEntry {
    slug: String,
    base_url: String,
    #[serde(default)]
    display_name: Option<String>,
    /// Env var holding the bearer token. Omit for a no-auth/local endpoint.
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    default_model: Option<String>,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    context_window: Option<u32>,
    #[serde(default)]
    openai_compatible: Option<bool>,
    #[serde(default)]
    cost_tier: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UserProvidersFile {
    #[serde(default)]
    providers: Vec<UserProviderEntry>,
}

fn usable_credential_value(value: &str) -> bool {
    !value.trim().is_empty()
}

fn no_provider_guidance() -> &'static str {
    "Configure one usable provider: an API key, an authenticated supported CLI, or a local model server."
}

fn safe_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('A'..='Z' | '_'))
        && chars.all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
}

fn is_reserved_transport_slug(slug: &str) -> bool {
    matches!(
        slug,
        "grok_api"
            | "grok_build"
            | "grok_hermes"
            | "claude_api"
            | "claude_code"
            | "openai_api"
            | "codex_cli"
            | "gemini_agy"
            | "gemini_vertex"
            | "gemini_cli"
            | "nvidia"
            | "nous"
            | "deepseek"
            | "mistral"
            | "groq"
            | "perplexity"
            | "together"
            | "cohere"
            | "fireworks"
            | "openrouter"
            | "kimi"
            | "sambanova"
            | "cerebras"
            | "ollama"
            | "lmstudio"
            | "localai"
            | "llamacpp"
    )
}

/// Parse a `providers.toml` body into [`DiscoveredProvider`]s. Each entry is
/// marked `source = UserConfig` and `trusted = true` (the user configured it
/// explicitly). Separated from the filesystem read so it is unit-testable.
fn parse_user_providers(content: &str) -> Result<Vec<DiscoveredProvider>, &'static str> {
    let parsed: UserProvidersFile =
        toml::from_str(content).map_err(|_| "invalid provider configuration")?;
    if parsed.providers.iter().any(|provider| {
        provider
            .api_key_env
            .as_deref()
            .is_some_and(|name| !safe_env_var_name(name))
    }) {
        return Err("invalid api_key_env name");
    }
    if parsed
        .providers
        .iter()
        .any(|provider| is_reserved_transport_slug(provider.slug.trim()))
    {
        return Err("reserved provider transport slug");
    }
    Ok(parsed
        .providers
        .into_iter()
        .map(|e| {
            let auth_type = match e.api_key_env {
                Some(env_var) => AuthType::BearerToken { env_var },
                None => AuthType::None,
            };
            DiscoveredProvider {
                display_name: e.display_name.unwrap_or_else(|| e.slug.clone()),
                slug: e.slug,
                base_url: e.base_url,
                auth_type,
                trusted: true,
                source: ProviderSource::UserConfig,
                capabilities: Capabilities {
                    context_window: e.context_window.unwrap_or(0),
                    openai_compatible: e.openai_compatible.unwrap_or(true),
                    cost_tier: e.cost_tier.unwrap_or_else(|| "unknown".into()),
                    ..Default::default()
                },
                default_model: e.default_model.unwrap_or_else(|| "auto".into()),
                available_models: e.models,
            }
        })
        .collect())
}

impl ProviderRegistry {
    /// Run the full discovery pipeline.
    /// Order: env scan → BYOK → Vertex ADC → localhost probe (for local) →
    /// user TOML (overwrites) → remote /models probes (for openai_compatible
    /// keyed providers after scan_env_keys detection).
    pub fn discover() -> Self {
        let mut reg = Self::default();

        // Layer 1: Scan environment variables
        reg.scan_env_keys();

        // Layer 1a: BYOK — catch any *_API_KEY we don't already know about
        reg.scan_byok();

        // Layer 1b: Detect supported local CLI transports using the same
        // readiness helpers as dispatch. Presence is detection, not proof that
        // the CLI is currently authenticated.
        let local_cli = LocalCliReadiness::detect();
        reg.apply_local_cli_readiness(local_cli);

        // Vertex is an independent exact transport. Detect it on its own
        // merits; it must never overwrite an agy or Gemini CLI seat.
        reg.check_vertex_adc();

        // Layer 2: Probe localhost for local models (Ollama /api/tags, LM Studio /v1/models)
        reg.probe_localhost();

        // Layer 3: Load user TOML (overwrites auto-detected entries)
        reg.load_user_toml();

        // Layer 4: Probe remote OpenAI-compatible keyed providers for real model lists
        // (after toml; only fills if available_models still empty from scan_env_keys)
        reg.probe_remote_models();

        reg
    }

    fn apply_local_cli_readiness(&mut self, readiness: LocalCliReadiness) {
        if readiness.claude {
            self.insert_local_cli_provider("claude_code", "Claude Code", "claude-code");
            self.discovery_log
                .push("✅ Claude CLI — binary detected".into());
        }
        if readiness.grok_build {
            self.insert_local_cli_provider("grok_build", "Grok Build", "grok-build");
            self.discovery_log
                .push("✅ Grok Build CLI — fingerprinted binary detected".into());
        }
        if readiness.hermes {
            self.insert_local_cli_provider("grok_hermes", "Grok via Hermes", "hermes");
            self.discovery_log
                .push("✅ Hermes seat adapter — executable detected".into());
        }
        if readiness.codex {
            self.insert_local_cli_provider("codex_cli", "Codex CLI", "codex");
            self.discovery_log
                .push("✅ Codex CLI — binary detected".into());
        }
        if readiness.agy {
            self.insert_local_cli_provider("gemini_agy", "Gemini via agy", "agy");
            self.discovery_log
                .push("✅ agy CLI — binary detected".into());
        }
        if readiness.gemini {
            self.insert_local_cli_provider("gemini_cli", "Gemini CLI", "gemini");
            self.discovery_log
                .push("✅ Gemini CLI — binary detected".into());
        }
    }

    fn insert_local_cli_provider(&mut self, slug: &str, display_name: &str, base_url: &str) {
        self.providers.insert(
            slug.into(),
            DiscoveredProvider {
                slug: slug.into(),
                display_name: display_name.into(),
                base_url: base_url.into(),
                auth_type: AuthType::None,
                trusted: true,
                source: ProviderSource::LocalProbe,
                capabilities: default_capabilities(slug),
                default_model: default_model(slug),
                available_models: known_cli_models(slug),
            },
        );
    }

    fn scan_env_keys(&mut self) {
        for (env_var, slug, display, base_url) in KNOWN_KEYS {
            if std::env::var(env_var).is_ok_and(|value| usable_credential_value(&value)) {
                self.discovery_log
                    .push(format!("✅ {} — {} detected", display, env_var));
                self.providers.insert(
                    slug.to_string(),
                    DiscoveredProvider {
                        slug: slug.to_string(),
                        display_name: display.to_string(),
                        base_url: base_url.to_string(),
                        auth_type: AuthType::BearerToken {
                            env_var: env_var.to_string(),
                        },
                        trusted: true, // Env vars are implicitly trusted
                        source: ProviderSource::EnvScan,
                        capabilities: default_capabilities(slug),
                        default_model: default_model(slug),
                        available_models: vec![],
                    },
                );
            }
        }
    }

    /// BYOK wildcard scanner — any *_API_KEY env var not in KNOWN_KEYS.
    /// Registered as generic OpenAI-compatible providers.
    /// Users configure base_url via ~/.config/council/providers.toml.
    fn scan_byok(&mut self) {
        let known_vars: std::collections::HashSet<&str> =
            KNOWN_KEYS.iter().map(|(var, _, _, _)| *var).collect();

        // Exclude known non-LLM API keys and aliases of known providers
        let exclude = [
            "GROK_API_KEY",             // xAI alias (use XAI_API_KEY)
            "ELEVENLABS_API_KEY",       // TTS, not LLM
            "FIRECRAWL_API_KEY",        // Web scraper
            "LINEAR_API_KEY",           // Project management
            "SLACK_API_KEY",            // Chat platform
            "GITHUB_API_KEY",           // Code hosting
            "NOTION_API_KEY",           // Docs
            "STRIPE_API_KEY",           // Payments
            "SENDGRID_API_KEY",         // Email
            "TWILIO_API_KEY",           // SMS
            "PINECONE_API_KEY",         // Vector DB
            "WEAVIATE_API_KEY",         // Vector DB
            "CLAWDBOT_API_KEY",         // Bot framework
            "EXA_API_KEY",              // Search
            "SERPER_API_KEY",           // Search
            "TAVILY_API_KEY",           // Search
            "GW_API_KEY",               // Gateway stack infra (not council LLM seat)
            "SUPERMEMORY_API_KEY",      // External memory stack (not council LLM seat)
            "SEMANTIC_SCHOLAR_API_KEY", // Scholarly search
        ];
        let exclude_set: std::collections::HashSet<&str> = exclude.iter().copied().collect();

        for (key, value) in std::env::vars() {
            if key.ends_with("_API_KEY")
                && usable_credential_value(&value)
                && !known_vars.contains(key.as_str())
                && !exclude_set.contains(key.as_str())
            {
                // Derive slug from env var: SOME_THING_API_KEY → some_thing
                let slug = key.strip_suffix("_API_KEY").unwrap_or(&key).to_lowercase();
                if is_reserved_transport_slug(&slug) {
                    self.discovery_log.push(format!(
                        "⚠️ {} ignored — reserved provider transport name",
                        key
                    ));
                    continue;
                }
                let display = slug.replace('_', " ");
                let display = display
                    .split_whitespace()
                    .map(|w| {
                        let mut c = w.chars();
                        match c.next() {
                            Some(first) => format!("{}{}", first.to_uppercase(), c.as_str()),
                            None => String::new(),
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ");

                self.discovery_log.push(format!(
                    "🔑 {} — {} (BYOK, configure base_url in providers.toml)",
                    display, key
                ));
                self.providers.insert(
                    slug.clone(),
                    DiscoveredProvider {
                        slug: slug.clone(),
                        display_name: format!("{} (BYOK)", display),
                        base_url: String::new(), // Must be configured in TOML
                        auth_type: AuthType::BearerToken { env_var: key },
                        trusted: false, // BYOK needs explicit trust
                        source: ProviderSource::EnvScan,
                        capabilities: Capabilities {
                            openai_compatible: true,
                            cost_tier: "unknown".into(),
                            ..Default::default()
                        },
                        default_model: "auto".into(),
                        available_models: vec![],
                    },
                );
            }
        }
    }

    fn check_vertex_adc(&mut self) {
        if !crate::provider::gemini::has_vertex_project_config() {
            self.discovery_log
                .push("⚠️ Gemini — Vertex project configuration not detected".into());
            return;
        }
        let ok = std::process::Command::new("gcloud")
            .args(["auth", "print-access-token"])
            .stderr(std::process::Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success());
        if ok {
            self.discovery_log
                .push("✅ Gemini — Vertex AI ADC detected".into());
            self.providers.insert(
                "gemini_vertex".into(),
                DiscoveredProvider {
                    slug: "gemini_vertex".into(),
                    display_name: "Gemini — Vertex AI".into(),
                    base_url: "https://aiplatform.googleapis.com/v1".into(),
                    auth_type: AuthType::Adc,
                    trusted: true,
                    source: ProviderSource::EnvScan,
                    capabilities: default_capabilities("gemini_vertex"),
                    default_model: "gemini-3.1-pro-preview".into(),
                    available_models: vec![],
                },
            );
        } else {
            self.discovery_log
                .push("⚠️ Gemini — Vertex ADC not available".into());
        }
    }

    fn probe_localhost(&mut self) {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("static local discovery client configuration");
        for (slug, display, port) in LOCAL_PROBES {
            // Quick TCP connect test — don't make HTTP calls during discovery
            let addr = format!("127.0.0.1:{}", port);
            let ok = std::net::TcpStream::connect_timeout(
                &addr.parse().unwrap(),
                std::time::Duration::from_millis(200),
            )
            .is_ok();

            if ok {
                self.discovery_log
                    .push(format!("✅ {} — localhost:{} responding", display, port));

                // Now do the HTTP model list (Ollama /api/tags or OpenAI /v1/models).
                // This was the missing piece for "plug and play" dropdowns.
                let base = format!("http://127.0.0.1:{}", port);
                let models = match *slug {
                    "ollama" => {
                        let url = format!("{}/api/tags", base);
                        match client.get(&url).send() {
                            Ok(resp) if resp.status().is_success() => {
                                if let Ok(v) = resp.json::<serde_json::Value>() {
                                    v.get("models")
                                        .and_then(|m| m.as_array())
                                        .map(|arr| {
                                            arr.iter()
                                                .filter_map(|m| {
                                                    m.get("name")
                                                        .and_then(|n| n.as_str())
                                                        .map(|s| s.to_string())
                                                })
                                                .collect::<Vec<_>>()
                                        })
                                        .unwrap_or_default()
                                } else {
                                    vec![]
                                }
                            }
                            _ => vec![],
                        }
                    }
                    "lmstudio" => {
                        let url = format!("{}/v1/models", base);
                        match client.get(&url).send() {
                            Ok(resp) if resp.status().is_success() => {
                                if let Ok(v) = resp.json::<serde_json::Value>() {
                                    v.get("data")
                                        .and_then(|d| d.as_array())
                                        .map(|arr| {
                                            arr.iter()
                                                .filter_map(|m| {
                                                    m.get("id")
                                                        .and_then(|i| i.as_str())
                                                        .map(|s| s.to_string())
                                                })
                                                .collect::<Vec<_>>()
                                        })
                                        .unwrap_or_default()
                                } else {
                                    vec![]
                                }
                            }
                            _ => vec![],
                        }
                    }
                    _ => vec![],
                };

                let default = models.first().cloned().unwrap_or_default();
                self.providers.insert(
                    slug.to_string(),
                    DiscoveredProvider {
                        slug: slug.to_string(),
                        display_name: display.to_string(),
                        base_url: format!("http://127.0.0.1:{}/v1", port),
                        auth_type: AuthType::None,
                        trusted: true, // Localhost is trusted
                        source: ProviderSource::LocalProbe,
                        capabilities: Capabilities {
                            openai_compatible: true,
                            cost_tier: "free".into(),
                            ..Default::default()
                        },
                        default_model: default,
                        available_models: models,
                    },
                );
            }
        }
    }

    fn probe_remote_models(&mut self) {
        // After scan_env_keys (and BYOK/ADC), for openai_compatible + BearerToken
        // providers (nvidia, groq, together, fireworks, openrouter, mistral,
        // deepseek, etc.), GET {base}/models (or /v1/models), parse data[].id,
        // filter embedding-only etc, cap 64, set available_models + default_model.
        // On failure: log ⚠️ warning, keep prior default_model (no regression).
        // Called after load_user_toml so user-provided model lists win.
        let client = reqwest::blocking::Client::new();
        // Collect to avoid borrow issues while mutating
        let to_probe: Vec<_> = self
            .providers
            .iter()
            .filter(|(_slug, p)| {
                p.capabilities.openai_compatible
                    && matches!(&p.auth_type, AuthType::BearerToken { .. })
                    && p.available_models.is_empty() // only if not already populated (e.g. from toml)
            })
            .map(|(slug, p)| {
                let env_var = if let AuthType::BearerToken { env_var } = &p.auth_type {
                    env_var.clone()
                } else {
                    String::new()
                };
                (slug.clone(), p.base_url.clone(), env_var)
            })
            .collect();

        for (slug, base_url, env_var) in to_probe {
            if env_var.is_empty() {
                continue;
            }
            let key = match std::env::var(&env_var) {
                Ok(k) if usable_credential_value(&k) => k,
                Err(_) => continue,
                Ok(_) => continue,
            };
            let url = if base_url.ends_with("/v1") {
                format!("{}/models", base_url)
            } else if base_url.ends_with('/') {
                format!("{}models", base_url)
            } else {
                format!("{}/models", base_url)
            };
            match client
                .get(&url)
                .header("Authorization", format!("Bearer {}", key))
                .timeout(std::time::Duration::from_secs(5))
                .send()
            {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(v) = resp.json::<serde_json::Value>()
                        && let Some(arr) = v.get("data").and_then(|d| d.as_array())
                    {
                        let mut models: Vec<String> = arr
                            .iter()
                            .filter_map(|m| {
                                m.get("id").and_then(|i| i.as_str()).map(|s| s.to_string())
                            })
                            .filter(|id| {
                                // filter obvious non-chat models
                                !id.contains("embed")
                                    && !id.contains("whisper")
                                    && !id.contains("dall")
                                    && !id.contains("tts")
                            })
                            .collect();
                        models.truncate(64);
                        if !models.is_empty() {
                            if let Some(p) = self.providers.get_mut(&slug) {
                                p.available_models = models.clone();
                                if p.default_model.is_empty() || p.default_model == "auto" {
                                    p.default_model = models[0].clone();
                                }
                            }
                            self.discovery_log.push(format!(
                                "✅ {} — fetched {} models via /models",
                                slug,
                                models.len()
                            ));
                        }
                    }
                }
                Ok(resp) => {
                    self.discovery_log.push(format!(
                        "⚠️ {} — /models probe returned HTTP {}",
                        slug,
                        resp.status()
                    ));
                }
                Err(e) => {
                    self.discovery_log
                        .push(format!("⚠️ {} — /models probe failed: {}", slug, e));
                }
            }
        }
    }

    fn load_user_toml(&mut self) {
        let config_path = dirs_config_path().join("providers.toml");
        if !config_path.exists() {
            return;
        }

        let content = match std::fs::read_to_string(&config_path) {
            Ok(c) => c,
            Err(_) => {
                self.discovery_log
                    .push("⚠️ Failed to read provider config".into());
                return;
            }
        };

        // Parse `[[providers]]` sections and merge. Reserved built-in transport
        // IDs are rejected so user config cannot change what a canonical ID means.
        match parse_user_providers(&content) {
            Ok(providers) => {
                for p in providers {
                    self.discovery_log
                        .push(format!("📄 User config: {} ({})", p.display_name, p.slug));
                    self.providers.insert(p.slug.clone(), p);
                }
            }
            Err(_) => {
                self.discovery_log
                    .push("⚠️ Failed to parse provider config".into());
            }
        }
    }

    fn sanitized_discovery_log(&self) -> Vec<String> {
        self.discovery_log
            .iter()
            .map(|line| scrub_discovery_log_line(line))
            .collect()
    }

    /// Print the opencode-style discovery summary.
    pub fn print_summary(&self) {
        if self.providers.is_empty() {
            eprintln!("\n⚠️  No providers detected!");
            eprintln!("   {}\n", no_provider_guidance());
            return;
        }

        eprintln!("\n┌─────────────────────────────────────────┐");
        eprintln!("│      🔍 Provider Auto-Discovery         │");
        eprintln!("├─────────────────────────────────────────┤");
        for msg in self.sanitized_discovery_log() {
            eprintln!(
                "│  {}{}│",
                msg,
                " ".repeat(38usize.saturating_sub(msg.chars().count()))
            );
        }
        eprintln!("├─────────────────────────────────────────┤");
        eprintln!(
            "│  {} provider path(s) detected            │",
            self.providers.len()
        );
        eprintln!("└─────────────────────────────────────────┘\n");
    }

    /// JSON mirror of the discovery summary for `GET /api/discover` (feature contract).
    ///
    /// Pinned wire contract:
    /// `{ "providers": [{ "name", "label", "family", "transport", "available", "gateway_supported", "source", "env_hint", "models" }], "log": [...] }`
    ///
    /// `env_hint` is the env var NAME only — never values or key fragments.
    /// Detected providers are `available: true`; known-but-undetected
    /// providers are merged in as `available: false` rows so the UI can show
    /// which `*_API_KEY` would enable them. The discovery log is scrubbed of
    /// anything token-shaped as a second guard (current log lines only ever
    /// embed var names).
    pub fn to_discover_json(&self) -> serde_json::Value {
        use serde_json::{Value, json};

        let mut rows: Vec<Value> = self
            .providers
            .values()
            .map(|p| {
                let (label, family, transport) = provider_identity(&p.slug, Some(&p.display_name));
                let env_hint = match &p.auth_type {
                    AuthType::BearerToken { env_var } if safe_env_var_name(env_var) => {
                        Value::String(env_var.clone())
                    }
                    AuthType::BearerToken { .. } => Value::Null,
                    // ADC (gcloud auth) and local providers have no env var.
                    AuthType::Adc | AuthType::None => Value::Null,
                };
                // available_models populated by probe_localhost() or probe_remote_models()
                // (for keyed OpenAI-compat) or from user providers.toml; fall back to
                // default_model if still empty.
                let models: Vec<String> = if !p.available_models.is_empty() {
                    p.available_models.clone()
                } else {
                    known_cli_models(&p.slug)
                };
                json!({
                    "name": p.slug,
                    "label": label,
                    "family": family,
                    "transport": transport,
                    "available": true,
                    "gateway_supported": gateway_supports_transport(&p.slug),
                    "source": provider_source_str(&p.source),
                    "env_hint": env_hint,
                    "models": models,
                })
            })
            .collect();

        // Known-but-undetected providers: surface the env var that enables them.
        for (env_var, slug, display, _base_url) in KNOWN_KEYS {
            if !self.providers.contains_key(*slug) {
                let (label, family, transport) = provider_identity(slug, Some(display));
                rows.push(json!({
                    "name": slug,
                    "label": label,
                    "family": family,
                    "transport": transport,
                    "available": false,
                    "gateway_supported": gateway_supports_transport(slug),
                    "source": "builtin",
                    "env_hint": env_var,
                    "models": known_cli_models(slug),
                }));
            }
        }
        // Local/OAuth transports are selectable seats too. Always emit them,
        // including unavailable rows, so the UI can explain rather than hide.
        for slug in [
            "grok_build",
            "grok_hermes",
            "claude_code",
            "codex_cli",
            "gemini_agy",
            "gemini_vertex",
            "gemini_cli",
        ] {
            if !self.providers.contains_key(slug) {
                let (label, family, transport) = provider_identity(slug, None);
                rows.push(json!({
                    "name": slug,
                    "label": label,
                    "family": family,
                    "transport": transport,
                    "available": false,
                    "gateway_supported": gateway_supports_transport(slug),
                    "source": "builtin",
                    "env_hint": Value::Null,
                    "models": known_cli_models(slug),
                }));
            }
        }

        // HashMap iteration order is random — sort for a stable response.
        rows.sort_by(|a, b| {
            a["name"]
                .as_str()
                .unwrap_or("")
                .cmp(b["name"].as_str().unwrap_or(""))
        });

        json!({
            "providers": rows,
            "log": self.sanitized_discovery_log(),
        })
    }

    /// Auto-assemble a cabinet from discovered providers.
    /// Rules: highest-tier reasoning model per provider, capped at 4 seats.
    pub fn auto_cabinet(&self) -> Option<crate::types::Cabinet> {
        if self.providers.len() < 2 {
            return None;
        }

        // Rank providers by cost tier (premium > high > medium > low > free)
        let tier_rank = |tier: &str| -> u8 {
            match tier {
                "premium" => 5,
                "high" => 4,
                "medium" => 3,
                "low" => 2,
                "free" => 1,
                _ => 0,
            }
        };

        let mut ranked: Vec<_> = self.providers.values().collect();
        ranked.sort_by(|a, b| {
            tier_rank(&b.capabilities.cost_tier).cmp(&tier_rank(&a.capabilities.cost_tier))
        });

        // Take top 4 for seats, top 1 for chair
        let seat_count = ranked.len().min(4);
        let seats: Vec<crate::types::Seat> = ranked[..seat_count]
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let role = match i {
                    0 => "strategist",
                    1 => "mirror",
                    2 => "operator",
                    _ => "analyst",
                };
                crate::types::Seat {
                    name: format!("Seat-{} ({})", i + 1, p.display_name),
                    provider: p.slug.clone(),
                    model: p.default_model.clone(),
                    system: role.to_string(),
                }
            })
            .collect();

        // Chair: highest-tier provider
        let chair_provider = &ranked[0];
        let chair = crate::types::Chair {
            name: "Chair".into(),
            provider: chair_provider.slug.clone(),
            model: chair_provider.default_model.clone(),
            system: None,
            thinking_effort: Some("high".into()),
        };

        Some(crate::types::Cabinet {
            hash: String::new(),
            name: "Auto-Assembled".into(),
            description: format!("Auto-assembled from {} detected providers", seat_count),
            rounds: if seat_count >= 4 { 2 } else { 1 },
            seats,
            chair,
            local_code_only: false,
            synthesis_mode: crate::types::SynthesisMode::Generic,
        })
    }
}

/// Stable wire string for a provider source — matches the serde
/// `rename_all = "lowercase"` casing of `ProviderSource`.
fn provider_source_str(source: &ProviderSource) -> &'static str {
    match source {
        ProviderSource::Builtin => "builtin",
        ProviderSource::EnvScan => "envscan",
        ProviderSource::LocalProbe => "localprobe",
        ProviderSource::UserConfig => "userconfig",
    }
}

/// Redact token-shaped substrings before discovery log lines leave the
/// process (`GET /api/discover`). Env var NAMES survive (UPPER_SNAKE /
/// lower_snake, no digits); long mixed-case, digit-bearing, all-numeric, or
/// unbroken single-case runs — the shape of actual key material — get
/// replaced. Defense in depth: current log lines only ever embed var names
/// (see scan_env_keys / scan_byok).
fn scrub_key_material(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut token = String::new();
    for ch in line.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            token.push(ch);
        } else {
            push_scrubbed(&mut out, &token);
            token.clear();
            out.push(ch);
        }
    }
    push_scrubbed(&mut out, &token);
    out
}

fn scrub_discovery_log_line(line: &str) -> String {
    let home = std::env::var("HOME").ok();
    let mut out = String::with_capacity(line.len());
    for token in line.split_inclusive(char::is_whitespace) {
        let content = token.trim_end_matches(char::is_whitespace);
        let whitespace = &token[content.len()..];
        let candidate = content.trim_start_matches(['(', '[', '{', '\'', '"']);
        let is_user_path = home
            .as_deref()
            .is_some_and(|home| !home.is_empty() && candidate.starts_with(home))
            || candidate.starts_with("/Users/")
            || candidate.starts_with("/home/")
            || candidate.starts_with("/private/var/")
            || candidate.starts_with("/private/tmp/")
            || candidate.starts_with("/var/folders/")
            || candidate.starts_with("/tmp/");
        if is_user_path {
            out.push_str("[PATH]");
        } else {
            out.push_str(content);
        }
        out.push_str(whitespace);
    }
    scrub_key_material(&out)
}

fn push_scrubbed(out: &mut String, token: &str) {
    if looks_like_key_material(token) {
        out.push_str("[REDACTED]");
    } else {
        out.push_str(token);
    }
}

fn looks_like_key_material(token: &str) -> bool {
    if token.len() < 20 {
        return false;
    }
    let has_lower = token.chars().any(|c| c.is_ascii_lowercase());
    let has_upper = token.chars().any(|c| c.is_ascii_uppercase());
    let has_digit = token.chars().any(|c| c.is_ascii_digit());
    // Mixed-case or digit-bearing letter runs — the classic key shape.
    if (has_lower && has_upper) || (has_digit && (has_lower || has_upper)) {
        return true;
    }
    // Known-safe shapes stay readable: env var NAMES and snake/kebab words
    // (single-case letters broken by `_` / `-`, no digits) — e.g.
    // SEMANTIC_SCHOLAR_API_KEY and similar environment variable names.
    if !has_digit && (token.contains('_') || token.contains('-')) {
        return false;
    }
    // Remaining long single-class runs (all-numeric, unbroken all-lower /
    // all-upper) with enough alphanumerics are still key-shaped.
    token.chars().filter(|c| c.is_ascii_alphanumeric()).count() >= 12
}

/// Default capabilities for known providers.
fn default_capabilities(slug: &str) -> Capabilities {
    match slug {
        "grok" | "grok_api" | "grok_build" | "grok_hermes" => Capabilities {
            context_window: 256_000,
            reasoning: true,
            tool_use: true,
            vision: true,
            cost_tier: "high".into(),
            openai_compatible: true,
        },
        "claude" | "claude_api" | "claude_code" => Capabilities {
            context_window: 200_000,
            reasoning: true,
            tool_use: true,
            vision: true,
            cost_tier: "premium".into(),
            openai_compatible: false,
        },
        "gpt" | "openai_api" | "codex_cli" => Capabilities {
            context_window: 1_000_000,
            reasoning: true,
            tool_use: true,
            vision: true,
            cost_tier: "premium".into(),
            openai_compatible: true,
        },
        "gemini" | "gemini_agy" | "gemini_vertex" | "gemini_cli" => Capabilities {
            context_window: 2_000_000,
            reasoning: true,
            tool_use: true,
            vision: true,
            cost_tier: "high".into(),
            openai_compatible: false,
        },
        "deepseek" => Capabilities {
            context_window: 128_000,
            reasoning: true,
            tool_use: true,
            vision: false,
            cost_tier: "low".into(),
            openai_compatible: true,
        },
        "mistral" => Capabilities {
            context_window: 128_000,
            reasoning: true,
            tool_use: true,
            vision: true,
            cost_tier: "medium".into(),
            openai_compatible: true,
        },
        "groq" => Capabilities {
            context_window: 128_000,
            reasoning: false,
            tool_use: true,
            vision: false,
            cost_tier: "low".into(),
            openai_compatible: true,
        },
        "perplexity" => Capabilities {
            context_window: 128_000,
            reasoning: true,
            tool_use: false,
            vision: false,
            cost_tier: "medium".into(),
            openai_compatible: true,
        },
        "together" => Capabilities {
            context_window: 128_000,
            reasoning: false,
            tool_use: true,
            vision: false,
            cost_tier: "low".into(),
            openai_compatible: true,
        },
        "openrouter" => Capabilities {
            context_window: 200_000,
            reasoning: true,
            tool_use: true,
            vision: true,
            cost_tier: "medium".into(),
            openai_compatible: true,
        },
        "nvidia" => Capabilities {
            context_window: 128_000,
            reasoning: true,
            tool_use: true,
            vision: true,
            cost_tier: "free".into(),
            openai_compatible: true,
        },
        "nous" => Capabilities {
            context_window: 128_000,
            reasoning: true,
            tool_use: true,
            vision: false,
            cost_tier: "low".into(),
            openai_compatible: true,
        },
        "kimi" => Capabilities {
            context_window: 200_000,
            reasoning: true,
            tool_use: true,
            vision: false,
            cost_tier: "low".into(),
            openai_compatible: true,
        },
        "sambanova" => Capabilities {
            context_window: 128_000,
            reasoning: false,
            tool_use: false,
            vision: false,
            cost_tier: "low".into(),
            openai_compatible: true,
        },
        "cerebras" => Capabilities {
            context_window: 128_000,
            reasoning: false,
            tool_use: false,
            vision: false,
            cost_tier: "low".into(),
            openai_compatible: true,
        },
        _ => Capabilities {
            context_window: 32_000,
            reasoning: false,
            tool_use: false,
            vision: false,
            cost_tier: "low".into(),
            openai_compatible: true,
        },
    }
}

/// Default model for known providers.
fn default_model(slug: &str) -> String {
    match slug {
        "grok" | "grok_api" | "grok_hermes" => "grok-4.3",
        "grok_build" => "grok-build",
        "claude" | "claude_api" | "claude_code" => "claude-opus-4-8",
        "gpt" | "openai_api" | "codex_cli" => "gpt-5.6-sol",
        "gemini" | "gemini_vertex" | "gemini_cli" => "gemini-3.1-pro-preview",
        "gemini_agy" => "agy-default",
        "deepseek" => "deepseek-v4-flash",
        "mistral" => "mistral-large-latest",
        "groq" => "llama-4-maverick-17b-128e-instruct",
        "perplexity" => "sonar-pro",
        "together" => "meta-llama/Llama-4-Maverick-17B-128E-Instruct-FP8",
        "fireworks" => "accounts/fireworks/models/llama-v4-maverick-instruct-basic",
        "openrouter" => "anthropic/claude-opus-4-8",
        "cohere" => "command-a-03-2025",
        "nvidia" => "nvidia/nemotron-3-super-120b-a12b",
        "nous" => "Hermes-4-405B",
        "kimi" => "moonshot-v1-auto",
        "sambanova" => "Meta-Llama-3.3-70B-Instruct",
        "cerebras" => "llama-4-scout-17b-16e-instruct",
        _ => "auto",
    }
    .into()
}

/// Reasonable model lists for providers that don't get full /models probes
/// (e.g. claude/grok/gemini CLI transports, or when no API key for probe).
/// Used for undetected and non-probed detected providers so dropdowns have
/// useful choices instead of single default or manual entry.
fn known_cli_models(slug: &str) -> Vec<String> {
    match slug {
        "claude" | "claude_api" => vec![
            "claude-opus-4-8".into(),
            "claude-opus-4-6".into(),
            "claude-sonnet-4-5".into(),
            "claude-sonnet-5".into(),
        ],
        "claude_code" => vec![
            "claude-fable-5".into(),
            "claude-opus-4-8".into(),
            "claude-opus-4-6".into(),
            "claude-sonnet-4-5".into(),
            "claude-sonnet-5".into(),
        ],
        "grok" => vec![
            "grok-4.3".into(),
            "grok-build".into(),
            "grok-composer-2.5-fast".into(),
            "grok-multi-agent".into(),
        ],
        "grok_api" | "grok_hermes" => vec![
            "grok-4.3".into(),
            "grok-4.20-0309-reasoning".into(),
            "grok-4.20-0309-non-reasoning".into(),
            "grok-4.20-multi-agent-0309".into(),
            "grok-4-1-fast-non-reasoning".into(),
        ],
        "grok_build" => vec![
            "grok-4.5".into(),
            "grok-build".into(),
            "grok-composer-2.5-fast".into(),
            "grok-multi-agent".into(),
        ],
        "gemini" => vec![
            "agy-default".into(),
            "gemini-3.1-pro-preview".into(),
            "gemini-3.5-flash".into(),
            "gemini-3.1-flash-lite".into(),
        ],
        "gemini_vertex" | "gemini_cli" => vec![
            "gemini-3.1-pro-preview".into(),
            "gemini-3.5-flash".into(),
            "gemini-3.1-flash-lite".into(),
        ],
        "gemini_agy" => vec![
            "agy-default".into(),
            "gemini-3.1-pro-preview".into(),
            "gemini-3.5-flash".into(),
            "gemini-3.1-flash-lite".into(),
        ],
        "gpt" | "openai_api" | "codex_cli" => vec![
            "gpt-5.6-sol".into(),
            "gpt-5.5-2026-04-23".into(),
            "o1".into(),
            "gpt-4o".into(),
            "gpt-4o-mini".into(),
        ],
        _ => vec![default_model(slug)],
    }
}

/// Stable product identity for a selectable transport. Custom providers retain
/// their configured display name and use their slug as the family.
fn provider_identity(slug: &str, configured_label: Option<&str>) -> (String, String, String) {
    let known = match slug {
        "grok_api" => Some(("Grok — xAI API", "xai", "api")),
        "grok_build" => Some(("Grok Build", "xai", "grok_build_cli")),
        "grok_hermes" => Some(("Grok via Hermes", "xai", "hermes_adapter")),
        "claude_api" => Some(("Claude — Anthropic API", "anthropic", "api")),
        "claude_code" => Some(("Claude Code", "anthropic", "claude_code_cli")),
        "openai_api" => Some(("OpenAI API", "openai", "api")),
        "codex_cli" => Some(("Codex CLI", "openai", "codex_cli")),
        "gemini_agy" => Some(("Gemini via agy", "google", "agy_cli")),
        "gemini_vertex" => Some(("Gemini — Vertex AI", "google", "vertex_adc")),
        "gemini_cli" => Some(("Gemini CLI", "google", "gemini_cli")),
        "ollama" | "lmstudio" | "localai" | "llamacpp" => {
            Some((configured_label.unwrap_or(slug), "local", "local_http"))
        }
        _ => None,
    };
    if let Some((label, family, transport)) = known {
        return (label.into(), family.into(), transport.into());
    }
    (
        configured_label.unwrap_or(slug).to_string(),
        slug.to_string(),
        "api".into(),
    )
}

/// Whether Gateway has a concrete adapter for this Council transport ID.
/// Keep this list aligned with `gateway/lua/lib/council_transport.lua`.
fn gateway_supports_transport(slug: &str) -> bool {
    matches!(
        slug,
        "grok_api"
            | "claude_api"
            | "claude_code"
            | "openai_api"
            | "codex_cli"
            | "gemini_vertex"
            | "gemini_cli"
            | "nvidia"
            | "nim"
    )
}

/// Platform-aware config directory.
fn dirs_config_path() -> std::path::PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return std::path::PathBuf::from(xdg).join("council");
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            return std::path::PathBuf::from(home).join(".config/council");
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(home) = std::env::var("HOME") {
            return std::path::PathBuf::from(home).join(".config/council");
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return std::path::PathBuf::from(appdata).join("council");
        }
    }
    std::path::PathBuf::from(".config/council")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_user_providers_minimal_entry() {
        let toml = r#"
[[providers]]
slug = "myhost"
base_url = "https://api.myhost.example/v1"
"#;
        let providers = parse_user_providers(toml).expect("parse");
        assert_eq!(providers.len(), 1);
        let p = &providers[0];
        assert_eq!(p.slug, "myhost");
        assert_eq!(p.display_name, "myhost"); // defaults to slug
        assert_eq!(p.base_url, "https://api.myhost.example/v1");
        assert!(matches!(p.auth_type, AuthType::None)); // no api_key_env → no auth
        assert!(p.trusted);
        assert!(matches!(p.source, ProviderSource::UserConfig));
        assert!(p.capabilities.openai_compatible); // defaults true
        assert_eq!(p.default_model, "auto");
        assert!(p.available_models.is_empty());
    }

    #[test]
    fn parse_user_providers_full_entry() {
        let toml = r#"
[[providers]]
slug = "acme"
display_name = "Acme LLM"
base_url = "https://acme.example/v1"
api_key_env = "ACME_API_KEY"
default_model = "acme-large"
models = ["acme-large", "acme-small"]
context_window = 128000
openai_compatible = false
cost_tier = "high"
"#;
        let providers = parse_user_providers(toml).expect("parse");
        assert_eq!(providers.len(), 1);
        let p = &providers[0];
        assert_eq!(p.display_name, "Acme LLM");
        assert!(
            matches!(&p.auth_type, AuthType::BearerToken { env_var } if env_var == "ACME_API_KEY")
        );
        assert_eq!(p.default_model, "acme-large");
        assert_eq!(p.available_models, vec!["acme-large", "acme-small"]);
        assert_eq!(p.capabilities.context_window, 128000);
        assert!(!p.capabilities.openai_compatible);
        assert_eq!(p.capabilities.cost_tier, "high");
    }

    #[test]
    fn parse_user_providers_empty_is_ok() {
        assert!(parse_user_providers("").expect("empty parses").is_empty());
    }

    #[test]
    fn parse_user_providers_rejects_malformed() {
        // missing required `base_url`
        let toml = "[[providers]]\nslug = \"x\"\n";
        assert!(parse_user_providers(toml).is_err());
    }

    #[test]
    fn user_provider_cannot_redefine_a_reserved_transport() {
        let toml = r#"
[[providers]]
slug = "grok_build"
base_url = "https://example.invalid/v1"
api_key_env = "CUSTOM_API_KEY"
"#;
        assert_eq!(
            parse_user_providers(toml).unwrap_err(),
            "reserved provider transport slug"
        );
        assert!(is_reserved_transport_slug("claude_code"));
        assert!(!is_reserved_transport_slug("my_private_endpoint"));
    }

    #[test]
    fn unsafe_user_api_key_name_never_becomes_an_env_hint() {
        let toml = r#"
[[providers]]
slug = "custom"
base_url = "https://example.invalid/v1"
        api_key_env = "not an env name/or a value"
"#;
        assert!(
            parse_user_providers(toml).is_err(),
            "unsafe api_key_env must reject the keyed provider, not downgrade it to no-auth"
        );
    }

    #[test]
    fn credential_values_must_be_nonempty_after_trim() {
        assert!(!usable_credential_value(""));
        assert!(!usable_credential_value("   \t\n"));
        assert!(usable_credential_value("synthetic-nonempty-value"));
    }

    #[test]
    fn no_provider_guidance_names_one_usable_provider_path() {
        let guidance = no_provider_guidance();
        assert!(guidance.contains("one usable provider"));
        assert!(guidance.contains("API key"));
        assert!(guidance.contains("authenticated supported CLI"));
        assert!(guidance.contains("local model server"));
        assert!(!guidance.contains("2 API keys"));
    }

    #[test]
    fn supported_local_cli_transports_appear_as_detected_canonical_providers() {
        let mut grok_build_only = ProviderRegistry::default();
        grok_build_only.apply_local_cli_readiness(LocalCliReadiness {
            grok_build: true,
            ..Default::default()
        });
        let grok = grok_build_only.providers.get("grok_build").unwrap();
        assert_eq!(grok.default_model, "grok-build");
        assert!(!grok.available_models.contains(&"grok-4.3".to_string()));

        let mut reg = ProviderRegistry::default();
        reg.providers
            .insert("grok_api".into(), detected("grok_api", "XAI_API_KEY"));
        reg.providers.insert(
            "claude_api".into(),
            detected("claude_api", "ANTHROPIC_API_KEY"),
        );
        reg.providers.insert(
            "openai_api".into(),
            detected("openai_api", "OPENAI_API_KEY"),
        );
        reg.apply_local_cli_readiness(LocalCliReadiness {
            claude: true,
            grok_build: true,
            codex: true,
            agy: true,
            gemini: true,
            hermes: true,
        });

        let json = reg.to_discover_json();
        let providers = json["providers"].as_array().unwrap();
        for name in [
            "claude_code",
            "grok_build",
            "grok_hermes",
            "codex_cli",
            "gemini_agy",
            "gemini_cli",
        ] {
            let row = providers.iter().find(|row| row["name"] == name).unwrap();
            assert_eq!(row["available"], true, "{name} was not detected");
            assert_eq!(row["source"], "localprobe");
            assert!(row["env_hint"].is_null());
        }
        let log = json["log"].as_array().unwrap();
        assert!(
            log.iter()
                .any(|line| line.as_str().unwrap().contains("Claude CLI"))
        );
        assert!(
            log.iter()
                .any(|line| line.as_str().unwrap().contains("Grok Build CLI"))
        );
        assert!(
            log.iter()
                .any(|line| line.as_str().unwrap().contains("Hermes"))
        );
        assert_eq!(reg.providers.len(), 9);
        assert!(reg.providers.contains_key("grok_api"));
        assert!(reg.providers.contains_key("claude_api"));
        assert!(reg.providers.contains_key("openai_api"));
        assert!(reg.providers.contains_key("grok_build"));
        assert!(reg.providers.contains_key("grok_hermes"));
    }

    /// Build a registry by hand — `discover()` reads env / shells out, which
    /// would make these tests machine-dependent.
    fn detected(slug: &str, env_var: &str) -> DiscoveredProvider {
        DiscoveredProvider {
            slug: slug.to_string(),
            display_name: slug.to_string(),
            base_url: String::new(),
            auth_type: AuthType::BearerToken {
                env_var: env_var.to_string(),
            },
            trusted: true,
            source: ProviderSource::EnvScan,
            capabilities: default_capabilities(slug),
            default_model: default_model(slug),
            available_models: vec![],
        }
    }

    #[test]
    fn discover_json_matches_pinned_contract_shape() {
        let mut reg = ProviderRegistry::default();
        reg.providers
            .insert("grok_api".into(), detected("grok_api", "XAI_API_KEY"));
        reg.discovery_log
            .push("✅ Grok (xAI) — XAI_API_KEY detected".into());

        let v = reg.to_discover_json();
        let providers = v["providers"].as_array().unwrap();
        assert!(!providers.is_empty());
        // The original fields remain and transport identity is additive.
        for row in providers {
            let obj = row.as_object().unwrap();
            assert_eq!(obj.len(), 9, "unexpected keys in {row}");
            for key in [
                "name",
                "label",
                "family",
                "transport",
                "available",
                "gateway_supported",
                "source",
                "env_hint",
                "models",
            ] {
                assert!(obj.contains_key(key), "missing {key} in {row}");
            }
        }

        let grok = providers.iter().find(|r| r["name"] == "grok_api").unwrap();
        assert_eq!(grok["available"], true);
        assert_eq!(grok["source"], "envscan");
        assert_eq!(grok["env_hint"], "XAI_API_KEY");
        assert_eq!(grok["models"][0], "grok-4.3");
        assert_eq!(grok["label"], "Grok — xAI API");
        assert_eq!(grok["family"], "xai");
        assert_eq!(grok["transport"], "api");
        assert_eq!(grok["gateway_supported"], true);

        // Undetected known providers appear with available:false + env hint.
        let claude = providers
            .iter()
            .find(|r| r["name"] == "claude_api")
            .unwrap();
        assert_eq!(claude["available"], false);
        assert_eq!(claude["env_hint"], "ANTHROPIC_API_KEY");

        for name in [
            "grok_build",
            "grok_hermes",
            "claude_code",
            "codex_cli",
            "gemini_agy",
            "gemini_vertex",
            "gemini_cli",
        ] {
            let row = providers.iter().find(|r| r["name"] == name).unwrap();
            assert_eq!(row["available"], false, "{name}");
            assert!(row["env_hint"].is_null(), "{name}");
            assert!(!row["models"].as_array().unwrap().is_empty(), "{name}");
        }

        for name in ["grok_build", "grok_hermes", "gemini_agy", "nous"] {
            let row = providers.iter().find(|r| r["name"] == name).unwrap();
            assert_eq!(row["gateway_supported"], false, "{name}");
        }

        assert_eq!(v["log"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn discover_json_scrubs_token_shaped_log_lines() {
        let mut reg = ProviderRegistry::default();
        reg.discovery_log
            .push("oops sk-proj-Abc123def456Ghi789jkl leaked".into());
        let v = reg.to_discover_json();
        let line = v["log"][0].as_str().unwrap();
        assert!(line.contains("[REDACTED]"), "got: {line}");
        assert!(
            !serde_json::to_string(&v).unwrap().contains("Abc123def456"),
            "key material leaked through /api/discover payload"
        );
    }

    #[test]
    fn discovery_log_scrubs_absolute_paths_and_keys_for_every_surface() {
        let mut reg = ProviderRegistry::default();
        let synthetic_home = ["/", "Users", "/example/.config/council/providers.toml"].concat();
        reg.discovery_log.push(format!(
            "failed to parse {synthetic_home}: sk-proj-Abc123def456Ghi789jkl"
        ));

        let expected = reg.sanitized_discovery_log();
        assert_eq!(expected.len(), 1);
        assert!(expected[0].contains("[PATH]"));
        assert!(expected[0].contains("[REDACTED]"));
        assert!(!expected[0].contains("/Users/"));
        assert!(!expected[0].contains("Abc123def456"));
        assert_eq!(reg.to_discover_json()["log"], serde_json::json!(expected));
    }

    #[test]
    fn scrub_preserves_env_var_names_and_known_log_lines() {
        for line in [
            "✅ Grok (xAI) — XAI_API_KEY detected",
            "✅ Nous Research — NOUS_API_KEY detected",
            "✅ Gemini (Vertex AI) — gcloud ADC detected",
            "🔑 Semantic Scholar — SEMANTIC_SCHOLAR_API_KEY (BYOK, configure base_url in providers.toml)",
            "✅ Ollama — localhost:11434 responding",
            "no key material found while scanning environment variables for providers",
        ] {
            assert_eq!(scrub_key_material(line), line);
        }
    }

    #[test]
    fn scrub_redacts_numeric_only_and_single_case_long_tokens() {
        // Purely numeric secrets previously survived the letters-AND-length
        // check; long unbroken lowercase tokens did too.
        for (line, expected) in [
            (
                "oops 123456789012345678901234 leaked",
                "oops [REDACTED] leaked",
            ),
            (
                "oops abcdefghijklmnopqrstuvwx leaked",
                "oops [REDACTED] leaked",
            ),
        ] {
            assert_eq!(scrub_key_material(line), expected);
        }
    }

    #[test]
    fn known_cli_models_codex_includes_codex_friendly_entries() {
        let models = known_cli_models("codex_cli");
        assert!(models.contains(&"gpt-5.6-sol".to_string()));
        assert!(models.len() >= 4, "expected at least a few codex models");
    }

    #[test]
    fn known_cli_models_exposes_fable_only_for_claude_code() {
        assert!(
            known_cli_models("claude_code")
                .iter()
                .any(|model| model == "claude-fable-5")
        );
        assert!(
            !known_cli_models("claude_api")
                .iter()
                .any(|model| model == "claude-fable-5")
        );
    }
}
