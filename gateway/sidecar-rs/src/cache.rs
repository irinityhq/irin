use async_trait::async_trait;
use redis::AsyncCommands;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::time::{timeout, Duration};
use tracing::{debug, error, info};

const REDIS_OP_TIMEOUT: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// Public constants — the gateway contract pins these. Any change here
// requires a matching change in COUNCIL_GATEWAY_CONTRACT.md AND the
// `make contract-check` target validates they agree.
// ---------------------------------------------------------------------------

/// Cache key prefix. Bump on any change to the on-the-wire cache shape:
///   v1 → v2: spine refactor — raw-bytes hashing replaces re-encoded JSON.
///   v2 → v3: cache-shape fix (B1) — entries now carry `provider` and
///            `translator_version` so cache hits can re-run translate_response
///            and emit the correct wire shape per provider.
///   v3 → v4: `gemini-3.1-pro-preview` and
///            `gemini-3-flash-preview` migrated provider:vertex →
///            provider:gemini-cli, and 6 grok-4.x models flipped path
///            /v1/responses → /v1/chat/completions. Cache key is
///            (alias, raw_body); aliases unchanged, but pre-migration
///            entries may have empty/normalized-wrong bodies (response-
///            shape passthrough bug). Bumping the prefix evicts them.
///   v4 → v5: Gemini aliases migrated provider:gemini-cli →
///            provider:vertex. Alias/raw-body keys are unchanged, but cached
///            proxy responses have a different native response shape and
///            usage parser lineage than Vertex generateContent responses.
pub const CACHE_KEY_PREFIX: &str = "gateway:cache:v5:";

// ---------------------------------------------------------------------------
// Cache Configuration & Traits
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CacheEntry {
    /// The NATIVE upstream response body, as the provider returned it.
    /// On cache hit the gateway re-runs translate_response over this value
    /// using `provider`, so the wire shape sent to the client matches what
    /// a fresh request would have produced. Storing native preserves
    /// provider-specific fields (Anthropic stop_reason, Vertex
    /// safetyRatings, Vertex groundingMetadata, etc.) for forensics.
    pub response: serde_json::Value,
    /// The provider that produced `response`. Used by the cache-hit path
    /// to pick the right translator. Required — entries without a provider
    /// cannot be replayed safely.
    pub provider: String,
    /// Version of the Lua translator pipeline that produced this entry's
    /// expected shape. Bumped whenever lua/translator.lua changes wire
    /// shape. Cache hits with a mismatched version are treated as misses
    /// — cheap insurance against silent translator drift.
    pub translator_version: u32,
    pub cached_at: u64,
    pub ttl_secs: u64,
}

#[async_trait]
pub trait CacheBackend: Send + Sync {
    async fn get(&self, key: &str) -> Option<CacheEntry>;
    async fn set(&self, key: &str, entry: CacheEntry) -> Result<(), String>;
}

// ---------------------------------------------------------------------------
// Moka (Local In-Memory) Implementation
// ---------------------------------------------------------------------------

pub struct LocalCache {
    cache: moka::future::Cache<String, CacheEntry>,
}

impl LocalCache {
    pub fn new(max_capacity: u64, time_to_live: Duration) -> Self {
        let cache = moka::future::Cache::builder()
            .max_capacity(max_capacity)
            .time_to_live(time_to_live)
            .build();
        Self { cache }
    }
}

#[async_trait]
impl CacheBackend for LocalCache {
    async fn get(&self, key: &str) -> Option<CacheEntry> {
        self.cache.get(key).await
    }

    async fn set(&self, key: &str, entry: CacheEntry) -> Result<(), String> {
        self.cache.insert(key.to_string(), entry).await;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Redis (Distributed) Implementation
// ---------------------------------------------------------------------------

pub struct RedisCache {
    client: redis::Client,
}

impl RedisCache {
    pub fn new(redis_url: &str) -> Result<Self, String> {
        let client = redis::Client::open(redis_url).map_err(|e| e.to_string())?;
        Ok(Self { client })
    }
}

#[async_trait]
impl CacheBackend for RedisCache {
    async fn get(&self, key: &str) -> Option<CacheEntry> {
        let mut con = match timeout(
            REDIS_OP_TIMEOUT,
            self.client.get_multiplexed_async_connection(),
        )
        .await
        {
            Ok(Ok(con)) => con,
            Ok(Err(e)) => {
                error!("Redis connection error: {}", e);
                return None;
            }
            Err(_) => {
                error!("Redis connection timed out");
                return None;
            }
        };

        let result: redis::RedisResult<Option<String>> =
            match timeout(REDIS_OP_TIMEOUT, con.get(key)).await {
                Ok(result) => result,
                Err(_) => {
                    error!("Redis get timed out");
                    return None;
                }
            };
        match result {
            Ok(Some(data)) => serde_json::from_str(&data).ok(),
            Ok(None) => None,
            Err(e) => {
                error!("Redis get error: {}", e);
                None
            }
        }
    }

    async fn set(&self, key: &str, entry: CacheEntry) -> Result<(), String> {
        let con = match timeout(
            REDIS_OP_TIMEOUT,
            self.client.get_multiplexed_async_connection(),
        )
        .await
        {
            Ok(con) => con,
            Err(e) => {
                return Err(format!("Redis connection timed out: {}", e));
            }
        };
        let mut con = con.map_err(|e| e.to_string())?;

        let serialized = serde_json::to_string(&entry).map_err(|e| e.to_string())?;

        let _: () = timeout(
            REDIS_OP_TIMEOUT,
            con.set_ex(key, serialized, entry.ttl_secs),
        )
        .await
        .map_err(|_| "Redis set timed out".to_string())?
        .map_err(|e| e.to_string())?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SQLite (Durable) Implementation — GATEWAY_DURABLE=1
// ---------------------------------------------------------------------------

pub struct SqliteCache {
    conn: tokio_rusqlite::Connection,
}

impl SqliteCache {
    pub async fn new(db_path: &str) -> Result<Self, String> {
        let conn = tokio_rusqlite::Connection::open(db_path)
            .await
            .map_err(|e| format!("SQLite open: {}", e))?;

        conn.call(|c| {
            c.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=NORMAL;
                 PRAGMA busy_timeout=5000;
                 PRAGMA foreign_keys=ON;
                 CREATE TABLE IF NOT EXISTS cache_entries (
                     key TEXT PRIMARY KEY,
                     response TEXT NOT NULL,
                     provider TEXT NOT NULL,
                     translator_version INTEGER NOT NULL,
                     cached_at INTEGER NOT NULL,
                     ttl_secs INTEGER NOT NULL
                 );",
            )?;
            Ok::<_, rusqlite::Error>(())
        })
        .await
        .map_err(|e| format!("SQLite init: {}", e))?;

        info!("SQLite cache enabled at {}", db_path);
        Ok(Self { conn })
    }
}

#[async_trait]
impl CacheBackend for SqliteCache {
    async fn get(&self, key: &str) -> Option<CacheEntry> {
        let key = key.to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        self.conn
            .call(move |c| {
                let mut stmt = c.prepare(
                    "SELECT response, provider, translator_version, cached_at, ttl_secs
                 FROM cache_entries WHERE key = ?1",
                )?;
                let entry = stmt
                    .query_row(rusqlite::params![key], |row| {
                        let cached_at: u64 = row.get(3)?;
                        let ttl_secs: u64 = row.get(4)?;
                        Ok(CacheEntry {
                            response: serde_json::from_str(row.get::<_, String>(0)?.as_str())
                                .unwrap_or(serde_json::Value::Null),
                            provider: row.get(1)?,
                            translator_version: row.get(2)?,
                            cached_at,
                            ttl_secs,
                        })
                    })
                    .optional()?;

                if let Some(ref e) = entry {
                    if now > e.cached_at + e.ttl_secs {
                        let _ = c.execute(
                            "DELETE FROM cache_entries WHERE key = ?1",
                            rusqlite::params![key],
                        );
                        return Ok::<_, rusqlite::Error>(None);
                    }
                }
                Ok(entry)
            })
            .await
            .unwrap_or(None)
    }

    async fn set(&self, key: &str, entry: CacheEntry) -> Result<(), String> {
        let key = key.to_string();
        let response = serde_json::to_string(&entry.response).map_err(|e| e.to_string())?;
        let provider = entry.provider;
        let tv = entry.translator_version;
        let cached_at = entry.cached_at;
        let ttl = entry.ttl_secs;

        self.conn
            .call(move |c| {
                c.execute(
                    "INSERT OR REPLACE INTO cache_entries
                 (key, response, provider, translator_version, cached_at, ttl_secs)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![key, response, provider, tv, cached_at, ttl],
                )?;
                Ok::<_, rusqlite::Error>(())
            })
            .await
            .map_err(|e| format!("SQLite set: {}", e))
    }
}

// ---------------------------------------------------------------------------
// Hybrid / Gateway Cache Manager
// ---------------------------------------------------------------------------

pub struct GatewayCache {
    local: Arc<LocalCache>,
    sqlite: Option<Arc<SqliteCache>>,
    redis: Option<Arc<RedisCache>>,
}

impl GatewayCache {
    pub fn new(redis_url: Option<String>) -> Self {
        let local = Arc::new(LocalCache::new(10_000, Duration::from_secs(3600)));

        let redis = redis_url.and_then(|url| match RedisCache::new(&url) {
            Ok(r) => {
                info!("Redis cache enabled.");
                Some(Arc::new(r))
            }
            Err(e) => {
                error!(
                    "Failed to initialize Redis: {}. Falling back to local only.",
                    e
                );
                None
            }
        });

        Self {
            local,
            sqlite: None,
            redis,
        }
    }

    pub fn with_sqlite(mut self, sqlite: Arc<SqliteCache>) -> Self {
        self.sqlite = Some(sqlite);
        self
    }

    /// Generate a deterministic cache key from the client alias and the raw request body bytes.
    ///
    /// Hashing the literal request bytes (not a re-encoded JSON form) is intentional:
    /// JSON canonicalization differs between cjson (Lua) and serde_json (Rust), which
    /// would otherwise cause the same prompt to hash to different keys depending on
    /// the round-trip path. The alias is the client-supplied model name (e.g. "opus")
    /// not the resolved id (e.g. "claude-opus-4-7") — so two different aliases that
    /// resolve to the same model are correctly cached separately.
    ///
    /// Prefix v2 introduced raw-byte hashing; v3 added provider and
    /// translator version fields so cache hits can
    /// re-run translate_response and emit the correct wire shape.
    pub fn generate_cache_key(alias: &str, raw_body: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(alias.as_bytes());
        hasher.update(b"\0");
        hasher.update(raw_body.as_bytes());
        let result = hasher.finalize();
        format!("{}{}", CACHE_KEY_PREFIX, hex::encode(result))
    }

    pub async fn get(&self, key: &str) -> Option<CacheEntry> {
        // L1: Moka in-memory
        if let Some(entry) = self.local.get(key).await {
            debug!("L1 cache hit for key: {}", key);
            return Some(entry);
        }

        // L2: SQLite durable (if enabled)
        if let Some(sqlite) = &self.sqlite {
            if let Some(entry) = sqlite.get(key).await {
                debug!("L2-sqlite cache hit for key: {}", key);
                let _ = self.local.set(key, entry.clone()).await;
                return Some(entry);
            }
        }

        // L3: Redis distributed (if enabled)
        if let Some(redis) = &self.redis {
            if let Some(entry) = redis.get(key).await {
                debug!("L3-redis cache hit for key: {}", key);
                let _ = self.local.set(key, entry.clone()).await;
                return Some(entry);
            }
        }

        None
    }

    pub async fn set(
        &self,
        key: &str,
        response: serde_json::Value,
        provider: String,
        translator_version: u32,
        ttl_secs: u64,
    ) {
        let entry = CacheEntry {
            response,
            provider,
            translator_version,
            cached_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            ttl_secs,
        };

        let _ = self.local.set(key, entry.clone()).await;

        if let Some(sqlite) = &self.sqlite {
            if let Err(e) = sqlite.set(key, entry.clone()).await {
                error!("Failed to write to SQLite cache: {}", e);
            }
        }

        if let Some(redis) = &self.redis {
            if let Err(e) = redis.set(key, entry).await {
                error!("Failed to write to Redis cache: {}", e);
            }
        }
    }
}

// ---------------------------------------------------------------------
// Redis 1.x degradation tests (added for redis 0.26→1.x wave)
// Prove RedisCache degrades safely on connection failure under 1.2.x.
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    async fn stalling_redis_url() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let _socket = socket;
                    tokio::time::sleep(Duration::from_secs(30)).await;
                });
            }
        });
        (format!("redis://{}", addr), handle)
    }

    #[tokio::test]
    async fn redis_cache_unavailable_returns_none_no_panic() {
        let cache = match RedisCache::new("redis://127.0.0.1:1") {
            Ok(c) => c,
            Err(_) => return, // construction may fail; that's also safe
        };

        let result = timeout(
            Duration::from_secs(2),
            cache.get("nonexistent-key-under-bad-redis"),
        )
        .await
        .expect("RedisCache get must complete promptly");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn redis_cache_slow_path_bounded() {
        let (url, server) = stalling_redis_url().await;
        let cache = match RedisCache::new(&url) {
            Ok(c) => c,
            Err(_) => return,
        };

        let start = std::time::Instant::now();
        let result = timeout(Duration::from_secs(2), cache.get("slow"))
            .await
            .expect("RedisCache get must use bounded Redis timeout");
        server.abort();
        let elapsed = start.elapsed();

        assert!(result.is_none());
        assert!(
            elapsed < Duration::from_secs(3),
            "RedisCache get must not hang forever on bad host"
        );
    }

    #[tokio::test]
    async fn redis_cache_set_slow_path_bounded() {
        let (url, server) = stalling_redis_url().await;
        let cache = match RedisCache::new(&url) {
            Ok(c) => c,
            Err(_) => return,
        };
        let entry = CacheEntry {
            response: serde_json::json!({"ok": true}),
            provider: "test".to_string(),
            translator_version: 1,
            cached_at: 0,
            ttl_secs: 60,
        };

        let start = std::time::Instant::now();
        let result = timeout(Duration::from_secs(2), cache.set("slow-set", entry))
            .await
            .expect("RedisCache set must use bounded Redis timeout");
        server.abort();
        let elapsed = start.elapsed();

        assert!(result.is_err());
        assert!(
            elapsed < Duration::from_secs(3),
            "RedisCache set must not hang forever on bad host"
        );
    }
}
