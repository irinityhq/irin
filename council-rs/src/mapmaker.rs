//! Mapmaker — auto-scan codebase into compressed context
//!
//! Port of Python's _gather_map_context. Uses `git ls-files` (respects
//! .gitignore) with a fallback recursive glob. Compresses to ~400KB
//! token budget. Sorted smallest-first to maximize file count.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Code extensions to include in map scans (matches Python's _MAP_EXTENSIONS).
const MAP_EXTENSIONS: &[&str] = &[
    "py", "js", "ts", "tsx", "jsx", "go", "rs", "rb", "java", "c", "h", "cpp", "hpp", "swift",
    "kt", "sh", "sql", "md", "yaml", "yml", "toml", "json",
];

/// Max bytes to include (~400KB ≈ ~100K tokens).
const MAP_MAX_BYTES: usize = 400_000;

/// Noise directories to skip in fallback glob mode.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "__pycache__",
    ".egg-info",
    "venv",
    ".venv",
    "dist",
    "build",
    "target",
];

/// Gather map context from a directory. Returns the concatenated file
/// contents with path headers, truncated to budget.
pub fn gather(dir_path: &str, quiet: bool) -> String {
    let target = match std::fs::canonicalize(dir_path) {
        Ok(p) => p,
        Err(_) => {
            eprintln!("⚠️  --map: {} is not a valid directory", dir_path);
            return String::new();
        }
    };
    if !target.is_dir() {
        eprintln!("⚠️  --map: {} is not a directory", dir_path);
        return String::new();
    }

    // Try git ls-files first (respects .gitignore)
    let mut files = git_ls_files(&target);

    // Fallback: recursive walk
    if files.is_empty() {
        files = recursive_glob(&target);
    }

    // Sort by size (smallest first — more files in budget)
    files.sort_by_key(|f| std::fs::metadata(f).map(|m| m.len()).unwrap_or(u64::MAX));

    // Build context within budget
    let mut parts = Vec::new();
    let mut total_bytes: usize = 0;
    let mut included: usize = 0;

    for fp in &files {
        let content = match std::fs::read_to_string(fp) {
            Ok(c) => c,
            Err(_) => continue, // Skip binary / unreadable files
        };
        let rel = fp.strip_prefix(&target).unwrap_or(fp);
        let entry = format!("--- {} ---\n{}\n", rel.display(), content);
        let entry_bytes = entry.len();

        if total_bytes + entry_bytes > MAP_MAX_BYTES {
            break;
        }

        parts.push(entry);
        total_bytes += entry_bytes;
        included += 1;
    }

    if !quiet {
        eprintln!(
            "🗺️  Map: {}/{} files from {} ({} bytes)",
            included,
            files.len(),
            dir_path,
            total_bytes
        );
    }

    parts.join("\n")
}

/// Use `git ls-files` to find tracked + untracked-but-not-ignored files.
fn git_ls_files(target: &Path) -> Vec<PathBuf> {
    let output = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(target)
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| target.join(line))
        .filter(|p| has_map_extension(p) && p.is_file())
        .collect()
}

/// Recursive directory walk, skipping noise directories.
fn recursive_glob(target: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    walk_dir(target, &mut files);
    files
}

fn walk_dir(dir: &Path, files: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if SKIP_DIRS.iter().any(|s| name_str == *s) {
                continue;
            }
            walk_dir(&path, files);
        } else if path.is_file() && has_map_extension(&path) {
            files.push(path);
        }
    }
}

/// Check if a path has one of the recognized code extensions.
fn has_map_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| MAP_EXTENSIONS.contains(&ext))
}
