pub mod metrics;
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

#[cfg(feature = "server")]
pub mod access;
pub mod activity;
pub mod ai;
pub mod api;
pub mod auth;
#[cfg(feature = "server")]
pub mod backfill;
pub mod baseline;
#[cfg(feature = "server")]
pub mod compression;
pub mod db;
#[cfg(feature = "server")]
pub mod email_policy;
#[cfg(feature = "server")]
pub mod email_router;
#[cfg(feature = "server")]
pub mod events;
#[cfg(feature = "server")]
pub mod fetcher;
#[cfg(feature = "server")]
pub mod forge;
pub mod git_cmd;
pub mod git_ops;
#[cfg(feature = "server")]
pub mod ingestor;
pub mod local_review;
pub mod maintainers;
#[cfg(feature = "server")]
pub mod mbox;
#[cfg(feature = "server")]
pub mod nntp;
pub mod patch;
#[cfg(feature = "server")]
pub mod patchwork;
#[cfg(feature = "server")]
pub mod prerequisites;
pub mod project;
pub mod prompt_bundle;
pub mod references;
#[cfg(feature = "server")]
pub mod reviewer;
#[cfg(feature = "server")]
pub mod server;
pub mod settings;
#[cfg(test)]
pub(crate) mod test_support;
pub mod toolbox;
pub mod utils;
pub mod worker;
pub mod workflow;
pub mod workflows;

use std::fmt;
use std::str::FromStr;

/// Represents the current status of a patchset review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewStatus {
    /// The patchset is not yet fully received (e.g., waiting for more parts).
    Incomplete,
    /// The patchset is complete and waiting for review.
    Pending,
    /// The patchset is currently under AI review.
    InReview,
    /// The review process was cancelled.
    Cancelled,
    /// The review was skipped (e.g., due to configuration or filtering).
    Skipped,
    /// The review process completed successfully.
    Reviewed,
    /// The review process failed due to an error.
    Failed,
    /// The patchset failed to apply to the baseline.
    FailedToApply,
}

impl fmt::Display for ReviewStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReviewStatus::Incomplete => write!(f, "Incomplete"),
            ReviewStatus::Pending => write!(f, "Pending"),
            ReviewStatus::InReview => write!(f, "In Review"),
            ReviewStatus::Cancelled => write!(f, "Cancelled"),
            ReviewStatus::Skipped => write!(f, "Skipped"),
            ReviewStatus::Reviewed => write!(f, "Reviewed"),
            ReviewStatus::Failed => write!(f, "Failed"),
            ReviewStatus::FailedToApply => write!(f, "Failed To Apply"),
        }
    }
}

impl FromStr for ReviewStatus {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Incomplete" => Ok(ReviewStatus::Incomplete),
            "Pending" => Ok(ReviewStatus::Pending),
            "In Review" => Ok(ReviewStatus::InReview),
            "Cancelled" => Ok(ReviewStatus::Cancelled),
            "Skipped" => Ok(ReviewStatus::Skipped),
            "Reviewed" => Ok(ReviewStatus::Reviewed),
            "Failed" => Ok(ReviewStatus::Failed),
            "Failed To Apply" => Ok(ReviewStatus::FailedToApply),
            _ => Err(()),
        }
    }
}

impl ReviewStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReviewStatus::Incomplete => "Incomplete",
            ReviewStatus::Pending => "Pending",
            ReviewStatus::InReview => "In Review",
            ReviewStatus::Cancelled => "Cancelled",
            ReviewStatus::Skipped => "Skipped",
            ReviewStatus::Reviewed => "Reviewed",
            ReviewStatus::Failed => "Failed",
            ReviewStatus::FailedToApply => "Failed To Apply",
        }
    }
}

/// The verdict the reviewer writes to a patch's status column.
///
/// The type is narrower than `ReviewStatus` on purpose: it rules out
/// in-flight values such as `Pending` or `In Review`, which a finished
/// attempt must never leave on a patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchOutcome {
    /// The patch has a full review.
    Reviewed,
    /// The patch has no review because its attempts failed.
    Failed,
    /// The patch was deliberately not reviewed.
    Skipped,
    /// The review of the patch was cancelled.
    Cancelled,
}

impl PatchOutcome {
    /// The stored spelling. Values shared with `ReviewStatus` take its
    /// spelling so the two vocabularies cannot drift apart.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reviewed => ReviewStatus::Reviewed.as_str(),
            Self::Failed => ReviewStatus::Failed.as_str(),
            Self::Skipped => ReviewStatus::Skipped.as_str(),
            Self::Cancelled => ReviewStatus::Cancelled.as_str(),
        }
    }
}

impl fmt::Display for PatchOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub mod logging;

#[cfg(test)]
mod tests {
    use super::*;

    /// The `ReviewStatus` each outcome shares its spelling with. The match
    /// is exhaustive, so a new outcome must state its counterpart here.
    fn shared_status(outcome: PatchOutcome) -> ReviewStatus {
        match outcome {
            PatchOutcome::Reviewed => ReviewStatus::Reviewed,
            PatchOutcome::Failed => ReviewStatus::Failed,
            PatchOutcome::Skipped => ReviewStatus::Skipped,
            PatchOutcome::Cancelled => ReviewStatus::Cancelled,
        }
    }

    const ALL_OUTCOMES: [PatchOutcome; 4] = [
        PatchOutcome::Reviewed,
        PatchOutcome::Failed,
        PatchOutcome::Skipped,
        PatchOutcome::Cancelled,
    ];

    #[test]
    fn a_patch_outcome_is_spelled_like_its_review_status() {
        for outcome in ALL_OUTCOMES {
            let status = shared_status(outcome);
            assert_eq!(outcome.as_str(), status.as_str(), "{outcome:?}");
            assert_eq!(outcome.to_string(), status.to_string(), "{outcome:?}");
            assert_eq!(
                ReviewStatus::from_str(outcome.as_str()),
                Ok(status),
                "{outcome:?} does not read back as the status it shares"
            );
        }
    }
}
