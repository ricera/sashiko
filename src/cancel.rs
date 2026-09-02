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

//! Signal path from a cancel request to the work it should interrupt.
//!
//! Cancelling used to be a single `UPDATE` on a status column, read in exactly
//! one place — *after* the review had already finished. The LLM ran to
//! completion and its result was discarded, so cancelling saved nothing.
//!
//! This registry gives the API a handle on running work. The API task and the
//! reviewer task are separate `tokio::spawn`s, so they need shared state to
//! reach each other.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// Tracks a cancellation token per in-flight review.
#[derive(Debug, Default)]
pub struct CancelRegistry {
    tokens: Mutex<HashMap<i64, CancellationToken>>,
}

impl CancelRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Registers a patchset as cancellable and returns its token.
    ///
    /// Replacing an existing entry also cancels the old token: a second review of
    /// the same patchset means the first is defunct, and leaving its token
    /// unfired would strand whatever is waiting on it.
    pub fn register(&self, patchset_id: i64) -> CancellationToken {
        let token = CancellationToken::new();
        let mut tokens = self.tokens.lock().unwrap();
        if let Some(previous) = tokens.insert(patchset_id, token.clone()) {
            previous.cancel();
        }
        token
    }

    /// Fires the token for `patchset_id`.
    ///
    /// Returns whether live work was actually signalled, so callers can tell
    /// "database row updated" apart from "running work interrupted".
    pub fn cancel(&self, patchset_id: i64) -> bool {
        let tokens = self.tokens.lock().unwrap();
        match tokens.get(&patchset_id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// Removes the entry. Call when the review ends, however it ends.
    pub fn unregister(&self, patchset_id: i64) {
        self.tokens.lock().unwrap().remove(&patchset_id);
    }

    /// The token for a patchset, if it has interruptible work registered.
    ///
    /// Lets code deep in the review path observe cancellation without threading
    /// the token through every intervening signature.
    pub fn token_for(&self, patchset_id: i64) -> Option<CancellationToken> {
        self.tokens.lock().unwrap().get(&patchset_id).cloned()
    }

    /// Whether a patchset currently has interruptible work registered.
    pub fn is_registered(&self, patchset_id: i64) -> bool {
        self.tokens.lock().unwrap().contains_key(&patchset_id)
    }

    pub fn len(&self) -> usize {
        self.tokens.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Unregisters on drop, so an early return or panic cannot leave a stale token
/// that makes finished work look cancellable.
pub struct CancelGuard {
    registry: Arc<CancelRegistry>,
    patchset_id: i64,
    token: CancellationToken,
}

impl CancelGuard {
    pub fn new(registry: Arc<CancelRegistry>, patchset_id: i64) -> Self {
        let token = registry.register(patchset_id);
        Self {
            registry,
            patchset_id,
            token,
        }
    }

    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        self.registry.unregister(self.patchset_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cancel_fires_registered_token() {
        let reg = CancelRegistry::new();
        let token = reg.register(1);

        assert!(!token.is_cancelled());
        assert!(reg.cancel(1));
        assert!(token.is_cancelled());
    }

    #[test]
    fn test_cancel_unknown_patchset_reports_no_live_work() {
        let reg = CancelRegistry::new();
        assert!(
            !reg.cancel(99),
            "callers rely on this to distinguish a status update from an interrupt"
        );
    }

    #[test]
    fn test_unregister_makes_patchset_uncancellable() {
        let reg = CancelRegistry::new();
        let token = reg.register(2);

        reg.unregister(2);
        assert!(!reg.cancel(2));
        assert!(
            !token.is_cancelled(),
            "unregistering is not cancelling; the work completed normally"
        );
    }

    #[test]
    fn test_reregistering_cancels_the_previous_token() {
        let reg = CancelRegistry::new();
        let first = reg.register(3);
        let second = reg.register(3);

        assert!(
            first.is_cancelled(),
            "a superseded review must not be left waiting on a token nobody will fire"
        );
        assert!(!second.is_cancelled());
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn test_guard_unregisters_on_drop() {
        let reg = CancelRegistry::new();

        {
            let guard = CancelGuard::new(reg.clone(), 4);
            assert!(reg.is_registered(4));
            assert!(!guard.token().is_cancelled());
        }

        assert!(!reg.is_registered(4));
        assert!(reg.is_empty());
    }

    #[test]
    fn test_guard_token_observes_cancellation() {
        let reg = CancelRegistry::new();
        let guard = CancelGuard::new(reg.clone(), 5);
        let token = guard.token();

        assert!(reg.cancel(5));
        assert!(token.is_cancelled());
    }
}
