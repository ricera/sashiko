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
    /// Revision used when the caller does not name one. Pinning this is what
    /// keeps a review reproducible against a moving tree.
    pub revision: String,
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

impl SashikoToolContext {
    /// Replaces occurrences of `HEAD` in a reference string with the virtualized head commit SHA.
    pub fn virtualize_ref(&self, r: &str) -> String {
        let vhead_lock = self.virtual_head.read().unwrap();
        let Some(ref vhead) = *vhead_lock else {
            return r.to_string();
        };
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = RE.get_or_init(|| regex::Regex::new(r"(^|[^/])\bHEAD($|[~^:.@])").unwrap());
        re.replace_all(r, format!("${{1}}{}${{2}}", vhead))
            .into_owned()
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

    /// Directory a tool should run git in for this call.
    pub fn repo_root(&self, args: &Value) -> Result<&Path> {
        Ok(match self.repo_target(args)? {
            RepoTarget::Review => &self.worktree_path,
            // repo_target only returns Reference when this is Some.
            RepoTarget::Reference => &self.reference.as_ref().unwrap().path,
        })
    }

    /// Resolves a revision against the repository the call is aimed at.
    ///
    /// Virtualization applies to the review repository alone. The virtual head
    /// is a commit that exists only in the review worktree, so rewriting `HEAD`
    /// into it before querying the kernel tree would ask for an object that is
    /// not there -- or, worse, one that is there and unrelated.
    pub fn resolve_ref(&self, args: &Value, raw: &str) -> Result<String> {
        Ok(match self.repo_target(args)? {
            RepoTarget::Review => self.virtualize_ref(raw),
            RepoTarget::Reference => {
                let reference = self.reference.as_ref().unwrap();
                // An unpinned caller gets the configured revision. Anything
                // explicit is honoured, so the model can still ask whether
                // something changed in a later release.
                if raw.is_empty() || raw == "HEAD" {
                    reference.revision.clone()
                } else {
                    raw.to_string()
                }
            }
        })
    }

    /// Whether a reference repository is available to be named at all.
    pub fn has_reference(&self) -> bool {
        self.reference.is_some()
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
    pub fn with_reference(mut self, path: PathBuf, revision: Option<String>) -> Self {
        self.context.reference = Some(ReferenceRepo {
            path,
            revision: revision.unwrap_or_else(|| "HEAD".to_string()),
        });
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
        self.registry
            .declarations()
            .into_iter()
            .map(|decl| {
                let mut parameters = decl["parameters"].clone();
                if !has_reference
                    && let Some(props) = parameters
                        .get_mut("properties")
                        .and_then(Value::as_object_mut)
                {
                    props.remove(REPO_ARG);
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

        let mut normalized_args = self.registry.normalize_tool_args(&name_normalized, &args);

        // Pin the repository into the cache key centrally rather than in eight
        // `normalize_args` bodies: a tool that forgot would serve the review
        // repo's answer to a question about the kernel tree, which is a wrong
        // answer that looks like a right one. Defaulting the absent case here
        // also keeps `{}` and `{"repo":"review"}` on the same cache entry.
        if let Some(obj) = normalized_args.as_object_mut()
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

        let res = self
            .registry
            .call(&name_normalized, args, &self.context)
            .await?;

        if let Some(k) = key {
            let mut cache = self.cache.write().unwrap();
            cache.insert(k, res.clone());
        }

        Ok(res)
    }
}

#[cfg(test)]
mod tools_test;
