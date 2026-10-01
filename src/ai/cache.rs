use anyhow::Result;
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

use super::{AiProvider, AiRequest, AiResponse, CacheStats, ProviderCapabilities, get_log_prefix};

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

    /// Looks up a stored response for `hash` and counts it as a hit.
    ///
    /// A stored response the provider cut off at its output limit is treated
    /// as a miss. Builds before this rule cached such answers, and replaying
    /// one would truncate the stage again without asking the model.
    async fn cached_response(&self, hash: &str) -> Result<Option<AiResponse>> {
        let hash_prefix = &hash[..12];
        let mut rows = self
            .conn
            .query(
                "SELECT response_json, tokens_saved, created_at FROM response_cache WHERE request_hash = ?",
                libsql::params![hash],
            )
            .await?;

        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        let response_json: String = row.get(0)?;
        let tokens_saved: i64 = row.get(1)?;
        let created_at: i64 = row.get(2)?;
        let Ok(mut resp) = serde_json::from_str::<AiResponse>(&response_json) else {
            return Ok(None);
        };
        if resp.truncated {
            debug!("Ignoring cached truncated response [{}]", hash_prefix);
            return Ok(None);
        }

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
            hash_prefix, origin, tokens_saved, origin, total
        );
        if let Some(ref mut usage) = resp.usage {
            // The hit serves the whole prompt from this cache, so all
            // of it counts as cached.  cached_tokens is a breakdown
            // of prompt_tokens rather than an addend.  The count
            // recorded with the response covers this same prompt.
            usage.cached_tokens = Some(usage.prompt_tokens);
        }
        Ok(Some(resp))
    }

    /// Stores a fresh response under `hash`. A failed write is ignored,
    /// because the response itself is still good.
    async fn store_response(
        &self,
        hash: String,
        request: &AiRequest,
        resp: &AiResponse,
    ) -> Result<()> {
        let response_json = serde_json::to_string(resp)?;
        let request_json = serde_json::to_string(request)?;
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
        Ok(())
    }
}

#[async_trait]
impl AiProvider for CachingAiProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let hash = self.compute_cache_key(&request);
        if let Some(resp) = self.cached_response(&hash).await? {
            return Ok(resp);
        }

        let hash_prefix = &hash[..12];
        debug!("Cache miss [{}]", hash_prefix);

        let resp = self.inner.generate_content(request.clone()).await?;

        // A truncated answer is not stored. Cached, it would be replayed for
        // every identical request from then on, so the next review of the
        // same patch would truncate again without the model being asked.
        // A retry after truncation does not depend on this rule, because the
        // feedback it appends makes it a different request.
        if resp.truncated {
            debug!("Not caching truncated response [{}]", hash_prefix);
            return Ok(resp);
        }

        self.store_response(hash, &request, &resp).await?;
        Ok(resp)
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

    async fn forget(&self, request: &AiRequest) {
        let hash = self.compute_cache_key(request);
        // Like the insert above, a failure here is not fatal: the worst case
        // is that a retry is served this answer again, as it was before.
        let result = self
            .conn
            .execute(
                "DELETE FROM response_cache WHERE request_hash = ?",
                libsql::params![hash.clone()],
            )
            .await;
        let prefix = get_log_prefix();
        match result {
            Ok(0) => debug!(
                "{}Cache forget [{}]: nothing to forget",
                prefix,
                &hash[..12]
            ),
            Ok(_) => info!("{}Cache forget [{}]", prefix, &hash[..12]),
            Err(e) => warn!("{}Cache forget [{}] failed: {:#}", prefix, &hash[..12], e),
        }
        self.inner.forget(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::ProviderCapabilities;
    use crate::ai::session::{LlmSession, SessionRunner, ValidationError};
    use serde_json::Value;

    /// Answers every request with text the session below rejects, and counts
    /// how often it was actually asked.
    struct RejectedAnswers {
        calls: Arc<AtomicU64>,
    }

    #[async_trait]
    impl AiProvider for RejectedAnswers {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(AiResponse {
                content: Some("not json".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 100_000,
            }
        }
    }

    struct JsonSession;

    #[async_trait]
    impl LlmSession for JsonSession {
        type Output = Value;

        fn system_prompt(&self) -> String {
            "system".to_string()
        }

        fn initial_user_prompt(&self) -> String {
            "answer in JSON".to_string()
        }

        async fn call_tool(&mut self, _name: &str, _args: Value) -> Result<Value> {
            unreachable!("the session offers no tools")
        }

        fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
            serde_json::from_str(response.content.as_deref().unwrap_or(""))
                .map_err(|e| ValidationError::FormatViolation(e.to_string()))
        }
    }

    /// Calls a tool on its first turn, then answers with text the session
    /// rejects. Counts how often it was actually asked.
    struct ToolThenRejected {
        calls: Arc<AtomicU64>,
    }

    #[async_trait]
    impl AiProvider for ToolThenRejected {
        async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let has_tool_result = request
                .messages
                .iter()
                .any(|m| m.role == crate::ai::AiRole::Tool);
            Ok(AiResponse {
                content: has_tool_result.then(|| "not json".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: (!has_tool_result).then(|| {
                    vec![crate::ai::ToolCall {
                        id: "call_1".to_string(),
                        function_name: "read".to_string(),
                        arguments: serde_json::json!({}),
                        thought_signature: None,
                    }]
                }),
                usage: None,
                truncated: false,
            })
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 100_000,
            }
        }
    }

    struct ToolThenJsonSession;

    #[async_trait]
    impl LlmSession for ToolThenJsonSession {
        type Output = Value;

        fn system_prompt(&self) -> String {
            "system".to_string()
        }

        fn initial_user_prompt(&self) -> String {
            "read, then answer in JSON".to_string()
        }

        async fn call_tool(&mut self, _name: &str, _args: Value) -> Result<Value> {
            Ok(serde_json::json!({ "ok": true }))
        }

        fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
            serde_json::from_str(response.content.as_deref().unwrap_or(""))
                .map_err(|e| ValidationError::FormatViolation(e.to_string()))
        }
    }

    #[tokio::test]
    async fn test_a_retry_still_replays_the_turns_that_were_accepted() {
        // Only the rejected answers are forgotten. The tool call turn before
        // them was fine, so the retry is served it from the cache and asks
        // the model again only for the answer.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("response_cache.db");
        let calls = Arc::new(AtomicU64::new(0));

        let mut per_attempt = Vec::new();
        for _attempt in 1..=2 {
            let before = calls.load(Ordering::SeqCst);
            let provider = CachingAiProvider::new(
                Arc::new(ToolThenRejected {
                    calls: calls.clone(),
                }),
                path.to_str().unwrap(),
                30,
            )
            .await
            .unwrap();
            let result = SessionRunner::new(&provider)
                .run(&mut ToolThenJsonSession)
                .await;
            assert!(result.is_err(), "every answer is rejected");
            per_attempt.push(calls.load(Ordering::SeqCst) - before);
        }

        // Attempt 1: the tool call turn, then three rejected answers.
        // Attempt 2: the tool call turn from the cache, then three answers.
        assert_eq!(per_attempt, vec![4, 3]);
    }

    /// Rejects the first answer of a conversation, accepts the answer given
    /// after the rejection, and counts how often it was actually asked.
    struct RejectedOnce {
        calls: Arc<AtomicU64>,
    }

    #[async_trait]
    impl AiProvider for RejectedOnce {
        async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let first_answer = request.messages.len() == 1;
            Ok(AiResponse {
                content: Some(if first_answer { "not json" } else { "{}" }.to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 100_000,
            }
        }
    }

    #[tokio::test]
    async fn test_a_stage_that_recovers_stays_cached() {
        // The first answer is rejected and the next one accepted, so the
        // stage succeeds. Nothing is forgotten, and a later run is served
        // the whole exchange from the cache.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("response_cache.db");
        let calls = Arc::new(AtomicU64::new(0));

        let mut per_run = Vec::new();
        for _run in 1..=2 {
            let before = calls.load(Ordering::SeqCst);
            let provider = CachingAiProvider::new(
                Arc::new(RejectedOnce {
                    calls: calls.clone(),
                }),
                path.to_str().unwrap(),
                30,
            )
            .await
            .unwrap();
            let result = SessionRunner::new(&provider).run(&mut JsonSession).await;
            assert!(result.is_ok(), "the second answer is accepted");
            per_run.push(calls.load(Ordering::SeqCst) - before);
        }

        assert_eq!(per_run, vec![2, 0], "calls per run");
    }

    /// Answers every request with a response the provider marked as cut off
    /// at the output limit, and counts how often it was actually asked.
    struct TruncatedAnswers {
        calls: Arc<AtomicU64>,
    }

    #[async_trait]
    impl AiProvider for TruncatedAnswers {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(AiResponse {
                content: Some("{\"conc".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: true,
            })
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 100_000,
            }
        }
    }

    #[tokio::test]
    async fn test_a_retry_after_a_truncated_answer_reaches_the_model() {
        // The runner asks again after a truncated answer, up to the provider
        // error retry limit, and then gives up. Nothing it was handed may be
        // served to the next attempt: each attempt reaches the model as often
        // as the first did.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("response_cache.db");
        let calls = Arc::new(AtomicU64::new(0));

        let mut per_attempt = Vec::new();
        for _attempt in 1..=2 {
            let before = calls.load(Ordering::SeqCst);
            let provider = CachingAiProvider::new(
                Arc::new(TruncatedAnswers {
                    calls: calls.clone(),
                }),
                path.to_str().unwrap(),
                30,
            )
            .await
            .unwrap();
            let result = SessionRunner::new(&provider).run(&mut JsonSession).await;
            assert!(
                result.is_err(),
                "answers truncated every time end the session"
            );
            per_attempt.push(calls.load(Ordering::SeqCst) - before);
        }

        // The first answer and three more before the default limit of three
        // truncated answers per turn is exceeded.
        assert_eq!(per_attempt, vec![4, 4], "calls per attempt");
    }

    /// Rejects the first answer of a conversation, then cuts the next one
    /// off, and counts how often it was actually asked.
    struct RejectedThenTruncated {
        calls: Arc<AtomicU64>,
    }

    #[async_trait]
    impl AiProvider for RejectedThenTruncated {
        async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let first_answer = request.messages.len() == 1;
            Ok(AiResponse {
                content: Some(if first_answer { "not json" } else { "{\"conc" }.to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: !first_answer,
            })
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 100_000,
            }
        }
    }

    #[tokio::test]
    async fn test_giving_up_on_a_truncated_answer_also_forgets_the_rejected_ones() {
        // A rejected answer, then truncated ones until the stage gives up: a
        // retry must be asked all of them again rather than be served the
        // rejection.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("response_cache.db");
        let calls = Arc::new(AtomicU64::new(0));

        let mut per_attempt = Vec::new();
        for _attempt in 1..=2 {
            let before = calls.load(Ordering::SeqCst);
            let provider = CachingAiProvider::new(
                Arc::new(RejectedThenTruncated {
                    calls: calls.clone(),
                }),
                path.to_str().unwrap(),
                30,
            )
            .await
            .unwrap();
            let result = SessionRunner::new(&provider).run(&mut JsonSession).await;
            assert!(
                result.is_err(),
                "the stage gives up on the truncated answers"
            );
            per_attempt.push(calls.load(Ordering::SeqCst) - before);
        }

        // The rejected answer, then four truncated ones before the default
        // limit of three truncated answers per turn is exceeded.
        assert_eq!(per_attempt, vec![5, 5], "calls per attempt");
    }

    #[tokio::test]
    async fn test_a_retry_after_rejected_answers_reaches_the_model() {
        // local_review retries a failed patch review by building a fresh
        // cached provider over the same cache file and running again. The
        // first attempt fails because every answer is rejected. A retry is
        // there to ask the model again, so it must reach the model at least
        // once rather than be served the rejected answers from the cache.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("response_cache.db");
        let calls = Arc::new(AtomicU64::new(0));

        let mut per_attempt = Vec::new();
        for _attempt in 1..=2 {
            let before = calls.load(Ordering::SeqCst);
            let provider = CachingAiProvider::new(
                Arc::new(RejectedAnswers {
                    calls: calls.clone(),
                }),
                path.to_str().unwrap(),
                30,
            )
            .await
            .unwrap();
            let result = SessionRunner::new(&provider).run(&mut JsonSession).await;
            assert!(result.is_err(), "every answer is rejected");
            per_attempt.push(calls.load(Ordering::SeqCst) - before);
        }

        assert_eq!(
            per_attempt[0], 3,
            "attempt 1 asks until the validation limit"
        );
        assert!(
            per_attempt[1] > 0,
            "the retry never reached the model: calls per attempt {:?}",
            per_attempt
        );
    }
}

#[cfg(test)]
mod truncation_tests {
    use super::*;
    use crate::ai::{AiMessage, AiRole};
    use std::sync::atomic::AtomicUsize;

    /// Answers "half" truncated for its first `truncated_calls` calls and
    /// "whole" after that, counting how often it was asked.
    struct TruncatesFirst {
        truncated_calls: usize,
        calls: AtomicUsize,
    }

    impl TruncatesFirst {
        fn new(truncated_calls: usize) -> Arc<Self> {
            Arc::new(Self {
                truncated_calls,
                calls: AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl AiProvider for TruncatesFirst {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            let truncated = self.calls.fetch_add(1, Ordering::SeqCst) < self.truncated_calls;
            Ok(answer(if truncated { "half" } else { "whole" }, truncated))
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "truncates-first".to_string(),
                context_window_size: 1000,
            }
        }
    }

    fn answer(text: &str, truncated: bool) -> AiResponse {
        AiResponse {
            content: Some(text.to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            usage: None,
            truncated,
        }
    }

    fn request() -> AiRequest {
        AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::User,
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
            reasoning_effort: None,
        }
    }

    async fn caching(inner: Arc<TruncatesFirst>, dir: &tempfile::TempDir) -> CachingAiProvider {
        let path = dir.path().join("cache.db");
        CachingAiProvider::new(inner, path.to_str().unwrap(), 7)
            .await
            .unwrap()
    }

    async fn stored_rows(cache: &CachingAiProvider) -> i64 {
        let mut rows = cache
            .conn
            .query("SELECT COUNT(*) FROM response_cache", ())
            .await
            .unwrap();
        rows.next().await.unwrap().unwrap().get(0).unwrap()
    }

    /// A half-written answer must not become the permanent answer.
    ///
    /// Cached, it would be replayed for every identical request from then on,
    /// so the next review of the same patch would truncate again without the
    /// model being asked.
    #[tokio::test]
    async fn a_truncated_response_is_not_cached() {
        let tmp = tempfile::tempdir().unwrap();
        let inner = TruncatesFirst::new(1);
        let cache = caching(inner.clone(), &tmp).await;

        let first = cache.generate_content(request()).await.unwrap();
        assert!(first.truncated, "the first answer is the truncated one");
        // The table is checked directly, because the read side also ignores
        // truncated entries and would hide a stored one from the calls below.
        assert_eq!(
            stored_rows(&cache).await,
            0,
            "the truncated answer was stored"
        );

        // The same question again: it must reach the provider, not the cache.
        let second = cache.generate_content(request()).await.unwrap();
        assert!(!second.truncated, "the replay would still be truncated");
        assert_eq!(second.content.as_deref(), Some("whole"));
        assert_eq!(inner.calls(), 2);

        // A whole answer is still cached, or this would have cost the cache
        // its point.
        let third = cache.generate_content(request()).await.unwrap();
        assert_eq!(third.content.as_deref(), Some("whole"));
        assert_eq!(
            inner.calls(),
            2,
            "a complete answer must still be served from the cache"
        );
        let stats = cache.cache_stats().unwrap();
        assert_eq!(stats.hits_this_session, 1);
    }

    /// An entry an older build stored while truncated is a miss: the model is
    /// asked again, and its whole answer replaces the stale entry.
    #[tokio::test]
    async fn a_cached_truncated_entry_is_served_as_a_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let inner = TruncatesFirst::new(0);
        let cache = caching(inner.clone(), &tmp).await;
        cache
            .store_response(
                cache.compute_cache_key(&request()),
                &request(),
                &answer("half", true),
            )
            .await
            .unwrap();

        let first = cache.generate_content(request()).await.unwrap();
        assert!(!first.truncated, "the stored truncated entry was replayed");
        assert_eq!(first.content.as_deref(), Some("whole"));
        assert_eq!(inner.calls(), 1);
        assert_eq!(
            cache.cache_stats().unwrap().hits_this_session,
            0,
            "an ignored entry must not count as a hit"
        );

        let second = cache.generate_content(request()).await.unwrap();
        assert_eq!(second.content.as_deref(), Some("whole"));
        assert_eq!(inner.calls(), 1, "the whole answer replaced the entry");
    }
}
