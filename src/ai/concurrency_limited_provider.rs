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

//! A pass-through [`AiProvider`] decorator that caps the number of concurrent
//! `generate_content` calls against a shared semaphore.
//!
//! A review fans its analysis stages out concurrently, so a single patch can
//! issue one model call per stage at once, and the worker reviews several
//! patches in parallel on top of that. The daemon bounds this globally with its
//! own LLM semaphore, but a worker running reviews in-process has nothing in
//! front of it. Sharing one semaphore across every provider the run creates
//! turns `[review] concurrency` into a real ceiling on in-flight model calls.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::Semaphore;

use crate::ai::{AiProvider, AiRequest, AiResponse, CacheStats, ProviderCapabilities};

/// How many concurrent model calls a review `concurrency` allows.
///
/// Derived from the shape of a review workflow: its analysis stages run as one
/// parallel fan-out, whose width the planning stage chooses per patch, followed
/// by consolidation stages that run sequentially. An active review therefore
/// averages roughly three concurrent calls, so scaling to `concurrency * 3`
/// saturates model capacity while worktrees and worker processes stay gated at
/// `concurrency` itself. The factor is empirical rather than a bound the
/// workflow guarantees.
///
/// A configuration asking for no parallelism stays fully serial rather than
/// being widened.
pub fn llm_permits(concurrency: usize) -> usize {
    if concurrency < 2 { 1 } else { concurrency * 3 }
}

/// Notified when a request has to wait for a slot, and again once it has one.
///
/// The wait is otherwise invisible. A stage queued behind its siblings looks
/// exactly like a stage waiting on a slow model, and the two call for opposite
/// responses: raise the concurrency, or be patient. Only this layer knows which
/// of the two is happening.
pub trait SlotObserver: Send + Sync {
    /// The request is blocked because every permit is taken.
    fn queued(&self, request: &AiRequest);
    /// The permit is held, so the time from here really is the model's.
    fn running(&self, request: &AiRequest);
}

/// Limits concurrent model calls to the permits of a shared semaphore. All
/// other behaviour is delegated unchanged to the inner provider.
pub struct ConcurrencyLimitedProvider {
    inner: Arc<dyn AiProvider>,
    semaphore: Arc<Semaphore>,
    observer: Option<Arc<dyn SlotObserver>>,
}

impl ConcurrencyLimitedProvider {
    /// The semaphore is shared, so every provider built from it draws on the
    /// same pool of permits.
    pub fn new(inner: Arc<dyn AiProvider>, semaphore: Arc<Semaphore>) -> Self {
        Self {
            inner,
            semaphore,
            observer: None,
        }
    }

    /// Reports slot waits to `observer`. Without one the limiter is silent, as
    /// it is for a local review that has nowhere to report to.
    pub fn with_observer(mut self, observer: Arc<dyn SlotObserver>) -> Self {
        self.observer = Some(observer);
        self
    }
}

#[async_trait]
impl AiProvider for ConcurrencyLimitedProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        // Tried without blocking first, so the common case of a free slot costs
        // no report at all. Only a request that genuinely waits is worth saying
        // anything about, and only that request needs telling when the wait
        // ends.
        let _permit = match self.semaphore.try_acquire() {
            Ok(permit) => permit,
            Err(_) => {
                if let Some(observer) = &self.observer {
                    observer.queued(&request);
                }
                let permit = self
                    .semaphore
                    .acquire()
                    .await
                    .map_err(|e| anyhow::anyhow!("concurrency semaphore closed: {e}"))?;
                if let Some(observer) = &self.observer {
                    observer.running(&request);
                }
                permit
            }
        };
        self.inner.generate_content(request).await
    }

    fn estimate_tokens(&self, request: &AiRequest) -> usize {
        self.inner.estimate_tokens(request)
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

    #[test]
    fn test_llm_permits_keeps_a_serial_configuration_serial() {
        // Widening these would defeat the point of asking for no parallelism,
        // which is what someone does to stay under a provider's limits.
        assert_eq!(llm_permits(0), 1);
        assert_eq!(llm_permits(1), 1);

        // Above that, calls are allowed to run wider than the worktrees are.
        assert_eq!(llm_permits(2), 6);
        assert_eq!(llm_permits(16), 48);
    }

    /// Blocks until released, so a second caller is provably still queued.
    struct GatedProvider {
        gate: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl AiProvider for GatedProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            self.gate.notified().await;
            Ok(AiResponse {
                content: Some("ok".into()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }

        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            0
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "gated".into(),
                context_window_size: 1000,
            }
        }
    }

    #[derive(Default)]
    struct RecordingObserver {
        events: std::sync::Mutex<Vec<&'static str>>,
    }

    impl SlotObserver for RecordingObserver {
        fn queued(&self, _request: &AiRequest) {
            self.events.lock().unwrap().push("queued");
        }
        fn running(&self, _request: &AiRequest) {
            self.events.lock().unwrap().push("running");
        }
    }

    fn request() -> AiRequest {
        AiRequest {
            system: None,
            messages: Vec::new(),
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        }
    }

    /// A request that waits for a slot says so, and says when the wait is over.
    ///
    /// Without this a stage queued behind its siblings reports "awaiting model",
    /// which is what a slow model reports -- and the two call for opposite
    /// responses.
    #[tokio::test]
    async fn test_a_queued_request_is_reported_as_queued() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let observer = Arc::new(RecordingObserver::default());
        // One permit, so the second caller cannot proceed until the first ends.
        let limiter = Arc::new(
            ConcurrencyLimitedProvider::new(
                Arc::new(GatedProvider { gate: gate.clone() }),
                Arc::new(Semaphore::new(1)),
            )
            .with_observer(observer.clone()),
        );

        let first = tokio::spawn({
            let limiter = limiter.clone();
            async move { limiter.generate_content(request()).await }
        });
        // Let the first call take the only permit and block on the gate.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let second = tokio::spawn({
            let limiter = limiter.clone();
            async move { limiter.generate_content(request()).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            observer.events.lock().unwrap().as_slice(),
            ["queued"],
            "the second call is waiting for a slot and nothing has told anyone"
        );

        gate.notify_waiters();
        gate.notify_waiters();
        let _ = first.await.unwrap();
        gate.notify_waiters();
        let _ = second.await.unwrap();

        assert_eq!(
            observer.events.lock().unwrap().as_slice(),
            ["queued", "running"],
            "the wait has to be cleared, or the stage reads as queued forever"
        );
    }

    /// The common case costs nothing: a free slot is not worth reporting, and
    /// reporting it would churn the activity clock on every single request.
    #[tokio::test]
    async fn test_an_unqueued_request_is_not_reported() {
        let gate = Arc::new(tokio::sync::Notify::new());
        gate.notify_waiters();
        let observer = Arc::new(RecordingObserver::default());
        let limiter = ConcurrencyLimitedProvider::new(
            Arc::new(GatedProvider { gate: gate.clone() }),
            Arc::new(Semaphore::new(4)),
        )
        .with_observer(observer.clone());

        let call = tokio::spawn(async move { limiter.generate_content(request()).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        gate.notify_waiters();
        let _ = call.await.unwrap();

        assert!(
            observer.events.lock().unwrap().is_empty(),
            "a request that never waited has nothing to report"
        );
    }
}
