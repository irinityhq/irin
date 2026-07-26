//! `--web-dist` one-origin War Room tests.
//!
//! Zero provider spend and zero sockets: these drive the real
//! `server::router_with_web_dist` through `tower::ServiceExt::oneshot` against a
//! temporary static export on disk. Going through the router directly (rather
//! than an HTTP client) is deliberate — a client would normalize `..` and
//! percent escapes before they ever reach the traversal guard.
//!
//! `COUNCIL_AUTH_TOKEN` is set once for this binary so the API-precedence
//! assertions exercise the real authenticated posture.

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use council_rs::server;
use council_rs::static_web::WebDist;
use tower::ServiceExt;

const TOKEN: &str = "static-origin-test-bearer";
const INDEX_MARKER: &str = "WARROOM_SPA_SHELL";
const SECRET_MARKER: &str = "TOPSECRET_OUTSIDE_ROOT";

/// `router()` initializes a process-global `AUTH_CONFIG` exactly once, so the
/// token has to exist before the first router in this binary is built.
fn init_env() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        // SAFETY: single-shot, before any router in this binary reads env.
        unsafe { std::env::set_var("COUNCIL_AUTH_TOKEN", TOKEN) };
    });
}

fn config() -> Arc<council_rs::config::Config> {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"));
    Arc::new(council_rs::config::Config::load(base).expect("config"))
}

/// Build a temp workspace: a `dist/` export plus a secret file that lives
/// *outside* it, which every traversal attempt tries to reach.
fn fixture(tag: &str) -> std::path::PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let workspace = std::env::temp_dir().join(format!(
        "council_static_{tag}_{}_{:?}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        std::thread::current().id()
    ));
    let dist = workspace.join("dist");
    std::fs::create_dir_all(dist.join("_next/static")).unwrap();
    std::fs::create_dir_all(dist.join("nested")).unwrap();

    std::fs::write(workspace.join("secret.txt"), format!("{SECRET_MARKER}\n")).unwrap();
    std::fs::write(
        dist.join("index.html"),
        format!("<!doctype html><title>{INDEX_MARKER}</title>"),
    )
    .unwrap();
    std::fs::write(
        dist.join("_next/static/chunk.js"),
        "export const chunk = 1;\n",
    )
    .unwrap();
    std::fs::write(dist.join("settings.html"), "<!doctype html>settings").unwrap();
    std::fs::write(dist.join("nested/index.html"), "<!doctype html>nested").unwrap();
    dist
}

fn app(dist: &Path) -> axum::Router {
    init_env();
    server::router_with_web_dist(config(), Some(WebDist::new(dist).expect("web dist")))
}

struct Res {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
}

impl Res {
    fn header(&self, name: &str) -> String {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }
}

async fn send(app: axum::Router, method: Method, uri: &str, bearer: Option<&str>) -> Res {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let resp = app
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .expect("router response");
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .expect("body");
    Res {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).to_string(),
    }
}

async fn get(app: axum::Router, uri: &str) -> Res {
    send(app, Method::GET, uri, None).await
}

#[tokio::test]
async fn serves_exact_static_files_and_directory_indexes() {
    let dist = fixture("exact");

    let root = get(app(&dist), "/").await;
    assert_eq!(root.status, StatusCode::OK);
    assert!(root.body.contains(INDEX_MARKER));
    assert_eq!(root.header("content-type"), "text/html; charset=utf-8");

    let chunk = get(app(&dist), "/_next/static/chunk.js").await;
    assert_eq!(chunk.status, StatusCode::OK);
    assert!(chunk.body.contains("export const chunk"));
    assert_eq!(
        chunk.header("content-type"),
        "text/javascript; charset=utf-8"
    );
    // Content-hashed build output is the only thing pinned in the browser.
    assert!(chunk.header("cache-control").contains("immutable"));

    // Extensionless route emitted as a sibling `.html` by `output: "export"`.
    let settings = get(app(&dist), "/settings").await;
    assert_eq!(settings.status, StatusCode::OK);
    assert!(settings.body.contains("settings"));

    let nested = get(app(&dist), "/nested/").await;
    assert_eq!(nested.status, StatusCode::OK);
    assert!(nested.body.contains("nested"));

    // HEAD keeps the headers, drops the body.
    let head = send(app(&dist), Method::HEAD, "/_next/static/chunk.js", None).await;
    assert_eq!(head.status, StatusCode::OK);
    assert!(head.body.is_empty());
    assert_eq!(head.header("content-length"), "24");
}

#[tokio::test]
async fn unknown_page_routes_fall_back_to_the_spa_shell() {
    let dist = fixture("spa");

    for uri in ["/deliberations/abc123", "/does/not/exist", "/page.html"] {
        let res = get(app(&dist), uri).await;
        assert_eq!(res.status, StatusCode::OK, "{uri} should serve the shell");
        assert!(res.body.contains(INDEX_MARKER), "{uri} body");
        assert_eq!(res.header("cache-control"), "no-cache", "{uri} cache");
    }

    // A missing *asset* stays a 404 — handing HTML to a script tag would fail
    // later and far less legibly.
    let missing_asset = get(app(&dist), "/_next/static/missing.js").await;
    assert_eq!(missing_asset.status, StatusCode::NOT_FOUND);
    assert!(!missing_asset.body.contains(INDEX_MARKER));
}

#[tokio::test]
async fn api_and_ws_routes_take_precedence_and_keep_their_auth() {
    let dist = fixture("precedence");

    // Real API route, correct bearer → the API answers, not the shell.
    let health = send(app(&dist), Method::GET, "/api/health", Some(TOKEN)).await;
    assert_eq!(health.status, StatusCode::OK);
    assert!(!health.body.contains(INDEX_MARKER));
    assert!(health.header("content-type").contains("json"));

    // Same route without the bearer is still rejected: enabling the static
    // origin must not weaken API auth.
    let unauthed = get(app(&dist), "/api/health").await;
    assert_eq!(unauthed.status, StatusCode::UNAUTHORIZED);
    assert!(!unauthed.body.contains(INDEX_MARKER));

    // Unmatched paths under the reserved prefixes retain the pre-existing auth
    // posture. With a valid bearer they are ordinary 404s, never the shell.
    for uri in ["/api/does-not-exist", "/api", "/ws/does-not-exist", "/ws"] {
        let unauthenticated = get(app(&dist), uri).await;
        assert_eq!(
            unauthenticated.status,
            StatusCode::UNAUTHORIZED,
            "{uri} unauthenticated status"
        );
        let res = send(app(&dist), Method::GET, uri, Some(TOKEN)).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{uri} status");
        assert!(
            !res.body.contains(INDEX_MARKER),
            "{uri} must not be the SPA"
        );
    }

    // A real WS route without an upgrade is rejected by the WS handler; it must
    // never be swallowed by the static fallback.
    let ws = get(app(&dist), "/ws/deliberate").await;
    assert_ne!(ws.status, StatusCode::OK);
    assert!(!ws.body.contains(INDEX_MARKER));

    // Paths that merely start with the same letters are ordinary SPA routes.
    let apiary = get(app(&dist), "/apiary").await;
    assert_eq!(apiary.status, StatusCode::OK);
    assert!(apiary.body.contains(INDEX_MARKER));
}

#[tokio::test]
async fn traversal_and_encoded_traversal_fail_closed() {
    let dist = fixture("traversal");

    for uri in [
        "/../secret.txt",
        "/_next/../../secret.txt",
        "/%2e%2e/secret.txt",
        "/%2E%2E/secret.txt",
        "/_next/%2e%2e/%2e%2e/secret.txt",
        "/%2e%2e%2fsecret.txt",
        "/%252e%252e/secret.txt",
        "/.env",
    ] {
        let res = get(app(&dist), uri).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{uri} status");
        assert!(!res.body.contains(SECRET_MARKER), "{uri} leaked the secret");
        assert!(
            !res.body.contains(INDEX_MARKER),
            "{uri} must not be the SPA"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlinks_out_of_the_export_root_fail_closed() {
    let dist = fixture("symlink");
    std::os::unix::fs::symlink(dist.join("../secret.txt"), dist.join("escape.txt")).unwrap();

    let res = get(app(&dist), "/escape.txt").await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    assert!(!res.body.contains(SECRET_MARKER));
}

#[tokio::test]
async fn static_responses_carry_the_browser_security_headers() {
    let dist = fixture("headers");

    // Applied to a served file, to the SPA fallback, and to a 404 alike.
    for uri in [
        "/",
        "/unknown/route",
        "/_next/static/chunk.js",
        "/_next/static/missing.js",
    ] {
        let res = get(app(&dist), uri).await;
        assert_eq!(res.header("x-content-type-options"), "nosniff", "{uri}");
        assert_eq!(res.header("x-frame-options"), "DENY", "{uri}");
        assert_eq!(res.header("referrer-policy"), "no-referrer", "{uri}");
        assert_eq!(
            res.header("permissions-policy"),
            "camera=(), microphone=(), geolocation=(), payment=()",
            "{uri}"
        );

        let csp = res.header("content-security-policy");
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
            assert!(csp.contains(directive), "{uri} missing {directive}: {csp}");
        }
        // The shell must still be able to reach this Council's own API/WS.
        assert!(csp.contains("connect-src 'self'"), "{uri} connect-src");
        assert!(csp.contains("ws://127.0.0.1:8765"), "{uri} ws origin");
        // The dev-server eval escape never reaches a built export.
        assert!(!csp.contains("unsafe-eval"), "{uri} unsafe-eval");
    }
}

#[tokio::test]
async fn non_read_methods_are_rejected_by_the_static_fallback() {
    let dist = fixture("methods");
    let res = send(app(&dist), Method::POST, "/some/spa/route", None).await;
    assert_eq!(res.status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(!res.body.contains(INDEX_MARKER));
}

#[tokio::test]
async fn without_web_dist_behavior_is_unchanged() {
    init_env();
    // No fallback is installed, so an unmatched path still goes through the
    // auth layer exactly as it did before `--web-dist` existed.
    let res = get(server::router(config()), "/does/not/exist").await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    assert!(res.header("content-security-policy").is_empty());
}
