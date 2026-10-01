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

//! Read-only reference repositories: `[[git.references]]`, the worker flags
//! that override it, and the checks that turn the list into pinned
//! [`ReferenceRepo`]s.
//!
//! References are named and listed rather than a single slot because the code
//! under review can depend on more than one tree it does not contain. An
//! out-of-tree kernel module needs the kernel it builds against; the same
//! module's RDMA half also needs rdma-core, the userspace side of its ABI. The
//! name is what the model passes as `repo`, and what every error and log line
//! uses, so it has to be short, unique and safe to show.
//!
//! A reference can be scoped with `applies_to`, globs over the repository
//! under review. It is then attached only to the patches that touch a matching
//! file and is dormant for the rest: an Ethernet patch to a driver whose RDMA
//! half needs rdma-core should neither pay the tokens for that tree in every
//! tool schema and the prompt, nor be invited to wander into it. The decision
//! is per patch, because a series is reviewed one patch at a time and its
//! patches can touch different halves of the driver.
//!
//! This module is not gated on the server feature: the worker validates the
//! list on every path into a review, including builds without the daemon.

use crate::project::ProjectId;
use crate::settings::{ReferenceKind, ReferenceSettings};
use crate::toolbox::{DormantReference, REPO_ARG_REVIEW, ReferenceRepo};
use anyhow::{Result, anyhow, bail};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use tracing::warn;

/// Most references a review may carry.
///
/// Each one adds an enum value to eight tool schemas and an entry, with its
/// description, to the prompt of every patch. A cap keeps a configuration
/// mistake from quietly spending that on dozens of trees.
pub const MAX_REFERENCES: usize = 8;

/// The name `kernel` is kept for the kernel tree so that `repo="kernel"` means
/// the same thing in every deployment and the out-of-tree caveats can rely on
/// it.
const KERNEL_NAME: &str = "kernel";

/// How an entry is named in errors: by its name when it has a usable one, by
/// its position otherwise, so an empty name still points at a table.
fn label(index: usize, entry: &ReferenceSettings) -> String {
    if entry.name.is_empty() {
        format!("git.references[#{index}]")
    } else {
        format!("git.references[{}]", entry.name)
    }
}

/// Whether `name` matches `^[a-z0-9][a-z0-9._-]{0,31}$`.
///
/// The name is shown to the model as an enum value and in the prompt, and
/// appears in log lines. Lowercase letters, digits and a little punctuation
/// cannot be misread in any of those, and cannot start like an option.
fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= 32
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
}

/// The compiled `applies_to` of one reference: which patches it is attached
/// to.
#[derive(Debug, Clone)]
pub struct AppliesTo {
    /// As configured, for the prompt-free places that explain a decision: the
    /// per-patch log line and the error for a dormant reference.
    patterns: Vec<String>,
    set: GlobSet,
}

impl AppliesTo {
    /// Checks and compiles the patterns of the entry labelled `at`.
    ///
    /// Each refusal names the entry and the pattern's index, because the
    /// pattern alone may appear in several entries. The shapes refused are
    /// those that compile but can never match a path in a patch, which would
    /// otherwise leave the reference silently dormant for every review.
    pub fn new(at: &str, patterns: &[String]) -> Result<Self> {
        if patterns.is_empty() {
            bail!(
                "{at}.applies_to is empty; remove the key to attach the reference to every \
                 patch, or list at least one pattern"
            );
        }
        let mut builder = GlobSetBuilder::new();
        for (i, p) in patterns.iter().enumerate() {
            if p.is_empty() {
                bail!("{at}.applies_to[{i}] is empty");
            }
            if p.starts_with('/') {
                bail!(
                    "{at}.applies_to[{i}] '{p}' is absolute; patterns are relative to the \
                     root of the repository under review"
                );
            }
            if p.split('/').any(|c| c == "..") {
                bail!(
                    "{at}.applies_to[{i}] '{p}' must not contain '..'; patterns are \
                     relative to the root of the repository under review"
                );
            }
            if p.split('/').any(|c| c == ".") {
                bail!(
                    "{at}.applies_to[{i}] '{p}' contains a '.' component; paths in a patch \
                     never do, so it would never match"
                );
            }
            if p.ends_with('/') {
                bail!(
                    "{at}.applies_to[{i}] '{p}' ends in '/', which matches no file; use \
                     '{p}**' to match everything under it"
                );
            }
            // Without literal_separator, `*` crosses '/', so
            // `eth/ionic/ionic_aux*` would also match files in directories
            // below eth/ionic, and `rdma/*.c` every C file under rdma/. Keeping
            // `*` inside one component, and leaving `**` to cross them, is what
            // an operator used to .gitignore-style patterns expects them to
            // mean.
            let glob = GlobBuilder::new(p)
                .literal_separator(true)
                .build()
                .map_err(|e| anyhow!("{at}.applies_to[{i}] '{p}': {e}"))?;
            builder.add(glob);
        }
        let set = builder
            .build()
            .map_err(|e| anyhow!("{at}.applies_to: {e}"))?;
        Ok(Self {
            patterns: patterns.to_vec(),
            set,
        })
    }

    /// The patterns as configured.
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    /// The first file that matches, with the first pattern it matches, or
    /// `None` when no file matches any pattern.
    pub fn first_match<'a>(&'a self, files: &'a [String]) -> Option<(&'a str, &'a str)> {
        files.iter().find_map(|file| {
            self.set
                .matches(file)
                .first()
                .map(|&i| (file.as_str(), self.patterns[i].as_str()))
        })
    }
}

/// Checks the rules that span the whole reference list, without touching git.
///
/// Runs before any repository is opened, so a configuration mistake is
/// reported as such rather than as whatever git makes of it. The per-entry
/// repository checks are in `git_ops::validate_reference_repository`.
pub fn validate_reference_list(refs: &[ReferenceSettings]) -> Result<()> {
    if refs.len() > MAX_REFERENCES {
        bail!(
            "git.references has {} entries; at most {} are supported",
            refs.len(),
            MAX_REFERENCES
        );
    }

    let mut seen = HashSet::new();
    let mut kernel: Option<&str> = None;
    for (index, entry) in refs.iter().enumerate() {
        let at = label(index, entry);
        if !valid_name(&entry.name) {
            bail!(
                "{at}: name '{}' must be 1-32 characters of a-z, 0-9, '.', '_' or '-', \
                 starting with a letter or digit",
                entry.name
            );
        }
        // "review" is how the model names the repository under review. A
        // reference called that would make the default unreachable.
        if entry.name == REPO_ARG_REVIEW {
            bail!("{at}: the name '{REPO_ARG_REVIEW}' is reserved for the repository under review");
        }
        if !seen.insert(entry.name.as_str()) {
            bail!("{at} is defined twice; reference names must be unique");
        }
        if entry.name == KERNEL_NAME && entry.kind != ReferenceKind::Kernel {
            bail!(
                "{at}.kind must be \"kernel\"; the name {KERNEL_NAME} is reserved for the \
                 kernel tree"
            );
        }
        // A kernel entry is what marks the review as out-of-tree and names the
        // tree in its caveats. Two would leave the prompt claiming two
        // different kernels the module builds against.
        if entry.kind == ReferenceKind::Kernel {
            if let Some(first) = kernel {
                bail!(
                    "{at} and git.references[{first}] are both kind \"kernel\"; at most one \
                     kernel reference is supported"
                );
            }
            kernel = Some(&entry.name);
        }
        if entry.kind == ReferenceKind::Source
            && entry
                .description
                .as_deref()
                .is_none_or(|d| d.trim().is_empty())
        {
            bail!(
                "{at}.description is required for kind \"source\": it is the only thing \
                 telling the model what the tree is for. Configure the entry in \
                 Settings.toml"
            );
        }
        // Checked here as well as in git_ops so the mistake is reported before
        // any git call. Every tool refuses a revision starting with '-'.
        if let Some(revision) = entry.revision.as_deref()
            && revision.starts_with('-')
        {
            bail!("{at}.revision '{revision}' is not a valid revision name");
        }
        // A kernel entry may be scoped too; docs/configuration.md explains
        // what that costs the patches it leaves it dormant for.
        if let Some(patterns) = entry.applies_to.as_deref() {
            AppliesTo::new(&at, patterns)?;
        }
    }
    Ok(())
}

/// A `--reference NAME=PATH[@REV]` worker flag.
#[derive(Debug, Clone, PartialEq)]
pub struct ReferenceOverride {
    pub name: String,
    /// Kept as the string it was given: it came from the command line as
    /// UTF-8 and goes into [`ReferenceSettings::path`], which is a string.
    pub path: String,
    pub revision: Option<String>,
}

/// A `--reference-revision NAME=REV` worker flag.
#[derive(Debug, Clone, PartialEq)]
pub struct RevisionOverride {
    pub name: String,
    pub revision: String,
}

/// Splits `NAME=VALUE` at the first `=`, refusing an empty side.
///
/// The name's syntax is left to [`validate_reference_list`], so there is one
/// rule for names whether they come from a flag or from Settings.toml.
fn split_name(flag: &str, s: &str, form: &str) -> Result<(String, String), String> {
    let (name, value) = s
        .split_once('=')
        .ok_or_else(|| format!("{flag} '{s}': expected {form}"))?;
    if name.is_empty() {
        return Err(format!("{flag} '{s}': NAME is empty; expected {form}"));
    }
    if value.is_empty() {
        return Err(format!("{flag} '{s}': nothing after '='; expected {form}"));
    }
    Ok((name.to_string(), value.to_string()))
}

/// Parses `--reference NAME=PATH[@REV]`.
///
/// `PATH@REV` splits at the last `@`, so a revision cannot contain one: give
/// such a revision with `--reference-revision`, which takes the whole value.
/// A path containing `@` must be followed by `@REV` (`@HEAD` for the clone's
/// current commit); without one, the part after the path's own `@` would be
/// taken as the revision.
pub fn parse_reference_flag(s: &str) -> Result<ReferenceOverride, String> {
    const FORM: &str = "NAME=PATH[@REV]";
    let (name, value) = split_name("--reference", s, FORM)?;
    let (path, revision) = match value.rsplit_once('@') {
        Some((path, revision)) => {
            if revision.is_empty() {
                return Err(format!(
                    "--reference '{s}': nothing after '@'; expected {FORM}"
                ));
            }
            (path.to_string(), Some(revision.to_string()))
        }
        None => (value, None),
    };
    if path.is_empty() {
        return Err(format!("--reference '{s}': PATH is empty; expected {FORM}"));
    }
    Ok(ReferenceOverride {
        name,
        path,
        revision,
    })
}

/// Parses `--reference-revision NAME=REV`.
pub fn parse_reference_revision_flag(s: &str) -> Result<RevisionOverride, String> {
    let (name, revision) = split_name("--reference-revision", s, "NAME=REV")?;
    Ok(RevisionOverride { name, revision })
}

/// Merges the worker's reference flags over the configured list, by name.
///
/// On the `--repo` and `--settings` paths `settings` is empty, because those
/// load `LocalReviewSettings`, which has no `[git]` section; the flags are the
/// only source there.
pub fn merge_references(
    settings: Vec<ReferenceSettings>,
    refs: &[ReferenceOverride],
    revs: &[RevisionOverride],
) -> Result<Vec<ReferenceSettings>> {
    // Two flags for one name would leave the result depending on flag order,
    // which nobody reading a launch script expects. Refuse instead of picking.
    let mut seen = HashSet::new();
    for r in refs {
        if !seen.insert(r.name.as_str()) {
            bail!("--reference {} is given twice", r.name);
        }
    }
    let mut seen = HashSet::new();
    for r in revs {
        if !seen.insert(r.name.as_str()) {
            bail!("--reference-revision {} is given twice", r.name);
        }
    }

    let mut merged = settings;
    for r in refs {
        match merged.iter_mut().find(|e| e.name == r.name) {
            // A new path drops the configured pin: a revision chosen for one
            // clone means nothing in another. Kind, description, applies_to and
            // position stay, since they describe what the tree is for, not
            // where it is.
            Some(entry) => {
                entry.path = r.path.clone();
                entry.revision = r.revision.clone();
            }
            // Only a kernel entry can be complete from a flag alone: a source
            // entry needs a description, which validation then asks for.
            None => merged.push(ReferenceSettings {
                name: r.name.clone(),
                kind: if r.name == KERNEL_NAME {
                    ReferenceKind::Kernel
                } else {
                    ReferenceKind::Source
                },
                path: r.path.clone(),
                revision: r.revision.clone(),
                description: None,
                // A flag cannot say which patches a tree is for; scope it in
                // Settings.toml.
                applies_to: None,
            }),
        }
    }

    // Applied after every --reference, so a re-pin wins over an @REV given in
    // the same invocation. A re-pin of a name that exists nowhere is a typo,
    // and reviewing against the wrong revision silently is worse than failing.
    for r in revs {
        let Some(entry) = merged.iter_mut().find(|e| e.name == r.name) else {
            let names: Vec<&str> = merged.iter().map(|e| e.name.as_str()).collect();
            bail!(
                "--reference-revision {}: no reference named {}; configured: {}",
                r.name,
                r.name,
                if names.is_empty() {
                    "none".to_string()
                } else {
                    names.join(", ")
                }
            );
        };
        entry.revision = Some(r.revision.clone());
    }
    Ok(merged)
}

fn names(refs: &[ReferenceSettings]) -> String {
    refs.iter()
        .map(|e| e.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Keeps the reference list only for a project whose review can use it.
///
/// Only the Linux prompt tells the model what reference trees are for. Anywhere
/// else they would be checkouts offered with no explanation, so the list is
/// dropped rather than half wired up. The daemon refuses to start in that
/// configuration ([`reference_refusal`]); this covers the worker and `sashiko
/// review`, which do not go through it.
pub fn references_for_project(
    project: ProjectId,
    refs: Vec<ReferenceSettings>,
) -> Vec<ReferenceSettings> {
    if !refs.is_empty() && !project.supports_reference_repositories() {
        warn!(
            "Ignoring git.references {}: project '{project}' does not support reference \
             repositories",
            names(&refs)
        );
        return Vec::new();
    }
    refs
}

/// Why the daemon must not start with this reference list, if it must not.
///
/// Lives here rather than in the binary so the daemon and the worker share one
/// wording, and so it can be tested without the bin.
pub fn reference_refusal(project: ProjectId, refs: &[ReferenceSettings]) -> Option<String> {
    (!refs.is_empty() && !project.supports_reference_repositories()).then(|| {
        format!(
            "git.references {} is set, but project '{project}' does not support reference \
             repositories; they are only used for Linux reviews",
            names(refs)
        )
    })
}

/// Validates the list and every entry, and pins each one to a commit.
///
/// Called at daemon start, where the result is discarded, and at every worker
/// start, where it is what the review reads. The path is made absolute so it
/// means the same thing whatever directory each tool's git command runs in, and
/// the revision is pinned to the commit it resolved to now, so a fetch in the
/// clone during the review cannot move what the rest of it reads.
pub async fn resolve_references(refs: &[ReferenceSettings]) -> Result<Vec<ReferenceRepo>> {
    validate_reference_list(refs)?;

    let mut resolved: Vec<ReferenceRepo> = Vec::with_capacity(refs.len());
    for (index, entry) in refs.iter().enumerate() {
        let path = PathBuf::from(&entry.path);
        let commit = crate::git_ops::validate_reference_repository(
            &entry.name,
            &path,
            entry.revision.as_deref(),
        )
        .await?;
        let path = std::fs::canonicalize(&path).unwrap_or(path);

        // One clone may back two entries at different commits -- net and
        // net-next pins, say -- because every read goes to a resolved commit
        // and the checkout never matters. Two entries at the same commit of the
        // same clone are the same tree under two names, which only splits the
        // model's reads between them.
        if let Some(twin) = resolved
            .iter()
            .find(|r| r.path == path && r.commit == commit)
        {
            return Err(anyhow!(
                "git.references[{}] reads the same commit of {} as git.references[{}] ({}); \
                 drop one",
                entry.name,
                path.display(),
                twin.name,
                commit
            ));
        }

        resolved.push(ReferenceRepo {
            name: entry.name.clone(),
            kind: entry.kind,
            path,
            revision: entry.revision.clone().unwrap_or_else(|| "HEAD".to_string()),
            commit,
            description: entry.description.clone(),
            applies_to: entry
                .applies_to
                .as_deref()
                .map(|p| AppliesTo::new(&label(index, entry), p))
                .transpose()?,
        });
    }
    Ok(resolved)
}

/// The references one patch is reviewed with, and those it is not.
#[derive(Debug, Clone, Default)]
pub struct PatchReferences {
    /// Offered to the model for this patch, in configured order.
    pub attached: Vec<ReferenceRepo>,
    /// Configured but not attached to this patch, in configured order. Kept
    /// only so that naming one gets a specific error.
    pub dormant: Vec<DormantReference>,
    /// Why each attached reference is attached, parallel to `attached`.
    reasons: Vec<AttachReason>,
}

/// Why a reference is attached to a patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachReason {
    /// The reference has no `applies_to`, so it goes with every patch.
    Always,
    /// The first touched file that matched, and the first pattern it matched.
    Match { file: String, glob: String },
}

impl std::fmt::Display for AttachReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttachReason::Always => f.write_str("every patch"),
            AttachReason::Match { file, glob } => write!(f, "{file} matches {glob}"),
        }
    }
}

/// Decides which references a patch is reviewed with, from the files it
/// touches.
///
/// An unscoped reference is attached to every patch. A scoped one is attached
/// when any touched file matches any of its patterns, and is dormant otherwise.
/// `touched` should carry both sides of a rename, so a file moving out of the
/// tree a reference is scoped to still brings the reference along.
pub fn attach_for_patch(refs: &[ReferenceRepo], touched: &[String]) -> PatchReferences {
    let mut out = PatchReferences::default();
    for r in refs {
        let reason = match &r.applies_to {
            None => Some(AttachReason::Always),
            Some(scope) => scope
                .first_match(touched)
                .map(|(file, pattern)| AttachReason::Match {
                    file: file.to_string(),
                    glob: pattern.to_string(),
                }),
        };
        match reason {
            Some(reason) => {
                out.attached.push(r.clone());
                out.reasons.push(reason);
            }
            None => out.dormant.push(DormantReference {
                name: r.name.clone(),
                applies_to: r
                    .applies_to
                    .as_ref()
                    .map(|a| a.patterns().to_vec())
                    .unwrap_or_default(),
            }),
        }
    }
    out
}

impl PatchReferences {
    /// One log line saying which references the patch was reviewed with and
    /// why, and which it was not and why, or `None` with none configured.
    ///
    /// This is the record an operator reads to learn why a review did or did
    /// not consult a tree: the prompt shows only what was attached.
    pub fn summary(&self, patch_index: i64) -> Option<String> {
        if self.attached.is_empty() && self.dormant.is_empty() {
            return None;
        }
        let attached = if self.attached.is_empty() {
            "none".to_string()
        } else {
            self.attached
                .iter()
                .zip(&self.reasons)
                .map(|(r, why)| format!("{} ({why})", r.name))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let dormant = if self.dormant.is_empty() {
            "none".to_string()
        } else {
            self.dormant
                .iter()
                .map(|d| {
                    format!(
                        "{} (applies only to changes matching {}; this patch touches none \
                         of them)",
                        d.name,
                        d.applies_to.join(", ")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        Some(format!(
            "Patch {patch_index} references: attached {attached}; dormant {dormant}"
        ))
    }

    /// What the review row records about this patch's references, or `None`
    /// with none configured, so an unconfigured deployment stores NULL.
    ///
    /// `reads[i]` is the number of tool calls that named `attached[i]`. It is
    /// `None` for the record sent before the review runs, and stays missing if
    /// the review never finishes, which a reader must not mistake for zero.
    pub fn record(&self, reads: Option<&[u64]>) -> Option<ReviewReferences> {
        if self.attached.is_empty() && self.dormant.is_empty() {
            return None;
        }
        let attached = self
            .attached
            .iter()
            .zip(&self.reasons)
            .enumerate()
            .map(|(i, (r, why))| {
                let (why, file, glob) = match why {
                    AttachReason::Always => (AttachWhy::Always, None, None),
                    AttachReason::Match { file, glob } => (
                        AttachWhy::Match,
                        Some(truncate_path(file)),
                        Some(glob.clone()),
                    ),
                };
                AttachedReference {
                    name: r.name.clone(),
                    kind: r.kind,
                    revision: r.revision.clone(),
                    commit: r.commit.clone(),
                    why,
                    file,
                    glob,
                    reads: reads.and_then(|n| n.get(i).copied()),
                }
            })
            .collect();
        let dormant = self
            .dormant
            .iter()
            .map(|d| DormantRecord {
                name: d.name.clone(),
                applies_to: d.applies_to.clone(),
            })
            .collect();
        Some(ReviewReferences { attached, dormant })
    }
}

/// Longest matched file kept in a record, in bytes.
const MAX_RECORDED_PATH: usize = 256;

/// Cuts a matched file name to [`MAX_RECORDED_PATH`] bytes on a character
/// boundary, marking the cut with "...".
///
/// The name comes from the patch, which anyone can send, so its length is not
/// ours to choose; the record is meant to stay a line on a review card.
fn truncate_path(file: &str) -> String {
    if file.len() <= MAX_RECORDED_PATH {
        return file.to_string();
    }
    let mut end = MAX_RECORDED_PATH;
    while !file.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &file[..end])
}

/// Which references one review had attached and which it did not, as stored
/// in `reviews.reference_repos` and returned by the API.
///
/// Kept on the review row because the per-patch log line is the only other
/// record, and the deployment's logs last about an hour. Unknown fields are
/// refused so that a malformed hand-off from a worker is dropped, not stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewReferences {
    pub attached: Vec<AttachedReference>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dormant: Vec<DormantRecord>,
}

/// One reference attached to the review.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachedReference {
    pub name: String,
    pub kind: ReferenceKind,
    /// The revision as configured, e.g. a release branch.
    pub revision: String,
    /// The commit `revision` resolved to for this review.
    pub commit: String,
    pub why: AttachWhy,
    /// For [`AttachWhy::Match`], the first touched file that matched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// For [`AttachWhy::Match`], the pattern `file` matched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    /// Tool calls that named this reference, cache hits and refused calls
    /// included. Missing until the worker finishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reads: Option<u64>,
}

/// The kind of [`AttachReason`], flattened beside `file` and `glob` in the
/// record. Not a tagged enum because serde cannot combine `flatten` with
/// `deny_unknown_fields`, and refusing unknown fields matters more here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachWhy {
    Always,
    Match,
}

/// A configured reference the review did not have, and what it applies to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DormantRecord {
    pub name: String,
    pub applies_to: Vec<String>,
}

/// The `type` of the stdout line a daemon-spawned worker sends with
/// [`ReviewReferences`] as soon as the patch's references are decided.
pub const REFERENCE_REPOS_MESSAGE: &str = "reference_repos";

/// The worker's side of the early hand-off: one JSON line for the daemon.
///
/// Both sides of the wire live here so one test covers the contract.
pub fn handoff_line(patch_index: i64, rec: &ReviewReferences) -> Result<String> {
    Ok(serde_json::to_string(&serde_json::json!({
        "type": REFERENCE_REPOS_MESSAGE,
        "patch_index": patch_index,
        "payload": rec,
    }))?)
}

/// The daemon's side of the early hand-off: the patch index and the record,
/// or `None` for anything that is not a well-formed hand-off.
///
/// More entries than a worker can be configured with means the line did not
/// come from a worker this daemon would run, so it is refused rather than
/// stored.
pub fn parse_handoff(msg: &serde_json::Value) -> Option<(i64, ReviewReferences)> {
    if msg.get("type")?.as_str()? != REFERENCE_REPOS_MESSAGE {
        return None;
    }
    let patch_index = msg.get("patch_index")?.as_i64()?;
    let rec = parse_record(msg.get("payload")?)?;
    Some((patch_index, rec))
}

/// A stored or reported record, if it is well formed and no larger than any
/// configuration could produce.
pub fn parse_record(value: &serde_json::Value) -> Option<ReviewReferences> {
    let rec: ReviewReferences = serde_json::from_value(value.clone()).ok()?;
    (rec.attached.len() + rec.dormant.len() <= MAX_REFERENCES).then_some(rec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: ReferenceKind, description: Option<&str>) -> ReferenceSettings {
        ReferenceSettings {
            name: name.to_string(),
            kind,
            path: format!("/srv/{name}"),
            revision: None,
            description: description.map(str::to_string),
            applies_to: None,
        }
    }

    fn kernel() -> ReferenceSettings {
        entry("kernel", ReferenceKind::Kernel, None)
    }

    fn source(name: &str) -> ReferenceSettings {
        entry(name, ReferenceKind::Source, Some("A userspace tree."))
    }

    fn list_err(refs: &[ReferenceSettings]) -> String {
        validate_reference_list(refs)
            .expect_err("the list must be refused")
            .to_string()
    }

    #[test]
    fn a_kernel_and_a_described_source_are_accepted() {
        validate_reference_list(&[kernel(), source("rdma-core")]).unwrap();
        validate_reference_list(&[]).unwrap();
    }

    #[test]
    fn names_must_be_short_lowercase_and_plain() {
        for bad in [
            "Rdma",
            "rdma core",
            "-rdma",
            ".rdma",
            "rdma/core",
            &"a".repeat(33),
        ] {
            let err = list_err(&[source(bad)]);
            assert!(err.contains(&format!("git.references[{bad}]")), "{err}");
        }
        // 32 characters is the limit, not past it.
        validate_reference_list(&[source(&"a".repeat(32))]).unwrap();
        validate_reference_list(&[source("0day"), source("net-next.v6_18")]).unwrap();

        let err = list_err(&[source("rdma-core"), source("")]);
        assert!(err.contains("git.references[#1]"), "{err}");
    }

    #[test]
    fn names_must_be_unique() {
        let err = list_err(&[source("rdma-core"), source("rdma-core")]);
        assert!(err.contains("git.references[rdma-core]"), "{err}");
        assert!(err.contains("twice"), "{err}");
    }

    #[test]
    fn review_is_reserved() {
        let err = list_err(&[source("review")]);
        assert!(err.contains("git.references[review]"), "{err}");
        assert!(err.contains("reserved"), "{err}");
    }

    #[test]
    fn the_name_kernel_needs_kind_kernel() {
        let err = list_err(&[source("kernel")]);
        assert!(err.contains("git.references[kernel].kind"), "{err}");
    }

    #[test]
    fn at_most_one_kernel_entry() {
        let err = list_err(&[kernel(), entry("net-next", ReferenceKind::Kernel, None)]);
        assert!(err.contains("git.references[net-next]"), "{err}");
        assert!(err.contains("git.references[kernel]"), "{err}");
    }

    #[test]
    fn at_most_eight_entries() {
        let refs: Vec<_> = (0..9).map(|i| source(&format!("r{i}"))).collect();
        let err = list_err(&refs);
        assert!(err.contains("git.references has 9 entries"), "{err}");
        validate_reference_list(&refs[..8]).unwrap();
    }

    #[test]
    fn a_source_entry_needs_a_description() {
        for description in [None, Some(""), Some("  \n\t")] {
            let err = list_err(&[entry("rdma-core", ReferenceKind::Source, description)]);
            assert!(
                err.contains("git.references[rdma-core].description"),
                "{err}"
            );
        }
    }

    #[test]
    fn a_revision_must_not_look_like_an_option() {
        let mut e = source("rdma-core");
        e.revision = Some("-x".to_string());
        let err = list_err(&[e]);
        assert!(err.contains("git.references[rdma-core].revision"), "{err}");
    }

    #[test]
    fn reference_flags_parse() {
        let ok = |s: &str, name: &str, path: &str, rev: Option<&str>| {
            assert_eq!(
                parse_reference_flag(s).unwrap(),
                ReferenceOverride {
                    name: name.to_string(),
                    path: path.to_string(),
                    revision: rev.map(str::to_string),
                },
                "{s}"
            );
        };
        ok("kernel=/srv/linux", "kernel", "/srv/linux", None);
        ok(
            "kernel=/srv/linux@v6.18",
            "kernel",
            "/srv/linux",
            Some("v6.18"),
        );
        ok(
            "rdma-core=/srv/rdma@origin/ionic-rdma-v59",
            "rdma-core",
            "/srv/rdma",
            Some("origin/ionic-rdma-v59"),
        );
        // The last '@' splits, so a path may contain one only if a revision
        // follows it.
        ok("x=/a@b@c", "x", "/a@b", Some("c"));
        ok(
            "kernel=/home/a@b/linux@v6.18",
            "kernel",
            "/home/a@b/linux",
            Some("v6.18"),
        );
        // A '=' in the path is kept: only the first one separates the name.
        ok("x=/a=b", "x", "/a=b", None);

        for bad in ["kernel", "=/p", "kernel=", "kernel=/p@", "kernel=@v6.18"] {
            assert!(parse_reference_flag(bad).is_err(), "{bad} must not parse");
        }
    }

    #[test]
    fn revision_flags_parse() {
        assert_eq!(
            parse_reference_revision_flag("rdma-core=ionic-rdma@v59").unwrap(),
            RevisionOverride {
                name: "rdma-core".to_string(),
                revision: "ionic-rdma@v59".to_string(),
            },
            "the whole value is the revision, '@' and all"
        );
        for bad in ["v6.12", "=v6.12", "kernel="] {
            assert!(
                parse_reference_revision_flag(bad).is_err(),
                "{bad} must not parse"
            );
        }
    }

    fn path_flag(name: &str, path: &str, revision: Option<&str>) -> ReferenceOverride {
        ReferenceOverride {
            name: name.to_string(),
            path: path.to_string(),
            revision: revision.map(str::to_string),
        }
    }

    fn rev_flag(name: &str, revision: &str) -> RevisionOverride {
        RevisionOverride {
            name: name.to_string(),
            revision: revision.to_string(),
        }
    }

    fn configured() -> Vec<ReferenceSettings> {
        let mut k = kernel();
        k.revision = Some("v6.12".to_string());
        let mut r = source("rdma-core");
        r.revision = Some("ionic-rdma-v59".to_string());
        r.applies_to = Some(vec!["rdma/drv/**".to_string()]);
        vec![k, r]
    }

    /// Flags override settings by name: a path flag replaces an entry's tree
    /// and drops its pin, a revision flag re-pins, and neither moves the entry
    /// or forgets what it is for.
    #[test]
    fn reference_flags_override_settings() {
        assert_eq!(
            merge_references(configured(), &[], &[]).unwrap(),
            configured()
        );

        let merged = merge_references(
            configured(),
            &[path_flag("kernel", "/srv/other", None)],
            &[],
        )
        .unwrap();
        assert_eq!(merged[0].name, "kernel", "the entry keeps its position");
        assert_eq!(merged[0].path, "/srv/other");
        assert_eq!(
            merged[0].revision, None,
            "a different tree must not inherit the configured pin"
        );
        assert_eq!(merged[0].kind, ReferenceKind::Kernel);
        assert_eq!(merged[1], configured()[1], "other entries are untouched");

        let merged = merge_references(
            configured(),
            &[path_flag("rdma-core", "/srv/rdma", Some("v60"))],
            &[],
        )
        .unwrap();
        assert_eq!(merged[1].path, "/srv/rdma");
        assert_eq!(merged[1].revision.as_deref(), Some("v60"));
        assert_eq!(
            merged[1].description,
            configured()[1].description,
            "the description describes the tree, not the path"
        );
        assert_eq!(
            merged[1].applies_to,
            configured()[1].applies_to,
            "which patches the tree is for does not depend on where it is"
        );

        let merged = merge_references(configured(), &[], &[rev_flag("kernel", "v6.13")]).unwrap();
        assert_eq!(merged[0].path, "/srv/kernel");
        assert_eq!(merged[0].revision.as_deref(), Some("v6.13"));

        // The revision flag is applied last and wins over @REV.
        let merged = merge_references(
            configured(),
            &[path_flag("kernel", "/srv/other", Some("v6.14"))],
            &[rev_flag("kernel", "v6.15")],
        )
        .unwrap();
        assert_eq!(merged[0].revision.as_deref(), Some("v6.15"));
    }

    #[test]
    fn an_unknown_revision_flag_lists_the_configured_names() {
        let err = merge_references(configured(), &[], &[rev_flag("iproute2", "v6.1")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("iproute2"), "{err}");
        assert!(err.contains("kernel, rdma-core"), "{err}");

        let err = merge_references(vec![], &[], &[rev_flag("kernel", "v6.1")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("configured: none"), "{err}");
    }

    #[test]
    fn duplicate_flags_are_refused() {
        let err = merge_references(
            configured(),
            &[
                path_flag("kernel", "/a", None),
                path_flag("kernel", "/b", None),
            ],
            &[],
        )
        .unwrap_err();
        assert!(err.to_string().contains("twice"), "{err}");

        let err = merge_references(
            configured(),
            &[],
            &[rev_flag("kernel", "v1"), rev_flag("kernel", "v2")],
        )
        .unwrap_err();
        assert!(err.to_string().contains("twice"), "{err}");
    }

    /// A flag can add a kernel tree on its own, as the old single-reference
    /// flag could. Any other new tree lacks the description the model needs,
    /// and validation says so.
    #[test]
    fn new_names_from_flags() {
        let merged = merge_references(
            vec![],
            &[path_flag("kernel", "/srv/linux", Some("v6.18"))],
            &[],
        )
        .unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].kind, ReferenceKind::Kernel);
        assert_eq!(merged[0].applies_to, None, "a flag-added entry is unscoped");
        validate_reference_list(&merged).unwrap();

        let merged =
            merge_references(configured(), &[path_flag("iproute2", "/srv/ip", None)], &[]).unwrap();
        assert_eq!(merged[2].name, "iproute2", "a new entry is appended");
        assert_eq!(merged[2].kind, ReferenceKind::Source);
        let err = validate_reference_list(&merged).unwrap_err().to_string();
        assert!(
            err.contains("git.references[iproute2].description"),
            "{err}"
        );
        assert!(err.contains("Settings.toml"), "{err}");
    }

    /// A Linux review keeps its references; any other project drops them,
    /// because only the Linux prompt explains reference trees to the model.
    #[test]
    fn references_are_kept_only_for_projects_that_can_use_them() {
        assert_eq!(
            references_for_project(ProjectId::Linux, configured()),
            configured()
        );
        assert!(references_for_project(ProjectId::Sashiko, configured()).is_empty());
        assert!(references_for_project(ProjectId::Linux, vec![]).is_empty());
    }

    /// The daemon refuses references only for a project whose prompt cannot
    /// explain them, and never when none are configured.
    #[test]
    fn reference_refusal_names_every_entry() {
        assert!(reference_refusal(ProjectId::Linux, &configured()).is_none());
        assert!(reference_refusal(ProjectId::Linux, &[]).is_none());
        assert!(reference_refusal(ProjectId::Sashiko, &[]).is_none());
        let reason = reference_refusal(ProjectId::Sashiko, &configured())
            .expect("a non-Linux project with references must be refused");
        for needle in ["git.references", "kernel", "rdma-core", "sashiko"] {
            assert!(reason.contains(needle), "missing {needle}: {reason}");
        }
    }

    fn scoped(name: &str, patterns: &[&str]) -> ReferenceSettings {
        let mut e = source(name);
        e.applies_to = Some(patterns.iter().map(|p| p.to_string()).collect());
        e
    }

    #[test]
    fn applies_to_patterns_are_validated() {
        let err = list_err(&[scoped("rdma-core", &["rdma/drv/**", "rdma/[drv"])]);
        assert!(
            err.contains("git.references[rdma-core].applies_to[1]"),
            "{err}"
        );
        assert!(err.contains("rdma/[drv"), "{err}");

        for (bad, why) in [
            ("/rdma/**", "absolute"),
            ("rdma/../eth/**", "'..'"),
            ("../x", "'..'"),
            ("./rdma/**", "'.' component"),
            ("rdma/drv/", "'rdma/drv/**'"),
            ("", "is empty"),
        ] {
            let err = list_err(&[scoped("rdma-core", &["rdma/**", bad])]);
            assert!(
                err.contains("git.references[rdma-core].applies_to[1]"),
                "{bad}: {err}"
            );
            assert!(err.contains(why), "{bad}: {err}");
        }

        let err = list_err(&[scoped("rdma-core", &[])]);
        assert!(
            err.contains("git.references[rdma-core].applies_to is empty"),
            "{err}"
        );

        validate_reference_list(&[source("rdma-core")]).unwrap();
        validate_reference_list(&[scoped("rdma-core", &["rdma/drv/**", "include/uapi/rdma/*"])])
            .unwrap();
        let mut k = kernel();
        k.applies_to = Some(vec!["rdma/drv/**".to_string()]);
        validate_reference_list(&[k]).unwrap();
    }

    fn matches(pattern: &str, file: &str) -> bool {
        AppliesTo::new("t", &[pattern.to_string()])
            .unwrap()
            .first_match(&[file.to_string()])
            .is_some()
    }

    #[test]
    fn applies_to_globs_do_not_cross_directories_with_a_single_star() {
        assert!(matches("eth/ionic/ionic_aux*", "eth/ionic/ionic_aux.c"));
        assert!(!matches("eth/ionic/ionic_aux*", "eth/ionic/ionic_aux/x.c"));
        assert!(!matches("rdma/*.c", "rdma/drv/a.c"));
        assert!(matches("rdma/drv/**", "rdma/drv/ionic/ionic_fw.h"));
        assert!(!matches("*.h", "include/x.h"));
        assert!(matches(
            "**/ionic-abi.h",
            "rdma/drv/ionic/uapi/rdma/ionic-abi.h"
        ));
    }

    /// A resolved reference without touching git: what attach_for_patch sees.
    fn repo(name: &str, kind: ReferenceKind, patterns: Option<&[&str]>) -> ReferenceRepo {
        ReferenceRepo {
            name: name.to_string(),
            kind,
            path: PathBuf::from(format!("/srv/{name}")),
            revision: "HEAD".to_string(),
            commit: "0".repeat(40),
            description: Some("A tree.".to_string()),
            applies_to: patterns.map(|p| {
                AppliesTo::new(name, &p.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
            }),
        }
    }

    fn driver_a() -> Vec<ReferenceRepo> {
        vec![
            repo("kernel", ReferenceKind::Kernel, None),
            repo(
                "rdma-core",
                ReferenceKind::Source,
                Some(&[
                    "rdma/drv/**",
                    "eth/ionic/ionic_aux*",
                    "include/uapi/rdma/**",
                ]),
            ),
        ]
    }

    fn touched(files: &[&str]) -> Vec<String> {
        files.iter().map(|f| f.to_string()).collect()
    }

    fn attached_names(p: &PatchReferences) -> Vec<&str> {
        p.attached.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn an_ethernet_patch_attaches_only_the_kernel() {
        let p = attach_for_patch(&driver_a(), &touched(&["eth/ionic/ionic_lif.c"]));
        assert_eq!(attached_names(&p), ["kernel"]);
        assert_eq!(p.reasons, [AttachReason::Always]);
        assert_eq!(p.dormant.len(), 1);
        assert_eq!(p.dormant[0].name, "rdma-core");
        assert_eq!(
            p.dormant[0].applies_to,
            [
                "rdma/drv/**",
                "eth/ionic/ionic_aux*",
                "include/uapi/rdma/**"
            ]
        );
        let line = p.summary(2).unwrap();
        assert!(line.starts_with("Patch 2 references: "), "{line}");
        assert!(line.contains("attached kernel (every patch)"), "{line}");
        assert!(
            line.contains(
                "dormant rdma-core (applies only to changes matching rdma/drv/**, \
                 eth/ionic/ionic_aux*, include/uapi/rdma/**"
            ),
            "{line}"
        );
    }

    #[test]
    fn an_rdma_patch_attaches_kernel_and_rdma_core() {
        let p = attach_for_patch(&driver_a(), &touched(&["rdma/drv/ionic/ionic_fw.h"]));
        assert_eq!(attached_names(&p), ["kernel", "rdma-core"]);
        assert!(p.dormant.is_empty());
        assert_eq!(
            p.reasons,
            [
                AttachReason::Always,
                AttachReason::Match {
                    file: "rdma/drv/ionic/ionic_fw.h".to_string(),
                    glob: "rdma/drv/**".to_string(),
                }
            ]
        );
        let line = p.summary(1).unwrap();
        assert!(
            line.contains("rdma-core (rdma/drv/ionic/ionic_fw.h matches rdma/drv/**)"),
            "{line}"
        );
        assert!(line.ends_with("dormant none"), "{line}");

        assert!(
            attach_for_patch(&[], &touched(&["a.c"]))
                .summary(1)
                .is_none()
        );

        // Everything dormant still earns a line, so the operator sees why.
        let only_rdma = &driver_a()[1..];
        let line = attach_for_patch(only_rdma, &touched(&["eth/a.c"]))
            .summary(3)
            .unwrap();
        assert!(line.contains("attached none; dormant rdma-core"), "{line}");
    }

    #[test]
    fn a_rename_out_of_a_matching_path_attaches() {
        let p = attach_for_patch(
            &driver_a(),
            &touched(&["eth/ionic/x.c", "rdma/drv/ionic/x.c"]),
        );
        assert_eq!(attached_names(&p), ["kernel", "rdma-core"]);
        assert_eq!(
            p.reasons[1],
            AttachReason::Match {
                file: "rdma/drv/ionic/x.c".to_string(),
                glob: "rdma/drv/**".to_string(),
            }
        );
    }

    #[test]
    fn record_says_why_each_reference_was_attached() {
        let p = attach_for_patch(&driver_a(), &touched(&["rdma/drv/ionic/ionic_fw.h"]));
        let rec = p.record(Some(&[3, 1])).unwrap();
        let zeros = "0".repeat(40);
        assert_eq!(
            serde_json::to_string(&rec).unwrap(),
            format!(
                "{{\"attached\":[\
                 {{\"name\":\"kernel\",\"kind\":\"kernel\",\"revision\":\"HEAD\",\
                 \"commit\":\"{zeros}\",\"why\":\"always\",\"reads\":3}},\
                 {{\"name\":\"rdma-core\",\"kind\":\"source\",\"revision\":\"HEAD\",\
                 \"commit\":\"{zeros}\",\"why\":\"match\",\
                 \"file\":\"rdma/drv/ionic/ionic_fw.h\",\"glob\":\"rdma/drv/**\",\"reads\":1}}]}}"
            )
        );
    }

    #[test]
    fn record_lists_dormant_references() {
        let p = attach_for_patch(&driver_a(), &touched(&["eth/ionic/ionic_lif.c"]));
        let rec = p.record(None).unwrap();
        assert_eq!(
            rec.dormant,
            [DormantRecord {
                name: "rdma-core".to_string(),
                applies_to: vec![
                    "rdma/drv/**".to_string(),
                    "eth/ionic/ionic_aux*".to_string(),
                    "include/uapi/rdma/**".to_string(),
                ],
            }]
        );
        let json = serde_json::to_string(&rec).unwrap();
        assert!(
            !json.contains("reads"),
            "an unfinished review has no count: {json}"
        );
        assert!(
            attach_for_patch(&[], &touched(&["a.c"]))
                .record(None)
                .is_none()
        );
        assert!(
            attach_for_patch(&[], &touched(&["a.c"]))
                .record(Some(&[]))
                .is_none()
        );
    }

    #[test]
    fn record_truncates_a_long_matched_file() {
        // Multi-byte characters put a char boundary off the 256-byte mark.
        let long = format!("rdma/drv/{}", "\u{e9}".repeat(500));
        let p = attach_for_patch(&driver_a(), std::slice::from_ref(&long));
        let rec = p.record(None).unwrap();
        let file = rec.attached[1].file.as_deref().unwrap();
        assert!(file.ends_with("..."), "{file}");
        let kept = file.strip_suffix("...").unwrap();
        assert!(kept.len() <= MAX_RECORDED_PATH, "{}", kept.len());
        assert!(long.starts_with(kept));
        assert_eq!(truncate_path("a/b.c"), "a/b.c");
    }

    #[test]
    fn handoff_round_trips() {
        let p = attach_for_patch(&driver_a(), &touched(&["eth/ionic/ionic_lif.c"]));
        let rec = p.record(None).unwrap();
        let line = handoff_line(2, &rec).unwrap();
        assert!(!line.contains('\n'), "one line on the wire");
        let msg: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parse_handoff(&msg), Some((2, rec.clone())));

        let mut wrong_type = msg.clone();
        wrong_type["type"] = "ai_request".into();
        assert_eq!(parse_handoff(&wrong_type), None);

        let mut unknown = msg.clone();
        unknown["payload"]["attached"][0]["extra"] = 1.into();
        assert_eq!(parse_handoff(&unknown), None);

        let mut no_commit = msg.clone();
        no_commit["payload"]["attached"][0]
            .as_object_mut()
            .unwrap()
            .remove("commit");
        assert_eq!(parse_handoff(&no_commit), None);

        let mut too_many = rec.clone();
        too_many.dormant = (0..MAX_REFERENCES)
            .map(|i| DormantRecord {
                name: format!("r{i}"),
                applies_to: vec!["x/**".to_string()],
            })
            .collect();
        let msg: serde_json::Value =
            serde_json::from_str(&handoff_line(2, &too_many).unwrap()).unwrap();
        assert_eq!(parse_handoff(&msg), None);
    }

    async fn git(dir: &std::path::Path, args: &[&str]) {
        let status = crate::git_cmd::in_dir_async(dir)
            .args(args)
            .status()
            .await
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// One clone may back two entries at different commits; the same commit
    /// twice is refused, naming both entries.
    #[tokio::test]
    async fn resolution_refuses_the_same_commit_of_one_clone_twice() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("linux");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init"]).await;
        git(&repo, &["config", "user.name", "T"]).await;
        git(&repo, &["config", "user.email", "t@e"]).await;
        git(&repo, &["commit", "--allow-empty", "-m", "a"]).await;
        git(&repo, &["tag", "v1"]).await;
        git(&repo, &["commit", "--allow-empty", "-m", "b"]).await;
        git(&repo, &["tag", "v2"]).await;

        let at = |name: &str, kind, revision: Option<&str>| ReferenceSettings {
            name: name.to_string(),
            kind,
            path: repo.to_string_lossy().into_owned(),
            revision: revision.map(str::to_string),
            description: Some("A tree.".to_string()),
            applies_to: None,
        };

        let resolved = resolve_references(&[
            at("net", ReferenceKind::Kernel, Some("v1")),
            at("net-next", ReferenceKind::Source, Some("v2")),
        ])
        .await
        .expect("one clone at two commits is allowed");
        assert_eq!(resolved.len(), 2);
        assert_ne!(resolved[0].commit, resolved[1].commit);
        assert_eq!(resolved[0].name, "net");
        assert_eq!(resolved[0].revision, "v1");
        assert!(resolved[0].path.is_absolute());
        assert!(resolved[0].applies_to.is_none());

        // A scoped entry carries its compiled patterns into the review.
        let mut scoped = at("net-next", ReferenceKind::Source, Some("v2"));
        scoped.applies_to = Some(vec!["rdma/drv/**".to_string(), "include/**".to_string()]);
        let resolved = resolve_references(&[scoped]).await.unwrap();
        let scope = resolved[0].applies_to.as_ref().expect("scope is carried");
        assert_eq!(scope.patterns(), ["rdma/drv/**", "include/**"]);
        let touched = vec!["include/x.h".to_string()];
        assert_eq!(
            scope.first_match(&touched),
            Some(("include/x.h", "include/**"))
        );

        // v2 and HEAD are the same commit.
        let err = resolve_references(&[
            at("net-next", ReferenceKind::Source, Some("v2")),
            at("tip", ReferenceKind::Source, None),
        ])
        .await
        .expect_err("the same commit of one clone twice must be refused")
        .to_string();
        assert!(err.contains("git.references[tip]"), "{err}");
        assert!(err.contains("git.references[net-next]"), "{err}");

        // An entry that does not resolve is named by the per-entry check.
        let err = resolve_references(&[at("net", ReferenceKind::Kernel, Some("v9"))])
            .await
            .expect_err("an unresolvable pin must be refused")
            .to_string();
        assert!(err.contains("git.references[net].revision"), "{err}");
    }
}
