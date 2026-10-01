// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! A pass-through [`AiProvider`] decorator that retries with backoff on
//! rate-limit and transient (e.g. 503 "overloaded") errors.
//!
//! The daemon classifies provider errors and backs off before retrying them.
//! A worker running reviews in-process calls the provider directly, so its only
//! recourse is the stage loop retrying immediately and blindly. This decorator
//! gives that path the same behaviour, reusing [`QuotaManager`] and the typed
//! [`AiErrorClass`] classification the providers already produce:
//!
//! - `RateLimit` (429 / quota): account-wide, so it is reported to a shared
//!   [`QuotaManager`] and every concurrent request waits out the window.
//! - `Transient` (503 overloaded, 500/502/504, 529): a momentary server-side
//!   failure, so only this call backs off, exponentially and with jitter so
//!   concurrent stages do not resynchronise onto the same retry instant.
//! - `Fatal`: propagated immediately.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::time::sleep;
use tracing::warn;

use crate::ai::concurrency_limited_provider::SlotObserver;
use crate::ai::quota::QuotaManager;
use crate::ai::{
    AiErrorClass, AiProvider, AiRequest, AiResponse, CacheStats, ProviderCapabilities,
    classify_ai_error, get_log_prefix,
};

/// Maximum number of attempts (initial try plus retries) for a single call
/// before the last error is propagated. Bounds wall-clock so a sustained
/// outage cannot hang a review indefinitely; transient blips resolve well
/// within this.
const MAX_ATTEMPTS: u32 = 6;

/// Lets a caller account for the time spent waiting on the shared quota gate
/// and stop the retry loop early.
///
/// The daemon bounds a review by an activity deadline rather than by an attempt
/// count, and does not want a rate-limit wait to consume that budget, so it
/// credits the wait back and fails once the deadline passes.
pub trait RetryBudget: Send + Sync {
    /// Time spent blocked on the shared quota gate before an attempt.
    fn credit_wait(&self, slept: Duration);
    /// Return an error to abandon further retries.
    fn check(&self) -> Result<()>;
}

/// The review's deadline passed before it finished.
///
/// Typed so the daemon tells a timeout from any other failure by downcasting
/// rather than by matching text. The message is the one failed review rows
/// have always stored, so rows written before and after read the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("Review tool timed out (active time exceeded)")]
pub struct ReviewTimedOut {
    /// The active time the review was given, in seconds. Time parked at the
    /// quota gate is credited back and is not part of it.
    pub active_seconds: u64,
    /// The further time it was given after that to salvage its findings, in
    /// seconds; 0 when salvage is disabled.
    pub salvage_seconds: u64,
}

/// The wall-clock limits of one review run, shared by whoever enforces them.
///
/// The review is asked to wind down at the soft limit and is stopped at the
/// hard limit, `salvage` later. Both move together when time spent waiting
/// at the quota gate is credited back, because that wait is not active time
/// in either phase.
#[derive(Debug)]
pub struct ReviewDeadline {
    /// The active time granted, kept to say how long the review ran when it
    /// times out.
    timeout: Duration,
    /// The extra time granted after the soft limit. It never changes, so it
    /// is kept outside the lock.
    salvage: Duration,
    /// When the review is asked to wind down. Moved later by credited waits.
    soft: std::sync::Mutex<tokio::time::Instant>,
}

impl ReviewDeadline {
    /// Starts the clock now: the soft limit is `timeout` away and the hard
    /// limit a further `salvage`.
    pub fn new(timeout: Duration, salvage: Duration) -> Self {
        Self {
            timeout,
            salvage,
            soft: std::sync::Mutex::new(tokio::time::Instant::now() + timeout),
        }
    }

    /// When the review is asked to wind down.
    pub fn soft(&self) -> tokio::time::Instant {
        *self.lock_soft()
    }

    /// When the review is stopped: `salvage` after the soft limit, and equal
    /// to it when salvage is disabled.
    pub fn hard(&self) -> tokio::time::Instant {
        self.soft() + self.salvage
    }

    /// Credits a wait at the quota gate by moving both limits later.
    pub fn credit(&self, waited: Duration) {
        *self.lock_soft() += waited;
    }

    /// The error that reports this deadline passing.
    pub fn timed_out(&self) -> ReviewTimedOut {
        ReviewTimedOut {
            active_seconds: self.timeout.as_secs(),
            salvage_seconds: self.salvage.as_secs(),
        }
    }

    /// The guard holds a plain instant that every writer leaves valid, so a
    /// lock poisoned by a panic elsewhere still guards a usable value and is
    /// taken as it is rather than spreading the panic.
    fn lock_soft(&self) -> std::sync::MutexGuard<'_, tokio::time::Instant> {
        self.soft
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A [`RetryBudget`] backed by a [`ReviewDeadline`] shared with the caller.
///
/// Time spent parked on the quota gate is credited back, so waiting out a rate
/// limit does not consume the caller's budget. Retries stop only at the hard
/// limit, so a request made while the review winds down is not abandoned
/// when the soft limit passes.
pub struct DeadlineBudget {
    deadline: Arc<ReviewDeadline>,
    last_credit_end: std::sync::Mutex<Option<tokio::time::Instant>>,
}

impl DeadlineBudget {
    pub fn new(deadline: Arc<ReviewDeadline>) -> Self {
        Self {
            deadline,
            last_credit_end: std::sync::Mutex::new(None),
        }
    }
}

impl RetryBudget for DeadlineBudget {
    fn credit_wait(&self, slept: Duration) {
        if slept.is_zero() {
            return;
        }
        let now = tokio::time::Instant::now();
        let sleep_start = now.checked_sub(slept).unwrap_or(now);
        // The guard holds only the end of the last credited wait, which every
        // writer leaves valid, so a poisoned lock is taken as it is.
        let mut last_end = self
            .last_credit_end
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let effective_start = match *last_end {
            Some(end) if end > sleep_start => end,
            _ => sleep_start,
        };
        if now > effective_start {
            let uncredited = now - effective_start;
            // Filter out sub-millisecond thread scheduling jitter between
            // concurrent tasks waking from the same rate-limit window.
            if uncredited >= Duration::from_millis(1) {
                self.deadline.credit(uncredited);
                *last_end = Some(now);
            }
        }
    }

    fn check(&self) -> Result<()> {
        if tokio::time::Instant::now() > self.deadline.hard() {
            return Err(self.deadline.timed_out().into());
        }
        Ok(())
    }
}

/// Adds rate-limit and transient retry with backoff around an inner provider.
pub struct BackoffProvider {
    inner: Arc<dyn AiProvider>,
    /// Shared across the run, so one rate-limit response backs every
    /// concurrent request off together.
    quota: Arc<QuotaManager>,
    /// Base unit of the exponential transient backoff. Tests use a tiny value
    /// so they run in real time.
    base_delay: Duration,
    /// Attempt ceiling, or None to retry until `budget` says to stop.
    max_attempts: Option<u32>,
    budget: Option<Arc<dyn RetryBudget>>,
    /// Told when a call is about to back off and when it resumes. In the
    /// daemon this is the only place a rate-limit wait happens, so it is the
    /// only place that can report one: the worker's own backoff almost never
    /// runs there, because the error is retried here before it reaches it.
    observer: Option<Arc<dyn SlotObserver>>,
}

impl BackoffProvider {
    /// With a `budget`, retries until it reports the caller's deadline has
    /// passed. Without one, falls back to a fixed attempt ceiling, which bounds
    /// wall-clock for callers that have no deadline of their own.
    pub fn new(
        inner: Arc<dyn AiProvider>,
        quota: Arc<QuotaManager>,
        budget: Option<Arc<dyn RetryBudget>>,
    ) -> Self {
        Self {
            inner,
            quota,
            base_delay: Duration::from_secs(1),
            max_attempts: budget.is_none().then_some(MAX_ATTEMPTS),
            budget,
            observer: None,
        }
    }

    /// Reports backoff waits to `observer`.
    pub fn with_observer(mut self, observer: Arc<dyn SlotObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Reports a wait that is about to happen. Returns whether one was
    /// reported, so the caller knows to report the end of it: a stage left
    /// reading "rate limited" after it resumed would be as wrong as one that
    /// never said so.
    fn report_backoff(&self, request: &AiRequest, retry_in: Duration) -> bool {
        match &self.observer {
            Some(observer) if !retry_in.is_zero() => {
                observer.backing_off(request, retry_in);
                true
            }
            _ => false,
        }
    }

    fn report_resumed(&self, request: &AiRequest) {
        if let Some(observer) = &self.observer {
            observer.running(request);
        }
    }
}

#[async_trait]
impl AiProvider for BackoffProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let mut attempt: u32 = 0;
        let mut transient_streak: i32 = 0;
        loop {
            // Honour any active global rate-limit window before trying. The
            // wait is reported so a caller can keep it off its own deadline,
            // and announced first, so whoever is watching the request can tell
            // it is rate limited rather than slow.
            let announced = self.report_backoff(&request, self.quota.pending_wait().await);
            let slept = self.quota.wait_for_access().await;
            if announced {
                self.report_resumed(&request);
            }
            if let Some(budget) = &self.budget {
                budget.credit_wait(slept);
                budget.check()?;
            }

            match self.inner.generate_content(request.clone()).await {
                Ok(response) => {
                    self.quota.report_success().await;
                    return Ok(response);
                }
                Err(e) => {
                    attempt += 1;
                    if let Some(max) = self.max_attempts
                        && attempt >= max
                    {
                        return Err(e);
                    }
                    match classify_ai_error(&e) {
                        AiErrorClass::RateLimit { retry_after } => {
                            // Account-wide: block every concurrent request
                            // until it clears. The next iteration's
                            // wait_for_access() performs the sleep.
                            self.quota.report_quota_error(retry_after).await;
                        }
                        AiErrorClass::Transient { retry_after } => {
                            // Server-side blip. Exponential backoff with
                            // jitter, floored by any server-suggested delay.
                            // Per-call, not global.
                            transient_streak += 1;
                            let mult = 2.0_f64.powi(transient_streak - 1).min(60.0);
                            let backoff = self.base_delay.mul_f64(mult).max(retry_after);
                            let jittered = backoff + backoff.mul_f64(0.25 * fastrand::f64());
                            let retry_target = match self.max_attempts {
                                Some(max) => format!("{attempt}/{max}"),
                                None => format!("{attempt}"),
                            };
                            warn!(
                                "{}Transient AI error (streak {}); backing off {:.1}s then retry {}: {}",
                                get_log_prefix(),
                                transient_streak,
                                jittered.as_secs_f64(),
                                retry_target,
                                e
                            );
                            let announced = self.report_backoff(&request, jittered);
                            sleep(jittered).await;
                            if announced {
                                self.report_resumed(&request);
                            }
                        }
                        AiErrorClass::Fatal => return Err(e),
                    }
                }
            }
        }
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        self.inner.get_capabilities()
    }

    fn cache_stats(&self) -> Option<CacheStats> {
        self.inner.cache_stats()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::gemini::GeminiError;
    use crate::ai::{AiMessage, AiRole};
    use std::sync::atomic::{AtomicU32, Ordering};

    enum Behaviour {
        Transient,
        RateLimit,
        Fatal,
    }

    struct MockProvider {
        calls: AtomicU32,
        fail_times: u32,
        behaviour: Behaviour,
        rate_limit_after: Duration,
    }

    #[async_trait]
    impl AiProvider for MockProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_times {
                let err = match self.behaviour {
                    Behaviour::Transient => {
                        GeminiError::TransientError(Duration::from_secs(0), "503 overloaded".into())
                    }
                    Behaviour::RateLimit => GeminiError::QuotaExceeded(self.rate_limit_after),
                    Behaviour::Fatal => GeminiError::PermissionDenied("nope".into()),
                };
                return Err(err.into());
            }
            Ok(AiResponse {
                content: Some("ok".into()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".into(),
                context_window_size: 1000,
            }
        }
    }

    fn mock(
        behaviour: Behaviour,
        fail_times: u32,
        rate_limit_after: Duration,
    ) -> Arc<MockProvider> {
        Arc::new(MockProvider {
            calls: AtomicU32::new(0),
            fail_times,
            behaviour,
            rate_limit_after,
        })
    }

    /// A BackoffProvider with a tiny base delay so transient backoff runs in
    /// real time without needing the tokio virtual clock.
    fn fast(inner: Arc<dyn AiProvider>) -> BackoffProvider {
        BackoffProvider {
            inner,
            quota: Arc::new(QuotaManager::new()),
            base_delay: Duration::from_millis(1),
            max_attempts: Some(MAX_ATTEMPTS),
            budget: None,
            observer: None,
        }
    }

    fn dummy_request() -> AiRequest {
        AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some("hi".into()),
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

    #[tokio::test]
    async fn transient_backs_off_then_succeeds() {
        let m = mock(Behaviour::Transient, 3, Duration::ZERO);
        let resp = fast(m.clone())
            .generate_content(dummy_request())
            .await
            .unwrap();
        assert_eq!(resp.content.as_deref(), Some("ok"));
        assert_eq!(m.calls.load(Ordering::SeqCst), 4); // 3 transient failures + success
    }

    #[tokio::test]
    async fn transient_gives_up_after_max_attempts() {
        let m = mock(Behaviour::Transient, u32::MAX, Duration::ZERO);
        let err = fast(m.clone()).generate_content(dummy_request()).await;
        assert!(err.is_err());
        assert_eq!(m.calls.load(Ordering::SeqCst), MAX_ATTEMPTS); // bounded
    }

    #[tokio::test]
    async fn fatal_propagates_without_retry() {
        let m = mock(Behaviour::Fatal, u32::MAX, Duration::ZERO);
        let err = fast(m.clone()).generate_content(dummy_request()).await;
        assert!(err.is_err());
        assert_eq!(m.calls.load(Ordering::SeqCst), 1); // no retry on fatal
    }

    #[tokio::test]
    async fn expired_budget_stops_before_calling() {
        struct Expired;
        impl RetryBudget for Expired {
            fn credit_wait(&self, _slept: Duration) {}
            fn check(&self) -> Result<()> {
                Err(anyhow::anyhow!("deadline exceeded"))
            }
        }
        let m = mock(Behaviour::Transient, u32::MAX, Duration::ZERO);
        let provider = BackoffProvider::new(
            m.clone(),
            Arc::new(QuotaManager::new()),
            Some(Arc::new(Expired)),
        );
        assert!(provider.generate_content(dummy_request()).await.is_err());
        // The budget gates the loop, so the inner provider is never reached.
        assert_eq!(m.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rate_limit_waits_then_succeeds() {
        // QuotaManager uses std::Instant, so use a tiny real delay here.
        let m = mock(Behaviour::RateLimit, 2, Duration::from_millis(5));
        let resp = fast(m.clone())
            .generate_content(dummy_request())
            .await
            .unwrap();
        assert_eq!(resp.content.as_deref(), Some("ok"));
        assert_eq!(m.calls.load(Ordering::SeqCst), 3); // 2 rate-limit failures + success
    }

    #[derive(Default)]
    struct RecordingObserver {
        events: std::sync::Mutex<Vec<String>>,
    }

    impl SlotObserver for RecordingObserver {
        fn queued(&self, _request: &AiRequest) {
            self.events.lock().unwrap().push("queued".into());
        }
        fn running(&self, _request: &AiRequest) {
            self.events.lock().unwrap().push("running".into());
        }
        fn backing_off(&self, _request: &AiRequest, retry_in: Duration) {
            self.events
                .lock()
                .unwrap()
                .push(format!("backing_off {}", !retry_in.is_zero()));
        }
    }

    /// In the daemon this is the only place a rate-limit or transient wait
    /// happens, so unless it says so the stage reads as waiting on a slow model.
    /// It must also say when the wait is over, or the stage stays "rate
    /// limited" while the model works.
    #[tokio::test]
    async fn a_backoff_is_reported_and_so_is_its_end() {
        let observer = Arc::new(RecordingObserver::default());
        let provider =
            fast(mock(Behaviour::Transient, 1, Duration::ZERO)).with_observer(observer.clone());
        provider.generate_content(dummy_request()).await.unwrap();
        assert_eq!(
            observer.events.lock().unwrap().as_slice(),
            ["backing_off true", "running"]
        );

        // A rate limit is waited out on the shared quota gate, at the top of
        // the next attempt, and reported the same way.
        let observer = Arc::new(RecordingObserver::default());
        let provider = fast(mock(Behaviour::RateLimit, 1, Duration::from_millis(20)))
            .with_observer(observer.clone());
        provider.generate_content(dummy_request()).await.unwrap();
        assert_eq!(
            observer.events.lock().unwrap().as_slice(),
            ["backing_off true", "running"]
        );
    }

    /// A call that never waits has nothing to report; reporting anyway would
    /// churn the stage's state on every request.
    #[tokio::test]
    async fn a_call_that_never_waits_reports_nothing() {
        let observer = Arc::new(RecordingObserver::default());
        let provider =
            fast(mock(Behaviour::Transient, 0, Duration::ZERO)).with_observer(observer.clone());
        provider.generate_content(dummy_request()).await.unwrap();
        assert!(observer.events.lock().unwrap().is_empty());
    }

    /// Paused, so the clock moves only when the test says and the credited
    /// amounts are exact rather than within scheduling jitter.
    #[tokio::test(start_paused = true)]
    async fn deadline_budget_credit_wait_deduplicates_overlapping_sleeps() {
        let deadline = Arc::new(ReviewDeadline::new(Duration::from_secs(60), Duration::ZERO));
        let start = deadline.soft();
        let budget = DeadlineBudget::new(deadline.clone());

        // A 10s wait that has just ended credits all 10s.
        tokio::time::advance(Duration::from_secs(10)).await;
        budget.credit_wait(Duration::from_secs(10));
        let d1 = deadline.soft();
        assert_eq!(d1, start + Duration::from_secs(10));

        // A second task reporting the same wait adds nothing.
        budget.credit_wait(Duration::from_secs(10));
        assert_eq!(deadline.soft(), d1);

        // A zero wait does not touch the deadline.
        budget.credit_wait(Duration::ZERO);
        assert_eq!(deadline.soft(), d1);
    }

    /// A credited wait is not active time in either phase, so it moves the
    /// hard limit with the soft one and the salvage window keeps its length.
    #[tokio::test(start_paused = true)]
    async fn a_credit_moves_both_limits() {
        let deadline = ReviewDeadline::new(Duration::from_secs(60), Duration::from_secs(30));
        let (soft, hard) = (deadline.soft(), deadline.hard());
        assert_eq!(hard - soft, Duration::from_secs(30));

        deadline.credit(Duration::from_secs(5));
        assert_eq!(deadline.soft(), soft + Duration::from_secs(5));
        assert_eq!(deadline.hard(), hard + Duration::from_secs(5));
    }

    /// With salvage disabled the review is stopped the moment it is due to
    /// wind down.
    #[tokio::test(start_paused = true)]
    async fn without_salvage_the_hard_limit_is_the_soft_one() {
        let deadline = ReviewDeadline::new(Duration::from_secs(60), Duration::ZERO);
        assert_eq!(deadline.hard(), deadline.soft());
    }

    /// Retries keep going through the salvage window, so a consolidation
    /// request in backoff is not abandoned at the soft limit, and stop with
    /// the typed timeout once the hard limit has passed.
    #[tokio::test(start_paused = true)]
    async fn the_budget_runs_out_at_the_hard_limit_not_the_soft_one() {
        let deadline = Arc::new(ReviewDeadline::new(
            Duration::from_secs(60),
            Duration::from_secs(30),
        ));
        let budget = DeadlineBudget::new(deadline);
        assert!(budget.check().is_ok());

        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(budget.check().is_ok(), "between soft and hard");

        tokio::time::advance(Duration::from_secs(30)).await;
        let err = budget.check().expect_err("past the hard limit");
        assert_eq!(
            err.downcast_ref::<ReviewTimedOut>(),
            Some(&ReviewTimedOut {
                active_seconds: 60,
                salvage_seconds: 30,
            })
        );
        assert_eq!(
            err.to_string(),
            "Review tool timed out (active time exceeded)"
        );
    }
}
