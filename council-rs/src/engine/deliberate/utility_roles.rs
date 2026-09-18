//! Utility roles (convergence judge, frame check): cascade resolution from
//! roles.yaml + env pins, the R1 frame check, and verbose cascade printing.

use crate::engine::context::RequestContext;
use crate::provider;
use crate::text::truncate_utf8;

use super::provider_auth_ready;

/// Shared provider/model attempt for frame-check and convergence-judge cascades.
#[derive(Debug, Clone)]
pub(crate) struct CascadeCandidate {
    pub provider: String,
    pub model: String,
    pub max_tok: u32,
}

fn cascade_from_env(
    model_var: &str,
    provider_var: &str,
    models: &crate::types::ModelRegistry,
    default_provider: &str,
    max_tok: u32,
) -> Option<Vec<CascadeCandidate>> {
    let model = std::env::var(model_var).ok()?;
    let model = model.trim().to_string();
    if model.is_empty() {
        return None;
    }
    let provider = std::env::var(provider_var)
        .ok()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .or_else(|| models.provider_for_model(&model))
        .unwrap_or_else(|| default_provider.to_string());
    let provider = crate::provider::canonical_provider_name(&provider);
    Some(vec![CascadeCandidate {
        provider,
        model,
        max_tok,
    }])
}

fn role_cascade_candidates(
    role: &crate::types::RoleDefinition,
    models: &crate::types::ModelRegistry,
    env_model_var: &str,
    env_provider_var: &str,
    default_provider: &str,
) -> Vec<CascadeCandidate> {
    if let Some(pin) = cascade_from_env(
        env_model_var,
        env_provider_var,
        models,
        default_provider,
        role.cascade.first().map(|s| s.max_tokens).unwrap_or(512),
    ) {
        return pin;
    }
    role.cascade
        .iter()
        .map(|step| CascadeCandidate {
            provider: crate::provider::canonical_provider_name(&step.provider),
            model: step.model.clone(),
            max_tok: step.max_tokens,
        })
        .collect()
}

pub(crate) fn frame_check_candidates(
    roles: &crate::types::RolesConfig,
    models: &crate::types::ModelRegistry,
) -> Vec<CascadeCandidate> {
    role_cascade_candidates(
        &roles.frame_check,
        models,
        "COUNCIL_FRAME_CHECK_MODEL",
        "COUNCIL_FRAME_CHECK_PROVIDER",
        "grok",
    )
}

pub(crate) fn convergence_judge_candidates(
    roles: &crate::types::RolesConfig,
    models: &crate::types::ModelRegistry,
) -> Vec<CascadeCandidate> {
    role_cascade_candidates(
        &roles.convergence_judge,
        models,
        "COUNCIL_JUDGE_MODEL",
        "COUNCIL_JUDGE_PROVIDER",
        "nvidia",
    )
}

/// Anti-prompt-poisoning: scan R1 prompt for embedded assumptions.
///
/// Uses the cheapest available LLM to identify constraints, negations,
/// and assumptions stated as facts in the prompt. Returns the prompt
/// with flagged items tagged `[UNVERIFIED]` so each seat can independently
/// challenge them.
///
/// Embedded assumptions can make every seat reason from the same false frame,
/// so they are marked before fan-out.
///
/// Cost: ~500 tokens. Skip with `--no-frame-check`.
/// Default cascade loaded from roles.yaml. Pin via COUNCIL_FRAME_CHECK_MODEL.
pub(super) async fn run_frame_check(
    prompt: &str,
    verbose: bool,
    roles: &crate::types::RolesConfig,
    models: &crate::types::ModelRegistry,
    req_ctx: &RequestContext,
) -> String {
    let truncated = truncate_utf8(prompt, 3000);
    let scan_prompt = format!(
        "You are a constraint auditor. Read the following deliberation \
         prompt and list every stated constraint, negation, or assumption \
         presented as fact. Focus on phrases like:\n\
         - 'we don't have X' / 'without X' / 'X is not available'\n\
         - 'given only Y' / 'limited to Y'\n\
         - 'there is no Z' / 'Z doesn't exist'\n\
         - 'the only option is W'\n\n\
         For each, output ONE LINE in this exact format:\n\
         ASSUMPTION: <quoted phrase> | VERIFY: <what to check>\n\n\
         If no embedded assumptions are found, respond with exactly: CLEAN\n\n\
         ---\n{}\n---",
        truncated
    );

    for candidate in frame_check_candidates(roles, models) {
        if req_ctx.via_gateway != Some(true) && !provider_auth_ready(&candidate.provider) {
            continue;
        }

        let resp = provider::ask_with_opts_and_context(
            &candidate.provider,
            &scan_prompt,
            "",
            &candidate.model,
            candidate.max_tok,
            req_ctx,
        )
        .await;
        if resp.error.is_some() || resp.text.is_empty() {
            if verbose {
                let why = resp.error.as_deref().unwrap_or("empty response");
                eprintln!(
                    "   ⏭️  Frame check {} ({}) skipped: {}",
                    candidate.model, candidate.provider, why
                );
            }
            continue;
        }

        let result = resp.text.trim().to_string();

        if verbose {
            eprintln!("   🔍 Frame check ({}) — {}ms", resp.model, resp.latency_ms);
        }

        if result.to_uppercase().starts_with("CLEAN") {
            if verbose {
                eprintln!("   ✅ No embedded assumptions detected.\n");
            }
            return prompt.to_string();
        }

        let mut assumptions: Vec<(String, String)> = Vec::new();
        for line in result.lines() {
            let line = line.trim();
            if line.to_uppercase().starts_with("ASSUMPTION:") {
                let rest = &line["ASSUMPTION:".len()..];
                let parts: Vec<&str> = rest.splitn(2, '|').collect();
                let quoted = parts[0].trim().to_string();
                let verify = if parts.len() > 1 {
                    parts[1].replace("VERIFY:", "").trim().to_string()
                } else {
                    String::new()
                };
                assumptions.push((quoted, verify));
            }
        }

        if assumptions.is_empty() {
            if verbose {
                eprintln!("   ✅ No parseable assumptions.\n");
            }
            return prompt.to_string();
        }

        if verbose {
            eprintln!(
                "   ⚠️  {} unverified constraint(s) detected:",
                assumptions.len()
            );
            for (q, v) in &assumptions {
                eprintln!("      • {}", q);
                if !v.is_empty() {
                    eprintln!("        → Verify: {}", v);
                }
            }
            eprintln!();
        }

        // Append structured warning block to the prompt
        let mut warning = String::from(
            "\n\n--- FRAME CHECK (auto-generated) ---\n\
             The following constraints were stated as facts but \
             have NOT been independently verified. Each seat should \
             challenge these before building on them:\n\n",
        );
        for (q, v) in &assumptions {
            warning.push_str(&format!("• [UNVERIFIED] {}\n", q));
            if !v.is_empty() {
                warning.push_str(&format!("  → To verify: {}\n", v));
            }
        }
        warning.push_str(
            "\nIf ANY of these assumptions are false, your analysis \
             may need to change fundamentally. State which of your \
             conclusions depend on which assumptions.\n\
             --- END FRAME CHECK ---\n",
        );

        return format!("{}{}", prompt, warning);
    }

    // No judge available — pass through unchanged
    if verbose {
        eprintln!("   ⏭️  Frame check skipped (no judge available).\n");
    }
    prompt.to_string()
}

/// Public entry point for frame checking — used by the streaming path.
pub async fn frame_check_prompt(
    prompt: &str,
    roles: &crate::types::RolesConfig,
    models: &crate::types::ModelRegistry,
    req_ctx: &RequestContext,
) -> String {
    run_frame_check(prompt, true, roles, models, req_ctx).await
}

/// Print active utility-role cascades (verbose startup — never invisible again).
pub fn print_role_cascades(roles: &crate::types::RolesConfig) {
    eprintln!("  Utility roles (roles.yaml):");
    for (name, def) in [
        ("convergence_judge", &roles.convergence_judge),
        ("frame_check", &roles.frame_check),
    ] {
        eprintln!("    {name}:");
        for (i, step) in def.cascade.iter().enumerate() {
            eprintln!(
                "      {}. {}/{} (max {} tok)",
                i + 1,
                step.provider,
                step.model,
                step.max_tokens
            );
        }
    }
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize tests that mutate process env (cascade pin vars are global;
    /// a parallel set/remove pair can otherwise cross the assertion window).
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn convergence_judge_default_cascade_from_roles_yaml() {
        let _guard = env_lock();
        let roles = crate::types::RolesConfig::built_in_defaults();
        let models = crate::types::ModelRegistry {
            models: std::collections::HashMap::new(),
        };
        unsafe {
            std::env::remove_var("COUNCIL_JUDGE_MODEL");
            std::env::remove_var("COUNCIL_JUDGE_PROVIDER");
        }
        let cascade = convergence_judge_candidates(&roles, &models);
        assert_eq!(cascade.len(), 3);
        assert_eq!(cascade[0].model, "grok-4.20-0309-reasoning");
        assert_eq!(cascade[1].model, "grok-4.3");
        assert_eq!(cascade[2].model, "mistralai/mistral-small-4-119b-2603");
    }

    #[test]
    fn frame_check_default_cascade_from_roles_yaml() {
        let _guard = env_lock();
        let roles = crate::types::RolesConfig::built_in_defaults();
        let models = crate::types::ModelRegistry {
            models: std::collections::HashMap::new(),
        };
        unsafe {
            std::env::remove_var("COUNCIL_FRAME_CHECK_MODEL");
            std::env::remove_var("COUNCIL_FRAME_CHECK_PROVIDER");
        }
        let cascade = frame_check_candidates(&roles, &models);
        assert_eq!(cascade.len(), 4);
        assert_eq!(cascade[0].model, "grok-4.3");
        assert_eq!(cascade[1].model, "grok-4.20-0309-reasoning");
        assert_eq!(cascade[2].model, "mistralai/mistral-small-4-119b-2603");
        assert_eq!(cascade[3].model, "gemini-3.5-flash");
        assert_eq!(cascade[3].provider, "gemini_agy");
    }

    #[test]
    fn frame_check_env_override_uses_registry_provider() {
        let _guard = env_lock();
        let roles = crate::types::RolesConfig::built_in_defaults();
        let mut entries = std::collections::HashMap::new();
        entries.insert(
            "nim_glm".into(),
            crate::types::ModelEntry {
                id: "mistralai/mistral-small-4-119b-2603".into(),
                provider: "nvidia".into(),
                description: String::new(),
                pricing: crate::types::ModelPricing {
                    input: 0.0,
                    cached_input: 0.0,
                    output: 0.0,
                },
            },
        );
        let models = crate::types::ModelRegistry { models: entries };
        unsafe {
            std::env::set_var(
                "COUNCIL_FRAME_CHECK_MODEL",
                "mistralai/mistral-small-4-119b-2603",
            );
            std::env::remove_var("COUNCIL_FRAME_CHECK_PROVIDER");
        }
        let cascade = frame_check_candidates(&roles, &models);
        assert_eq!(cascade.len(), 1);
        assert_eq!(cascade[0].provider, "nvidia");
        unsafe {
            std::env::remove_var("COUNCIL_FRAME_CHECK_MODEL");
        }
    }

    #[test]
    fn legacy_nim_slug_in_roles_normalizes_to_nvidia() {
        let mut roles = crate::types::RolesConfig::built_in_defaults();
        roles.frame_check.cascade[0].provider = "nim".into();
        roles.normalize_provider_slugs();
        assert_eq!(roles.frame_check.cascade[0].provider, "nvidia");
    }
}
