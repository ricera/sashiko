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
//! This module is not gated on the server feature: the worker validates the
//! list on every path into a review, including builds without the daemon.

use crate::project::ProjectId;
use crate::settings::{ReferenceKind, ReferenceSettings};
use crate::toolbox::{REPO_ARG_REVIEW, ReferenceRepo};
use anyhow::{Result, anyhow, bail};
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
            // clone means nothing in another. Kind, description and position
            // stay, since they describe what the tree is for, not where it is.
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
    for entry in refs {
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
        });
    }
    Ok(resolved)
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
