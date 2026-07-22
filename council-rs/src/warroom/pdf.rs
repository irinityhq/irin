//! Session PDF export (N06) — minimal hand-rolled paginated text PDF.
//!
//! `POST /api/sessions/{id}/export/pdf` renders a session's ruling to a
//! downloadable PDF. The synthesis is just text, so a simple paginated
//! single-font (Helvetica, a PDF base-14 font — no font embedding needed) text
//! document is the honest v1. We avoid a new crate (`printpdf` would pull
//! image/font deps the map flagged as unresolved weight) by emitting the PDF
//! byte structure directly.
//!
//! Layout per page:
//!   - Title (topic) on page 1
//!   - Metadata line (date / cabinet / mode / convergence) on page 1
//!   - Synthesis body, word-wrapped and paginated
//!   - Footer with `session_id` on every page
//!
//! Markdown is stripped to plain text (v1) and the text is escaped for the PDF
//! string syntax + downcoded to WinAnsi-safe ASCII (the base-14 fonts use
//! single-byte encodings; non-ASCII is replaced with '?').

use crate::types::CouncilSession;

const PAGE_W: f64 = 612.0; // US Letter, 72 dpi
const PAGE_H: f64 = 792.0;
const MARGIN: f64 = 54.0;
const BODY_SIZE: f64 = 11.0;
const TITLE_SIZE: f64 = 18.0;
const META_SIZE: f64 = 9.0;
const FOOTER_SIZE: f64 = 8.0;
const LEADING: f64 = 15.0; // line height for body
const MAX_LINE_CHARS: usize = 92; // ~Helvetica 11pt across the text column

/// Render a session to PDF bytes. Always succeeds — even an empty synthesis
/// produces a valid (mostly metadata) PDF.
pub fn render_session(session: &CouncilSession) -> Vec<u8> {
    let lines = layout_lines(session);
    let pages = paginate(&lines);
    build_pdf(&pages, &session.session_id)
}

/// A laid-out line plus the font size to render it at.
struct Line {
    text: String,
    size: f64,
}

/// Turn the session into a flat list of laid-out lines (title, metadata, body).
fn layout_lines(session: &CouncilSession) -> Vec<Line> {
    let mut lines: Vec<Line> = Vec::new();

    // Title — the topic, wrapped at the title width.
    let topic = if session.topic.trim().is_empty() {
        "(untitled session)"
    } else {
        session.topic.trim()
    };
    for wrapped in wrap_text(topic, 62) {
        lines.push(Line {
            text: wrapped,
            size: TITLE_SIZE,
        });
    }
    lines.push(Line {
        text: String::new(),
        size: BODY_SIZE,
    });

    // Metadata line.
    let date = session.timestamp.format("%Y-%m-%d %H:%M UTC").to_string();
    let mode = session_mode_label(session);
    let convergence = session
        .rounds
        .last()
        .map(|r| format!("{:.0}%", r.convergence_score * 100.0))
        .unwrap_or_else(|| "n/a".to_string());
    let route = execution_route_label(session);
    let meta = format!(
        "Date: {}    Cabinet: {}    Mode: {}    Route: {}    Convergence: {}",
        date, session.cabinet_name, mode, route, convergence
    );
    for wrapped in wrap_text(&meta, MAX_LINE_CHARS + 20) {
        lines.push(Line {
            text: wrapped,
            size: META_SIZE,
        });
    }
    lines.push(Line {
        text: String::new(),
        size: BODY_SIZE,
    });
    lines.push(Line {
        text: "RULING".to_string(),
        size: BODY_SIZE + 1.0,
    });
    lines.push(Line {
        text: String::new(),
        size: BODY_SIZE,
    });

    // Synthesis body, markdown-stripped and word-wrapped.
    let synthesis = session
        .synthesis
        .as_deref()
        .map(strip_markdown)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "(no synthesis recorded)".to_string());

    for raw_line in synthesis.lines() {
        let trimmed = raw_line.trim_end();
        if trimmed.is_empty() {
            lines.push(Line {
                text: String::new(),
                size: BODY_SIZE,
            });
            continue;
        }
        for wrapped in wrap_text(trimmed, MAX_LINE_CHARS) {
            lines.push(Line {
                text: wrapped,
                size: BODY_SIZE,
            });
        }
    }

    lines
}

/// The serde wire string for the session mode (e.g. "teardown", "pathfind").
fn session_mode_label(session: &CouncilSession) -> String {
    serde_json::to_value(&session.mode)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| "normal".to_string())
}

fn execution_route_label(session: &CouncilSession) -> String {
    let route = serde_json::to_value(session.execution_route)
        .ok()
        .and_then(|value| value.as_str().map(String::from))
        .unwrap_or_else(|| "unknown".to_string());
    match session.gateway_sensitivity.as_deref() {
        Some(sensitivity) if route == "governed" => {
            format!("governed ({})", sensitivity.to_lowercase())
        }
        _ => route,
    }
}

/// Greedy word-wrap to `max_chars` per line. Long single words are hard-split.
fn wrap_text(text: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(8);
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        // Hard-split a word longer than the line width.
        let mut word = word.to_string();
        while word.len() > max_chars {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            let (head, tail) = word.split_at(max_chars);
            out.push(head.to_string());
            word = tail.to_string();
        }
        if current.is_empty() {
            current = word;
        } else if current.len() + 1 + word.len() <= max_chars {
            current.push(' ');
            current.push_str(&word);
        } else {
            out.push(std::mem::take(&mut current));
            current = word;
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Strip a small subset of markdown to plain text (v1): heading hashes, bold/
/// italic markers, inline-code backticks, and bullet/number list prefixes are
/// normalized; everything else passes through.
fn strip_markdown(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    for line in md.lines() {
        let mut s = line.trim_end().to_string();
        // Leading heading hashes.
        let trimmed = s.trim_start();
        let lead_ws = &s[..s.len() - trimmed.len()];
        let mut content = trimmed.to_string();
        if let Some(rest) = content.strip_prefix("######") {
            content = rest.trim_start().to_string();
        } else if let Some(rest) = content.strip_prefix("#####") {
            content = rest.trim_start().to_string();
        } else if let Some(rest) = content.strip_prefix("####") {
            content = rest.trim_start().to_string();
        } else if let Some(rest) = content.strip_prefix("###") {
            content = rest.trim_start().to_string();
        } else if let Some(rest) = content.strip_prefix("##") {
            content = rest.trim_start().to_string();
        } else if let Some(rest) = content.strip_prefix("# ") {
            content = rest.trim_start().to_string();
        }
        // Bullet markers → "- ".
        if let Some(rest) = content.strip_prefix("* ") {
            content = format!("- {}", rest);
        } else if let Some(rest) = content.strip_prefix("+ ") {
            content = format!("- {}", rest);
        }
        s = format!("{lead_ws}{content}");
        // Inline markers: drop **, __, `, * (emphasis). Keep the text.
        let s = s.replace("**", "").replace("__", "").replace('`', "");
        out.push_str(&s);
        out.push('\n');
    }
    out
}

/// One page is an ordered list of laid-out lines that fit vertically.
fn paginate(lines: &[Line]) -> Vec<Vec<&Line>> {
    let top = PAGE_H - MARGIN - TITLE_SIZE;
    let bottom = MARGIN + LEADING; // leave room for the footer
    let usable = top - bottom;
    let per_page = (usable / LEADING).floor().max(1.0) as usize;

    let mut pages: Vec<Vec<&Line>> = Vec::new();
    let mut current: Vec<&Line> = Vec::new();
    for line in lines {
        if current.len() >= per_page {
            pages.push(std::mem::take(&mut current));
        }
        current.push(line);
    }
    if !current.is_empty() {
        pages.push(current);
    }
    if pages.is_empty() {
        pages.push(vec![]);
    }
    pages
}

/// Escape a string for a PDF literal `( ... )` and downcode to ASCII so the
/// base-14 single-byte encoding renders it. Non-ASCII → '?'.
fn pdf_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        let c = if ch.is_ascii() { ch } else { '?' };
        match c {
            '\\' => out.push_str("\\\\"),
            '(' => out.push_str("\\("),
            ')' => out.push_str("\\)"),
            '\r' => {}
            '\n' => {}
            _ => out.push(c),
        }
    }
    out
}

/// Emit the content stream for one page: text lines top-to-bottom plus the
/// footer.
fn page_content(page: &[&Line], session_id: &str) -> String {
    let mut s = String::new();
    let mut y = PAGE_H - MARGIN - TITLE_SIZE;

    for line in page {
        if !line.text.is_empty() {
            s.push_str("BT\n");
            s.push_str(&format!("/F1 {:.1} Tf\n", line.size));
            s.push_str(&format!("1 0 0 1 {:.1} {:.1} Tm\n", MARGIN, y));
            s.push_str(&format!("({}) Tj\n", pdf_escape(&line.text)));
            s.push_str("ET\n");
        }
        y -= LEADING;
    }

    // Footer — session id, bottom-left.
    s.push_str("BT\n");
    s.push_str(&format!("/F1 {:.1} Tf\n", FOOTER_SIZE));
    s.push_str(&format!("1 0 0 1 {:.1} {:.1} Tm\n", MARGIN, MARGIN - 18.0));
    s.push_str(&format!(
        "(council session {}) Tj\n",
        pdf_escape(session_id)
    ));
    s.push_str("ET\n");

    s
}

/// Assemble the full PDF document from paginated content.
fn build_pdf(pages: &[Vec<&Line>], session_id: &str) -> Vec<u8> {
    // Object plan:
    //   1: Catalog
    //   2: Pages tree
    //   3: Font (Helvetica)
    //   for each page p (0-based): page object id = 4 + 2*p,
    //                              content stream id = 5 + 2*p
    let page_count = pages.len();
    let mut objects: Vec<String> = Vec::new();

    let kids: Vec<String> = (0..page_count)
        .map(|p| format!("{} 0 R", 4 + 2 * p))
        .collect();

    // 1 Catalog
    objects.push("<< /Type /Catalog /Pages 2 0 R >>".to_string());
    // 2 Pages
    objects.push(format!(
        "<< /Type /Pages /Count {} /Kids [{}] >>",
        page_count,
        kids.join(" ")
    ));
    // 3 Font
    objects.push(
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_string(),
    );

    // Page + content objects.
    for (p, page) in pages.iter().enumerate() {
        let content = page_content(page, session_id);
        let content_id = 5 + 2 * p;
        let page_id = 4 + 2 * p;
        let _ = page_id;
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {:.0} {:.0}] \
             /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
            PAGE_W, PAGE_H, content_id
        ));
        objects.push(format!(
            "<< /Length {} >>\nstream\n{}\nendstream",
            content.len() + 1,
            content
        ));
    }

    // Serialize with a cross-reference table.
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");
    // Binary marker comment (helps some viewers treat the file as binary).
    buf.extend_from_slice(b"%\xE2\xE3\xCF\xD3\n");

    let mut offsets: Vec<usize> = Vec::with_capacity(objects.len());
    for (i, obj) in objects.iter().enumerate() {
        offsets.push(buf.len());
        let header = format!("{} 0 obj\n", i + 1);
        buf.extend_from_slice(header.as_bytes());
        buf.extend_from_slice(obj.as_bytes());
        buf.extend_from_slice(b"\nendobj\n");
    }

    // xref
    let xref_start = buf.len();
    let count = objects.len() + 1; // +1 for the free object 0
    buf.extend_from_slice(format!("xref\n0 {}\n", count).as_bytes());
    buf.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }

    // trailer
    buf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            count, xref_start
        )
        .as_bytes(),
    );

    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SessionMode;
    use chrono::Utc;

    fn fixture_session(synthesis: Option<&str>) -> CouncilSession {
        CouncilSession {
            session_id: "abc123def456".to_string(),
            topic: "Should we migrate auth to passkeys?".to_string(),
            cabinet_name: "standard".to_string(),
            rounds: vec![],
            synthesis: synthesis.map(String::from),
            synthesis_model: Some("grok-4.3".to_string()),
            total_tokens: 0,
            total_latency_ms: 0,
            total_cost_usd: 0.0,
            specops_triggered: false,
            specops_cost_usd: 0.0,
            mode: SessionMode::TearDown,
            precedent_ids: vec![],
            timestamp: Utc::now(),
            schema_version: 2,
            tier: "best".to_string(),
            budget: None,
            context_sources: vec![],
            origin: crate::types::SessionOrigin::Warroom,
            execution_route: Default::default(),
            gateway_sensitivity: None,
            chair_tokens_in: 0,
            chair_tokens_out: 0,
            chair_cost_usd: 0.0,
            chair_provider_provenance: None,
            chair_gateway_provenance: None,
            parent_request_id: None,
            worker_provenance: None,
            worker_metrics: None,
        }
    }

    #[test]
    fn render_produces_pdf_magic_and_eof() {
        let session = fixture_session(Some("## Ruling\n\nShip it. Confidence HIGH."));
        let bytes = render_session(&session);
        assert!(bytes.starts_with(b"%PDF-"), "must start with PDF magic");
        assert!(bytes.len() > 200, "non-trivial length, got {}", bytes.len());
        // Trailer present.
        let tail = String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(64)..]);
        assert!(tail.contains("%%EOF"), "must end with EOF marker");
    }

    #[test]
    fn render_handles_missing_synthesis() {
        let session = fixture_session(None);
        let bytes = render_session(&session);
        assert!(bytes.starts_with(b"%PDF-"));
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("no synthesis recorded"));
    }

    #[test]
    fn governed_route_is_part_of_exported_metadata() {
        let mut session = fixture_session(Some("Ship it."));
        session.execution_route = crate::types::ExecutionRoute::Governed;
        session.gateway_sensitivity = Some("yellow".to_string());
        let lines = layout_lines(&session);
        assert!(
            lines
                .iter()
                .any(|line| line.text.contains("Route: governed (yellow)"))
        );
    }

    #[test]
    fn render_paginates_long_synthesis() {
        // ~400 lines forces multiple pages.
        let body = (0..400)
            .map(|i| format!("Line {i} of the ruling with enough words to wrap around."))
            .collect::<Vec<_>>()
            .join("\n");
        let session = fixture_session(Some(&body));
        let bytes = render_session(&session);
        let text = String::from_utf8_lossy(&bytes);
        // More than one /Type /Page object.
        let page_count =
            text.matches("/Type /Page ").count() + text.matches("/Type /Page\n").count();
        // Count via the Pages /Count entry instead (robust to spacing).
        assert!(text.contains("/Type /Pages /Count "));
        let count_marker = "/Type /Pages /Count ";
        let idx = text.find(count_marker).unwrap() + count_marker.len();
        let count_str: String = text[idx..].chars().take_while(|c| c.is_numeric()).collect();
        let count: usize = count_str.parse().unwrap();
        assert!(count > 1, "long synthesis must span >1 page, got {count}");
        let _ = page_count;
    }

    #[test]
    fn pdf_escape_neutralizes_parens_and_backslashes() {
        assert_eq!(pdf_escape("a(b)c\\d"), "a\\(b\\)c\\\\d");
        // Non-ASCII downcoded.
        assert_eq!(pdf_escape("café"), "caf?");
    }

    #[test]
    fn strip_markdown_removes_headings_and_emphasis() {
        let out = strip_markdown("## Heading\n**bold** and `code`\n* bullet");
        assert!(out.contains("Heading"));
        assert!(!out.contains("##"));
        assert!(!out.contains("**"));
        assert!(!out.contains('`'));
        assert!(out.contains("- bullet"));
    }
}
