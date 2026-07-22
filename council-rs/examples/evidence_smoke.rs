use council_rs::evidence;

#[tokio::main]
async fn main() {
    let report = evidence::smoke_configured_sources(true).await;

    if !report.available {
        eprintln!("FAIL: no native web evidence sources available");
        eprintln!(
            "Configure Exa, Tavily, Firecrawl/fastCRW, or Semantic Scholar before running this smoke."
        );
        std::process::exit(1);
    }

    eprintln!("\n--- Native evidence smoke ---");
    eprintln!("Exa: {} results", report.exa_results);
    eprintln!("Tavily: {} results", report.tavily_results);
    eprintln!("News: {} results", report.news_results);
    eprintln!("Scholar: {} results", report.scholar_results);
    match report.firecrawl_chars {
        Some(chars) => eprintln!("Firecrawl: {} chars", chars),
        None => eprintln!("Firecrawl: no content"),
    }

    eprintln!();
    if report.success() {
        eprintln!("Configured evidence sources operational.");
    } else {
        eprintln!("FAILED sources: {}", report.failures.join(", "));
        std::process::exit(1);
    }
}
