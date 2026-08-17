// Evidence orchestration: native web gather and operator-provided repo context.

use super::web_evidence::gather_web_evidence;
use super::{EvidenceCache, REPO_CONTEXT_MAX_BYTES, truncate};
use crate::evidence;
use crate::types::SeatResponse;

pub(super) async fn gather_evidence(
    topic: &str,
    _valid: &[&SeatResponse],
    context: &str,
    verbose: bool,
    cache: Option<&EvidenceCache>,
) -> String {
    gather_evidence_from(topic, context, verbose, cache, evidence::is_available()).await
}

async fn gather_evidence_from(
    topic: &str,
    context: &str,
    verbose: bool,
    cache: Option<&EvidenceCache>,
    has_web: bool,
) -> String {
    let repo_context = repo_context_evidence(context);

    if !has_web && repo_context.is_none() {
        if verbose {
            eprintln!("🔍 Validator: no evidence sources available — claim extraction only");
        }
        return String::new();
    }

    if verbose {
        let mut sources = Vec::new();
        if has_web {
            sources.push("native web");
        }
        if repo_context.is_some() {
            sources.push("repo context");
        }
        eprintln!("🔍 Validator: {} detected", sources.join(" + "));
    }

    let web_parts = gather_web_evidence(topic, has_web, verbose, cache).await;

    let mut parts = Vec::new();
    if let Some(repo_context) = repo_context {
        parts.push(repo_context);
    }
    parts.extend(web_parts);

    if parts.is_empty() {
        return String::new();
    }

    if verbose {
        eprintln!("      📡 {} evidence sections gathered", parts.len());
    }

    format!(
        "\n\n<evidence source=\"multi-source intelligence pipeline\">\n\
         Use this evidence to VERIFY or CONTRADICT claims. You MUST \
         cite specific items when they are relevant to a claim.\n\n\
         {}\n</evidence>\n",
        parts.join("\n")
    )
}

fn repo_context_evidence(context: &str) -> Option<String> {
    let trimmed = context.trim();
    if trimmed.is_empty() {
        return None;
    }

    let clipped = truncate(trimmed, REPO_CONTEXT_MAX_BYTES);
    let truncated = clipped.len() < trimmed.len();
    let suffix = if truncated {
        "\n\n[repo context truncated]"
    } else {
        ""
    };

    Some(format!(
        "## Local Repo Context (operator-provided)\n\
         Source: the same --context/--map text supplied to deliberation seats. \
         Use this as the ONLY evidence for local source files, symbols, tests, \
         build scripts, and repository runtime behavior.\n\n\
         <repo_context>\n{}{}\n</repo_context>",
        clipped, suffix
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]

    fn repo_context_evidence_labels_local_code_source() {
        let evidence =
            repo_context_evidence("src/types.rs\nfn from_provider() { /* trims empty text */ }")
                .expect("repo context evidence");

        assert!(evidence.contains("## Local Repo Context"));

        assert!(evidence.contains("<repo_context>"));

        assert!(evidence.contains("src/types.rs"));

        assert!(evidence.contains("ONLY evidence for local source files"));
    }

    #[test]

    fn repo_context_evidence_caps_context_without_splitting_characters() {
        let context = format!("{}é", "a".repeat(REPO_CONTEXT_MAX_BYTES));

        let evidence = repo_context_evidence(&context).expect("repo context evidence");

        assert!(evidence.contains("[repo context truncated]"));

        assert!(evidence.is_char_boundary(evidence.len()));
    }

    #[tokio::test]
    async fn gather_evidence_with_web_off_emits_repo_context_without_live_x() {
        let out = gather_evidence_from(
            "What does Sheldon validate?",
            "src/engine/sheldon/mod.rs\nclaim validator",
            false,
            None,
            false,
        )
        .await;

        assert!(out.contains("## Local Repo Context"));
        assert!(out.contains("src/engine/sheldon/mod.rs"));
        let lowered = out.to_ascii_lowercase();
        assert!(!lowered.contains("live x"));
        assert!(!lowered.contains("xmcp"));
        assert!(!out.contains("## Web Intelligence"));
        assert!(!out.contains("## Recency-Biased Web"));
        assert!(!out.contains("## Breaking News"));
        assert!(!out.contains("## Academic Papers"));
        assert!(!out.contains("## Cited URL Content"));
    }
}
