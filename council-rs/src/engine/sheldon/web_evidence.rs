// Web evidence pipeline + SSRF-safe URL scrape target classification.

use super::{
    EvidenceCache, evidence_cache_key, search_safe_query, sheldon_evidence_cache_enabled, truncate,
};
use crate::evidence;
use reqwest::Url;
use std::net::{Ipv4Addr, Ipv6Addr};

pub(super) async fn gather_web_evidence(
    topic: &str,
    available: bool,
    verbose: bool,
    cache: Option<&EvidenceCache>,
) -> Vec<String> {
    if !available {
        return vec![];
    }

    let mut parts = Vec::new();
    let evidence_run = evidence::EvidenceRun::new();

    // Extract URLs from topic for Firecrawl verification. Blocked targets are
    // logged before dispatch, so SSRF regression checks do not depend on model
    // prose or third-party scraper behavior.
    let (urls, blocked_urls) = extract_scrape_targets(topic);
    if verbose && !blocked_urls.is_empty() {
        let examples = blocked_urls
            .iter()
            .take(3)
            .map(|blocked| format!("{} ({})", truncate(&blocked.raw, 80), blocked.reason))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "      🛡️  URL sanitizer: blocked {} scrape target(s) before dispatch: {}",
            blocked_urls.len(),
            examples
        );
    }

    // Truncate topic to a search-safe query length (Tavily max ~400 chars,
    // but shorter queries produce better results across all engines).
    let search_query = search_safe_query(topic);

    // Session cache check (P0 evidence dedup). Key per source + stable query.
    // On hit we skip the (paid) network call and reuse the previously formatted block.
    let use_cache = cache.is_some() && sheldon_evidence_cache_enabled();

    // Provider failures are local to each native source; one auth or outage
    // problem should not suppress the rest of the evidence gather.
    let exa_key = evidence_cache_key("exa", topic);
    let exa_results = if use_cache {
        if let Some(c) = cache {
            if c.get(&exa_key).is_some() {
                // We will push the cached formatted block after fetch phase.
                // For now signal empty so join shape preserved; post-process below.
                vec![]
            } else {
                evidence::exa_search_with_run(search_query, 5, Some(&evidence_run)).await
            }
        } else {
            evidence::exa_search_with_run(search_query, 5, Some(&evidence_run)).await
        }
    } else {
        evidence::exa_search_with_run(search_query, 5, Some(&evidence_run)).await
    };

    let tavily_key = evidence_cache_key("tavily", topic);
    let tavily_results = if use_cache {
        if let Some(c) = cache {
            if c.get(&tavily_key).is_some() {
                vec![]
            } else {
                evidence::tavily_search_with_run(search_query, 5, Some(&evidence_run)).await
            }
        } else {
            evidence::tavily_search_with_run(search_query, 5, Some(&evidence_run)).await
        }
    } else {
        evidence::tavily_search_with_run(search_query, 5, Some(&evidence_run)).await
    };

    let news_key = evidence_cache_key("news", topic);
    let news_results = if use_cache {
        if let Some(c) = cache {
            if c.get(&news_key).is_some() {
                vec![]
            } else {
                evidence::news_search_with_run(search_query, Some(&evidence_run)).await
            }
        } else {
            evidence::news_search_with_run(search_query, Some(&evidence_run)).await
        }
    } else {
        evidence::news_search_with_run(search_query, Some(&evidence_run)).await
    };

    let scholar_key = evidence_cache_key("scholar", topic);
    let scholar_results = if use_cache {
        if let Some(c) = cache {
            if c.get(&scholar_key).is_some() {
                vec![]
            } else {
                evidence::scholar_search_with_run(search_query, Some(&evidence_run)).await
            }
        } else {
            evidence::scholar_search_with_run(search_query, Some(&evidence_run)).await
        }
    } else {
        evidence::scholar_search_with_run(search_query, Some(&evidence_run)).await
    };

    let scrape_fut = scrape_topic_urls(&urls, verbose, &evidence_run);
    let scraped = scrape_fut.await;

    // Note: the join shape for the four searches was replaced by individual awaits on miss
    // to allow per-source cache decisions. Scrapes remain after url extract.

    // Source: Exa semantic web search
    let exa_key = evidence_cache_key("exa", topic);
    if let Some(c) = cache
        && use_cache
        && let Some(hit) = c.get(&exa_key)
        && !hit.is_empty()
    {
        parts.push(hit);
        if verbose {
            eprintln!("      🌐 Exa: cache hit");
        }
    }
    if !exa_results.is_empty() {
        let mut block = "\n## Web Intelligence (Exa semantic search)".to_string();
        for item in exa_results.iter().take(5) {
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let score = item.get("score").and_then(|v| v.as_f64());
            let text_short = truncate(text, 300);
            let mut entry = if !title.is_empty() {
                format!("- [{}]({}) — {}", title, url, text_short)
            } else {
                format!("- {} — {}", url, text_short)
            };
            if let Some(s) = score {
                entry.push_str(&format!(" [relevance={:.2}]", s));
            }
            block.push('\n');
            block.push_str(&entry);
        }
        if let Some(c) = cache
            && use_cache
            && sheldon_evidence_cache_enabled()
        {
            c.insert(exa_key, block.clone());
        }
        parts.push(block);
        if verbose {
            eprintln!("      🌐 Exa: {} web results", exa_results.len().min(5));
        }
    }

    // Source: Tavily recency-tuned web search (last 7 days)
    let tavily_key = evidence_cache_key("tavily", topic);
    if let Some(c) = cache
        && use_cache
        && let Some(hit) = c.get(&tavily_key)
        && !hit.is_empty()
    {
        parts.push(hit);
        if verbose {
            eprintln!("      🔎 Tavily: cache hit");
        }
    }
    if !tavily_results.is_empty() {
        let mut block = "\n## Recency-Biased Web (Tavily)".to_string();
        for item in tavily_results.iter().take(5) {
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let content = item
                .get("content")
                .or_else(|| item.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let score = item.get("score").and_then(|v| v.as_f64());
            let content_short = truncate(content, 300);
            let mut entry = if !title.is_empty() {
                format!("- [{}]({}) — {}", title, url, content_short)
            } else {
                format!("- {} — {}", url, content_short)
            };
            if let Some(s) = score {
                entry.push_str(&format!(" [relevance={:.2}]", s));
            }
            block.push('\n');
            block.push_str(&entry);
        }
        if let Some(c) = cache
            && use_cache
            && sheldon_evidence_cache_enabled()
        {
            c.insert(tavily_key, block.clone());
        }
        parts.push(block);
        if verbose {
            eprintln!(
                "      🔎 Tavily: {} recent web results",
                tavily_results.len().min(5)
            );
        }
    }

    // Source: Real-time news (timestamped, attributed)
    let news_key = evidence_cache_key("news", topic);
    if let Some(c) = cache
        && use_cache
        && let Some(hit) = c.get(&news_key)
        && !hit.is_empty()
    {
        parts.push(hit);
        if verbose {
            eprintln!("      📰 News: cache hit");
        }
    }
    if !news_results.is_empty() {
        let mut block = "\n## Breaking News (Tavily News, last 7 days)".to_string();
        for item in news_results.iter().take(5) {
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let snippet = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let source = item.get("source").and_then(|v| v.as_str()).unwrap_or("");
            let date = item.get("date").and_then(|v| v.as_str()).unwrap_or("");
            let published = item
                .get("published_at")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let snippet_short = truncate(snippet, 300);
            let timestamp = if !published.is_empty() {
                published
            } else {
                date
            };
            let mut entry = format!("- [{}]({}) — {}", title, url, snippet_short);
            if !source.is_empty() {
                entry.push_str(&format!(" [{}]", source));
            }
            if !timestamp.is_empty() {
                entry.push_str(&format!(" ({})", timestamp));
            }
            block.push('\n');
            block.push_str(&entry);
        }
        if let Some(c) = cache
            && use_cache
            && sheldon_evidence_cache_enabled()
        {
            c.insert(news_key, block.clone());
        }
        parts.push(block);
        if verbose {
            eprintln!("      📰 News: {} articles", news_results.len().min(5));
        }
    }

    // Source: Academic papers (citation-weighted)
    let scholar_key = evidence_cache_key("scholar", topic);
    if let Some(c) = cache
        && use_cache
        && let Some(hit) = c.get(&scholar_key)
        && !hit.is_empty()
    {
        parts.push(hit);
        if verbose {
            eprintln!("      🎓 Scholar: cache hit");
        }
    }
    if !scholar_results.is_empty() {
        let mut block = "\n## Academic Papers (Semantic Scholar)".to_string();
        for item in scholar_results.iter().take(5) {
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let snippet = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let source = item.get("source").and_then(|v| v.as_str()).unwrap_or("");
            let citations = item
                .get("citation_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let year = item.get("year").and_then(|v| v.as_str()).unwrap_or("");
            let snippet_short = truncate(snippet, 300);
            let mut entry = if !url.is_empty() {
                format!("- [{}]({}) — {}", title, url, snippet_short)
            } else {
                format!("- {} — {}", title, snippet_short)
            };
            if !source.is_empty() {
                entry.push_str(&format!(" [{}]", source));
            }
            if !year.is_empty() {
                entry.push_str(&format!(" ({})", year));
            }
            if citations > 0 {
                entry.push_str(&format!(" [{} citations]", citations));
            }
            block.push('\n');
            block.push_str(&entry);
        }
        if let Some(c) = cache
            && use_cache
            && sheldon_evidence_cache_enabled()
        {
            c.insert(scholar_key, block.clone());
        }
        parts.push(block);
        if verbose {
            eprintln!("      🎓 Scholar: {} papers", scholar_results.len().min(5));
        }
    }

    // Source: Firecrawl URL scraping (for URLs cited in the topic)
    if !scraped.is_empty() {
        parts.push("\n## Cited URL Content (Firecrawl)".into());
        parts.extend(scraped);
    }

    parts
}

#[cfg(test)]
fn extract_urls(text: &str) -> Vec<String> {
    extract_scrape_targets(text).0
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BlockedScrapeUrl {
    raw: String,
    reason: &'static str,
}

fn extract_scrape_targets(text: &str) -> (Vec<String>, Vec<BlockedScrapeUrl>) {
    let mut allowed = Vec::new();
    let mut blocked = Vec::new();

    text.split_whitespace()
        .filter_map(classify_scrape_candidate)
        .for_each(|decision| match decision {
            Ok(url) => {
                if allowed.len() < 3 {
                    allowed.push(url);
                }
            }
            Err(blocked_url) => blocked.push(blocked_url),
        });

    (allowed, blocked)
}

fn classify_scrape_candidate(raw: &str) -> Option<Result<String, BlockedScrapeUrl>> {
    // Strip trailing punctuation that gets swept in from prose before parsing.
    let clean = raw.trim_end_matches([')', '.', ',', ']', ';', '>', '"', '\'']);
    if !(clean.starts_with("http://") || clean.starts_with("https://")) {
        return None;
    }

    let blocked = |reason| {
        Some(Err(BlockedScrapeUrl {
            raw: clean.to_string(),
            reason,
        }))
    };

    let parsed = match Url::parse(clean) {
        Ok(parsed) => parsed,
        Err(_) => return blocked("invalid_url"),
    };
    if parsed.scheme() != "https" {
        return blocked("non_https");
    }
    let host = match parsed.host_str() {
        Some(host) => host,
        None => return blocked("missing_host"),
    };
    if let Some(reason) = private_host_block_reason(host) {
        return blocked(reason);
    }
    Some(Ok(parsed.to_string()))
}

fn private_host_block_reason(host: &str) -> Option<&'static str> {
    let host = host
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase();

    if host == "localhost" || host == "metadata.google.internal" {
        return Some("local_name");
    }

    if host.ends_with(".local") || host.ends_with(".internal") {
        return Some("internal_tld");
    }

    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return is_private_ipv4(ip).then_some("private_ipv4");
    }

    if is_wildcard_local_dns_host(&host) || embeds_private_ipv4_labels(&host) {
        return Some("wildcard_or_embedded_private_ip");
    }

    if let Ok(ip) = host.parse::<Ipv6Addr>() {
        if ip.is_loopback() || ip.is_unspecified() {
            return Some("private_ipv6");
        }
        if let Some(v4) = ip.to_ipv4_mapped() {
            return is_private_ipv4(v4).then_some("private_ipv4_mapped_ipv6");
        }
        let first = ip.segments()[0];
        if (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80 {
            return Some("private_ipv6");
        }
    }

    None
}

fn is_wildcard_local_dns_host(host: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        "nip.io",
        "sslip.io",
        "xip.io",
        "localtest.me",
        "lvh.me",
        "vcap.me",
    ];

    SUFFIXES
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{}", suffix)))
}

fn embeds_private_ipv4_labels(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    labels.windows(4).any(|window| {
        let octets = window
            .iter()
            .map(|label| label.parse::<u8>())
            .collect::<Result<Vec<_>, _>>();
        if let Ok(octets) = octets
            && let [a, b, c, d] = octets.as_slice()
        {
            return is_private_ipv4(Ipv4Addr::new(*a, *b, *c, *d));
        }
        false
    })
}

fn is_private_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 0
        || octets[0] == 10
        || octets[0] == 127
        || (octets[0] == 172 && (16..=31).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 168)
        || (octets[0] == 169 && octets[1] == 254)
}

async fn scrape_topic_urls(
    urls: &[String],
    verbose: bool,
    evidence_run: &evidence::EvidenceRun,
) -> Vec<String> {
    if urls.is_empty() {
        return vec![];
    }

    let mut parts = Vec::new();
    let futs: Vec<_> = urls
        .iter()
        .map(|url| evidence::scrape_url_with_run(url.as_str(), Some(evidence_run)))
        .collect();
    let results = futures_util::future::join_all(futs).await;

    for (url, content) in urls.iter().zip(results) {
        if let Some(md) = content {
            parts.push(format!("### {}", url));
            parts.push(truncate(&md, 1500).to_string());
            if verbose {
                eprintln!(
                    "      🔥 Firecrawl: scraped {} ({} chars)",
                    url,
                    md.len().min(1500)
                );
            }
        }
    }

    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]

    fn extract_urls_strips_trailing_punctuation_before_scrape() {
        let urls = extract_urls("Check https://example.com/path?q=1), then https://ok.example/a.");

        assert_eq!(
            urls,
            vec![
                "https://example.com/path?q=1".to_string(),
                "https://ok.example/a".to_string()
            ]
        );
    }

    #[test]

    fn extract_urls_blocks_private_hosts() {
        let urls = extract_urls(
            "Skip https://127.0.0.1/health and https://metadata.google.internal/latest.",
        );

        assert!(urls.is_empty());
    }

    #[test]

    fn extract_urls_blocks_http_localhost_and_internal_hosts() {
        let (allowed, blocked) = extract_scrape_targets(
            "Skip http://example.com/plain and https://localhost/admin and https://api.service.internal/path.",
        );

        assert!(allowed.is_empty());

        assert_eq!(blocked.len(), 3);

        assert!(blocked.iter().any(|b| b.reason == "non_https"));

        assert!(blocked.iter().any(|b| b.reason == "local_name"));

        assert!(blocked.iter().any(|b| b.reason == "internal_tld"));
    }

    #[test]

    fn extract_urls_blocks_userinfo_authority_confusion() {
        let urls = extract_urls(
            "Skip https://example.com@169.254.169.254/latest and https://user:pass@metadata.google.internal/latest.",
        );

        assert!(urls.is_empty());
    }

    #[test]

    fn extract_urls_blocks_private_ipv6_ranges() {
        let urls =
            extract_urls("Skip https://[fe80::1]/ and https://[fc00::1]/ and https://[::1]/.");

        assert!(urls.is_empty());
    }

    #[test]

    fn extract_urls_blocks_ipv4_mapped_ipv6_loopback() {
        let urls = extract_urls("Skip https://[::ffff:127.0.0.1]/.");

        assert!(urls.is_empty());
    }

    #[test]

    fn extract_urls_blocks_encoded_private_ip_hosts() {
        let urls = extract_urls(
            "Skip https://2130706433/ and https://0x7f000001/ and https://017700000001/.",
        );

        assert!(urls.is_empty());
    }

    #[test]

    fn extract_urls_blocks_wildcard_dns_private_targets() {
        let urls = extract_urls(
            "Skip https://169.254.169.254.nip.io/latest and https://127.0.0.1.sslip.io/.",
        );

        assert!(urls.is_empty());
    }

    #[test]

    fn extract_scrape_targets_reports_local_block_reasons() {
        let (allowed, blocked) = extract_scrape_targets(
            "Scrape https://example.com and skip https://2130706433/ plus https://127.0.0.1.sslip.io/.",
        );

        assert_eq!(allowed, vec!["https://example.com/".to_string()]);

        assert_eq!(blocked.len(), 2);

        assert!(blocked.iter().any(|b| b.reason == "private_ipv4"));

        assert!(
            blocked
                .iter()
                .any(|b| b.reason == "wildcard_or_embedded_private_ip")
        );
    }

    #[test]

    fn extract_urls_allows_public_https_hosts() {
        let urls = extract_urls("Keep https://example.com/ok and https://docs.rs/url/latest/url/.");

        assert_eq!(
            urls,
            vec![
                "https://example.com/ok".to_string(),
                "https://docs.rs/url/latest/url/".to_string()
            ]
        );
    }

    #[test]

    fn extract_scrape_targets_caps_allowed_urls_at_three() {
        let (allowed, blocked) = extract_scrape_targets(
            "Keep https://one.example/a https://two.example/b https://three.example/c https://four.example/d.",
        );

        assert!(blocked.is_empty());

        assert_eq!(
            allowed,
            vec![
                "https://one.example/a".to_string(),
                "https://two.example/b".to_string(),
                "https://three.example/c".to_string()
            ]
        );
    }
}
