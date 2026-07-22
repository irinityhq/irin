//! Mirror of warroom/backend/safe_map.py — workspace-aware directory preview.
//!
//! Uses `git ls-files` when available; falls back to walkdir-style recursion.
//! Filters out anything that looks sensitive (keys, secrets, private dirs).

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

const MAP_EXTENSIONS: &[&str] = &[
    ".py", ".rs", ".ts", ".tsx", ".js", ".jsx", ".go", ".java", ".kt", ".rb", ".php", ".swift",
    ".c", ".cpp", ".h", ".hpp", ".cs", ".md", ".txt", ".yaml", ".yml", ".toml", ".json", ".sh",
    ".sql", ".html", ".css", ".scss",
];

const MAP_MAX_BYTES: usize = 200_000;

const SENSITIVE_NAMES: &[&str] = &[
    ".env",
    ".env.local",
    ".envrc",
    ".netrc",
    ".npmrc",
    ".pypirc",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "id_rsa",
    "known_hosts",
];

const SENSITIVE_SUFFIXES: &[&str] = &[
    ".asc", ".cer", ".crt", ".der", ".key", ".p12", ".pem", ".pfx",
];

const SENSITIVE_PARTS: &[&str] = &[".aws", ".azure", ".gnupg", ".ssh"];

const SENSITIVE_MARKERS: &[&str] = &[
    "apikey",
    "api_key",
    "client_secret",
    "credential",
    "credentials",
    "private_key",
    "secret",
    "service-account",
    "token",
];

const SKIP_PARTS: &[&str] = &[
    "node_modules",
    ".git/",
    "__pycache__",
    ".egg-info",
    "venv/",
    ".venv/",
    "dist/",
    "build/",
    "target/",
];

fn looks_sensitive(rel: &Path) -> bool {
    let parts: Vec<String> = rel
        .iter()
        .filter_map(|p| p.to_str().map(|s| s.to_lowercase()))
        .collect();
    if parts.iter().any(|p| SENSITIVE_PARTS.contains(&p.as_str())) {
        return true;
    }
    let name = rel
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();
    if SENSITIVE_NAMES.contains(&name.as_str()) {
        return true;
    }
    if let Some(suffix) = rel.extension().and_then(|e| e.to_str()) {
        let dot_suffix = format!(".{}", suffix.to_lowercase());
        if SENSITIVE_SUFFIXES.contains(&dot_suffix.as_str()) {
            return true;
        }
    }
    let stem = rel
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    SENSITIVE_MARKERS.iter().any(|m| stem.contains(m))
}

fn has_map_extension(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let dot_ext = format!(".{}", ext);
    MAP_EXTENSIONS.contains(&dot_ext.as_str())
}

fn git_ls_files(target: &Path) -> Vec<PathBuf> {
    let out = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(target)
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };
    let stdout = match String::from_utf8(out.stdout) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| target.join(l))
        .filter(|p| p.is_file() && has_map_extension(p))
        .collect()
}

fn walk_fallback(target: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_recursive(target, target, &mut out);
    out
}

fn walk_recursive(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Never follow symlinks: prevents cycles and in-tree escape to content
        // outside the allowed root (the allowlist guards only the entry path).
        if entry.file_type().map(|t| t.is_symlink()).unwrap_or(true) {
            continue;
        }
        let rel_str = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        if SKIP_PARTS.iter().any(|s| rel_str.contains(s)) {
            continue;
        }
        if path.is_dir() {
            walk_recursive(root, &path, out);
        } else if has_map_extension(&path) {
            out.push(path);
        }
    }
}

fn candidate_files(target: &Path) -> Vec<PathBuf> {
    let mut files = git_ls_files(target);
    if files.is_empty() {
        files = walk_fallback(target);
    }
    // Canonical root for containment: every candidate's real path must stay inside
    // it. Backstops the symlink-skip in walk_recursive and contains the git
    // ls-files path (which can surface symlinked-to-outside files). Fail-closed:
    // a candidate that cannot be canonicalized (broken/dangling symlink) is dropped.
    let root = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf());
    files
        .into_iter()
        .filter(|p| {
            let rel = p.strip_prefix(target).unwrap_or(p);
            !looks_sensitive(rel)
        })
        .filter(|p| {
            p.canonicalize()
                .map(|real| real.starts_with(&root))
                .unwrap_or(false)
        })
        .collect()
}

fn gather_map_context(target: &Path) -> String {
    let mut files = candidate_files(target);
    files.sort_by_key(|p| p.metadata().map(|m| m.len()).unwrap_or(0));
    let mut out = String::new();
    let mut total_bytes = 0;
    for fp in &files {
        let content = match std::fs::read_to_string(fp) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let rel = fp.strip_prefix(target).unwrap_or(fp).to_string_lossy();
        let entry = format!("--- {} ---\n{}\n", rel, content);
        let bytes = entry.len();
        if total_bytes + bytes > MAP_MAX_BYTES {
            break;
        }
        out.push_str(&entry);
        out.push('\n');
        total_bytes += bytes;
    }
    out
}

/// Allowed workspace roots for Mapmaker operations.
///
/// From `COUNCIL_MAPMAKER_ROOTS` (comma-separated paths, `~`-expanded and
/// canonicalized). If unset/empty, defaults to the project root only — never
/// arbitrary filesystem roots. Entries that cannot be canonicalized are dropped.
fn allowed_roots() -> Vec<PathBuf> {
    let roots: Vec<PathBuf> = std::env::var("COUNCIL_MAPMAKER_ROOTS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .filter_map(shellexpand_simple)
        .collect();
    if !roots.is_empty() {
        return roots;
    }
    let root = super::project_root();
    vec![root.canonicalize().unwrap_or(root)]
}

/// Resolve a requested Mapmaker directory against the allowed roots.
///
/// Canonicalizes the request (resolving `..` and symlinks, requiring the path to
/// exist) and confirms it is a directory contained in one of `roots`. Fail-closed:
/// returns `Err` for traversal, symlink escape, missing dir, or any path outside
/// the allowlist. Split from `resolve_map_target` so the containment rule is unit
/// testable without mutating process env.
fn resolve_within(dir_path: &str, roots: &[PathBuf]) -> Result<PathBuf, String> {
    let target =
        shellexpand_simple(dir_path).ok_or_else(|| format!("Not a directory: {}", dir_path))?;
    if !target.is_dir() {
        return Err(format!("Not a directory: {}", dir_path));
    }
    if roots.iter().any(|r| target.starts_with(r)) {
        Ok(target)
    } else {
        Err(format!(
            "Path outside Mapmaker workspace allowlist: {}",
            target.to_string_lossy()
        ))
    }
}

/// Resolve and validate a Mapmaker target directory against `allowed_roots()`.
/// All provider-bound Mapmaker paths MUST go through this before scanning.
pub fn resolve_map_target(dir_path: &str) -> Result<PathBuf, String> {
    resolve_within(dir_path, &allowed_roots())
}

/// Gather full allowlisted map context for WS deliberation (same scan as preview).
pub fn gather_map_context_for_deliberation(dir_path: &str) -> Result<String, String> {
    let target = resolve_map_target(dir_path)?;
    let content = gather_map_context(&target);
    if content.trim().is_empty() {
        Err(format!(
            "No mappable files found under {} (check path and allowlist)",
            dir_path
        ))
    } else {
        Ok(content)
    }
}

pub fn gather_map_preview(dir_path: &str) -> Value {
    let target = match resolve_map_target(dir_path) {
        Ok(t) => t,
        Err(e) => return json!({"error": e}),
    };
    let content = gather_map_context(&target);
    let files: Vec<&str> = content
        .lines()
        .filter(|line| line.starts_with("--- ") && line.trim_end().ends_with(" ---"))
        .map(|line| {
            let line = line.trim_end();
            line.trim_start_matches("--- ")
                .trim_end_matches(" ---")
                .trim()
        })
        .collect();

    let preview_end = content.len().min(5000);
    json!({
        "directory": target.to_string_lossy(),
        "file_count": files.len(),
        "files": files.iter().take(200).collect::<Vec<_>>(),
        "total_bytes": content.len(),
        "preview": &content[..preview_end],
    })
}

fn shellexpand_simple(path: &str) -> Option<PathBuf> {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").ok()?;
        PathBuf::from(home).join(rest)
    } else if path == "~" {
        PathBuf::from(std::env::var("HOME").ok()?)
    } else {
        PathBuf::from(path)
    };
    expanded.canonicalize().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_base(tag: &str) -> PathBuf {
        let b = std::env::temp_dir().join(format!("council_rs_map_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&b);
        std::fs::create_dir_all(&b).unwrap();
        b.canonicalize().unwrap()
    }

    #[test]
    fn allows_path_inside_root() {
        let root = unique_base("inside");
        let sub = root.join("pkg/src");
        std::fs::create_dir_all(&sub).unwrap();
        let got = resolve_within(sub.to_str().unwrap(), std::slice::from_ref(&root));
        assert!(
            got.is_ok(),
            "subdir under an allowed root must resolve: {got:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_sibling_outside_root() {
        let base = unique_base("sibling");
        let root = base.join("allowed");
        let sibling = base.join("private");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let got = resolve_within(sibling.to_str().unwrap(), &[root]);
        assert!(
            got.is_err(),
            "a sibling dir outside the root must be rejected"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rejects_parent_traversal() {
        let base = unique_base("traverse");
        let root = base.join("allowed");
        std::fs::create_dir_all(&root).unwrap();
        // `root/..` canonicalizes to `base`, which sits outside `root`.
        let escaped = format!("{}/..", root.to_str().unwrap());
        let got = resolve_within(&escaped, &[root]);
        assert!(got.is_err(), "parent traversal must be rejected");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let base = unique_base("symlink");
        let root = base.join("allowed");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let link = root.join("escape");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        // canonicalize follows the link to `outside`, which is outside `root`.
        let got = resolve_within(link.to_str().unwrap(), &[root]);
        assert!(got.is_err(), "a symlink escaping the root must be rejected");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn walk_excludes_in_tree_symlink_escape() {
        let base = unique_base("walkescape");
        let root = base.join("repo");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
        // A secret living outside the allowed root...
        std::fs::write(outside.join("secret.rs"), "const KEY: &str = \"sk-zzz\";\n").unwrap();
        // ...reachable only via an in-tree symlink that hides its real name.
        std::os::unix::fs::symlink(outside.join("secret.rs"), root.join("notes.rs")).unwrap();

        let root_canon = root.canonicalize().unwrap();
        let names: Vec<String> = candidate_files(&root_canon)
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        assert!(
            names.iter().any(|n| n.ends_with("main.rs")),
            "legit in-tree file must be kept: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("secret.rs")),
            "a symlink escaping the root must not be scanned: {names:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn gather_map_context_for_deliberation_rejects_outside_allowlist() {
        let base = unique_base("ws_gather");
        let root = base.join("allowed");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("leak.rs"), "fn leak() {}\n").unwrap();
        let err = gather_map_context_for_deliberation(outside.to_str().unwrap())
            .expect_err("path outside allowlist must fail");
        assert!(
            err.contains("allowlist") || err.contains("Not a directory"),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn gather_map_context_for_deliberation_accepts_project_root() {
        let root = crate::warroom::project_root();
        let got = gather_map_context_for_deliberation(root.to_string_lossy().as_ref())
            .expect("project root must be allowlisted");
        assert!(
            !got.trim().is_empty(),
            "project root should yield map context"
        );
    }

    #[test]
    fn allowed_roots_defaults_to_project_root_when_unset() {
        // Asserts the fail-closed default without mutating process env.
        if std::env::var("COUNCIL_MAPMAKER_ROOTS").is_err() {
            assert_eq!(
                allowed_roots().len(),
                1,
                "with the env unset the allowlist is exactly the project root"
            );
        }
    }
}
