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

use crate::ai::AiTool;
use crate::toolbox::framework::ToolRegistry;
use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

pub mod command;
pub mod framework;
pub mod utils;

pub mod git_blame;
pub mod git_diff;
pub mod git_find_files;
pub mod git_grep;
pub mod git_log;
pub mod git_ls;
pub mod git_read_files;
pub mod git_show;
pub mod read_prompt;

/// The Sashiko-specific context passed to LLM tools.
///
/// It encapsulates the active worktree, currently reviewed files, the virtual head commit,
/// and a shared cache to avoid redundant command executions across tool runs.
pub struct SashikoToolContext {
    pub worktree_path: PathBuf,
    pub prompts_path: Option<PathBuf>,
    pub active_patch_files: RwLock<Vec<String>>,
    pub virtual_head: RwLock<Option<String>>,
    /// A read-only second repository for grounding, and the revision to read it
    /// at. See [`RepoTarget`] and `git.reference_repository_path`.
    pub reference: Option<ReferenceRepo>,
    pub(crate) cache: Arc<RwLock<std::collections::HashMap<String, Value>>>,
}

/// A read-only repository consulted for context but never reviewed or written.
#[derive(Debug, Clone)]
pub struct ReferenceRepo {
    pub path: PathBuf,
    /// The revision as configured, e.g. a tag. Shown to the model so it knows
    /// which kernel it is reading; never handed to git.
    pub revision: String,
    /// The commit `revision` resolved to when it was validated. This, not the
    /// name, is what an unpinned read is served: a branch name, or the default
    /// `HEAD`, would otherwise be resolved afresh on every tool call, and a
    /// fetch or checkout in the clone mid-review would change what the rest of
    /// the review reads.
    pub commit: String,
}

/// Which repository a tool call reads from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoTarget {
    /// The worktree holding the patch under review. The default, and the only
    /// option unless a reference repository is configured.
    Review,
    /// The kernel tree the reviewed code builds against.
    Reference,
}

/// Argument name the model uses to pick a repository.
pub const REPO_ARG: &str = "repo";

/// Value of [`REPO_ARG`] selecting the reference repository.
pub const REPO_ARG_REFERENCE: &str = "kernel";

/// Value of [`REPO_ARG`] selecting the repository under review.
pub const REPO_ARG_REVIEW: &str = "review";

/// Key naming the revision a reference-repository read was served from.
///
/// Distinct from the `revision` argument, which records what the caller asked
/// for. The two differ whenever the caller left the revision unpinned, which is
/// most of the time, and only this one says what the answer describes.
pub const REVISION_READ_KEY: &str = "revision_read";

/// Arguments that name a revision, per tool, in the order they appear on the
/// command line. Most tools take one `revision`; git_show and git_log embed the
/// revision in an object or range instead, and git_diff takes two.
const REVISION_ARGS: [&str; 5] = [
    "revision",
    "object",
    "range",
    "base_revision",
    "target_revision",
];

/// Replaces every `HEAD` token in a revision expression with `replacement`.
///
/// Token-aware rather than an exact-match comparison, because the model rarely
/// sends a bare `HEAD`: it sends `HEAD:path`, `HEAD~1`, or `a..HEAD`, and each
/// of those has to be rewritten for the substitution to mean anything. A
/// `HEAD` preceded by `/` is part of a ref name such as `origin/HEAD` and is
/// left alone.
///
/// Git's `@` shorthand for `HEAD` is rewritten too, when it stands alone as
/// a revision: at the start or after a `..`, and followed by nothing, a
/// suffix operator, a `:path` or a `..`. Missing it would let `@:path` or
/// `a..@` read straight past the substitution. `@{upstream}` and `@{-1}`
/// name other things and are left alone.
///
/// The replacement goes in through a closure rather than a `$`-template, so
/// a `$` in it is inserted literally instead of being expanded as a capture
/// group.
fn substitute_head(r: &str, replacement: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE
        .get_or_init(|| regex::Regex::new(r"(^|[^/])\bHEAD($|[~^:.@])|(^|\.)@($|[~^:.])").unwrap());
    re.replace_all(r, |caps: &regex::Captures| {
        let (before, after) = if caps.get(3).is_some() {
            (&caps[3], &caps[4])
        } else {
            (&caps[1], &caps[2])
        };
        format!("{before}{replacement}{after}")
    })
    .into_owned()
}

impl SashikoToolContext {
    /// Replaces occurrences of `HEAD` in a reference string with the virtualized head commit SHA.
    pub fn virtualize_ref(&self, r: &str) -> String {
        let vhead_lock = self.virtual_head.read().unwrap();
        let Some(ref vhead) = *vhead_lock else {
            return r.to_string();
        };
        substitute_head(r, vhead)
    }

    /// Reads the repository a call is aimed at out of its arguments.
    ///
    /// An unknown value is an error rather than a silent fall back to the review
    /// repository: answering a question about kernel headers with a search of
    /// the driver returns "no matches", which the model reads as a fact about
    /// the kernel.
    pub fn repo_target(&self, args: &Value) -> Result<RepoTarget> {
        match args[REPO_ARG].as_str() {
            None | Some(REPO_ARG_REVIEW) => Ok(RepoTarget::Review),
            Some(REPO_ARG_REFERENCE) if self.reference.is_some() => Ok(RepoTarget::Reference),
            Some(REPO_ARG_REFERENCE) => Err(anyhow::anyhow!(
                "No reference repository is configured, so '{}: \"{}\"' cannot be served. \
                 Either this review has no separate kernel tree to consult, or \
                 git.reference_repository_path is unset in the daemon's settings. \
                 Retry without '{}' to search the repository under review.",
                REPO_ARG,
                REPO_ARG_REFERENCE,
                REPO_ARG
            )),
            Some(other) => Err(anyhow::anyhow!(
                "Unknown {} '{}'. Valid values are \"{}\" (the code under review) and \"{}\" \
                 (the kernel tree it builds against).",
                REPO_ARG,
                other,
                REPO_ARG_REVIEW,
                REPO_ARG_REFERENCE
            )),
        }
    }

    /// The reference repository a call is aimed at, or `None` when the call is
    /// aimed at the repository under review.
    fn reference_for(&self, args: &Value) -> Result<Option<&ReferenceRepo>> {
        Ok(match self.repo_target(args)? {
            RepoTarget::Review => None,
            // repo_target only returns Reference when a reference is attached,
            // so the `None` here is unreachable rather than a silent fallback.
            RepoTarget::Reference => self.reference.as_ref(),
        })
    }

    /// Directory a tool should run git in for this call.
    pub fn repo_root(&self, args: &Value) -> Result<&Path> {
        Ok(match self.reference_for(args)? {
            Some(reference) => &reference.path,
            None => &self.worktree_path,
        })
    }

    /// Resolves a revision against the repository the call is aimed at.
    ///
    /// Virtualization applies to the review repository alone. The virtual head
    /// is a commit that exists only in the review worktree, so rewriting `HEAD`
    /// into it before querying the kernel tree would ask for an object that is
    /// not there -- or, worse, one that is there and unrelated.
    ///
    /// In the reference repository, `HEAD` means the pinned commit instead.
    /// That covers `HEAD` inside a larger expression too:
    /// `HEAD:include/linux/foo.h` or `HEAD~5..HEAD` would otherwise read the
    /// tip of the clone, which on a tracking clone can be hundreds of commits
    /// away from the kernel the module is built against.
    pub fn resolve_ref(&self, args: &Value, raw: &str) -> Result<String> {
        Ok(match self.reference_for(args)? {
            None => self.virtualize_ref(raw),
            // An unpinned caller gets the validated commit. Anything explicit
            // is honoured, so the model can still ask whether something
            // changed in a later release.
            Some(reference) if raw.is_empty() => reference.commit.clone(),
            Some(reference) => substitute_head(raw, &reference.commit),
        })
    }

    /// Whether a reference repository is available to be named at all.
    pub fn has_reference(&self) -> bool {
        self.reference.is_some()
    }

    /// The revision expression a reference read was served, resolved from
    /// whichever argument the tool takes its revision in.
    ///
    /// Taken per tool rather than from `revision` alone: git_show, git_log and
    /// git_diff name their revision in `object`, `range`, or a base and target
    /// pair, and reading only `revision` would report the pin for a call that
    /// explicitly named some other tag.
    ///
    /// A `rev:path` object is reported by its revision alone; the path is
    /// already in the arguments beside it.
    fn served_revision(&self, args: &Value) -> Result<String> {
        let named: Vec<&str> = REVISION_ARGS
            .iter()
            .filter_map(|key| args.get(*key).and_then(Value::as_str))
            .map(|raw| match raw.split_once(':') {
                Some((rev, _)) if !rev.is_empty() => rev,
                _ => raw,
            })
            .collect();
        if named.is_empty() {
            return self.resolve_ref(args, "");
        }
        let resolved = named
            .into_iter()
            .map(|raw| self.resolve_ref(args, raw))
            .collect::<Result<Vec<_>>>()?;
        Ok(resolved.join(".."))
    }
}

/// JSON-schema fragment for the `repo` argument, shared by every tool that reads
/// a repository so the model is told the same thing eight times.
///
/// Stripped from the declarations when no reference repository is configured, so
/// the model is never offered an option that can only fail.
pub fn repo_arg_schema() -> Value {
    serde_json::json!({
        "type": "string",
        "enum": [REPO_ARG_REVIEW, REPO_ARG_REFERENCE],
        "description":
            "Which repository to read. \"review\" (default) is the code under review. \"kernel\" \
             is the read-only Linux tree that code builds against -- use it to check API \
             contracts, struct layouts, and header definitions rather than recalling them. \
             Never use \"kernel\" to look for the change under review; it is not there."
    })
}

/// A backward-compatible adapter that coordinates Sashiko's LLM tools.
///
/// It wraps the generic `ToolRegistry` and manages the shared execution context and caching.
pub struct ToolBox {
    context: SashikoToolContext,
    registry: ToolRegistry<SashikoToolContext>,
    /// Thread-safe cache of tool invocation results.
    /// Shared with the execution context so that tools can access it internally.
    pub(crate) cache: Arc<RwLock<std::collections::HashMap<String, Value>>>,
}

impl ToolBox {
    /// Creates a new `ToolBox` configured for the given worktree and optional prompt registry.
    pub fn new(worktree_path: PathBuf, prompts_path: Option<PathBuf>) -> Self {
        let cache = Arc::new(RwLock::new(std::collections::HashMap::new()));

        let context = SashikoToolContext {
            worktree_path,
            prompts_path,
            active_patch_files: RwLock::new(Vec::new()),
            virtual_head: RwLock::new(None),
            reference: None,
            cache: cache.clone(),
        };

        let mut registry = ToolRegistry::new();
        registry.register(git_read_files::GitReadFilesTool);
        registry.register(git_blame::GitBlameTool);
        registry.register(git_diff::GitDiffTool);
        registry.register(git_show::GitShowTool);
        registry.register(git_log::GitLogTool);
        registry.register(git_ls::GitLsTool);
        registry.register(git_grep::GitGrepTool);
        registry.register(git_find_files::GitFindFilesTool);

        if context.prompts_path.is_some() {
            registry.register(read_prompt::ReadPromptTool);
        }

        Self {
            context,
            registry,
            cache,
        }
    }

    /// Registers an extra tool. Test-only: the review toolbox is fixed, and
    /// this exists so a test can drive the tool loop with a tool of its own.
    #[cfg(test)]
    pub(crate) fn register_tool(
        &mut self,
        tool: impl framework::LlmTool<SashikoToolContext> + 'static,
    ) {
        self.registry.register(tool);
    }

    /// Attaches a read-only repository the model can consult for grounding.
    ///
    /// Without one, the `repo` argument is not advertised to the model at all --
    /// see `get_declarations_generic` -- so an unconfigured deployment behaves
    /// exactly as it did before.
    pub fn with_reference(mut self, reference: ReferenceRepo) -> Self {
        self.context.reference = Some(reference);
        self
    }

    /// Sets the virtual head commit SHA for the current review session.
    pub fn set_virtual_head(&mut self, sha: String) {
        let mut vhead = self.context.virtual_head.write().unwrap();
        *vhead = Some(sha);
    }

    /// Sets the list of files modified by the patch currently under review.
    pub fn set_active_patch_files(&mut self, files: Vec<String>) {
        let mut active = self.context.active_patch_files.write().unwrap();
        *active = files;
    }

    /// Replaces occurrences of HEAD in a reference string with the virtualized head commit SHA.
    pub fn virtualize_ref(&self, r: &str) -> String {
        self.context.virtualize_ref(r)
    }

    /// Returns the absolute path to the worktree where tools are executed.
    pub fn get_worktree_path(&self) -> &Path {
        &self.context.worktree_path
    }

    /// Revision of the reference repository, if one is attached.
    ///
    /// The prompt layer uses this both to tell the model which kernel it is
    /// reading and to decide whether this is an out-of-tree review at all.
    pub fn reference_revision(&self) -> Option<&str> {
        self.context.reference.as_ref().map(|r| r.revision.as_str())
    }

    /// Generates LLM-facing declarations for all registered tools.
    ///
    /// With no reference repository configured, the `repo` argument is removed
    /// from every schema. Advertising a value that always errors would spend a
    /// turn per attempt to teach the model something the declaration could have
    /// said for free.
    pub fn get_declarations_generic(&self) -> Vec<AiTool> {
        let has_reference = self.context.has_reference();
        let pin = self.reference_revision();
        self.registry
            .declarations()
            .into_iter()
            .map(|decl| {
                let mut parameters = decl["parameters"].clone();
                if let Some(props) = parameters
                    .get_mut("properties")
                    .and_then(Value::as_object_mut)
                {
                    if !has_reference {
                        props.remove(REPO_ARG);
                    }
                    // Name the pin in the one place the model is deciding what
                    // to pass. Told the revision only elsewhere in the prompt,
                    // it was unsure whether HEAD in the kernel tree meant the
                    // pin or the tip of the clone. A description on the
                    // argument itself is free and arrives at the moment of the
                    // choice. Every argument that carries a revision gets it,
                    // not only those named `revision`.
                    //
                    // HEAD is steered towards rather than the pin's name,
                    // because HEAD is served the commit validated at startup,
                    // whereas a name -- a branch in particular -- is resolved
                    // again on every read and can move under a running review.
                    if let Some(pin) = pin {
                        for key in REVISION_ARGS {
                            if let Some(arg) = props.get_mut(key).and_then(Value::as_object_mut) {
                                let base = arg
                                    .get("description")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string();
                                arg.insert(
                                    "description".to_string(),
                                    Value::from(format!(
                                        "{base} When {REPO_ARG} is \"{REPO_ARG_REFERENCE}\", that \
                                         tree is pinned to {pin}: \"HEAD\" (or \"@\"), or leaving \
                                         the revision out, reads {pin} and never the tip of the \
                                         clone. Name a different revision only to ask whether \
                                         something changed in another release."
                                    )),
                                );
                            }
                        }
                    }
                }
                AiTool {
                    name: decl["name"].as_str().unwrap().to_string(),
                    description: decl["description"].as_str().unwrap().to_string(),
                    parameters,
                }
            })
            .collect()
    }

    /// Invokes a tool by name with the given JSON arguments.
    ///
    /// It handles argument normalization, caching of final results, and dispatches
    /// the execution to the corresponding tool struct.
    pub async fn call(&self, name: &str, args: Value) -> Result<Value> {
        let name_normalized = name.trim().to_lowercase();
        let should_cache = name_normalized != "todowrite";
        let args = utils::normalize_json_numbers(args);

        let mut normalized_args = self.registry.normalize_tool_args(&name_normalized, &args);

        // Only tools that declare `repo` read a repository chosen by it. The
        // rest -- read_prompt, todowrite, anything registered later -- ignore
        // the argument, so a stray `repo` on them must neither be validated
        // nor earn a revision_read note describing a read that never happened.
        let takes_repo = self
            .registry
            .parameters(&name_normalized)
            .is_some_and(|p| p["properties"].get(REPO_ARG).is_some());

        // Pin the repository into the cache key centrally rather than in eight
        // `normalize_args` bodies: a tool that forgot would serve the review
        // repo's answer to a question about the kernel tree, which is a wrong
        // answer that looks like a right one. Defaulting the absent case here
        // also keeps `{}` and `{"repo":"review"}` on the same cache entry.
        if takes_repo
            && let Some(obj) = normalized_args.as_object_mut()
            && !obj.contains_key(REPO_ARG)
        {
            obj.insert(REPO_ARG.to_string(), Value::from(REPO_ARG_REVIEW));
        }

        let key = if should_cache {
            let k = format!(
                "{}:{}",
                name_normalized,
                serde_json::to_string(&normalized_args)?
            );
            {
                let cache = self.cache.read().unwrap();
                if let Some(val) = cache.get(&k) {
                    return Ok(val.clone());
                }
            }
            Some(k)
        } else {
            None
        };

        // Resolved before the call, so a bad `repo` fails without running the
        // tool, and a call that did run is never turned into an error after
        // the fact.
        let served =
            if takes_repo && self.context.repo_target(&normalized_args)? == RepoTarget::Reference {
                Some(self.context.served_revision(&normalized_args)?)
            } else {
                None
            };

        let mut res = self
            .registry
            .call(&name_normalized, args, &self.context)
            .await?;

        // Say which revision of the reference repository was actually read.
        //
        // HEAD in the kernel tree is served the pinned commit rather than the
        // tip of the clone, so the arguments recorded in the log are what was
        // asked for and not what was done. Reading a tracking clone at
        // whatever was last fetched, instead of the tag the module is built
        // against, is exactly the failure pinning exists to prevent -- and it
        // would leave the log looking identical to a correct run.
        //
        // Annotated here rather than in each of the eight tools, and before the
        // cache write so a hit carries it too. Resolved for this call rather
        // than taken from the configuration: an explicitly named revision is
        // honoured, so the pin is not always what was read. HEAD and an
        // omitted revision are reported as the commit SHA they were served.
        if let Some(served) = served
            && let Some(obj) = res.as_object_mut()
        {
            obj.entry(REVISION_READ_KEY)
                .or_insert_with(|| Value::from(served));
        }

        if let Some(k) = key {
            let mut cache = self.cache.write().unwrap();
            cache.insert(k, res.clone());
        }

        Ok(res)
    }
}

#[cfg(test)]
mod tools_test;
