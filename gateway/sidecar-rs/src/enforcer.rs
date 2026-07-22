// ==========================================================================
// enforcer.rs — Tool authorization + path sandboxing.
//
// Port of Python enforcer.py to Rust.
// Invariant: READ_ONLY compliance gate.
//   1. Tool allowlist — reject unknown tools
//   2. Path sandboxing — canonicalize, symlink-safe
//   3. Network scheme blocking — no http/https/tcp/file
//   4. Size caps — reject reads > configured max
// ==========================================================================

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::{env, fs};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

fn default_data_dir() -> PathBuf {
    PathBuf::from(env::var("GATEWAY_DATA_DIR").unwrap_or_else(|_| "/data".to_string()))
}

fn max_read_size() -> u64 {
    let mb: u64 = env::var("MAX_READ_SIZE_MB")
        .unwrap_or_else(|_| "5".to_string())
        .parse()
        .unwrap_or(5);
    mb * 1024 * 1024
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct EnforcementResult {
    pub allowed: bool,
    pub tool: String,
    pub violations: Vec<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
pub struct ViolationDetail {
    pub reason: String,
    pub tool: String,
    pub arg: String,
}

#[derive(Debug)]
pub struct ReadOnlyViolation {
    pub reason: String,
    pub tool: String,
    pub arg: String,
}

impl std::fmt::Display for ReadOnlyViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "READ_ONLY violation: {} (tool={}, arg={:?})",
            self.reason, self.tool, self.arg
        )
    }
}

impl std::error::Error for ReadOnlyViolation {}

impl ReadOnlyViolation {
    #[allow(dead_code)]
    pub fn to_detail(&self) -> ViolationDetail {
        ViolationDetail {
            reason: self.reason.clone(),
            tool: self.tool.clone(),
            arg: self.arg.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool allowlist
// ---------------------------------------------------------------------------

const READ_ONLY_TOOLS: &[&str] = &["fs.read", "fs.list", "sys.time"];

const BLOCKED_SCHEMES: &[&str] = &["http://", "https://", "tcp://", "file://"];

// ---------------------------------------------------------------------------
// Individual checks
// ---------------------------------------------------------------------------

fn check_tool(tool_name: &str) -> Result<(), ReadOnlyViolation> {
    if !READ_ONLY_TOOLS.contains(&tool_name) {
        return Err(ReadOnlyViolation {
            reason: format!("tool '{}' not in READ_ONLY allowlist", tool_name),
            tool: tool_name.to_string(),
            arg: String::new(),
        });
    }
    Ok(())
}

fn check_path(path_str: &str, data_dir: &Path) -> Result<(), ReadOnlyViolation> {
    // Null byte check
    if path_str.contains('\0') {
        return Err(ReadOnlyViolation {
            reason: "null byte in path".to_string(),
            tool: String::new(),
            arg: path_str.to_string(),
        });
    }

    // Traversal check
    let has_traversal = path_str.split('/').any(|seg| seg == "..")
        || path_str
            .split(std::path::MAIN_SEPARATOR)
            .any(|seg| seg == "..");
    if has_traversal {
        return Err(ReadOnlyViolation {
            reason: ".. traversal in path".to_string(),
            tool: String::new(),
            arg: path_str.to_string(),
        });
    }

    // Resolve symlinks and canonicalize
    let resolved = match fs::canonicalize(path_str) {
        Ok(p) => p,
        Err(_) => {
            // If file doesn't exist, use the path as-is for the check
            PathBuf::from(path_str)
        }
    };
    let data_dir_resolved = fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());

    if !resolved.starts_with(&data_dir_resolved) {
        return Err(ReadOnlyViolation {
            reason: format!(
                "path resolves outside data dir ({})",
                data_dir_resolved.display()
            ),
            tool: String::new(),
            arg: path_str.to_string(),
        });
    }

    Ok(())
}

fn check_no_network(value: &str) -> Result<(), ReadOnlyViolation> {
    let lower = value.to_lowercase();
    for scheme in BLOCKED_SCHEMES {
        if lower.contains(scheme) {
            return Err(ReadOnlyViolation {
                reason: format!("blocked network scheme: {}", scheme),
                tool: String::new(),
                arg: value.to_string(),
            });
        }
    }
    Ok(())
}

fn check_read_size(path_str: &str, max_bytes: u64) -> Result<(), ReadOnlyViolation> {
    match fs::metadata(path_str) {
        Ok(meta) => {
            if meta.len() > max_bytes {
                return Err(ReadOnlyViolation {
                    reason: format!("file size {} exceeds {} byte cap", meta.len(), max_bytes),
                    tool: String::new(),
                    arg: path_str.to_string(),
                });
            }
            Ok(())
        }
        Err(_) => Ok(()), // File doesn't exist — not our problem
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Full enforcement gate. Returns Ok(EnforcementResult) or Err(ReadOnlyViolation).
pub fn enforce(
    tool_name: &str,
    args: &serde_json::Map<String, serde_json::Value>,
    data_dir: Option<&Path>,
    max_read_bytes: Option<u64>,
) -> Result<EnforcementResult, ReadOnlyViolation> {
    check_tool(tool_name)?;

    let data_dir = data_dir.map(PathBuf::from).unwrap_or_else(default_data_dir);
    let max_bytes = max_read_bytes.unwrap_or_else(max_read_size);

    for (_key, val) in args.iter() {
        if let serde_json::Value::String(s) = val {
            check_no_network(s)?;
            // Only check path/size for values that aren't network schemes
            if !BLOCKED_SCHEMES
                .iter()
                .any(|sch| s.to_lowercase().starts_with(sch))
            {
                check_path(s, &data_dir)?;
                check_read_size(s, max_bytes)?;
            }
        }
    }

    Ok(EnforcementResult {
        allowed: true,
        tool: tool_name.to_string(),
        violations: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn empty_args() -> serde_json::Map<String, serde_json::Value> {
        serde_json::Map::new()
    }

    fn args_with(key: &str, val: &str) -> serde_json::Map<String, serde_json::Value> {
        let mut m = serde_json::Map::new();
        m.insert(key.to_string(), json!(val));
        m
    }

    #[test]
    fn allowed_tool() {
        let r = enforce("fs.read", &empty_args(), None, None);
        assert!(r.is_ok());
        assert!(r.unwrap().allowed);
    }

    #[test]
    fn blocked_tool() {
        let r = enforce("fs.write", &empty_args(), None, None);
        assert!(r.is_err());
        let e = r.unwrap_err();
        assert!(e.reason.contains("not in READ_ONLY allowlist"));
    }

    #[test]
    fn network_scheme_blocked() {
        let r = enforce("fs.read", &args_with("url", "https://evil.com"), None, None);
        assert!(r.is_err());
        let e = r.unwrap_err();
        assert!(e.reason.contains("blocked network scheme"));
    }

    #[test]
    fn null_byte_in_path() {
        let r = enforce(
            "fs.read",
            &args_with("path", "/data/foo\0bar"),
            Some(Path::new("/data")),
            None,
        );
        assert!(r.is_err());
        let e = r.unwrap_err();
        assert!(e.reason.contains("null byte"));
    }

    #[test]
    fn traversal_blocked() {
        let r = enforce(
            "fs.read",
            &args_with("path", "/data/../etc/passwd"),
            Some(Path::new("/data")),
            None,
        );
        assert!(r.is_err());
        let e = r.unwrap_err();
        assert!(e.reason.contains("traversal"));
    }
}
