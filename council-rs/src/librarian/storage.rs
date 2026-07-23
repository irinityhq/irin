//! JSONL append-only chat storage.
//!
//! - Chat dir is configurable per `Store`. Default `<cwd>/librarian_chats/`.
//! - Chat ID:  `^lib_\d{4}-\d{2}-\d{2}_[a-z0-9]{6}$` (UTC date + 6 hex).
//! - Schema v1. First line is `{"type":"meta", ...}`; subsequent lines are
//!   `user`, `assistant`, or `meta_update` events.
//! - 2 MB hard cap per chat. Append fails with `ChatTooLarge` once exceeded.
//! - Torn final record on read → skip and stop (crash-safety).

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use chrono::{DateTime, SecondsFormat, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_CHAT_BYTES: u64 = 2 * 1024 * 1024;
pub const CHAT_DIR_DEFAULT: &str = "librarian_chats";

fn id_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^lib_\d{4}-\d{2}-\d{2}_[a-z0-9]{6}$").unwrap())
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("invalid chat id: {0}")]
    InvalidChatId(String),
    #[error("chat not found: {0}")]
    NotFound(String),
    #[error("unsupported schema: {0}")]
    UnsupportedSchema(String),
    #[error("chat too large: {0}")]
    ChatTooLarge(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSummary {
    pub id: String,
    pub title: String,
    pub cabinet: String,
    pub updated_at: String,
    pub ask_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub cabinet: String,
    pub created_at: String,
    pub title: String,
    pub schema_version: u32,
    pub updated_at: String,
    pub messages: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct Store {
    pub chat_dir: PathBuf,
    pub max_bytes: u64,
}

impl Store {
    pub fn new<P: Into<PathBuf>>(chat_dir: P) -> Self {
        Self {
            chat_dir: chat_dir.into(),
            max_bytes: MAX_CHAT_BYTES,
        }
    }

    pub fn from_env() -> Self {
        let dir = std::env::var("LIBRARIAN_CHAT_DIR").unwrap_or_else(|_| CHAT_DIR_DEFAULT.into());
        Self::new(dir)
    }

    fn resolve_path(&self, chat_id: &str) -> Result<PathBuf, StorageError> {
        if !id_re().is_match(chat_id) {
            return Err(StorageError::InvalidChatId(chat_id.to_string()));
        }
        let candidate = self.chat_dir.join(format!("{chat_id}.jsonl"));
        // Belt + suspenders: ensure parent matches chat_dir after canonicalisation
        // when both exist; if chat_dir hasn't been created yet, the regex check
        // is sufficient (no separator can appear in chat_id).
        if let (Ok(c), Ok(d)) = (candidate.canonicalize(), self.chat_dir.canonicalize())
            && c.parent() != Some(d.as_path())
        {
            return Err(StorageError::InvalidChatId(chat_id.to_string()));
        }
        Ok(candidate)
    }

    pub fn create_chat(&self, cabinet: &str) -> Result<String, StorageError> {
        fs::create_dir_all(&self.chat_dir)?;
        let mut chat_id = new_chat_id();
        let mut path = self.resolve_path(&chat_id)?;
        if path.exists() {
            chat_id = new_chat_id();
            path = self.resolve_path(&chat_id)?;
        }
        let meta = json!({
            "type": "meta",
            "schema_version": SCHEMA_VERSION,
            "id": chat_id,
            "cabinet": cabinet,
            "created_at": now_iso(),
            "title": "",
        });
        let mut f = fs::File::create(&path)?;
        writeln!(f, "{}", serde_json::to_string(&meta)?)?;
        Ok(chat_id)
    }

    pub fn append_event(&self, chat_id: &str, event: &Value) -> Result<(), StorageError> {
        let path = self.resolve_path(chat_id)?;
        if !path.exists() {
            return Err(StorageError::NotFound(chat_id.to_string()));
        }
        let mut encoded = serde_json::to_string(event)?;
        encoded.push('\n');
        let cur_size = fs::metadata(&path)?.len();
        if cur_size + encoded.len() as u64 > self.max_bytes {
            return Err(StorageError::ChatTooLarge(chat_id.to_string()));
        }
        let mut f = fs::OpenOptions::new().append(true).open(&path)?;
        f.write_all(encoded.as_bytes())?;
        Ok(())
    }

    pub fn load_chat(&self, chat_id: &str) -> Result<Conversation, StorageError> {
        let path = self.resolve_path(chat_id)?;
        if !path.exists() {
            return Err(StorageError::NotFound(chat_id.to_string()));
        }
        let f = fs::File::open(&path)?;
        let mut reader = BufReader::new(f);

        let mut first = String::new();
        reader.read_line(&mut first)?;
        let meta: Value = serde_json::from_str(first.trim_end())
            .map_err(|e| StorageError::UnsupportedSchema(format!("meta line unparseable: {e}")))?;
        if meta.get("type").and_then(|v| v.as_str()) != Some("meta") {
            return Err(StorageError::UnsupportedSchema(
                "first line is not meta".into(),
            ));
        }
        let schema_version = meta
            .get("schema_version")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if schema_version != SCHEMA_VERSION as u64 {
            return Err(StorageError::UnsupportedSchema(format!(
                "version {schema_version}"
            )));
        }

        let mut convo = Conversation {
            id: chat_id.to_string(),
            cabinet: meta
                .get("cabinet")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            created_at: meta
                .get("created_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            title: meta
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            schema_version: schema_version as u32,
            updated_at: String::new(),
            messages: Vec::new(),
        };

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if line.is_empty() {
                continue;
            }
            let event: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => break, // torn final record
            };
            match event.get("type").and_then(|v| v.as_str()) {
                Some("meta_update") => {
                    if let Some(t) = event.get("title").and_then(|v| v.as_str()) {
                        convo.title = t.to_string();
                    }
                    if let Some(c) = event.get("cabinet").and_then(|v| v.as_str()) {
                        convo.cabinet = c.to_string();
                    }
                }
                Some("user") | Some("assistant") => convo.messages.push(event),
                _ => {}
            }
        }

        let mtime = fs::metadata(&path)?.modified()?;
        convo.updated_at = mtime_to_iso(mtime);
        Ok(convo)
    }

    pub fn list_chats(&self) -> Vec<ChatSummary> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(&self.chat_dir) {
            Ok(e) => e,
            Err(_) => return out,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let name = match p.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if !name.starts_with("lib_") || !name.ends_with(".jsonl") {
                continue;
            }
            if let Some(s) = summarize(&p) {
                out.push(s);
            }
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        out
    }

    pub fn delete_chat(&self, chat_id: &str) -> Result<(), StorageError> {
        let path = self.resolve_path(chat_id)?;
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }
}

fn summarize(path: &Path) -> Option<ChatSummary> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();
    let first = lines.next()?.ok()?;
    let meta: Value = serde_json::from_str(&first).ok()?;
    let id = meta.get("id").and_then(|v| v.as_str())?.to_string();
    let cabinet = meta.get("cabinet").and_then(|v| v.as_str())?.to_string();
    let mut current_title = meta
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut ask_count: u64 = 0;
    for line in lines.map_while(Result::ok) {
        if line.is_empty() {
            continue;
        }
        let ev: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => break,
        };
        match ev.get("type").and_then(|v| v.as_str()) {
            Some("user") => ask_count += 1,
            Some("meta_update") => {
                if let Some(t) = ev.get("title").and_then(|v| v.as_str()) {
                    current_title = t.to_string();
                }
            }
            _ => {}
        }
    }
    let mtime = fs::metadata(path).ok()?.modified().ok()?;
    Some(ChatSummary {
        id,
        title: current_title,
        cabinet,
        updated_at: mtime_to_iso(mtime),
        ask_count,
    })
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn mtime_to_iso(t: std::time::SystemTime) -> String {
    let dt: DateTime<Utc> = t.into();
    dt.to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn new_chat_id() -> String {
    let today = Utc::now().format("%Y-%m-%d");
    // 6-char lowercase hex from a uuid v4 (no security implications — collision
    // resistance comes from the date prefix + retry in create_chat).
    let suffix: String = Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(6)
        .collect();
    format!("lib_{today}_{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let unique = Uuid::new_v4().simple();
        let p = std::env::temp_dir().join(format!("librarian_test_{nanos}_{unique}"));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn resolve_rejects_traversal_and_bad_shape() {
        let s = Store::new(tmp_dir());
        for bad in [
            "../etc/passwd",
            "/lib_2026-05-10_abc123",
            "lib_X-Y-Z_abc123",
            "lib_2026-05-10_abc",
            "lib_2026-05-10_ABCDEF",
            "evil.jsonl",
            "lib_2026-05-10_abc123.jsonl",
        ] {
            assert!(matches!(
                s.resolve_path(bad),
                Err(StorageError::InvalidChatId(_))
            ));
        }
    }

    #[test]
    fn create_chat_writes_meta_line() {
        let s = Store::new(tmp_dir());
        let id = s.create_chat("research-default").unwrap();
        let path = s.chat_dir.join(format!("{id}.jsonl"));
        let body = fs::read_to_string(&path).unwrap();
        let first: Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(first["type"], "meta");
        assert_eq!(first["schema_version"], 1);
        assert_eq!(first["cabinet"], "research-default");
        assert_eq!(first["title"], "");
        assert_eq!(first["id"], id);
    }

    #[test]
    fn append_event_round_trip() {
        let s = Store::new(tmp_dir());
        let id = s.create_chat("research-default").unwrap();
        s.append_event(
            &id,
            &json!({
                "type": "user", "id": "u_001", "content": "hello",
                "ts": "2026-05-10T10:00:00Z", "client_msg_id": "c_xyz"
            }),
        )
        .unwrap();
        let convo = s.load_chat(&id).unwrap();
        assert_eq!(convo.cabinet, "research-default");
        assert_eq!(convo.title, "");
        assert_eq!(convo.messages.len(), 1);
        assert_eq!(convo.messages[0]["content"], "hello");
    }

    #[test]
    fn unsupported_schema_rejected() {
        let s = Store::new(tmp_dir());
        let id = "lib_2026-05-10_zzzzzz";
        let path = s.chat_dir.join(format!("{id}.jsonl"));
        fs::create_dir_all(&s.chat_dir).unwrap();
        fs::write(
            &path,
            r#"{"type":"meta","schema_version":99,"id":"lib_2026-05-10_zzzzzz","cabinet":"x","created_at":"2026-05-10T00:00:00Z","title":""}
"#,
        )
        .unwrap();
        assert!(matches!(
            s.load_chat(id),
            Err(StorageError::UnsupportedSchema(_))
        ));
    }

    #[test]
    fn torn_trailing_record_skipped() {
        let s = Store::new(tmp_dir());
        let id = s.create_chat("research-default").unwrap();
        let path = s.chat_dir.join(format!("{id}.jsonl"));
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","id":"u1","content":"ok","ts":"t","client_msg_id":"c"}}"#
        )
        .unwrap();
        write!(f, r#"{{"type":"user","id":"u2","content":"torn"#).unwrap();
        let convo = s.load_chat(&id).unwrap();
        assert_eq!(convo.messages.len(), 1);
        assert_eq!(convo.messages[0]["content"], "ok");
    }

    #[test]
    fn meta_update_applies() {
        let s = Store::new(tmp_dir());
        let id = s.create_chat("research-default").unwrap();
        s.append_event(
            &id,
            &json!({"type":"meta_update","title":"renamed","ts":"t"}),
        )
        .unwrap();
        let convo = s.load_chat(&id).unwrap();
        assert_eq!(convo.title, "renamed");
    }

    #[test]
    fn list_chats_newest_first() {
        let s = Store::new(tmp_dir());
        let a = s.create_chat("research-default").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = s.create_chat("research-default").unwrap();
        let chats = s.list_chats();
        let ids: Vec<_> = chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec![b.as_str(), a.as_str()]);
    }

    #[test]
    fn size_cap_enforced() {
        let mut s = Store::new(tmp_dir());
        s.max_bytes = 200;
        let id = s.create_chat("research-default").unwrap();
        let mut hit = false;
        for i in 0..50 {
            let r = s.append_event(
                &id,
                &json!({
                    "type":"user","id":format!("u_{i}"),
                    "content":"x".repeat(100),"ts":"t","client_msg_id":format!("c_{i}")
                }),
            );
            if matches!(r, Err(StorageError::ChatTooLarge(_))) {
                hit = true;
                break;
            }
        }
        assert!(hit);
    }

    #[test]
    fn delete_chat_removes_file() {
        let s = Store::new(tmp_dir());
        let id = s.create_chat("research-default").unwrap();
        s.delete_chat(&id).unwrap();
        assert!(!s.chat_dir.join(format!("{id}.jsonl")).exists());
    }
}
