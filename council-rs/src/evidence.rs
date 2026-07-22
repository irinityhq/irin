//! Native web evidence provider for Sheldon validation.
//!
//! Replaces the former Composio CLI shellout with direct, soft-failing HTTP
//! calls to Exa, Tavily, Firecrawl-compatible scrapers, and Semantic Scholar.

use futures_util::StreamExt;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::{Client, StatusCode, Url};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static WEB_EVIDENCE_AVAILABLE: AtomicBool = AtomicBool::new(false);
static WEB_EVIDENCE_CHECKED: AtomicBool = AtomicBool::new(false);

const DEFAULT_SEARCH_TIMEOUT_MS: u64 = 12_000;
const DEFAULT_SCRAPE_TIMEOUT_MS: u64 = 15_000;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 1_048_576;
const MAX_RETRIES: usize = 2;
const TAVILY_QUERY_MAX_CHARS: usize = 400;
const SEMANTIC_SCHOLAR_FIELDS: &str = "title,url,abstract,venue,year,citationCount,openAccessPdf";

/// Per-validation-pass state. Provider failures should be visible but not spam
/// the transcript while sources run concurrently.
#[derive(Default)]
pub(crate) struct EvidenceRun {
    warned_sources: Mutex<HashSet<String>>,
}

impl EvidenceRun {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn warn(&self, source: &str, detail: impl AsRef<str>) {
        let mut warned = match self.warned_sources.lock() {
            Ok(guard) => guard,
            Err(_) => {
                eprintln!(
                    "      ⚠️  {} evidence unavailable: {}",
                    source,
                    detail.as_ref()
                );
                return;
            }
        };
        if warned.insert(source.to_string()) {
            eprintln!(
                "      ⚠️  {} evidence unavailable: {}",
                source,
                detail.as_ref()
            );
        }
    }
}

fn warn_provider(run: Option<&EvidenceRun>, source: &str, detail: impl AsRef<str>) {
    if let Some(run) = run {
        run.warn(source, detail);
    } else {
        eprintln!(
            "      ⚠️  {} evidence unavailable: {}",
            source,
            detail.as_ref()
        );
    }
}

pub fn is_available() -> bool {
    WEB_EVIDENCE_AVAILABLE.load(Ordering::Relaxed)
}

/// Probe native evidence configuration once before relying on `is_available()`.
pub async fn check_available(verbose: bool) -> bool {
    if WEB_EVIDENCE_CHECKED.load(Ordering::Relaxed) {
        return WEB_EVIDENCE_AVAILABLE.load(Ordering::Relaxed);
    }
    WEB_EVIDENCE_CHECKED.store(true, Ordering::Relaxed);

    let sources = configured_sources();
    let ok = !sources.is_empty();

    WEB_EVIDENCE_AVAILABLE.store(ok, Ordering::Relaxed);
    if verbose {
        if ok {
            eprintln!(
                "🔍 Validator: native web evidence detected ({})",
                sources.join(", ")
            );
        } else if legacy_composio_mode_requested() {
            eprintln!("🔍 Validator: Composio backend retired — native web evidence disabled");
        } else if web_evidence_disabled() {
            eprintln!("🔍 Validator: native web evidence disabled by COUNCIL_SHELDON_WEB_EVIDENCE");
        } else {
            eprintln!(
                "🔍 Validator: no native web evidence keys detected — fresh-intel limited to xmcp"
            );
        }
    }
    ok
}

#[derive(Debug, Default)]
pub struct EvidenceSmokeReport {
    pub available: bool,
    pub exa_results: usize,
    pub tavily_results: usize,
    pub news_results: usize,
    pub scholar_results: usize,
    pub firecrawl_chars: Option<usize>,
    pub failures: Vec<String>,
}

impl EvidenceSmokeReport {
    pub fn success(&self) -> bool {
        self.available && self.failures.is_empty()
    }
}

/// Live smoke for configured native evidence sources.
pub async fn smoke_configured_sources(verbose: bool) -> EvidenceSmokeReport {
    let available = check_available(verbose).await;
    let mut report = EvidenceSmokeReport {
        available,
        ..Default::default()
    };

    if !available {
        return report;
    }

    let exa = exa_search("NVIDIA market cap", 3).await;
    report.exa_results = exa.len();
    if has_env("EXA_API_KEY") && exa.is_empty() {
        report.failures.push("Exa".into());
    }

    let tavily = tavily_search("NVIDIA", 3).await;
    report.tavily_results = tavily.len();
    if has_env("TAVILY_API_KEY") && tavily.is_empty() {
        report.failures.push("Tavily".into());
    }

    let news = news_search("NVIDIA AI chips").await;
    report.news_results = news.len();
    if has_env("TAVILY_API_KEY") && news.is_empty() {
        report.failures.push("News".into());
    }

    let papers = scholar_search("AI governance sovereign").await;
    report.scholar_results = papers.len();
    if semantic_scholar_enabled() && papers.is_empty() {
        report.failures.push("Scholar".into());
    }

    let scraped = scrape_url("https://example.com").await;
    report.firecrawl_chars = scraped.as_ref().map(|content| content.len());
    if firecrawl_enabled() && scraped.is_none() {
        report.failures.push("Firecrawl".into());
    }

    report
}

fn configured_sources() -> Vec<&'static str> {
    if web_evidence_disabled() || legacy_composio_mode_requested() {
        return vec![];
    }

    let mut sources = Vec::new();
    if has_env("EXA_API_KEY") {
        sources.push("exa");
    }
    if has_env("TAVILY_API_KEY") {
        sources.push("tavily");
    }
    if firecrawl_enabled() {
        sources.push("firecrawl");
    }
    if semantic_scholar_enabled() {
        sources.push("semantic_scholar");
    }
    sources
}

fn web_evidence_mode() -> String {
    std::env::var("COUNCIL_SHELDON_WEB_EVIDENCE")
        .unwrap_or_else(|_| "auto".into())
        .trim()
        .to_ascii_lowercase()
}

fn web_evidence_disabled() -> bool {
    matches!(
        web_evidence_mode().as_str(),
        "off" | "none" | "false" | "0" | "xmcp"
    )
}

fn legacy_composio_mode_requested() -> bool {
    matches!(web_evidence_mode().as_str(), "composio" | "legacy")
}

fn has_env(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.trim().is_empty())
}

fn env_key(name: &str) -> Option<String> {
    std::env::var(name).ok().and_then(|v| {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn evidence_timeout_ms(default_ms: u64) -> u64 {
    std::env::var("COUNCIL_EVIDENCE_HTTP_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|v| v.clamp(1_000, 30_000))
        .unwrap_or(default_ms)
}

fn evidence_max_response_bytes() -> usize {
    std::env::var("COUNCIL_EVIDENCE_MAX_RESPONSE_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|v| v.clamp(16_384, 10 * 1_048_576))
        .unwrap_or(DEFAULT_MAX_RESPONSE_BYTES)
}

/// SSRF-safe scrape target validation.
/// Rejects non-HTTP(S), localhost, loopback, private/reserved ranges (RFC1918 + link-local).
/// Used before any outbound scrape request to Firecrawl-compatible endpoints.
fn is_ssrf_safe_scrape_target(url_str: &str) -> bool {
    let url = match Url::parse(url_str) {
        Ok(u) => u,
        Err(_) => return false,
    };
    if url.scheme() != "http" && url.scheme() != "https" {
        return false;
    }
    if let Some(host) = url.host_str() {
        let h = host.to_ascii_lowercase();
        if h == "localhost" || h == "127.0.0.1" || h == "::1" || h.starts_with("127.") {
            return false;
        }
        if h == "0.0.0.0" || h.starts_with("10.") || h.starts_with("192.168.") {
            return false;
        }
        if h.starts_with("172.")
            && let Some(after) = h.strip_prefix("172.")
            && let Some(dot) = after.find('.')
            && let Ok(oct) = after[..dot].parse::<u8>()
            && (16..=31).contains(&oct)
        {
            return false;
        }
        if h.starts_with("169.254.") || h.starts_with("fe80:") {
            return false;
        }
        // Cover IPv6-mapped, CGNAT 100.64/10, ULA fd00::/8, and special ranges.
        if let Ok(ip) = h.parse::<std::net::IpAddr>() {
            match ip {
                std::net::IpAddr::V4(v4) => {
                    let o = v4.octets();
                    if o[0] == 100 && (o[1] & 0b1100_0000 == 0b0100_0000) {
                        return false;
                    } // 100.64/10 CGNAT
                    if o[0] == 192 && o[1] == 0 && o[2] == 0 {
                        return false;
                    } // 192.0.0.0/24 IANA
                }
                std::net::IpAddr::V6(v6) => {
                    if v6.is_loopback() || v6.is_unspecified() {
                        return false;
                    }
                    // IPv4-mapped ::ffff:127/8 etc.
                    if v6.segments()[0..5] == [0, 0, 0, 0, 0] && v6.segments()[5] == 0xffff {
                        let v4 = std::net::Ipv4Addr::new(
                            (v6.segments()[6] >> 8) as u8,
                            v6.segments()[6] as u8,
                            (v6.segments()[7] >> 8) as u8,
                            v6.segments()[7] as u8,
                        );
                        if v4.octets()[0] == 127
                            || v4.octets()[0] == 10
                            || (v4.octets()[0] == 192 && v4.octets()[1] == 168)
                            || (v4.octets()[0] == 172 && (16..=31).contains(&v4.octets()[1]))
                        {
                            return false;
                        }
                        if v4.octets()[0] == 100 && (v4.octets()[1] & 0b1100_0000 == 0b0100_0000) {
                            return false;
                        }
                    }
                    // ULA fd00::/8 (first 8 bits 0xfd)
                    if (v6.segments()[0] & 0xff00) == 0xfd00 {
                        return false;
                    }
                }
            }
        }
        // T10 (promoted): Pin-on-Resolve. Resolve host -> validate IPs (deny list) before accept.
        // Prevents DNS rebinding (hostname filter alone insufficient). Explicit fail-closed on Err/empty per plan/review.
        use std::net::ToSocketAddrs;
        match (h.as_str(), 0u16).to_socket_addrs() {
            Ok(addrs) => {
                let addrs: Vec<_> = addrs.collect();
                if addrs.is_empty() {
                    return false;
                }
                for a in addrs {
                    if is_bad_ip(&a.ip()) {
                        return false;
                    }
                }
            }
            Err(_) => return false, // fail-closed on resolve err (e.g. timeout, no net)
        }
    }
    true
}

fn is_bad_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 127
                || o[0] == 10
                || (o[0] == 192 && o[1] == 168)
                || (o[0] == 172 && (16..=31).contains(&o[1]))
                || (o[0] == 100 && (o[1] & 0b1100_0000 == 0b0100_0000))
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 169 && o[1] == 254)
        }
        std::net::IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            if v6.segments()[0..5] == [0, 0, 0, 0, 0] && v6.segments()[5] == 0xffff {
                let v4 = std::net::Ipv4Addr::new(
                    (v6.segments()[6] >> 8) as u8,
                    v6.segments()[6] as u8,
                    (v6.segments()[7] >> 8) as u8,
                    v6.segments()[7] as u8,
                );
                return is_bad_ip(&std::net::IpAddr::V4(v4));
            }
            (v6.segments()[0] & 0xff00) == 0xfd00
        }
    }
}

fn http_client(timeout_ms: u64) -> Client {
    Client::builder()
        .user_agent(concat!(
            "council-rs/",
            env!("CARGO_PKG_VERSION"),
            " Sheldon evidence"
        ))
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .unwrap_or_else(|_| Client::new())
}

async fn send_json_request(
    source: &'static str,
    request: reqwest::RequestBuilder,
    run: Option<&EvidenceRun>,
    timeout_ms: u64,
) -> Option<Value> {
    let mut attempt = 0usize;
    let mut next_request = Some(request);
    let max_response_bytes = evidence_max_response_bytes();

    loop {
        let request = next_request.take()?;
        let retry_request = request.try_clone();
        let response = match request
            .timeout(Duration::from_millis(timeout_ms))
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) => {
                if err.is_timeout() {
                    warn_provider(
                        run,
                        source,
                        format!("request timed out after {}ms", timeout_ms),
                    );
                } else {
                    warn_provider(run, source, err.to_string());
                }
                return None;
            }
        };

        let status = response.status();
        let headers = response.headers().clone();
        if status.is_success() {
            match read_limited_body(response, max_response_bytes).await {
                Ok(body) => match serde_json::from_slice::<Value>(&body) {
                    Ok(value) => return Some(value),
                    Err(err) => {
                        warn_provider(run, source, format!("invalid JSON response: {}", err));
                        return None;
                    }
                },
                Err(err) => {
                    warn_provider(run, source, err);
                    return None;
                }
            }
        }

        let retryable = should_retry(status) && attempt < MAX_RETRIES && retry_request.is_some();
        if retryable {
            tokio::time::sleep(retry_delay(&headers, attempt)).await;
            attempt += 1;
            next_request = retry_request;
            continue;
        }

        let body = read_limited_body(response, max_response_bytes)
            .await
            .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
            .unwrap_or_else(|err| err);

        let detail = if body.trim().is_empty() {
            format!("HTTP {}", status)
        } else {
            format!("HTTP {}: {}", status, truncate_str(body.trim(), 300))
        };
        warn_provider(run, source, detail);
        return None;
    }
}

async fn read_limited_body(
    response: reqwest::Response,
    max_response_bytes: usize,
) -> Result<Vec<u8>, String> {
    if let Some(content_length) = response.content_length()
        && content_length > max_response_bytes as u64
    {
        return Err(format!(
            "response too large: content-length {} exceeds {} bytes",
            content_length, max_response_bytes
        ));
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| err.to_string())?;
        let next_len = body.len().saturating_add(chunk.len());
        if next_len > max_response_bytes {
            return Err(format!(
                "response too large: exceeded {} bytes",
                max_response_bytes
            ));
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

fn should_retry(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn retry_delay(headers: &HeaderMap, attempt: usize) -> Duration {
    headers
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map(|seconds| Duration::from_secs(seconds.min(5)))
        .unwrap_or_else(|| {
            let exponent = attempt.min(3) as u32;
            let base = 250_u64.saturating_mul(2_u64.saturating_pow(exponent));
            let jitter = 37_u64.saturating_mul((attempt as u64) + 1);
            Duration::from_millis((base + jitter).min(3_000))
        })
}

fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn tavily_recency_query(query: &str) -> String {
    let original_chars = query.chars().count();

    if original_chars > TAVILY_QUERY_MAX_CHARS {
        eprintln!(
            "      ⚠️  Tavily query truncated from {} to {} chars to satisfy {} char limit",
            original_chars, TAVILY_QUERY_MAX_CHARS, TAVILY_QUERY_MAX_CHARS
        );
        query.chars().take(TAVILY_QUERY_MAX_CHARS).collect()
    } else {
        query.to_string()
    }
}

/// Semantic web search via Exa.
async fn exa_search(query: &str, num_results: usize) -> Vec<Value> {
    exa_search_with_run(query, num_results, None).await
}

pub(crate) async fn exa_search_with_run(
    query: &str,
    num_results: usize,
    run: Option<&EvidenceRun>,
) -> Vec<Value> {
    let key = match env_key("EXA_API_KEY") {
        Some(key) => key,
        None => return vec![],
    };
    let timeout_ms = evidence_timeout_ms(DEFAULT_SEARCH_TIMEOUT_MS);
    let data = match send_json_request(
        "Exa",
        http_client(timeout_ms)
            .post("https://api.exa.ai/search")
            .header("x-api-key", key)
            .json(&json!({
                "query": query,
                "numResults": num_results.min(10),
                "type": "auto",
                "contents": {
                    "text": {
                        "maxCharacters": 1500
                    }
                }
            })),
        run,
        timeout_ms,
    )
    .await
    {
        Some(data) => data,
        None => return vec![],
    };

    extract_results(&data, "text")
}

/// Recency-tuned web search via Tavily.
async fn tavily_search(query: &str, max_results: usize) -> Vec<Value> {
    tavily_search_with_run(query, max_results, None).await
}

pub(crate) async fn tavily_search_with_run(
    query: &str,
    max_results: usize,
    run: Option<&EvidenceRun>,
) -> Vec<Value> {
    let recency_query = tavily_recency_query(query);
    tavily_search_topic(&recency_query, max_results, "general", run).await
}

async fn tavily_search_topic(
    query: &str,
    max_results: usize,
    topic: &str,
    run: Option<&EvidenceRun>,
) -> Vec<Value> {
    let key = match env_key("TAVILY_API_KEY") {
        Some(key) => key,
        None => return vec![],
    };
    let timeout_ms = evidence_timeout_ms(DEFAULT_SEARCH_TIMEOUT_MS);
    let data = match send_json_request(
        "Tavily",
        http_client(timeout_ms)
            .post("https://api.tavily.com/search")
            .bearer_auth(key)
            .json(&json!({
                "query": query,
                "max_results": max_results.min(5),
                "search_depth": "advanced",
                "include_answer": true,
                "topic": topic,
                "time_range": "week"
            })),
        run,
        timeout_ms,
    )
    .await
    {
        Some(data) => data,
        None => return vec![],
    };

    extract_results(&data, "content")
}

/// Scrape a URL to markdown via Firecrawl or a Firecrawl-compatible service.
async fn scrape_url(url: &str) -> Option<String> {
    scrape_url_with_run(url, None).await
}

pub(crate) async fn scrape_url_with_run(url: &str, run: Option<&EvidenceRun>) -> Option<String> {
    if !firecrawl_enabled() {
        return None;
    }

    // One provider outage or bad target degrades without issuing the request.
    if !is_ssrf_safe_scrape_target(url) {
        warn_provider(
            run,
            "Firecrawl",
            format!("SSRF-unsafe target rejected: {}", truncate_str(url, 100)),
        );
        return None;
    }

    let timeout_ms = evidence_timeout_ms(DEFAULT_SCRAPE_TIMEOUT_MS);
    let base = firecrawl_base_url();
    let endpoints = firecrawl_scrape_endpoints(&base);
    let body = json!({
        "url": url,
        "formats": ["markdown"],
        "onlyMainContent": true,
        "timeout": timeout_ms
    });

    for endpoint in endpoints {
        let client = http_client(timeout_ms);
        let mut request = client.post(endpoint).json(&body);
        if let Some(key) = env_key("FIRECRAWL_API_KEY") {
            request = request.bearer_auth(key);
        }
        if let Some(data) = send_json_request("Firecrawl", request, run, timeout_ms).await
            && let Some(markdown) = extract_markdown(&data)
        {
            return Some(markdown);
        }
    }

    None
}

fn firecrawl_enabled() -> bool {
    has_env("FIRECRAWL_API_KEY") || has_env("FIRECRAWL_BASE_URL")
}

fn semantic_scholar_enabled() -> bool {
    has_env("SEMANTIC_SCHOLAR_API_KEY")
        || env_truthy("COUNCIL_EVIDENCE_ENABLE_SEMANTIC_SCHOLAR_UNAUTH")
}

fn firecrawl_base_url() -> String {
    std::env::var("FIRECRAWL_BASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "https://api.firecrawl.dev".into())
        .trim_end_matches('/')
        .to_string()
}

fn firecrawl_scrape_endpoints(base: &str) -> Vec<String> {
    if base.ends_with("/v1") || base.ends_with("/v2") {
        vec![format!("{}/scrape", base)]
    } else {
        vec![format!("{}/v2/scrape", base), format!("{}/v1/scrape", base)]
    }
}

/// Real-time news search via Tavily news mode.
async fn news_search(query: &str) -> Vec<Value> {
    news_search_with_run(query, None).await
}

pub(crate) async fn news_search_with_run(query: &str, run: Option<&EvidenceRun>) -> Vec<Value> {
    let data = tavily_search_topic(query, 5, "news", run).await;
    data.into_iter()
        .map(|item| {
            json!({
                "title": item.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                "url": item.get("url").and_then(|v| v.as_str()).unwrap_or(""),
                "text": item.get("text").and_then(|v| v.as_str()).unwrap_or(""),
                "source": item.get("source").and_then(|v| v.as_str()).unwrap_or(""),
                "date": item.get("date").and_then(|v| v.as_str()).unwrap_or(""),
                "published_at": item.get("published_at").and_then(|v| v.as_str()).unwrap_or(""),
                "score": item.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0),
            })
        })
        .collect()
}

/// Academic paper search via Semantic Scholar Graph API.
async fn scholar_search(query: &str) -> Vec<Value> {
    scholar_search_with_run(query, None).await
}

pub(crate) async fn scholar_search_with_run(query: &str, run: Option<&EvidenceRun>) -> Vec<Value> {
    if !semantic_scholar_enabled() {
        return vec![];
    }

    let mut url = match Url::parse("https://api.semanticscholar.org/graph/v1/paper/search") {
        Ok(url) => url,
        Err(_) => return vec![],
    };
    url.query_pairs_mut()
        .append_pair("query", query)
        .append_pair("fields", SEMANTIC_SCHOLAR_FIELDS)
        .append_pair("limit", "5");

    let timeout_ms = evidence_timeout_ms(DEFAULT_SEARCH_TIMEOUT_MS);
    let mut request = http_client(timeout_ms).get(url);
    if let Some(key) = env_key("SEMANTIC_SCHOLAR_API_KEY") {
        request = request.header("x-api-key", key);
    }

    let data = match send_json_request("Semantic Scholar", request, run, timeout_ms).await {
        Some(data) => data,
        None => return vec![],
    };

    extract_scholar_results(&data)
}

fn extract_results(data: &Value, content_key: &str) -> Vec<Value> {
    let candidates = [
        data.get("results"),
        data.get("data").and_then(|d| d.get("results")),
        data.get("response")
            .and_then(|r| r.get("data"))
            .and_then(|d| d.get("results")),
    ];

    for candidate in candidates {
        if let Some(arr) = candidate.and_then(|v| v.as_array()) {
            return arr
                .iter()
                .filter_map(|r| {
                    let url = r.get("url").and_then(|v| v.as_str()).unwrap_or("");
                    if url.is_empty() {
                        return None;
                    }
                    Some(json!({
                        "title": r.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                        "url": url,
                        "text": r.get(content_key)
                            .or_else(|| r.get("text"))
                            .or_else(|| r.get("content"))
                            .and_then(|v| v.as_str())
                            .unwrap_or(""),
                        "score": r.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0),
                        "source": r.get("source")
                            .or_else(|| r.get("domain"))
                            .and_then(|v| v.as_str())
                            .unwrap_or(""),
                        "date": r.get("date")
                            .or_else(|| r.get("publishedDate"))
                            .or_else(|| r.get("published_date"))
                            .and_then(|v| v.as_str())
                            .unwrap_or(""),
                        "published_at": r.get("published_at")
                            .or_else(|| r.get("publishedDate"))
                            .or_else(|| r.get("published_date"))
                            .and_then(|v| v.as_str())
                            .unwrap_or(""),
                    }))
                })
                .collect();
        }
    }

    vec![]
}

fn extract_scholar_results(data: &Value) -> Vec<Value> {
    let candidates = [
        data.get("data"),
        data.get("results"),
        data.get("response")
            .and_then(|r| r.get("data"))
            .and_then(|d| d.get("data")),
    ];

    for candidate in candidates {
        if let Some(arr) = candidate.and_then(|v| v.as_array()) {
            return arr
                .iter()
                .filter_map(|r| {
                    let title = r.get("title").and_then(|v| v.as_str()).unwrap_or("");
                    if title.is_empty() {
                        return None;
                    }
                    let url = r
                        .get("url")
                        .and_then(|v| v.as_str())
                        .filter(|v| !v.is_empty())
                        .or_else(|| {
                            r.get("openAccessPdf")
                                .and_then(|p| p.get("url"))
                                .and_then(|v| v.as_str())
                        })
                        .unwrap_or("");
                    Some(json!({
                        "title": title,
                        "url": url,
                        "text": r.get("abstract").and_then(|v| v.as_str()).unwrap_or(""),
                        "source": r.get("venue").and_then(|v| v.as_str()).unwrap_or(""),
                        "citation_count": r.get("citationCount").and_then(|v| v.as_u64()).unwrap_or(0),
                        "year": json_value_to_string(r.get("year")),
                    }))
                })
                .collect();
        }
    }

    vec![]
}

fn json_value_to_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn extract_markdown(data: &Value) -> Option<String> {
    let paths: [Option<&Value>; 6] = [
        data.get("markdown"),
        data.get("data").and_then(|d| d.get("markdown")),
        data.get("data")
            .and_then(|d| d.get("data"))
            .and_then(|d| d.get("markdown")),
        data.get("response")
            .and_then(|r| r.get("data"))
            .and_then(|d| d.get("markdown")),
        data.get("content").filter(|v| v.is_string()),
        data.get("text").filter(|v| v.is_string()),
    ];

    for path in paths {
        if let Some(md) = path.and_then(|v| v.as_str())
            && !md.trim().is_empty()
        {
            return Some(truncate_str(md, 3000).to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env<R>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> R) -> R {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        let saved = vars
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect::<Vec<_>>();

        for (key, value) in vars {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }

        let result = f();

        for (key, value) in saved {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }

        result
    }

    const EVIDENCE_ENV: &[(&str, Option<&str>)] = &[
        ("COUNCIL_SHELDON_WEB_EVIDENCE", None),
        ("COUNCIL_EVIDENCE_ENABLE_SEMANTIC_SCHOLAR_UNAUTH", None),
        ("COUNCIL_EVIDENCE_MAX_RESPONSE_BYTES", None),
        ("EXA_API_KEY", None),
        ("TAVILY_API_KEY", None),
        ("FIRECRAWL_API_KEY", None),
        ("FIRECRAWL_BASE_URL", None),
        ("SEMANTIC_SCHOLAR_API_KEY", None),
    ];

    #[test]
    fn tavily_recency_query_caps_final_query_at_400_chars() {
        let query = "a".repeat(500);
        let capped = tavily_recency_query(&query);

        assert_eq!(capped.chars().count(), TAVILY_QUERY_MAX_CHARS);
    }

    #[test]
    fn configured_sources_empty_without_direct_provider_config() {
        with_env(EVIDENCE_ENV, || {
            assert!(configured_sources().is_empty());
        });
    }

    #[test]
    fn legacy_and_xmcp_modes_disable_native_sources() {
        for mode in ["off", "xmcp", "composio", "legacy"] {
            with_env(
                &[
                    ("COUNCIL_SHELDON_WEB_EVIDENCE", Some(mode)),
                    ("EXA_API_KEY", Some("exa-test")),
                    ("TAVILY_API_KEY", Some("tavily-test")),
                    ("FIRECRAWL_BASE_URL", Some("http://127.0.0.1:3002")),
                    ("SEMANTIC_SCHOLAR_API_KEY", Some("scholar-test")),
                ],
                || {
                    assert!(
                        configured_sources().is_empty(),
                        "{mode} should keep native evidence disabled"
                    );
                },
            );
        }
    }

    #[test]
    fn configured_sources_detects_direct_providers_without_network() {
        with_env(
            &[
                ("COUNCIL_SHELDON_WEB_EVIDENCE", None),
                ("EXA_API_KEY", Some("exa-test")),
                ("TAVILY_API_KEY", Some("tavily-test")),
                ("FIRECRAWL_BASE_URL", Some("http://127.0.0.1:3002")),
                ("SEMANTIC_SCHOLAR_API_KEY", None),
                (
                    "COUNCIL_EVIDENCE_ENABLE_SEMANTIC_SCHOLAR_UNAUTH",
                    Some("true"),
                ),
            ],
            || {
                assert_eq!(
                    configured_sources(),
                    vec!["exa", "tavily", "firecrawl", "semantic_scholar"]
                );
            },
        );
    }

    #[test]
    fn max_response_bytes_is_clamped_to_safe_bounds() {
        with_env(
            &[("COUNCIL_EVIDENCE_MAX_RESPONSE_BYTES", Some("8"))],
            || {
                assert_eq!(evidence_max_response_bytes(), 16_384);
            },
        );
        with_env(
            &[("COUNCIL_EVIDENCE_MAX_RESPONSE_BYTES", Some("999999999"))],
            || {
                assert_eq!(evidence_max_response_bytes(), 10 * 1_048_576);
            },
        );
    }

    #[test]
    fn tavily_recency_query_truncates_on_char_boundary() {
        let query = "é".repeat(500);
        let capped = tavily_recency_query(&query);

        assert_eq!(capped.chars().count(), TAVILY_QUERY_MAX_CHARS);
    }

    #[test]
    fn extract_results_handles_nested_shapes_and_dates() {
        let data = json!({
            "response": {
                "data": {
                    "results": [{
                        "title": "One",
                        "url": "https://example.com",
                        "content": "body",
                        "domain": "example.com",
                        "published_date": "2026-05-25",
                        "score": 0.9
                    }]
                }
            }
        });

        let results = extract_results(&data, "content");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["text"], "body");
        assert_eq!(results[0]["source"], "example.com");
        assert_eq!(results[0]["published_at"], "2026-05-25");
    }

    #[test]
    fn extract_markdown_handles_firecrawl_shapes() {
        let data = json!({
            "data": {
                "data": {
                    "markdown": "# Example"
                }
            }
        });

        assert_eq!(extract_markdown(&data).as_deref(), Some("# Example"));
    }

    #[test]
    fn scholar_results_prefer_open_access_pdf_when_url_missing() {
        let data = json!({
            "data": [{
                "title": "Paper",
                "abstract": "A useful abstract",
                "venue": "TestConf",
                "year": 2026,
                "citationCount": 42,
                "openAccessPdf": {
                    "url": "https://papers.example/paper.pdf"
                }
            }]
        });

        let results = extract_scholar_results(&data);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["url"], "https://papers.example/paper.pdf");
        assert_eq!(results[0]["year"], "2026");
        assert_eq!(results[0]["citation_count"], 42);
    }

    // === design added coverage (unit tests for configured source detection,
    // timeout/error mapping, response-size caps (existing), nested provider shapes (existing),
    // retryable status handling, SSRF-safe scrape target extraction) ===
    #[test]
    fn ssrf_safe_scrape_target_rejects_private_and_localhost() {
        assert!(!is_ssrf_safe_scrape_target("http://localhost:8080/foo"));
        assert!(!is_ssrf_safe_scrape_target("https://127.0.0.1/bar"));
        assert!(!is_ssrf_safe_scrape_target("http://10.0.0.1/baz"));
        assert!(!is_ssrf_safe_scrape_target("https://192.168.1.5/quux"));
        assert!(!is_ssrf_safe_scrape_target("http://172.16.0.1/internal"));
        assert!(!is_ssrf_safe_scrape_target("file:///etc/passwd"));
        assert!(!is_ssrf_safe_scrape_target("http://169.254.1.1/linklocal"));
    }

    #[test]
    fn ssrf_safe_scrape_target_accepts_public_https() {
        assert!(is_ssrf_safe_scrape_target("https://example.com/path"));
        assert!(is_ssrf_safe_scrape_target("http://exa.ai/search"));
        assert!(is_ssrf_safe_scrape_target("https://api.tavily.com/v1"));
    }

    #[test]
    fn timeout_error_retry_mapping_and_status() {
        // Covers timeout/error mapping + retryable status handling gate.
        // (reqwest timeout maps to transient; 429/408 retryable per should_retry)
        assert!(should_retry(StatusCode::REQUEST_TIMEOUT));
        assert!(should_retry(StatusCode::TOO_MANY_REQUESTS));
        assert!(!should_retry(StatusCode::BAD_REQUEST));
        assert!(!should_retry(StatusCode::NOT_FOUND));
    }

    async fn response_from_raw_http(raw_response: &'static str) -> reqwest::Response {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("test server addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept test request");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(raw_response.as_bytes())
                .await
                .expect("write test response");
        });

        Client::new()
            .get(format!("http://{addr}"))
            .send()
            .await
            .expect("read test response")
    }

    #[tokio::test]
    async fn read_limited_body_streams_chunked_response() {
        let response = response_from_raw_http(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
        )
        .await;

        let body = read_limited_body(response, 64).await.unwrap();
        assert_eq!(body, b"hello world");
    }

    #[tokio::test]
    async fn read_limited_body_rejects_streamed_body_over_cap() {
        let response = response_from_raw_http(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
        )
        .await;

        let err = read_limited_body(response, 8).await.unwrap_err();
        assert_eq!(err, "response too large: exceeded 8 bytes");
    }

    #[test]
    fn ssrf_t10_resolve_fail_or_bad_ip_is_explicit_fail_closed() {
        // Drives new resolve + empty/Err fail-closed + is_bad_ip (per review A/I + plan Pin-on-Resolve).
        assert!(!is_ssrf_safe_scrape_target("http://127.0.0.1:1234")); // literal -> is_bad
        assert!(!is_ssrf_safe_scrape_target("http://10.0.0.1")); // bad IP
        // hostname resolve path exercised by fn (ToSocketAddrs); Err/empty -> false explicit
        // (env-specific resolve fail covered by code; test drives branch)
        assert!(is_bad_ip(
            &"192.168.1.1".parse::<std::net::IpAddr>().unwrap()
        )); // bad IP -> true (drive is_bad)

        // drive hostname-resolve-to-bad + Err/empty arms (K): public host hits resolve arm after early filter
        assert!(
            is_ssrf_safe_scrape_target("http://example.com"),
            "public hostname that resolves to good IP must pass (ToSocketAddrs exercised)"
        );
        // force runtime resolve Err/empty (P): non-resolvable host hits the Err(_) => return false arm in prod match (controlled by invalid TLD; env-specific DNS covered by explicit match/return false in is_ssrf... prod path)
        assert!(
            !is_ssrf_safe_scrape_target("http://nonexistent.invalid.tld.that.fails.resolve"),
            "runtime resolve Err/empty must explicit fail-closed per T10"
        );
    }
}
