//! Optional War Room static export served from the Council loopback origin.
//!
//! When Council is started with `--web-dist <DIR>`, the built War Room export
//! is served from the *same* origin as `/api/**` and `/ws/**`. That removes the
//! second dev server and the cross-origin hop without introducing a second
//! port, a second process, or a change to API/WebSocket authentication.
//!
//! Invariants this module is responsible for:
//!
//! - `/api/**` and `/ws/**` never resolve to a static file or to the SPA shell.
//!   They are owned by the real router; anything unmatched under those prefixes
//!   is a 404, not `index.html`.
//! - Path resolution is fail-closed. A request path is decoded once, segment by
//!   segment, and any segment that is empty after decoding, starts with `.`
//!   (which covers `.` and `..`), or still carries a separator is rejected.
//!   The resolved file is then canonicalized and must stay inside the
//!   canonicalized root, which also closes the symlink escape.
//! - Every response leaving this module — including 404 and 405 — carries the
//!   browser security headers established by
//!   `council-rs/warroom/web/next.config.ts`.
//! - Nothing here reads credentials. The static shell and its assets are
//!   deliberately unauthenticated; the API and WebSocket auth posture is
//!   untouched because reserved-prefix catch-alls live inside the auth layer.
//!   This handler also refuses every reserved-prefix path as defense in depth.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::Response;

/// `Permissions-Policy` has no constant in the `http` crate.
const PERMISSIONS_POLICY: HeaderName = HeaderName::from_static("permissions-policy");

/// Loopback Council ports whose API/WS origins the browser is allowed to reach.
/// Mirrors the port list in `next.config.ts` (dev, smoke, and device lanes).
const COUNCIL_LOOPBACK_PORTS: [u16; 4] = [8765, 8766, 8767, 8768];

/// Default Gateway origin used for `connect-src` when nothing is configured.
const DEFAULT_GATEWAY_ORIGIN: &str = "http://127.0.0.1:18080";

/// Default Council API origin, matching the `--port` default.
const DEFAULT_API_ORIGIN: &str = "http://127.0.0.1:8765";

/// A validated War Room static export root plus the security headers that are
/// stamped on every response served from it.
#[derive(Clone, Debug)]
pub struct WebDist {
    /// Canonicalized export root. Every served file must live under it.
    root: PathBuf,
    /// Content-Security-Policy value, resolved once at server construction.
    csp: String,
}

impl WebDist {
    /// Validate `dir` and resolve the static response headers.
    ///
    /// Fails closed: a missing directory, a file passed where a directory is
    /// expected, or an unreadable path is a startup error rather than a server
    /// that silently answers every route with 404.
    pub fn new(dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let dir = dir.as_ref();
        let root = std::fs::canonicalize(dir)?;
        if !root.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} is not a directory", dir.display()),
            ));
        }
        Ok(Self {
            root,
            csp: build_csp(
                env_non_empty("NEXT_PUBLIC_API_BASE").as_deref(),
                env_non_empty("NEXT_PUBLIC_WS_BASE").as_deref(),
                env_non_empty("GATEWAY_URL")
                    .or_else(|| env_non_empty("NEXT_PUBLIC_GATEWAY_BASE"))
                    .as_deref(),
            ),
        })
    }

    /// Canonicalized export root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolved Content-Security-Policy header value.
    pub fn csp(&self) -> &str {
        &self.csp
    }

    /// Serve one request that the real router did not match.
    pub async fn serve(&self, method: &Method, uri_path: &str) -> Response {
        if method != Method::GET && method != Method::HEAD {
            return self.finish(
                StatusCode::METHOD_NOT_ALLOWED,
                "text/plain; charset=utf-8",
                CachePolicy::NoCache,
                Vec::new(),
                method,
            );
        }

        // `/api/**` and `/ws/**` belong to the API surface. An unmatched path
        // under those prefixes is a genuine 404 — never a static file and never
        // the SPA shell, which would otherwise turn a typo'd API call into a
        // 200 HTML body.
        if is_reserved_prefix(uri_path) {
            return self.not_found(method);
        }

        let Some(candidate) = resolve_request_path(&self.root, uri_path) else {
            // Traversal, encoded traversal, dotfile, or undecodable path.
            return self.not_found(method);
        };

        // 1. Exact file.
        if let Some(bytes) = self.read_contained(&candidate).await {
            let ct = content_type_for(&candidate);
            return self.finish(
                StatusCode::OK,
                ct,
                cache_policy(uri_path, ct),
                bytes,
                method,
            );
        }

        // 2. Directory index (`/settings/` → `settings/index.html`).
        if let Some(bytes) = self.read_contained(&candidate.join("index.html")).await {
            return self.finish(
                StatusCode::OK,
                HTML_CONTENT_TYPE,
                CachePolicy::NoCache,
                bytes,
                method,
            );
        }

        let ext = extension_of(uri_path);

        // 3. Extensionless route emitted as a sibling `.html` file by
        //    `output: "export"` (`/settings` → `settings.html`).
        if ext.is_none()
            && let Some(name) = candidate.file_name().and_then(|n| n.to_str())
            && let Some(bytes) = self
                .read_contained(&candidate.with_file_name(format!("{name}.html")))
                .await
        {
            return self.finish(
                StatusCode::OK,
                HTML_CONTENT_TYPE,
                CachePolicy::NoCache,
                bytes,
                method,
            );
        }

        // 4. A missing asset stays missing. Returning the SPA shell for a
        //    `.js`/`.css`/image miss hands the browser HTML where it expects
        //    code, which fails later and much more confusingly.
        if ext.is_some_and(|e| e != "html" && e != "htm") {
            return self.not_found(method);
        }

        // 5. SPA fallback.
        match self.read_contained(&self.root.join("index.html")).await {
            Some(bytes) => self.finish(
                StatusCode::OK,
                HTML_CONTENT_TYPE,
                CachePolicy::NoCache,
                bytes,
                method,
            ),
            None => self.not_found(method),
        }
    }

    /// Read `candidate` only if it canonicalizes to a regular file inside the
    /// export root. Symlinks that point outside the root fail here.
    async fn read_contained(&self, candidate: &Path) -> Option<Vec<u8>> {
        let canonical = tokio::fs::canonicalize(candidate).await.ok()?;
        if !canonical.starts_with(&self.root) {
            return None;
        }
        let meta = tokio::fs::metadata(&canonical).await.ok()?;
        if !meta.is_file() {
            return None;
        }
        tokio::fs::read(&canonical).await.ok()
    }

    fn not_found(&self, method: &Method) -> Response {
        self.finish(
            StatusCode::NOT_FOUND,
            "text/plain; charset=utf-8",
            CachePolicy::NoCache,
            Vec::new(),
            method,
        )
    }

    /// Build the response and stamp the browser security headers.
    fn finish(
        &self,
        status: StatusCode,
        content_type: &str,
        cache: CachePolicy,
        bytes: Vec<u8>,
        method: &Method,
    ) -> Response {
        let len = bytes.len();
        // HEAD keeps the headers (including Content-Length) and drops the body.
        let body = if method == Method::HEAD {
            Body::empty()
        } else {
            Body::from(bytes)
        };
        let mut resp = Response::new(body);
        *resp.status_mut() = status;
        let headers = resp.headers_mut();

        if let Ok(v) = HeaderValue::from_str(content_type) {
            headers.insert(header::CONTENT_TYPE, v);
        }
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
        if let Ok(v) = HeaderValue::from_str(&self.csp) {
            headers.insert(header::CONTENT_SECURITY_POLICY, v);
        }
        headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
        headers.insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        );
        headers.insert(
            PERMISSIONS_POLICY,
            HeaderValue::from_static("camera=(), microphone=(), geolocation=(), payment=()"),
        );
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(cache.value()),
        );
        resp
    }
}

/// Cache posture for a static response. HTML is always revalidated so a
/// redeployed export is never pinned in the browser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CachePolicy {
    NoCache,
    Immutable,
    ShortLived,
}

impl CachePolicy {
    fn value(self) -> &'static str {
        match self {
            CachePolicy::NoCache => "no-cache",
            CachePolicy::Immutable => "public, max-age=31536000, immutable",
            CachePolicy::ShortLived => "public, max-age=3600",
        }
    }
}

const HTML_CONTENT_TYPE: &str = "text/html; charset=utf-8";

/// Content-hashed build output is safe to pin; everything else is not.
pub(crate) fn cache_policy(uri_path: &str, content_type: &str) -> CachePolicy {
    if content_type.starts_with("text/html") {
        CachePolicy::NoCache
    } else if uri_path.starts_with("/_next/static/") {
        CachePolicy::Immutable
    } else {
        CachePolicy::ShortLived
    }
}

/// True for the request paths owned by the API/WebSocket surface.
pub(crate) fn is_reserved_prefix(uri_path: &str) -> bool {
    for prefix in ["/api", "/ws"] {
        if let Some(rest) = uri_path.strip_prefix(prefix)
            && (rest.is_empty() || rest.starts_with('/'))
        {
            return true;
        }
    }
    false
}

/// Lowercased extension of the final path segment, if any.
pub(crate) fn extension_of(uri_path: &str) -> Option<String> {
    let last = uri_path.rsplit('/').next()?;
    let (_, ext) = last.rsplit_once('.')?;
    if ext.is_empty() {
        None
    } else {
        Some(ext.to_ascii_lowercase())
    }
}

/// Map a request path onto a path inside `root`, or reject it.
///
/// Fail-closed rules, applied per segment after a single percent-decode:
/// undecodable input, an empty segment, a segment starting with `.` (`.`, `..`,
/// and dotfiles), a segment still containing `/`, `\`, or NUL, or anything that
/// is not exactly one path component is rejected. Because decoding happens
/// exactly once, `%2e%2e` becomes `..` and is rejected, while `%252e%252e`
/// becomes the literal `%2e%2e` and simply does not exist on disk.
pub(crate) fn resolve_request_path(root: &Path, uri_path: &str) -> Option<PathBuf> {
    let mut out = root.to_path_buf();
    for raw in uri_path.split('/') {
        if raw.is_empty() {
            continue;
        }
        let segment = percent_decode(raw)?;
        if segment.is_empty() || segment.starts_with('.') {
            return None;
        }
        if segment.contains('/') || segment.contains('\\') || segment.contains('\0') {
            return None;
        }
        if Path::new(&segment).components().count() != 1 {
            return None;
        }
        out.push(segment);
    }
    Some(out)
}

/// Decode one percent-encoded path segment. Returns `None` for a malformed
/// escape or for bytes that are not valid UTF-8.
fn percent_decode(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Content type for a resolved file. Covers what `output: "export"` emits;
/// anything else is served as an opaque download rather than guessed at.
pub(crate) fn content_type_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => HTML_CONTENT_TYPE,
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "map" => "application/json; charset=utf-8",
        "webmanifest" => "application/manifest+json",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Split `http://host:port/...` into (scheme, authority).
fn split_origin(raw: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = raw.trim().split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.split('/').next().unwrap_or(rest);
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    Some((scheme, authority))
}

/// HTTP origin plus its WebSocket sibling, as `connect-src` needs both.
fn origin_pair(raw: &str) -> Option<[String; 2]> {
    let (scheme, authority) = split_origin(raw)?;
    let ws = if scheme == "https" { "wss" } else { "ws" };
    Some([
        format!("{scheme}://{authority}"),
        format!("{ws}://{authority}"),
    ])
}

fn push_unique(origins: &mut Vec<String>, value: String) {
    if !origins.contains(&value) {
        origins.push(value);
    }
}

/// Reproduce the browser security policy from
/// `council-rs/warroom/web/next.config.ts` for a statically served response.
///
/// Pure in its inputs so the policy is unit-testable without touching process
/// env. `script-src` intentionally omits `'unsafe-eval'`: that escape only
/// existed for the Next dev server, and a built export never needs it.
pub(crate) fn build_csp(
    api_base: Option<&str>,
    ws_base: Option<&str>,
    gateway_base: Option<&str>,
) -> String {
    let mut connect: Vec<String> = vec!["'self'".to_string()];

    // Runtime Settings can point the browser at a different local Council
    // backend than the build-time default. Explicit ports only — no loopback
    // wildcards.
    for port in COUNCIL_LOOPBACK_PORTS {
        for origin in origin_pair(&format!("http://127.0.0.1:{port}"))
            .into_iter()
            .flatten()
        {
            push_unique(&mut connect, origin);
        }
    }

    let api_raw = api_base.unwrap_or(DEFAULT_API_ORIGIN);
    match origin_pair(api_raw) {
        Some(pair) => {
            for origin in pair {
                push_unique(&mut connect, origin);
            }
        }
        // An invalid runtime default must not widen the policy; fall back to
        // the documented loopback default instead.
        None => {
            for origin in origin_pair(DEFAULT_API_ORIGIN).into_iter().flatten() {
                push_unique(&mut connect, origin);
            }
        }
    }

    if let Some(ws_raw) = ws_base
        && let Some((scheme, authority)) = split_origin(ws_raw)
    {
        push_unique(&mut connect, format!("{scheme}://{authority}"));
    }

    for origin in origin_pair(gateway_base.unwrap_or(DEFAULT_GATEWAY_ORIGIN))
        .into_iter()
        .flatten()
    {
        push_unique(&mut connect, origin);
    }

    [
        "default-src 'self'".to_string(),
        "script-src 'self' 'unsafe-inline'".to_string(),
        "style-src 'self' 'unsafe-inline'".to_string(),
        "img-src 'self' data: blob:".to_string(),
        "font-src 'self' data:".to_string(),
        format!("connect-src {}", connect.join(" ")),
        "object-src 'none'".to_string(),
        "base-uri 'none'".to_string(),
        "form-action 'self'".to_string(),
        "frame-ancestors 'none'".to_string(),
    ]
    .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_prefixes_cover_api_and_ws_only() {
        assert!(is_reserved_prefix("/api"));
        assert!(is_reserved_prefix("/api/health"));
        assert!(is_reserved_prefix("/ws"));
        assert!(is_reserved_prefix("/ws/deliberate"));
        assert!(!is_reserved_prefix("/apiary"));
        assert!(!is_reserved_prefix("/wsx"));
        assert!(!is_reserved_prefix("/settings"));
        assert!(!is_reserved_prefix("/"));
    }

    #[test]
    fn traversal_and_encoded_traversal_are_rejected() {
        let root = Path::new("/tmp/dist");
        for path in [
            "/../etc/passwd",
            "/assets/../../etc/passwd",
            "/%2e%2e/etc/passwd",
            "/%2E%2E/etc/passwd",
            "/assets/%2e%2e%2fetc/passwd",
            "/.env",
            "/./secret",
            "/%2e/secret",
            "/assets/%ZZ",
            "/assets/%2",
        ] {
            assert!(
                resolve_request_path(root, path).is_none(),
                "{path} must fail closed"
            );
        }
    }

    #[test]
    fn ordinary_paths_resolve_under_root() {
        let root = Path::new("/tmp/dist");
        assert_eq!(resolve_request_path(root, "/"), Some(root.to_path_buf()));
        assert_eq!(
            resolve_request_path(root, "/_next/static/chunk.js"),
            Some(root.join("_next/static/chunk.js"))
        );
        // Double encoding decodes exactly once, so this is a literal filename,
        // not a traversal.
        assert_eq!(
            resolve_request_path(root, "/%252e%252e"),
            Some(root.join("%2e%2e"))
        );
        // A space is a legal, decoded filename character.
        assert_eq!(
            resolve_request_path(root, "/my%20file.txt"),
            Some(root.join("my file.txt"))
        );
    }

    #[test]
    fn extension_detection_matches_last_segment() {
        assert_eq!(extension_of("/a/b/chunk.JS").as_deref(), Some("js"));
        assert_eq!(extension_of("/settings"), None);
        assert_eq!(extension_of("/v1.2/settings"), None);
        assert_eq!(extension_of("/"), None);
    }

    #[test]
    fn cache_policy_pins_only_hashed_build_output() {
        assert_eq!(
            cache_policy("/_next/static/x.js", "text/javascript; charset=utf-8"),
            CachePolicy::Immutable
        );
        assert_eq!(
            cache_policy("/logo.png", "image/png"),
            CachePolicy::ShortLived
        );
        assert_eq!(
            cache_policy("/_next/static/page.html", HTML_CONTENT_TYPE),
            CachePolicy::NoCache
        );
    }

    #[test]
    fn csp_mirrors_next_config_directives() {
        let csp = build_csp(None, None, None);
        for directive in [
            "default-src 'self'",
            "script-src 'self' 'unsafe-inline'",
            "style-src 'self' 'unsafe-inline'",
            "img-src 'self' data: blob:",
            "font-src 'self' data:",
            "object-src 'none'",
            "base-uri 'none'",
            "form-action 'self'",
            "frame-ancestors 'none'",
        ] {
            assert!(csp.contains(directive), "missing {directive} in {csp}");
        }
        // Built export never needs the dev-server eval escape.
        assert!(!csp.contains("unsafe-eval"));
        assert!(csp.contains("connect-src 'self' http://127.0.0.1:8765 ws://127.0.0.1:8765"));
        assert!(csp.contains("http://127.0.0.1:18080"));
        assert!(csp.contains("ws://127.0.0.1:18080"));
    }

    #[test]
    fn csp_includes_configured_origins_and_ignores_invalid_ones() {
        let csp = build_csp(
            Some("https://council.example:9443"),
            Some("wss://council.example:9443"),
            Some("http://127.0.0.1:19090"),
        );
        assert!(csp.contains("https://council.example:9443"));
        assert!(csp.contains("wss://council.example:9443"));
        assert!(csp.contains("http://127.0.0.1:19090"));

        let fallback = build_csp(Some("not a url"), Some("also not a url"), Some("nope"));
        assert!(fallback.contains("http://127.0.0.1:8765"));
        assert!(!fallback.contains("not a url"));
        assert!(!fallback.contains("nope"));
    }

    #[test]
    fn content_types_cover_export_output() {
        assert_eq!(
            content_type_for(Path::new("a/index.html")),
            HTML_CONTENT_TYPE
        );
        assert_eq!(
            content_type_for(Path::new("a/chunk.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type_for(Path::new("a/x.woff2")), "font/woff2");
        assert_eq!(
            content_type_for(Path::new("a/unknown.bin")),
            "application/octet-stream"
        );
    }
}
