use anyhow::Result;
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info};

use super::{AiProvider, AiRequest, AiResponse, CacheStats, ProviderCapabilities};

pub fn fmt_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            result.push('.');
        }
        result.push(c);
    }
    result
}

pub struct CachingAiProvider {
    inner: Arc<dyn AiProvider>,
    conn: libsql::Connection,
    session_start: i64,
    hits_this: AtomicU64,
    hits_prev: AtomicU64,
    tokens_saved_this: AtomicU64,
    tokens_saved_prev: AtomicU64,
}

impl CachingAiProvider {
    pub async fn new(inner: Arc<dyn AiProvider>, cache_path: &str, ttl_days: u64) -> Result<Self> {
        let db = libsql::Builder::new_local(cache_path).build().await?;
        let conn = db.connect()?;

        let _ = conn
            .query("PRAGMA journal_mode=WAL;", ())
            .await?
            .next()
            .await;
        let _ = conn
            .query("PRAGMA busy_timeout = 5000;", ())
            .await?
            .next()
            .await;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS response_cache (
                request_hash TEXT PRIMARY KEY,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                request_json TEXT NOT NULL,
                response_json TEXT NOT NULL,
                tokens_saved INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            );",
        )
        .await?;

        let cutoff = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
            - ttl_days as i64 * 86400;
        let result = conn
            .execute(
                "DELETE FROM response_cache WHERE created_at < ?",
                libsql::params![cutoff],
            )
            .await;
        if let Ok(reaped) = result
            && reaped > 0
        {
            info!(
                "Response cache: reaped {} expired entries (>{} days old)",
                reaped, ttl_days
            );
        }

        let session_start = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        info!("Response cache enabled ({})", cache_path);

        Ok(Self {
            inner,
            conn,
            session_start,
            hits_this: AtomicU64::new(0),
            hits_prev: AtomicU64::new(0),
            tokens_saved_this: AtomicU64::new(0),
            tokens_saved_prev: AtomicU64::new(0),
        })
    }

    fn compute_cache_key(&self, request: &AiRequest) -> String {
        let mut val = serde_json::to_value(request).unwrap_or_default();
        // Strip nondeterministic fields
        if let serde_json::Value::Object(ref mut map) = val {
            map.remove("context_tag");
        }
        super::scrub_thought_signatures(&mut val);
        let canonical = serde_json::to_string(&val).unwrap_or_default();
        // The model and the provider's own knobs never appear in the request,
        // so hash them alongside it. Without them a raised reasoning effort
        // replays the answer recorded at the lower one.
        let mut hasher = Sha256::new();
        hasher.update(self.inner.cache_identity().as_bytes());
        hasher.update(b"\0");
        hasher.update(canonical.as_bytes());
        let hash = hasher.finalize();
        hash.iter().map(|b| format!("{:02x}", b)).collect()
    }
}

#[async_trait]
impl AiProvider for CachingAiProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let hash = self.compute_cache_key(&request);
        let hash_prefix = &hash[..12];

        let mut rows = self
            .conn
            .query(
                "SELECT response_json, tokens_saved, created_at FROM response_cache WHERE request_hash = ?",
                libsql::params![hash.clone()],
            )
            .await?;

        if let Some(row) = rows.next().await? {
            let response_json: String = row.get(0)?;
            let tokens_saved: i64 = row.get(1)?;
            let created_at: i64 = row.get(2)?;
            if let Ok(mut resp) = serde_json::from_str::<AiResponse>(&response_json) {
                let (origin, total) = if created_at >= self.session_start {
                    self.hits_this.fetch_add(1, Ordering::Relaxed);
                    let t = self
                        .tokens_saved_this
                        .fetch_add(tokens_saved as u64, Ordering::Relaxed)
                        + tokens_saved as u64;
                    ("this session", t)
                } else {
                    self.hits_prev.fetch_add(1, Ordering::Relaxed);
                    let t = self
                        .tokens_saved_prev
                        .fetch_add(tokens_saved as u64, Ordering::Relaxed)
                        + tokens_saved as u64;
                    ("previous session", t)
                };
                info!(
                    "Cache hit [{}] ({}) — {} tokens saved (total {}: {})",
                    hash_prefix,
                    origin,
                    fmt_thousands(tokens_saved as u64),
                    origin,
                    fmt_thousands(total)
                );
                if let Some(ref mut usage) = resp.usage {
                    // The hit serves the whole prompt from this cache, so all
                    // of it counts as cached.  cached_tokens is a breakdown
                    // of prompt_tokens rather than an addend.  The count
                    // recorded with the response covers this same prompt.
                    usage.cached_tokens = Some(usage.prompt_tokens);
                }
                return Ok(resp);
            }
        }

        debug!("Cache miss [{}]", hash_prefix);

        let resp = self.inner.generate_content(request.clone()).await?;

        // A truncated answer is not worth keeping. Stored, it would be replayed
        // for every identical request from here on -- so a stage that hit the
        // output ceiling once would hit it again on the next review of the same
        // patch without the model ever being asked, and the retry that recovers
        // from it would be spent on a cache hit.
        if resp.truncated {
            debug!("Not caching truncated response [{}]", hash_prefix);
            return Ok(resp);
        }

        let response_json = serde_json::to_string(&resp)?;
        let request_json = serde_json::to_string(&request)?;
        let caps = self.inner.get_capabilities();
        let tokens_saved = resp
            .usage
            .as_ref()
            .map(|u| u.prompt_tokens + u.completion_tokens)
            .unwrap_or(0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let _ = self
            .conn
            .execute(
                "INSERT OR REPLACE INTO response_cache (request_hash, provider, model, request_json, response_json, tokens_saved, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                libsql::params![
                    hash,
                    caps.model_name.clone(),
                    caps.model_name,
                    request_json,
                    response_json,
                    tokens_saved as i64,
                    now
                ],
            )
            .await;

        Ok(resp)
    }

    fn estimate_tokens(&self, request: &AiRequest) -> usize {
        self.inner.estimate_tokens(request)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        self.inner.get_capabilities()
    }

    fn cache_identity(&self) -> String {
        self.inner.cache_identity()
    }

    fn cache_stats(&self) -> Option<CacheStats> {
        Some(CacheStats {
            hits_this_session: self.hits_this.load(Ordering::Relaxed),
            hits_prev_session: self.hits_prev.load(Ordering::Relaxed),
            tokens_saved_this_session: self.tokens_saved_this.load(Ordering::Relaxed),
            tokens_saved_prev_session: self.tokens_saved_prev.load(Ordering::Relaxed),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::AiMessage;
    use std::sync::atomic::AtomicUsize;

    /// Answers once truncated, then properly, counting how often it was asked.
    struct TruncatesFirst {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl AiProvider for TruncatesFirst {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(AiResponse {
                content: Some(if n == 0 {
                    "half".into()
                } else {
                    "whole".into()
                }),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: n == 0,
            })
        }

        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            0
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "truncates-first".to_string(),
                context_window_size: 1000,
            }
        }
    }

    fn request() -> AiRequest {
        AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: crate::ai::AiRole::User,
                content: Some("same question".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        }
    }

    /// A half-written answer must not become the permanent answer.
    ///
    /// Cached, it would be replayed for every identical request from then on:
    /// the next review of the same patch would truncate without the model being
    /// asked, and the retry that recovers from truncation would be spent on a
    /// cache hit rather than on a real attempt.
    #[tokio::test]
    async fn a_truncated_response_is_not_cached() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cache.db");
        let inner = Arc::new(TruncatesFirst {
            calls: AtomicUsize::new(0),
        });
        let cache = CachingAiProvider::new(inner.clone(), path.to_str().unwrap(), 7)
            .await
            .unwrap();

        let first = cache.generate_content(request()).await.unwrap();
        assert!(first.truncated, "the first answer is the truncated one");

        // The same question again: it must reach the provider, not the cache.
        let second = cache.generate_content(request()).await.unwrap();
        assert!(!second.truncated, "the replay would still be truncated");
        assert_eq!(second.content.as_deref(), Some("whole"));
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);

        // And a whole answer is still cached, or this would have cost the
        // cache its point.
        let third = cache.generate_content(request()).await.unwrap();
        assert_eq!(third.content.as_deref(), Some("whole"));
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            2,
            "a complete answer must still be served from the cache"
        );
    }
}
