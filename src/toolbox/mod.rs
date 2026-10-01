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
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Read-only repositories for grounding, in configured order. See
    /// [`RepoTarget`] and `git.references`.
    ///
    /// Fixed for the ToolBox's lifetime, so an index into it is a stable
    /// identity for a cache key.
    pub references: Vec<ReferenceRepo>,
    /// Configured references left out of this patch by their `applies_to`.
    /// Never read and never offered; kept only so that naming one is answered
    /// with why it is missing rather than with "unknown".
    pub dormant: Vec<DormantReference>,
    pub(crate) cache: Arc<RwLock<std::collections::HashMap<String, Value>>>,
}

/// A read-only repository consulted for context but never reviewed or written.
#[derive(Debug, Clone)]
pub struct ReferenceRepo {
    /// The value of [`REPO_ARG`] that selects this repository. A lookup key
    /// only: it never reaches a git command line.
    pub name: String,
    /// What the tree is, which decides how the prompt frames it. A kernel entry
    /// is what marks the review as out-of-tree.
    pub kind: crate::settings::ReferenceKind,
    pub path: PathBuf,
    /// The revision as configured, e.g. a tag or a release branch. Shown to the
    /// model so it knows which release it is reading; never handed to git.
    pub revision: String,
    /// The commit `revision` resolved to when it was validated. This, not the
    /// name, is what an unpinned read is served: a branch name, or the default
    /// `HEAD`, would otherwise be resolved afresh on every tool call, and a
    /// fetch or checkout in the clone mid-review would change what the rest of
    /// the review reads.
    pub commit: String,
    /// What the tree is for, as configured. Shown to the model in the prompt;
    /// a kernel entry without one gets built-in text.
    pub description: Option<String>,
    /// Which patches the reference is attached to; `None` means every patch.
    /// Decided before the ToolBox is built, which only ever holds the
    /// references attached to its patch.
    pub applies_to: Option<crate::references::AppliesTo>,
}

/// A configured reference that is not attached to the patch under review,
/// because the patch touches none of the files it applies to.
#[derive(Debug, Clone)]
pub struct DormantReference {
    pub name: String,
    /// The `applies_to` patterns as configured, quoted back in the error.
    pub applies_to: Vec<String>,
}

/// Which repository a tool call reads from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoTarget {
    /// The worktree holding the patch under review. The default, and the only
    /// option unless a reference repository is configured.
    Review,
    /// A read-only reference repository: an index into
    /// [`SashikoToolContext::references`], stable because the list never
    /// changes after the ToolBox is built.
    Reference(usize),
}

/// Argument name the model uses to pick a repository.
pub const REPO_ARG: &str = "repo";

/// Value of [`REPO_ARG`] selecting the repository under review.
pub const REPO_ARG_REVIEW: &str = "review";

/// Key naming the revision a reference-repository read was served from.
///
/// Distinct from the `revision` argument, which records what the caller asked
/// for. The two differ whenever the caller left the revision unpinned, which is
/// most of the time, and only this one says what the answer describes.
pub const REVISION_READ_KEY: &str = "revision_read";

/// Key naming the reference repository a read was served from.
///
/// With several references attached, the commit in [`REVISION_READ_KEY`] alone
/// does not say which tree was read, and a tag like `v6.12` can exist in more
/// than one of them.
pub const REPO_READ_KEY: &str = "repo_read";

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
    /// the kernel. The error lists every valid name, so the retry can be right.
    pub fn repo_target(&self, args: &Value) -> Result<RepoTarget> {
        let name = match args[REPO_ARG].as_str() {
            None | Some(REPO_ARG_REVIEW) => return Ok(RepoTarget::Review),
            Some(name) => name,
        };
        if let Some(index) = self.references.iter().position(|r| r.name == name) {
            return Ok(RepoTarget::Reference(index));
        }
        // The model can know a dormant name from elsewhere -- an earlier patch
        // of the series, the description of another tree. "Unknown" would read
        // as a misconfiguration, so say it exists, why it is not here, and what
        // is.
        if let Some(dormant) = self.dormant.iter().find(|d| d.name == name) {
            let valid: Vec<String> =
                std::iter::once(format!("\"{REPO_ARG_REVIEW}\" (the code under review)"))
                    .chain(self.references.iter().map(|r| format!("\"{}\"", r.name)))
                    .collect();
            return Err(anyhow::anyhow!(
                "{} \"{}\" is configured but not attached to this review: it applies only \
                 to changes matching {}, and this patch touches none of them. Valid values \
                 here: {}.",
                REPO_ARG,
                name,
                dormant.applies_to.join(", "),
                valid.join(", ")
            ));
        }
        if self.references.is_empty() {
            return Err(anyhow::anyhow!(
                "No reference repositories are attached to this review (git.references \
                 or --reference), so '{}: \"{}\"' cannot be served. Retry without '{}' \
                 to search the repository under review.",
                REPO_ARG,
                name,
                REPO_ARG
            ));
        }
        let valid: Vec<String> = self
            .references
            .iter()
            .map(|r| format!("\"{}\"", r.name))
            .collect();
        Err(anyhow::anyhow!(
            "Unknown {} '{}'. Valid values: \"{}\" (the code under review), {}.",
            REPO_ARG,
            name,
            REPO_ARG_REVIEW,
            valid.join(", ")
        ))
    }

    /// The reference repository a call is aimed at, or `None` when the call is
    /// aimed at the repository under review.
    fn reference_for(&self, args: &Value) -> Result<Option<&ReferenceRepo>> {
        Ok(match self.repo_target(args)? {
            RepoTarget::Review => None,
            // repo_target only returns indices it found in the list, so the
            // `None` here is unreachable rather than a silent fallback.
            RepoTarget::Reference(index) => self.references.get(index),
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
    /// into it before querying a reference repository would ask for an object
    /// that is not there -- or, worse, one that is there and unrelated.
    ///
    /// In a reference repository, `HEAD` means that repository's own pinned
    /// commit instead. That covers `HEAD` inside a larger expression too:
    /// `HEAD:include/linux/foo.h` or `HEAD~5..HEAD` would otherwise read the
    /// tip of the clone, which on a tracking clone can be hundreds of commits
    /// away from the release the review is pinned to.
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

/// Placeholder JSON-schema fragment for the `repo` argument, embedded in the
/// static parameters of every tool that reads a repository.
///
/// It has to stay in each tool's own schema: [`ToolBox::call`] decides whether
/// a tool takes `repo` from its declared parameters, and only such tools are
/// validated and tagged. The values the model actually sees depend on the
/// configured references, so [`ToolBox::get_declarations_generic`] replaces
/// this with [`repo_arg_schema_for`], or strips it when there are none.
pub fn repo_arg_schema() -> Value {
    serde_json::json!({
        "type": "string",
        "enum": [REPO_ARG_REVIEW],
        "description": "Which repository to read. \"review\" (default) is the code under review."
    })
}

/// The `repo` argument as offered to the model: "review" plus every reference
/// in configured order, each named with its kind and pin.
///
/// Kept short because it is repeated in eight tools. What HEAD means in a
/// reference is already said on every revision argument, and what each tree
/// is for, and that the change is never in one, is said once in the prompt.
fn repo_arg_schema_for(refs: &[ReferenceRepo]) -> Value {
    let mut values = vec![Value::from(REPO_ARG_REVIEW)];
    values.extend(refs.iter().map(|r| Value::from(r.name.as_str())));
    let listed = refs
        .iter()
        .map(|r| {
            let kind = match r.kind {
                crate::settings::ReferenceKind::Kernel => "kernel tree",
                crate::settings::ReferenceKind::Source => "source",
            };
            format!("{} ({kind}, {})", r.name, r.revision)
        })
        .collect::<Vec<_>>()
        .join("; ");
    serde_json::json!({
        "type": "string",
        "enum": values,
        "description": format!(
            "Which repository to read. \"review\" (default) is the code under review. \
             Read-only references: {listed}."
        )
    })
}

/// Appends the note on what HEAD means in a reference repository to every
/// argument of a tool schema that names a revision.
fn add_pin_note(props: &mut serde_json::Map<String, Value>) {
    // Say what HEAD means in the one place the model is deciding what to
    // pass. Told only elsewhere in the prompt, it was unsure whether HEAD in
    // the kernel tree meant the pin or the tip of the clone. A description on
    // the argument itself arrives at the moment of the choice. Every argument
    // that carries a revision gets it, not only those named `revision`.
    //
    // The pins themselves are named once, in the repo argument, rather than
    // once per reference in each of the five revision arguments of eight
    // tools.
    //
    // HEAD is steered towards rather than the pin's name, because HEAD is
    // served the commit validated at startup, whereas a name -- a branch in
    // particular -- is resolved again on every read and can move under a
    // running review.
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
                    "{base} In a read-only reference repo, \"HEAD\" (or \"@\"), \
                     or leaving the revision out, reads that repo's pin (listed \
                     under {REPO_ARG}), never the tip of the clone. Name a \
                     different revision only to ask whether something changed \
                     in another release."
                )),
            );
        }
    }
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
    /// How many tool calls named each attached reference, parallel to
    /// `context.references`. Recorded on the review row so a reader can tell
    /// whether the model used a tree it was offered; the deployment's logs do
    /// not live long enough to answer that later.
    reference_reads: Vec<AtomicU64>,
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
            references: Vec::new(),
            dormant: Vec::new(),
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
            reference_reads: Vec::new(),
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

    /// Appends a read-only repository the model can consult for grounding.
    ///
    /// Without any, the `repo` argument is not advertised to the model at all --
    /// see `get_declarations_generic` -- so an unconfigured deployment behaves
    /// exactly as it did before. Must be called before the first tool call:
    /// cache keys identify a reference by its name and position.
    pub fn with_reference(mut self, reference: ReferenceRepo) -> Self {
        debug_assert!(
            !self
                .context
                .references
                .iter()
                .any(|r| r.name == reference.name),
            "reference {} attached twice",
            reference.name
        );
        self.context.references.push(reference);
        self.reference_reads.push(AtomicU64::new(0));
        self
    }

    /// Appends several read-only repositories, in order. See
    /// [`ToolBox::with_reference`].
    pub fn with_references(self, references: impl IntoIterator<Item = ReferenceRepo>) -> Self {
        references.into_iter().fold(self, Self::with_reference)
    }

    /// Records the configured references this patch is reviewed without, so
    /// that naming one gets a specific error. See [`DormantReference`].
    pub fn with_dormant_references(
        mut self,
        dormant: impl IntoIterator<Item = DormantReference>,
    ) -> Self {
        self.context.dormant.extend(dormant);
        debug_assert!(
            !self.context.dormant.iter().any(|d| self
                .context
                .references
                .iter()
                .any(|r| r.name == d.name)),
            "a reference is both attached and dormant"
        );
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

    /// The attached reference repositories, in configured order.
    ///
    /// The prompt layer uses these both to list the trees the model can read
    /// and to decide whether this is an out-of-tree review at all.
    pub fn references(&self) -> &[ReferenceRepo] {
        &self.context.references
    }

    /// How many tool calls have named each attached reference so far,
    /// parallel to [`ToolBox::references`].
    ///
    /// Every call that named the reference counts, including cache hits and
    /// calls git then refused: the number answers "did the model use this
    /// tree", not "how many reads succeeded".
    pub fn reference_reads(&self) -> Vec<u64> {
        self.reference_reads
            .iter()
            .map(|n| n.load(Ordering::Relaxed))
            .collect()
    }

    /// Generates LLM-facing declarations for all registered tools.
    ///
    /// With no reference repository configured, the `repo` argument is removed
    /// from every schema. Advertising a value that always errors would spend a
    /// turn per attempt to teach the model something the declaration could have
    /// said for free. With references, it lists exactly the attached names.
    ///
    /// Dormant references are excluded by construction: they are never in
    /// the attached list this is built from. A patch whose references are all
    /// dormant therefore sees no `repo` argument, exactly like a
    /// configuration with none.
    pub fn get_declarations_generic(&self) -> Vec<AiTool> {
        let references = &self.context.references;
        let repo_schema = (!references.is_empty()).then(|| repo_arg_schema_for(references));
        self.registry
            .declarations()
            .into_iter()
            .map(|decl| {
                let mut parameters = decl["parameters"].clone();
                if let Some(props) = parameters
                    .get_mut("properties")
                    .and_then(Value::as_object_mut)
                {
                    match &repo_schema {
                        None => {
                            props.remove(REPO_ARG);
                        }
                        Some(repo_schema) => {
                            if props.contains_key(REPO_ARG) {
                                props.insert(REPO_ARG.to_string(), repo_schema.clone());
                            }
                            add_pin_note(props);
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
        // repo's answer to a question about a reference tree, which is a wrong
        // answer that looks like a right one. Defaulting the absent case here
        // also keeps `{}` and `{"repo":"review"}` on the same cache entry. The
        // key carries the reference's name as the model gave it, so two
        // references asked the same question never share an entry.
        if takes_repo
            && let Some(obj) = normalized_args.as_object_mut()
            && !obj.contains_key(REPO_ARG)
        {
            obj.insert(REPO_ARG.to_string(), Value::from(REPO_ARG_REVIEW));
        }

        // Resolved before the cache lookup, so a bad `repo` fails without
        // running the tool, a call that did run is never turned into an error
        // after the fact, and a cache hit is counted against its reference:
        // counting after the early return would undercount exactly the reads
        // the model repeated. Unknown and dormant names were never cached, so
        // checking them first changes no answer.
        let target = if takes_repo {
            Some(self.context.repo_target(&normalized_args)?)
        } else {
            None
        };
        if let Some(RepoTarget::Reference(index)) = target {
            self.reference_reads[index].fetch_add(1, Ordering::Relaxed);
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

        let served = if let Some(RepoTarget::Reference(index)) = target {
            Some((
                self.context.served_revision(&normalized_args)?,
                self.context.references[index].name.clone(),
            ))
        } else {
            None
        };

        let mut res = self
            .registry
            .call(&name_normalized, args, &self.context)
            .await?;

        // Say which reference repository was read, and at which revision.
        //
        // HEAD in a reference repository is served its pinned commit rather
        // than the tip of the clone, so the arguments recorded in the log are
        // what was asked for and not what was done. Reading a tracking clone at
        // whatever was last fetched, instead of the release the review is
        // pinned to, is exactly the failure pinning exists to prevent -- and it
        // would leave the log looking identical to a correct run. The name goes
        // beside the commit because, with several references, the commit alone
        // does not say which tree it came from.
        //
        // Annotated here rather than in each of the eight tools, and before the
        // cache write so a hit carries it too. Resolved for this call rather
        // than taken from the configuration: an explicitly named revision is
        // honoured, so the pin is not always what was read. HEAD and an
        // omitted revision are reported as the commit SHA they were served.
        if let Some((served, name)) = served
            && let Some(obj) = res.as_object_mut()
        {
            obj.entry(REVISION_READ_KEY)
                .or_insert_with(|| Value::from(served));
            obj.entry(REPO_READ_KEY)
                .or_insert_with(|| Value::from(name));
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
