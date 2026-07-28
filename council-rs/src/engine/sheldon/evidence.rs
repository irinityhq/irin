// Evidence orchestration: multi-source gather, repo context, live X via xmcp.

use super::web_evidence::{build_query_pairs, extract_keywords, gather_web_evidence};
use super::{EvidenceCache, REPO_CONTEXT_MAX_BYTES, truncate};
use crate::evidence;
use crate::types::SeatResponse;
use crate::xmcp;

pub(super) async fn gather_evidence(
    topic: &str,
    valid: &[&SeatResponse],
    context: &str,
    verbose: bool,
    cache: Option<&EvidenceCache>,
) -> String {
    let has_xmcp = xmcp::is_available().await;
    let has_web = evidence::is_available();
    let repo_context = repo_context_evidence(context);

    if !has_xmcp && !has_web && repo_context.is_none() {
        if verbose {
            eprintln!("🔍 Validator: no evidence sources available — claim extraction only");
        }
        return String::new();
    }

    if verbose {
        let mut sources = Vec::new();
        if has_xmcp {
            sources.push("xmcp");
        }
        if has_web {
            sources.push("native web");
        }
        if repo_context.is_some() {
            sources.push("repo context");
        }
        eprintln!("🔍 Validator: {} detected", sources.join(" + "));
    }

    let combined_text: String = valid
        .iter()
        .take(3)
        .map(|r| truncate(&r.text, 500))
        .collect::<Vec<_>>()
        .join(" ");
    let keywords = extract_keywords(&combined_text, topic);
    let queries = build_query_pairs(&keywords);

    // Run all evidence sources in parallel.
    // xmcp is used *only* for live/recent X posts (raw intel via searchPostsRecent).
    // Personal bookmark/intel corpus is never consulted from Sheldon.
    let xmcp_fut = gather_xmcp_evidence(topic, &queries, has_xmcp, cache);
    let web_fut = gather_web_evidence(topic, has_web, verbose, cache);
    let (xmcp_parts, web_parts) = tokio::join!(xmcp_fut, web_fut);

    let mut parts = Vec::new();
    if let Some(repo_context) = repo_context {
        parts.push(repo_context);
    }
    parts.extend(xmcp_parts);
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

async fn gather_xmcp_evidence(
    _topic: &str,
    queries: &[String],
    available: bool,
    _cache: Option<&EvidenceCache>,
) -> Vec<String> {
    if !available {
        return vec![];
    }

    let mut parts = Vec::new();

    // Sheldon uses xmcp *strictly* as a bridge to live/recent X posts (raw intel).
    // The personal bookmark / intel corpus is **never** queried from here.
    // Bookmarks can be sparse/stale and would bias validation toward the
    // operator's existing collection rather than fresh public signals.
    //
    // We use xmcp::search_posts (searchPostsRecent) for live X only.
    let mut seen_ids = std::collections::HashSet::new();
    let mut post_results = Vec::new();
    for q in queries.iter().take(3) {
        let hits = xmcp::search_posts(q, 3).await;
        for p in hits {
            let pid = p
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !pid.is_empty() && !seen_ids.insert(pid) {
                continue;
            }
            post_results.push(p);
        }
    }

    if !post_results.is_empty() {
        parts.push("## Live X Posts (via xmcp)".into());
        for post in post_results.iter().take(8) {
            let text = post.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let text_short = truncate(text, 300);
            let likes = post
                .get("public_metrics")
                .and_then(|m| m.get("like_count"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let mut entry = format!("- {}", text_short);
            if likes > 0 {
                entry.push_str(&format!(" [{} likes]", likes));
            }
            // If the live search payload ever carries enrichment, we can surface
            // it here the same way (author, why, etc.). For now this is raw live.
            parts.push(entry);
        }
    }

    parts
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
}
