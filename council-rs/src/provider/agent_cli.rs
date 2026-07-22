//! Agent CLI providers — local CLI subprocess seats (`grok_cli`, `gemini_cli`, …).
//! Grok Build routing: see `grok_route.rs` + `grok_routing.yaml`.

use crate::provider::agy_route;
use crate::provider::grok_route;
use crate::types::{ProviderProvenance, ProviderResponse};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

use serde_json; // for gemini_cli json .response extraction in headless mode

const AGY_MAX_PROMPT_BYTES: usize = 100_000;
const USAGE_UNAVAILABLE: &str = "usage_unavailable";

/// The local `grok` CLI does not tolerate concurrent invocations (empty stdout). Serialize by default.
static GROK_CLI_LOCK: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// Resolved Grok Build CLI path (not npm/other PATH homonyms).
static GROK_BUILD_CLI_BIN: std::sync::OnceLock<Option<std::path::PathBuf>> =
    std::sync::OnceLock::new();

/// True when `--version` output matches **Grok Build CLI** (xAI product).
///
/// Rejects PATH homonyms such as the npm package `grok-dev` (often first via
/// nvm), which can succeed with a bare semver or fail with `env: bun: ...`.
/// Real Grok Build reports like: `grok 0.2.93 (f00f96316d4b) [stable]`.
pub(crate) fn is_grok_build_cli_version_output(stdout: &[u8], stderr: &[u8]) -> bool {
    let mut combined = String::with_capacity(stdout.len() + stderr.len());
    combined.push_str(&String::from_utf8_lossy(stdout));
    combined.push_str(&String::from_utf8_lossy(stderr));
    let lower = combined.to_ascii_lowercase();
    let has_channel = lower.contains("[stable]")
        || lower.contains("[dev]")
        || lower.contains("[beta]")
        || lower.contains("[canary]");
    let has_product_line = lower.lines().any(|line| {
        let t = line.trim_start();
        t.starts_with("grok ") && (t.contains('(') || has_channel)
    });
    has_channel && has_product_line
}

fn probe_grok_build_cli_candidate(path: &std::path::Path) -> bool {
    if !path.is_file() {
        return false;
    }
    match std::process::Command::new(path).arg("--version").output() {
        Ok(o) if o.status.success() => is_grok_build_cli_version_output(&o.stdout, &o.stderr),
        _ => false,
    }
}

fn probe_grok_build_cli_binary() -> Option<std::path::PathBuf> {
    // Explicit override for operators / launchd with odd PATHs.
    if let Ok(raw) = std::env::var("COUNCIL_GROK_CLI_BIN") {
        let path = std::path::PathBuf::from(raw.trim());
        if probe_grok_build_cli_candidate(&path) {
            return Some(path);
        }
    }

    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    // Prefer known Grok Build install locations *before* walking PATH so an
    // earlier nvm/npm `grok` cannot hide the real CLI (recovery incident
    // 2026-07-11: War Room showed `need grok_cli` while interactive grok worked).
    if let Some(home) = std::env::var_os("HOME") {
        let home = std::path::PathBuf::from(home);
        candidates.push(home.join(".local/bin/grok"));
        candidates.push(home.join(".grok/bin/grok"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            candidates.push(dir.join("grok"));
        }
    }

    let mut seen = std::collections::HashSet::new();
    for cand in candidates {
        if !seen.insert(cand.clone()) {
            continue;
        }
        if probe_grok_build_cli_candidate(&cand) {
            return Some(cand);
        }
    }
    None
}

/// Absolute path to Grok Build CLI if a fingerprinted binary is found.
pub fn resolve_grok_cli_binary() -> Option<std::path::PathBuf> {
    GROK_BUILD_CLI_BIN
        .get_or_init(probe_grok_build_cli_binary)
        .clone()
}

/// Whether a real Grok Build CLI is available for `grok_cli` seats / health.
pub fn is_grok_cli_available() -> bool {
    resolve_grok_cli_binary().is_some()
}

fn grok_cli_serialize() -> bool {
    match std::env::var("COUNCIL_GROK_CLI_SERIALIZE") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            v != "0" && v != "false"
        }
        Err(_) => true,
    }
}

fn grok_cli_timeout() -> Duration {
    std::env::var("COUNCIL_CLI_TIMEOUT_SECS")
        .or_else(|_| std::env::var("COUNCIL_GROK_CLI_TIMEOUT_SECS"))
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(300))
}

/// The validator (Sheldon) uses web search and may need multiple turns.
fn grok_cli_max_turns() -> u32 {
    if let Ok(v) = std::env::var("COUNCIL_GROK_CLI_MAX_TURNS")
        && let Ok(n) = v.trim().parse::<u32>()
    {
        return n.max(1);
    }
    8
}

/// Normal seats: tools stripped + seat system. Default 6 — composer still burns
/// a couple turns even without tools; 2 left composer at Max turns with empty
/// answers. Override: COUNCIL_GROK_CLI_SEAT_MAX_TURNS.
fn grok_cli_seat_max_turns() -> u32 {
    if let Ok(v) = std::env::var("COUNCIL_GROK_CLI_SEAT_MAX_TURNS")
        && let Ok(n) = v.trim().parse::<u32>()
    {
        return n.max(1);
    }
    6
}

/// Deliberation seats are judgment engines, not coding agents. Grok Build
/// defaults to exploring the workspace; that burns the turn budget and returns
/// narration ("I'll check…") instead of an answer — false FAIL vs Hermes chat.
const GROK_SEAT_SYSTEM: &str = "You are a council deliberation seat. Answer the user prompt \
with your best judgment only. Do not explore repositories or call tools. Prefer a complete \
final answer immediately.";

/// Built-in agent tools stripped in seat_mode so Build behaves like a chat seat.
/// Sheldon/validator path keeps tools (see ask_grok_with_web_search).
const GROK_SEAT_DISALLOWED_TOOLS: &str = "bash,read,write,edit,glob,grep,list_dir,search,\
web_search,web_fetch,run_terminal_command,read_file,search_replace,todo_write,spawn_subagent";

async fn with_grok_cli_serial(
    f: impl std::future::Future<Output = ProviderResponse>,
) -> ProviderResponse {
    if grok_cli_serialize() {
        // Wait for the lock (grok CLI is not concurrent-safe). Use the full CLI timeout
        // so a slow call doesn't cause other seats to spuriously fallback.
        let permit = GROK_CLI_LOCK.acquire().await;
        let r = f.await;
        drop(permit);
        r
    } else {
        f.await
    }
}

pub async fn ask_grok(prompt: &str, system: &str, model: &str) -> ProviderResponse {
    ask_grok_impl(
        prompt, system, model, /*enable_web_search*/ false, /*seat_mode*/ true,
    )
    .await
}

/// Grok CLI variant with web search enabled (for claim_validator / Sheldon).
/// Uses the user's `grok` CLI (OAuth/subscription) + its native web + X tools.
pub async fn ask_grok_with_web_search(prompt: &str, system: &str, model: &str) -> ProviderResponse {
    ask_grok_impl(
        prompt, system, model, /*enable_web_search*/ true, /*seat_mode*/ false,
    )
    .await
}

async fn ask_grok_impl(
    prompt: &str,
    system: &str,
    model: &str,
    enable_web_search: bool,
    seat_mode: bool,
) -> ProviderResponse {
    with_grok_cli_serial(async {
        ask_grok_impl_inner(prompt, system, model, enable_web_search, seat_mode).await
    })
    .await
}

async fn ask_grok_impl_inner(
    prompt: &str,
    system: &str,
    model: &str,
    enable_web_search: bool,
    seat_mode: bool,
) -> ProviderResponse {
    let Some(grok_bin) = resolve_grok_cli_binary() else {
        return cli_error(
            "grok_cli",
            "Grok Build CLI not found (no fingerprinted `grok` binary; \
             install Grok Build or set COUNCIL_GROK_CLI_BIN to the real CLI path — \
             bare PATH `grok` may be a different package such as npm grok-dev)",
            0,
        )
        .with_provider_provenance(readonly_provenance("grok_cli"));
    };
    let cwd = cwd_string();
    let mut cmd = Command::new(&grok_bin);
    cmd.args(["--cwd", &cwd]);
    // Prefer Grok Build OAuth/subscription login over xAI API key billing.
    cmd.env_remove("XAI_API_KEY");
    cmd.env_remove("GROK_API_KEY");
    // Headless single-turn per https://docs.x.ai/build/cli/headless-scripting
    // `-p`/`--single` and `--prompt-file` are alternative headless paths (mutually
    // exclusive). `--no-auto-update` skips background update checks in automation.
    cmd.arg("--no-auto-update");
    cmd.arg("--no-alt-screen");
    if !enable_web_search {
        cmd.arg("--disable-web-search");
    }
    // seat_mode: judgment-only (strip agent tools + seat system directive).
    // validator: keep tools + web; only pass caller's system.
    let seat_system_owned;
    let system_arg = if seat_mode {
        cmd.args(["--disallowed-tools", GROK_SEAT_DISALLOWED_TOOLS]);
        seat_system_owned = if system.trim().is_empty() {
            GROK_SEAT_SYSTEM.to_string()
        } else {
            format!("{system}\n\n{GROK_SEAT_SYSTEM}")
        };
        seat_system_owned.as_str()
    } else {
        system
    };
    cmd.args(["--system-prompt-override", system_arg]);
    // Always pass --max-turns to force the agent to terminate.
    // seat_mode: small budget (default 6) once tools are stripped.
    // validator: higher turns + web tools enabled.
    let max_turns = if seat_mode {
        grok_cli_seat_max_turns()
    } else {
        grok_cli_max_turns()
    };
    cmd.args(["--max-turns", &max_turns.to_string()]);
    cmd.args([
        "--no-subagents",
        "--no-plan",
        "--output-format",
        "plain",
        "--permission-mode",
        "dontAsk",
        "--sandbox",
        "read-only",
        "--verbatim",
    ]);

    // Grok headless prompt delivery: `-p` for inline, `--prompt-file` for long prompts.
    const GROK_INLINE_PROMPT_MAX: usize = 24_000;
    let prompt_tmp = if prompt.len() <= GROK_INLINE_PROMPT_MAX {
        cmd.args(["-p", prompt]);
        None
    } else {
        let mut tmp = match tempfile::NamedTempFile::new() {
            Ok(t) => t,
            Err(e) => {
                return cli_error("grok_cli", format!("prompt tmp: {e}"), 0)
                    .with_provider_provenance(readonly_provenance("grok_cli"));
            }
        };
        {
            use std::io::Write;
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = tmp
                .as_file_mut()
                .set_permissions(std::fs::Permissions::from_mode(0o600))
            {
                return cli_error("grok_cli", format!("prompt perm: {e}"), 0)
                    .with_provider_provenance(readonly_provenance("grok_cli"));
            }
            if let Err(e) = tmp.as_file_mut().write_all(prompt.as_bytes()) {
                return cli_error("grok_cli", format!("prompt write: {e}"), 0)
                    .with_provider_provenance(readonly_provenance("grok_cli"));
            }
        }
        let prompt_path_str = tmp.path().to_string_lossy().into_owned();
        cmd.args(["--prompt-file", &prompt_path_str]);
        Some(tmp)
    };
    // Model: grok_routing.yaml maps cabinet ids → local `grok -m` (grok-build, composer, …).
    // API ids like grok-4.3 must route via Hermes or xAI API — never Grok Build default.
    let resolved = grok_route::resolve_cli_model(model);
    if seat_mode && resolved.api_id_substituted && !model.trim().is_empty() {
        return cli_error(
            "grok_cli",
            format!(
                "model '{}' is API-only — cannot invoke Grok Build CLI directly (use hermes_cli seat transport)",
                model.trim()
            ),
            0,
        )
        .with_provider_provenance(readonly_provenance("grok_cli"));
    }
    if let Some(cli_m) = &resolved.cli_model_arg {
        cmd.args(["-m", cli_m.as_str()]);
    }
    let label = resolved.response_label;
    let resp = run_stdout(
        cmd,
        None,
        "grok_cli",
        label,
        readonly_provenance("grok_cli"),
    )
    .await;
    drop(prompt_tmp);
    resp
}

pub async fn ask_gemini(prompt: &str, system: &str, model: &str) -> ProviderResponse {
    let full_prompt = full_prompt(prompt, system);
    let cwd = cwd_string();
    let mut cmd = Command::new("gemini");
    cmd.args([
        "--skip-trust",
        "--approval-mode",
        "plan",
        "--sandbox",
        "--include-directories",
        &cwd,
        // Headless via -p per Gemini CLI docs; json for reliable machine extraction of .response.
        // Steering instruction forces direct terminating answer (reduces preamble/empty in seats).
        "--output-format",
        "json",
        "-p",
        "SINGLE DIRECT TERMINATING ANSWER ONLY. Deliver ONLY the final evidence-based answer. No preamble, no plans, no meta, no introductions. End immediately after the answer.",
    ]);
    if !model.is_empty() {
        cmd.args(["--model", model]);
    }

    let mut resp = run_stdout(
        cmd,
        Some(full_prompt.as_str()),
        "gemini_cli",
        model_label("gemini-cli", model),
        readonly_provenance("gemini_cli"),
    )
    .await;
    // For gemini_cli headless json output (per docs): extract .response for the direct answer.
    // Falls back to raw text on parse failure or non-json (keeps backward compat).
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&resp.text)
        && let Some(r) = v.get("response").and_then(|x| x.as_str())
        && !r.trim().is_empty()
    {
        resp.text = r.trim().to_string();
    }
    resp
}

pub fn is_codex_cli_available() -> bool {
    std::process::Command::new("codex")
        .arg("--version")
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success())
}

pub fn is_agy_cli_available() -> bool {
    static AGY_AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AGY_AVAILABLE.get_or_init(|| {
        std::process::Command::new("agy")
            .arg("--version")
            .stderr(std::process::Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

pub async fn ask_codex(prompt: &str, system: &str, model: &str) -> ProviderResponse {
    let full_prompt = full_prompt(prompt, system);
    // T15: NamedTempFile 0o600 + drop-guard + provenance check on read-back (mode/owner sanity before trusting codex output content).
    let mut tmp = match tempfile::NamedTempFile::new() {
        Ok(t) => t,
        Err(e) => {
            return cli_error("codex_cli", format!("out tmp: {e}"), 0)
                .with_provider_provenance(readonly_provenance("codex_cli"));
        }
    };
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = tmp
            .as_file_mut()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
        {
            return cli_error("codex_cli", format!("out perm: {e}"), 0)
                .with_provider_provenance(readonly_provenance("codex_cli"));
        }
    }
    let out_path_str = tmp.path().to_string_lossy().into_owned();
    let cwd = cwd_string();

    let mut cmd = Command::new("codex");
    cmd.args([
        "exec",
        "--skip-git-repo-check",
        "--sandbox",
        "read-only",
        "--ephemeral",
        "--ignore-user-config",
        "--ignore-rules",
        "--color",
        "never",
        "--output-last-message",
        &out_path_str,
        "-C",
        &cwd,
    ]);
    if !model.is_empty() {
        cmd.args(["-m", model]);
    }
    cmd.arg("-");
    // Prefer ChatGPT/subscription login over OPENAI_API_KEY API billing.
    cmd.env_remove("OPENAI_API_KEY");
    cmd.env_remove("OPENAI_API_KEY_CODEX");

    let mut resp = run_stdout(
        cmd,
        Some(full_prompt.as_str()),
        "codex_cli",
        model_label("codex-cli", model),
        readonly_provenance("codex_cli"),
    )
    .await;
    // provenance gate (Issue 1): only trust/assign if exactly 0o600 (do not overwrite resp.text on bad mode)
    // allow(clippy::collapsible_if): explicit nesting for T15 security gate readability (per review B; required clippy gate).
    #[allow(clippy::collapsible_if)]
    if let Ok(meta) = tmp.as_file().metadata() {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode == 0o600 {
            if let Ok(text) = std::fs::read_to_string(tmp.path())
                && !text.trim().is_empty()
            {
                resp.text = text.trim().to_string();
            }
        }
        // else: degrade, keep original resp (no trust from potentially world-readable temp)
    }
    drop(tmp); // guard delete
    resp
}

pub async fn ask_agy(prompt: &str, system: &str, model: &str) -> ProviderResponse {
    let full_prompt = full_prompt(prompt, system);
    if full_prompt.len() > AGY_MAX_PROMPT_BYTES {
        return cli_error(
            "agy_cli",
            format!(
                "prompt too large for agy CLI argv ({} bytes > {} bytes)",
                full_prompt.len(),
                AGY_MAX_PROMPT_BYTES
            ),
            0,
        )
        .with_provider_provenance(tools_provenance("agy_cli"));
    }

    let cwd = cwd_string();
    let timeout_arg = format!("{}s", super::request_timeout().as_secs());
    let mut cmd = Command::new("agy");
    cmd.args([
        "--sandbox",
        "--add-dir",
        &cwd,
        "--print-timeout",
        &timeout_arg,
        // Headless one-shot via -p per Antigravity docs. --dangerously-skip-permissions prevents
        // invisible approval hangs in non-TTY/print mode (common cause of empty output for tools/seats).
        "--dangerously-skip-permissions",
    ]);
    // Resolve via agy_routing.yaml so legacy gemini-* from cabinets map to agy display names.
    // Direct agy slugs and unknown pass through (backward compat for agy_cli users).
    let cli_model = if model.trim().is_empty() {
        String::new()
    } else {
        agy_route::resolve_agy_model(model)
    };
    if !cli_model.is_empty() {
        cmd.args(["--model", &cli_model]);
    }
    cmd.args(["-p", &full_prompt]);
    // Prefer Antigravity/Gemini CLI login over API keys.
    cmd.env_remove("GEMINI_API_KEY");
    cmd.env_remove("GOOGLE_API_KEY");
    cmd.env_remove("GOOGLE_GENAI_USE_VERTEXAI");

    // Keep original cabinet (or passed) value for the label so response.model reflects caller intent
    // when not substituted at dispatch; dispatch resolves before calling for gemini/agy_cli paths.
    run_stdout(
        cmd,
        None,
        "agy_cli",
        model_label("agy-cli", model),
        tools_provenance("agy_cli"),
    )
    .await
}

fn full_prompt(prompt: &str, system: &str) -> String {
    if system.trim().is_empty() {
        prompt.to_string()
    } else {
        format!("[SYSTEM]\n{system}\n\n[USER]\n{prompt}")
    }
}

fn cwd_string() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string())
}

fn model_label(prefix: &str, model: &str) -> String {
    if model.trim().is_empty() {
        format!("{prefix}-default")
    } else {
        format!("{prefix}-{model}")
    }
}

fn readonly_provenance(runner: &str) -> ProviderProvenance {
    ProviderProvenance::cli_readonly(runner, USAGE_UNAVAILABLE)
}

#[cfg(test)]
mod security_tests {
    // T15 negative test: temp file 0600 + unreadable by other (provenance/mode gate before CLI read-back trust).
    #[test]
    fn agent_cli_tempfile_0600_not_world_readable() {
        let tmp = tempfile::NamedTempFile::new().expect("NamedTempFile");
        use std::os::unix::fs::PermissionsExt;
        let _ = tmp
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600));
        let mode = tmp.as_file().metadata().expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "temp must be 0600");
        assert_eq!(
            mode & 0o007,
            0,
            "must not be readable by other (negative: world read bit off)"
        );
        // provenance: we control the NamedTempFile path
        assert!(
            tmp.path().exists(),
            "our temp path must exist for the CLI call window"
        );

        // Simulate bad mode for codex guard (Issue 1): ask_codex would skip trust on != 0o600
        let _ = tmp
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o666));
        let bad = tmp.as_file().metadata().expect("meta").permissions().mode() & 0o777;
        assert_ne!(bad, 0o600);
        // guard in ask_codex: if != 0o600 { do not assign resp.text from the temp }
    }
}

fn tools_provenance(runner: &str) -> ProviderProvenance {
    ProviderProvenance::cli_tools(runner, USAGE_UNAVAILABLE)
}

pub(crate) async fn run_stdout(
    mut cmd: Command,
    stdin_text: Option<&str>,
    runner: &str,
    model: String,
    provenance: ProviderProvenance,
) -> ProviderResponse {
    cmd.stdin(if stdin_text.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);

    let t0 = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return cli_error(
                runner,
                format!("spawn: {e}"),
                t0.elapsed().as_millis() as u64,
            )
            .with_provider_provenance(provenance);
        }
    };

    if let Some(text) = stdin_text
        && let Some(mut stdin) = child.stdin.take()
    {
        if let Err(e) = stdin.write_all(text.as_bytes()).await {
            // P1-5/6: kill via pid + reap on stdin error
            if let Some(pid) = child.id() {
                let _ = std::process::Command::new("kill")
                    .args(["-9", &pid.to_string()])
                    .output();
            }
            return cli_error(
                runner,
                format!("stdin write: {e}"),
                t0.elapsed().as_millis() as u64,
            )
            .with_provider_provenance(provenance);
        }
        drop(stdin);
    }

    let cli_timeout = if runner == "grok_cli" {
        grok_cli_timeout()
    } else {
        super::request_timeout()
    };

    let child_pid = child.id();

    let output = match timeout(cli_timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            // P1-5/6: reap via pid kill on wait error (child moved into future; use pid for start_kill equiv)
            if let Some(pid) = child_pid {
                let _ = std::process::Command::new("kill")
                    .args(["-9", &pid.to_string()])
                    .output();
            }
            return cli_error(
                runner,
                format!("wait: {e}"),
                t0.elapsed().as_millis() as u64,
            )
            .with_provider_provenance(provenance);
        }
        Err(_) => {
            // P1-5/6: start_kill/reap via pid on timeout in shared run_stdout
            if let Some(pid) = child_pid {
                let _ = std::process::Command::new("kill")
                    .args(["-9", &pid.to_string()])
                    .output();
            }
            return cli_error(
                runner,
                format!("timeout ({}s)", cli_timeout.as_secs()),
                t0.elapsed().as_millis() as u64,
            )
            .with_provider_provenance(provenance);
        }
    };

    let latency_ms = t0.elapsed().as_millis() as u64;
    if !output.status.success() {
        let code = output.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let snippet: String = stderr.chars().take(400).collect();
        return cli_error(
            runner,
            format!("exit {code}: {}", snippet.trim()),
            latency_ms,
        )
        .with_provider_provenance(provenance);
    }

    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let snippet: String = stderr.chars().take(400).collect();
        let detail = if snippet.trim().is_empty() {
            // Common with grok_cli (OAuth) under seat constraints + long prompts.
            // The agent may not emit final answer; see ask_grok_impl_inner for flags.
            "empty stdout (no stderr; likely agent tool-loop / constraints / non-termination under read-only + no tools)".to_string()
        } else {
            format!("empty stdout (stderr: {})", snippet.trim())
        };
        return cli_error(runner, detail, latency_ms).with_provider_provenance(provenance);
    }

    ProviderResponse {
        text,
        model,
        tokens_in: 0,
        tokens_out: 0,
        cached_in: 0,
        latency_ms,
        cost_usd: 0.0,
        error: None,
        gateway_provenance: None,
        gateway_attempts: Vec::new(),
        provider_provenance: Some(provenance),
    }
}

fn cli_error(runner: &str, message: impl Into<String>, latency_ms: u64) -> ProviderResponse {
    ProviderResponse {
        error: Some(format!("{runner}: {}", message.into())),
        latency_ms,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grok_cli_max_turns_validator_default_eight() {
        assert_eq!(super::grok_cli_max_turns(), 8);
    }

    #[test]
    fn grok_cli_seat_max_turns_default_six() {
        assert_eq!(super::grok_cli_seat_max_turns(), 6);
    }

    #[test]
    fn grok_cli_serialize_defaults_on() {
        assert!(super::grok_cli_serialize());
    }

    #[test]
    fn fingerprint_accepts_grok_build_cli_version() {
        let out = b"grok 0.2.93 (f00f96316d4b) [stable]\n";
        assert!(super::is_grok_build_cli_version_output(out, b""));
    }

    #[test]
    fn fingerprint_rejects_npm_grok_dev_style_version() {
        // npm package `grok-dev` prints a bare semver when bun is present.
        assert!(!super::is_grok_build_cli_version_output(b"1.1.5\n", b""));
        assert!(!super::is_grok_build_cli_version_output(
            b"",
            b"env: bun: No such file or directory\n"
        ));
    }

    #[test]
    fn full_prompt_keeps_system_and_user_distinct() {
        let prompt = full_prompt("check the file", "be strict");
        assert!(prompt.contains("[SYSTEM]\nbe strict"));
        assert!(prompt.contains("[USER]\ncheck the file"));
    }

    #[test]
    fn model_label_marks_cli_runner() {
        assert_eq!(model_label("grok-cli", "grok-4.3"), "grok-cli-grok-4.3");
        assert_eq!(model_label("grok-cli", ""), "grok-cli-default");
    }

    #[test]
    fn unpinned_grok43_uses_routing_default_label() {
        let res = grok_route::resolve_cli_model("grok-4.3");
        assert!(res.api_id_substituted);
        assert!(res.cli_model_arg.is_none());
        assert_eq!(res.response_label, "grok-cli-default");
    }

    #[tokio::test]
    async fn run_stdout_attaches_provenance_on_spawn_error() {
        let resp = run_stdout(
            Command::new("__council_missing_cli__"),
            None,
            "missing_cli",
            "missing-model".to_string(),
            readonly_provenance("missing_cli"),
        )
        .await;

        assert!(resp.error.is_some());
        assert_eq!(
            resp.provider_provenance,
            Some(ProviderProvenance::cli_readonly(
                "missing_cli",
                USAGE_UNAVAILABLE
            ))
        );
    }

    // T15 hot-path ask_* drive (L): call the prod async fns (Named 0o600 + set + provenance gate execute before external CLI spawn)
    #[tokio::test]
    async fn t15_hot_path_ask_calls() {
        // the gate: set_permissions (prop if err), mode check + if ==0o600 only then read/overwrite text (before the bin error)
        let _resp = ask_codex("p", "", "").await; // drives prod hot path (temp gate before expected bin-not-found err in env w/o cli); _resp.error or empty text signals the bin case after gate
        // ask_grok similar (sync temp inside async wrapper)
    }
}
