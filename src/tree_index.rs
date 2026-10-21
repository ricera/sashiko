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

//! The trees a patch's locations can name, indexed before the review runs.
//!
//! Stage validators are synchronous and cannot run git, yet checking that a
//! location's `file` exists in the repository its `repo` names, or that a
//! reference location cites code the module under review carries itself, needs
//! both trees' contents. [`TreeIndex`] holds what those checks need, built once
//! per patch off the async executor:
//!
//! - every path of the code under review at the patch's commit, at its
//!   parent (a file the patch deletes or renames is still evidence) and at
//!   the series tip (a later patch's new file can be cited), and of each
//!   attached reference at its pinned commit;
//! - when a kernel reference is attached, the names the code under review
//!   defines (functions with a body, structs, unions, enums, macros), each
//!   mapped to its first `file:line`. Only unconditional definitions count:
//!   a name defined under `#if` or in a `kcompat` file is usually a shim of
//!   a kernel API for older kernels, which the pinned kernel compiles out,
//!   and a directory a `Kbuild` or `Makefile` adds to the build only under a
//!   `CONFIG_` test may be a backport the pinned kernel does not build.
//!
//! The index only widens what a validator knows. A tree that could not be
//! listed is absent rather than empty, so a failed build never makes a path
//! look missing, and a name the deliberately simple matcher does not see only
//! means a check that does not fire.

use crate::location_repo::LocationRepo;
use crate::settings::ReferenceKind;
use crate::toolbox::ReferenceRepo;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

/// The most files the code under review may have for its definitions to be
/// scanned. Only an in-tree kernel is larger, and an in-tree review has no
/// kernel reference to tell apart from its own code.
const MAX_DEFINITION_SCAN_FILES: usize = 20_000;

/// Every path of a tree, shared between the patches that read it.
pub type PathSet = Arc<HashSet<String>>;

/// Where the code under review first defines a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub file: String,
    pub line: u32,
}

impl fmt::Display for Definition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.file, self.line)
    }
}

/// The paths of every tree a patch's locations can name, and the names the
/// code under review defines.
#[derive(Default)]
pub struct TreeIndex {
    /// Each listed tree's paths, the code under review first and then the
    /// references in configured order, so a message naming the trees that
    /// hold a path lists them in a stable order.
    paths: Vec<(LocationRepo, PathSet)>,
    /// The code under review's paths at the patch's commit alone; `None` when
    /// they were not listed. Its entry in `paths` adds the parent's and the
    /// series tip's, which may be cited but are not the module this patch
    /// builds.
    carried: Option<PathSet>,
    /// Names the code under review defines; `None` when they were not scanned
    /// (no kernel reference, a tree too large, or a failed scan).
    definitions: Option<HashMap<String, Definition>>,
}

impl fmt::Debug for TreeIndex {
    /// Counts only: a kernel tree's paths would swamp any log the review state
    /// is printed to.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let paths: Vec<String> = self
            .paths
            .iter()
            .map(|(repo, set)| format!("{repo}: {} paths", set.len()))
            .collect();
        f.debug_struct("TreeIndex")
            .field("paths", &paths)
            .field("definitions", &self.definitions.as_ref().map(HashMap::len))
            .finish()
    }
}

impl TreeIndex {
    /// Builds the index for one patch on a blocking thread.
    ///
    /// Returns `None` when no reference is attached: such a review has one
    /// tree, its locations name none, and it does exactly the work it did
    /// before locations carried a repository.
    pub async fn build_for_patch(
        review_dir: PathBuf,
        review_commit: String,
        series_tip: Option<String>,
        references: Vec<ReferenceRepo>,
    ) -> Option<Arc<Self>> {
        if references.is_empty() {
            return None;
        }
        let built = tokio::task::spawn_blocking(move || {
            Self::build_in_series(
                &review_dir,
                &review_commit,
                series_tip.as_deref(),
                &references,
            )
        })
        .await;
        match built {
            Ok(index) => Some(Arc::new(index)),
            Err(error) => {
                tracing::warn!(%error, "Tree index build did not finish; locations go unchecked");
                None
            }
        }
    }

    /// Builds the index of a patch that is not part of a longer series.
    pub fn build(review_dir: &Path, review_commit: &str, references: &[ReferenceRepo]) -> Self {
        Self::build_in_series(review_dir, review_commit, None, references)
    }

    /// Builds the index, running git synchronously. Every failure is logged
    /// and leaves its part of the index absent.
    pub fn build_in_series(
        review_dir: &Path,
        review_commit: &str,
        series_tip: Option<&str>,
        references: &[ReferenceRepo],
    ) -> Self {
        let mut index = Self::default();
        let carried = match list_paths(review_dir, review_commit) {
            Ok(set) => Some(Arc::new(set)),
            Err(error) => {
                tracing::warn!(
                    commit = review_commit,
                    %error,
                    "Could not list the code under review; its locations go unchecked"
                );
                None
            }
        };
        if let Some(set) = &carried {
            let citable = citable_review_paths(review_dir, review_commit, series_tip, set);
            index.paths.push((LocationRepo::Review, Arc::new(citable)));
        }
        for reference in references {
            match cached_reference_paths(reference) {
                Ok(set) => index
                    .paths
                    .push((LocationRepo::Reference(reference.name.clone()), set)),
                Err(error) => tracing::warn!(
                    reference = %reference.name,
                    commit = %reference.commit,
                    %error,
                    "Could not list a reference repository; its locations go unchecked"
                ),
            }
        }
        let has_kernel = references.iter().any(|r| r.kind == ReferenceKind::Kernel);
        if let (true, Some(set)) = (has_kernel, &carried) {
            index.definitions = scan_review_definitions(review_dir, review_commit, set.len());
        }
        index.carried = carried;
        index
    }

    /// Every path of `repo`'s tree; `None` when it was not listed, in which
    /// case nothing about the tree is known.
    pub fn paths(&self, repo: &LocationRepo) -> Option<&HashSet<String>> {
        self.paths
            .iter()
            .find(|(listed, _)| listed == repo)
            .map(|(_, set)| set.as_ref())
    }

    /// Whether `repo`'s tree holds `path`; `None` when the tree was not
    /// listed.
    pub fn contains_path(&self, repo: &LocationRepo, path: &str) -> Option<bool> {
        self.paths(repo).map(|set| set.contains(path))
    }

    /// Whether the code under review holds `path` at the patch's commit, the
    /// tree this patch builds and the one its definitions are scanned at;
    /// `None` when it was not listed. A file only the parent or the series
    /// tip holds can be cited, but the module does not ship it here.
    pub fn review_carries(&self, path: &str) -> Option<bool> {
        self.carried.as_ref().map(|set| set.contains(path))
    }

    /// The listed trees that hold `path`, the code under review first.
    pub fn repos_with_path<'a>(&'a self, path: &'a str) -> impl Iterator<Item = &'a LocationRepo> {
        self.paths
            .iter()
            .filter(move |(_, set)| set.contains(path))
            .map(|(repo, _)| repo)
    }

    /// Where the code under review first defines `name`; `None` when it does
    /// not, or when its definitions were not scanned.
    pub fn definition(&self, name: &str) -> Option<&Definition> {
        self.definitions.as_ref()?.get(name)
    }

    /// Whether the code under review's definitions were scanned.
    pub fn has_definitions(&self) -> bool {
        self.definitions.is_some()
    }
}

/// Reference trees already listed in this process, by commit. Every patch of
/// a series reads the same pinned commit, and a kernel listing is a few
/// megabytes, so it is built once. A commit names one tree whichever clone
/// holds it, which makes the commit alone a sound key.
static REFERENCE_PATHS: LazyLock<Mutex<HashMap<String, PathSet>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A reference's paths, from the cache or listed and cached. The lock is held
/// while listing so that patches starting together list a tree once.
fn cached_reference_paths(reference: &ReferenceRepo) -> Result<PathSet, String> {
    // A panic elsewhere while the lock was held cannot leave the map
    // inconsistent: entries are only ever inserted whole.
    let mut cache = REFERENCE_PATHS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(set) = cache.get(&reference.commit) {
        return Ok(set.clone());
    }
    let set = Arc::new(list_paths(&reference.path, &reference.commit)?);
    cache.insert(reference.commit.clone(), set.clone());
    Ok(set)
}

/// Accepts only an object name made of hex digits, so a value read from a
/// patch or from configuration can never reach git as an option.
fn checked_commit(commit: &str) -> Result<&str, String> {
    if !commit.is_empty() && commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(commit)
    } else {
        Err(format!("{commit:?} is not a commit id"))
    }
}

/// Every path a location in the code under review can name: the patch's
/// tree (`carried`, already listed), its parent's, whose files the patch
/// deletes or renames, and the series tip's, whose files a later patch adds.
/// A root commit has no parent, and a tip that cannot be listed is logged and
/// leaves out only the later patches' files.
fn citable_review_paths(
    repo: &Path,
    commit: &str,
    series_tip: Option<&str>,
    carried: &HashSet<String>,
) -> HashSet<String> {
    let mut paths = carried.clone();
    if let Ok(parent) =
        checked_commit(commit).and_then(|commit| list_revision_paths(repo, &format!("{commit}^")))
    {
        paths.extend(parent);
    }
    if let Some(tip) = series_tip.filter(|tip| *tip != commit) {
        match list_paths(repo, tip) {
            Ok(tip_paths) => paths.extend(tip_paths),
            Err(error) => tracing::warn!(
                tip,
                %error,
                "Could not list the series tip; later patches' files go unchecked"
            ),
        }
    }
    paths
}

/// Every path of the tree at `commit`.
fn list_paths(repo: &Path, commit: &str) -> Result<HashSet<String>, String> {
    list_revision_paths(repo, checked_commit(commit)?)
}

/// Every path of the tree at `revision`, which the caller builds from a
/// checked commit so that it never reaches git as an option.
fn list_revision_paths(repo: &Path, revision: &str) -> Result<HashSet<String>, String> {
    let output = crate::git_cmd::in_dir(repo)
        .args(["ls-tree", "-r", "-z", "--name-only", revision])
        .output()
        .map_err(|e| format!("git ls-tree: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git ls-tree: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output
        .stdout
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect())
}

/// Scans the code under review's definitions unless it is too large, logging
/// whatever it skips.
fn scan_review_definitions(
    review_dir: &Path,
    commit: &str,
    file_count: usize,
) -> Option<HashMap<String, Definition>> {
    if file_count > MAX_DEFINITION_SCAN_FILES {
        tracing::info!(
            commit,
            file_count,
            limit = MAX_DEFINITION_SCAN_FILES,
            "Code under review is too large to scan for definitions; skipped"
        );
        return None;
    }
    match scan_definitions(review_dir, commit) {
        Ok(definitions) => Some(definitions),
        Err(error) => {
            tracing::warn!(
                commit,
                %error,
                "Could not scan the code under review for definitions"
            );
            None
        }
    }
}

/// Lines git grep hands to the scan: every line that opens at column 0 with
/// an identifier, a directive or a brace, so the scan sees each column-0 line
/// of a file in order and can tell whether a function head is followed by a
/// body. Indented lines and comments never decide anything.
const DEFINITION_PREFILTER: &str = r"^[A-Za-z_#{}]";

/// The names the `.c` and `.h` files at `commit` define unconditionally,
/// each mapped to its first definition in path order, a `.c` file's
/// preferred over a header's.
fn scan_definitions(repo: &Path, commit: &str) -> Result<HashMap<String, Definition>, String> {
    let commit = checked_commit(commit)?;
    let build_files = grep_lines(repo, commit, "", &BUILD_FILE_PATHSPECS)?;
    let skipped_dirs = parse_conditional_dirs(&build_files, commit);
    let sources = grep_lines(repo, commit, DEFINITION_PREFILTER, &["*.c", "*.h"])?;
    Ok(parse_grep_definitions(&sources, commit, skipped_dirs))
}

/// `git grep -n -z` of the lines matching `pattern` in the files matching
/// `pathspecs` at `commit`, which [`grep_records`] reads. `--no-column` keeps
/// a `grep.column` setting in the user's git config from adding a field to
/// every record.
fn grep_lines(
    repo: &Path,
    commit: &str,
    pattern: &str,
    pathspecs: &[&str],
) -> Result<Vec<u8>, String> {
    let output = crate::git_cmd::in_dir(repo)
        .args([
            "grep",
            "-n",
            "-z",
            "-I",
            "-E",
            "--no-color",
            "--no-column",
            "-e",
        ])
        .arg(pattern)
        .args([commit, "--"])
        .args(pathspecs)
        .output()
        .map_err(|e| format!("git grep: {e}"))?;
    // Exit status 1 means no line matched, which is an empty answer.
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(format!(
            "git grep: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

/// Reads `git grep -n -z <commit>` output, whose lines are
/// `<commit>:<path>\0<line>\0<text>` grouped by path in line order, into
/// `(path, line, text)`.
fn grep_records<'a>(
    stdout: &'a [u8],
    commit: &str,
) -> impl Iterator<Item = (String, u32, String)> + 'a {
    let prefix = format!("{commit}:");
    stdout.split(|&b| b == b'\n').filter_map(move |record| {
        let mut fields = record.splitn(3, |&b| b == 0);
        let (path, line, text) = (fields.next()?, fields.next()?, fields.next()?);
        let path = String::from_utf8_lossy(path)
            .strip_prefix(&prefix)?
            .to_string();
        let line = std::str::from_utf8(line).ok()?.parse().ok()?;
        Some((path, line, String::from_utf8_lossy(text).into_owned()))
    })
}

/// Reads the source records [`grep_lines`] returns into the first definition
/// of each name, skipping the files under `skipped_dirs`.
fn parse_grep_definitions(
    stdout: &[u8],
    commit: &str,
    skipped_dirs: Vec<String>,
) -> HashMap<String, Definition> {
    let mut scan = DefinitionScan {
        skipped_dirs,
        ..DefinitionScan::default()
    };
    for (path, line, text) in grep_records(stdout, commit) {
        scan.line(&path, line, &text);
    }
    scan.definitions
}

/// The build files whose subdirectory lines the scan reads.
const BUILD_FILE_NAMES: [&str; 2] = ["Kbuild", "Makefile"];

/// Pathspecs matching every build file; the glob also matches names that
/// merely end in one, which [`parse_conditional_dirs`] drops.
const BUILD_FILE_PATHSPECS: [&str; 2] = ["*Kbuild", "*Makefile"];

/// The directories, each ending in `/`, that a `Kbuild` or `Makefile` adds
/// to an `obj-` variable only inside a make conditional testing a `CONFIG_`
/// option. driver-b builds its fwctl core backport under
/// `ifeq ($(CONFIG_FWCTL),)`, so against a kernel that has fwctl the
/// module's copy is never built and the kernel's is the one that runs.
/// Conditionals that test no `CONFIG_` option, such as the
/// `ifneq ($(KERNELRELEASE),)` split of an external module's Makefile, do
/// not make a directory conditional.
fn parse_conditional_dirs(stdout: &[u8], commit: &str) -> Vec<String> {
    let mut dirs = Vec::new();
    let mut file = String::new();
    let mut scan = BuildFileScan::default();
    for (path, _, text) in grep_records(stdout, commit) {
        if path != file {
            file = path;
            scan = BuildFileScan::default();
        }
        let (dir, name) = file.rsplit_once('/').unwrap_or(("", file.as_str()));
        if !BUILD_FILE_NAMES.contains(&name) {
            continue;
        }
        if let Some(values) = scan.line(text.trim()) {
            dirs.extend(obj_dirs(dir, values));
        }
    }
    dirs
}

/// A line of a build file, as [`BuildFileScan`] reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MakeLine<'a> {
    /// `ifeq`, `ifneq`, `ifdef` or `ifndef`, and whether it tests a
    /// `CONFIG_` option.
    Open { tests_config: bool },
    /// `else`, which makes the open conditional a `CONFIG_` test when it is
    /// an `else ifeq` testing one.
    Else { tests_config: bool },
    /// `endif`.
    End,
    /// The values of an `obj-` assignment, or the continuation of one.
    ObjValues(&'a str),
    /// Anything else, which changes nothing.
    Other,
}

/// The make conditionals open at a line of one build file, and whether the
/// line continues an `obj-` assignment.
#[derive(Debug, Default)]
struct BuildFileScan {
    /// Whether each open conditional, outermost first, tests a `CONFIG_`
    /// option.
    tests_config: Vec<bool>,
    /// Whether the next line continues an `obj-` assignment.
    continued: bool,
}

impl BuildFileScan {
    /// What the trimmed `line` is, given the lines before it.
    fn classify<'a>(&self, line: &'a str) -> MakeLine<'a> {
        if self.continued {
            return MakeLine::ObjValues(line);
        }
        let tests_config = line.contains("CONFIG_");
        match line.split_whitespace().next().unwrap_or("") {
            "ifeq" | "ifneq" | "ifdef" | "ifndef" => MakeLine::Open { tests_config },
            "else" => MakeLine::Else { tests_config },
            "endif" => MakeLine::End,
            _ => line
                .strip_prefix("obj-")
                .and_then(|l| l.split_once('='))
                .map_or(MakeLine::Other, |(_, values)| MakeLine::ObjValues(values)),
        }
    }

    /// Reads the trimmed `line`, returning the `obj-` values it adds inside a
    /// conditional testing a `CONFIG_` option.
    fn line<'a>(&mut self, line: &'a str) -> Option<&'a str> {
        match self.classify(line) {
            MakeLine::Open { tests_config } => self.tests_config.push(tests_config),
            MakeLine::Else { tests_config } => {
                if tests_config && let Some(top) = self.tests_config.last_mut() {
                    *top = true;
                }
            }
            MakeLine::End => {
                self.tests_config.pop();
            }
            MakeLine::ObjValues(values) => {
                self.continued = values.ends_with('\\');
                return self.tests_config.contains(&true).then_some(values);
            }
            MakeLine::Other => {}
        }
        None
    }
}

/// The directories an `obj-` assignment's `values` name, each ending in `/`
/// and joined to `dir`, the build file's directory. A value built from a make
/// variable names no directory the scan can place.
fn obj_dirs<'a>(dir: &'a str, values: &'a str) -> impl Iterator<Item = String> + 'a {
    values
        .split_whitespace()
        .map(|word| word.trim_start_matches("./"))
        .filter(|word| word.ends_with('/') && !word.contains('$'))
        .map(move |word| {
            if dir.is_empty() {
                word.to_string()
            } else {
                format!("{dir}/{word}")
            }
        })
}

/// Whether `path` is an out-of-tree module's compatibility file
/// (`kcompat.h`, `kcompat_*.h`, ...). Those files carry the kernel's own
/// names, re-implemented for older kernels, so nothing they define is code
/// the module carries in place of the kernel's.
fn is_compat_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.to_ascii_lowercase().starts_with("kcompat")
}

fn is_header(path: &str) -> bool {
    path.ends_with(".h")
}

/// An open `#if`, `#ifdef` or `#ifndef` of the file being scanned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Conditional {
    /// A header's include guard, which encloses the whole file and makes
    /// nothing in it conditional.
    IncludeGuard,
    /// Any other conditional: what it encloses may be compiled out, as a
    /// compatibility shim for another kernel version is.
    Guarded,
}

/// The state of one file's scan.
#[derive(Default)]
struct FileScan {
    path: String,
    /// The file defines nothing: it is a compatibility file, or it sits in a
    /// directory the build files add only under a `CONFIG_` test.
    skipped: bool,
    /// Whether a conditional has been opened yet; only a header's first one
    /// can be its include guard.
    seen_conditional: bool,
    /// The conditionals open at the current line, outermost first.
    conditionals: Vec<Conditional>,
    /// The name a header's first `#ifndef` tests, until the next line shows
    /// whether a `#define` of it makes that `#ifndef` the include guard.
    guard_candidate: Option<String>,
    /// A function head whose body has not been seen: it defines its name
    /// only when the next column-0 line opens a brace. A wrapped prototype or
    /// a macro call spanning lines is followed by something else.
    pending_head: Option<(String, u32)>,
}

impl FileScan {
    fn new(path: &str, skipped_dirs: &[String]) -> Self {
        Self {
            path: path.to_string(),
            skipped: is_compat_file(path) || skipped_dirs.iter().any(|d| path.starts_with(d)),
            ..Self::default()
        }
    }

    /// Whether the current line may be compiled out.
    fn is_guarded(&self) -> bool {
        self.conditionals.contains(&Conditional::Guarded)
    }
}

/// Walks the column-0 lines of every scanned file, recording what each
/// defines outside any conditional but its include guard.
#[derive(Default)]
struct DefinitionScan {
    definitions: HashMap<String, Definition>,
    file: FileScan,
    /// Directories, each ending in `/`, whose files define nothing.
    skipped_dirs: Vec<String>,
}

impl DefinitionScan {
    fn line(&mut self, path: &str, line: u32, text: &str) {
        if self.file.path != path {
            self.file = FileScan::new(path, &self.skipped_dirs);
        }
        if self.file.skipped {
            return;
        }
        let kind = ColumnZero::classify(text);
        if let Some(guard) = self.file.guard_candidate.take()
            && kind == ColumnZero::Macro(&guard)
            && let Some(first) = self.file.conditionals.first_mut()
        {
            *first = Conditional::IncludeGuard;
        }
        if let Some((name, head_line)) = self.file.pending_head.take()
            && kind == ColumnZero::OpenBrace
        {
            self.record(name, head_line);
            return;
        }
        match kind {
            ColumnZero::OpenConditional { ifndef } => {
                if !self.file.seen_conditional && is_header(path) {
                    self.file.guard_candidate = ifndef.map(str::to_string);
                }
                self.file.seen_conditional = true;
                self.file.conditionals.push(Conditional::Guarded);
            }
            ColumnZero::CloseConditional => {
                self.file.conditionals.pop();
            }
            ColumnZero::Macro(name) | ColumnZero::Defines(name) => {
                if !self.file.is_guarded() {
                    self.record(name.to_string(), line);
                }
            }
            ColumnZero::Head(name) => {
                if !self.file.is_guarded() {
                    self.file.pending_head = Some((name.to_string(), line));
                }
            }
            ColumnZero::OpenBrace | ColumnZero::Other => {}
        }
    }

    /// Records `name` at the current file's `line` unless it is already
    /// known from a `.c` file, or from a header while this is one too: a
    /// function's body is the better place to point at than a header.
    fn record(&mut self, name: String, line: u32) {
        let definition = Definition {
            file: self.file.path.clone(),
            line,
        };
        match self.definitions.entry(name) {
            Entry::Vacant(entry) => {
                entry.insert(definition);
            }
            Entry::Occupied(mut entry) => {
                if is_header(&entry.get().file) && !is_header(&definition.file) {
                    entry.insert(definition);
                }
            }
        }
    }
}

/// Words that can open a column-0 line followed by `(` without naming what
/// the line defines.
const NOT_DEFINED_NAMES: [&str; 13] = [
    "if",
    "for",
    "while",
    "switch",
    "return",
    "sizeof",
    "typedef",
    "do",
    "else",
    "__attribute__",
    "__acquires",
    "__releases",
    "__must_hold",
];

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The leading identifier of `text`, if it starts with one.
fn leading_ident(text: &str) -> Option<&str> {
    let end = text
        .bytes()
        .position(|b| !is_ident_byte(b))
        .unwrap_or(text.len());
    let ident = &text[..end];
    (!ident.is_empty() && !ident.as_bytes()[0].is_ascii_digit()).then_some(ident)
}

/// What a column-0 line of C means to the scan, by a deliberately simple
/// matcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnZero<'a> {
    /// `#define NAME`.
    Macro(&'a str),
    /// `#if`, `#ifdef` or `#ifndef`, with the name an `#ifndef` tests.
    OpenConditional { ifndef: Option<&'a str> },
    /// `#endif`.
    CloseConditional,
    /// `struct|union|enum NAME {`, or a function head whose body opens on
    /// the same line.
    Defines(&'a str),
    /// A function head whose body, if it has one, opens on a later line; a
    /// wrapped declaration looks the same until its `;`.
    Head(&'a str),
    /// A `{` opening the line.
    OpenBrace,
    /// Anything else, including `#else`, `#elif` and other directives.
    Other,
}

impl<'a> ColumnZero<'a> {
    fn classify(line: &'a str) -> Self {
        if let Some(rest) = line.strip_prefix('#') {
            return Self::directive(rest.trim_start());
        }
        if line.starts_with('{') {
            return Self::OpenBrace;
        }
        let Some(first) = leading_ident(line) else {
            return Self::Other;
        };
        if matches!(first, "struct" | "union" | "enum") {
            let rest = line[first.len()..].trim_start();
            if let Some(name) = leading_ident(rest)
                && rest[name.len()..].trim_start().starts_with('{')
            {
                return Self::Defines(name);
            }
        }
        match function_name(line) {
            Some((name, true)) => Self::Defines(name),
            Some((name, false)) => Self::Head(name),
            None => Self::Other,
        }
    }

    /// A directive, given the text after its `#`.
    fn directive(rest: &'a str) -> Self {
        let Some(keyword) = leading_ident(rest) else {
            return Self::Other;
        };
        let operand = &rest[keyword.len()..];
        let name = || {
            operand
                .starts_with([' ', '\t'])
                .then(|| leading_ident(operand.trim_start()))
                .flatten()
        };
        match keyword {
            "define" => name().map_or(Self::Other, Self::Macro),
            "if" | "ifdef" => Self::OpenConditional { ifndef: None },
            "ifndef" => Self::OpenConditional { ifndef: name() },
            "endif" => Self::CloseConditional,
            _ => Self::Other,
        }
    }
}

/// The function a column-0 line starts: the identifier directly before the
/// first `(`, preceded only by identifiers, spaces and `*`, on a line that
/// does not end in `;` (a declaration or a macro call). The flag says
/// whether the body opens on the same line. A name with no lower-case
/// letter is a macro call spanning lines, which defines nothing.
fn function_name(line: &str) -> Option<(&str, bool)> {
    if line.trim_end().ends_with(';') {
        return None;
    }
    let open = line.find('(')?;
    let head = &line[..open];
    if !head
        .bytes()
        .all(|b| is_ident_byte(b) || b == b' ' || b == b'\t' || b == b'*')
    {
        return None;
    }
    let start = head
        .bytes()
        .rposition(|b| !is_ident_byte(b))
        .map_or(0, |i| i + 1);
    let name = &head[start..];
    if leading_ident(name) != Some(name)
        || NOT_DEFINED_NAMES.contains(&name)
        || !name.bytes().any(|b| b.is_ascii_lowercase())
    {
        return None;
    }
    if leading_ident(head) == Some("typedef") {
        return None;
    }
    Some((name, line[open..].contains('{')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = crate::git_cmd::in_dir(repo).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    /// A repository holding `files`, committed; returns the commit.
    fn repo_with(dir: &Path, files: &[(&str, &str)]) -> String {
        git(dir, &["init", "-q"]);
        git(dir, &["config", "user.name", "Test"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        for (path, content) in files {
            let full = dir.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, content).unwrap();
        }
        git(dir, &["add", "--all"]);
        git(
            dir,
            &["-c", "commit.gpgsign=false", "commit", "-qm", "fixture"],
        );
        git(dir, &["rev-parse", "HEAD"])
    }

    fn reference(name: &str, kind: ReferenceKind, path: &Path, commit: &str) -> ReferenceRepo {
        ReferenceRepo {
            name: name.to_string(),
            kind,
            path: path.to_path_buf(),
            revision: "HEAD".to_string(),
            commit: commit.to_string(),
            description: None,
            applies_to: None,
        }
    }

    const MODULE_CORE: &str = "\
// SPDX-License-Identifier: GPL-2.0
#include <linux/pds/pds_adminq.h>

static BLOCKING_NOTIFIER_HEAD(pds_notify_chain);
MODULE_LICENSE(\"GPL\");

int pdsc_notify(unsigned long event, void *data)
{
\tif (event)
\t\treturn 1;
\treturn 0;
}
EXPORT_SYMBOL_GPL(pdsc_notify);

static inline struct pdsc *
pdsc_from_dev(struct device *dev)
{
\treturn NULL;
}
";

    const MODULE_HEADER: &str = "\
#ifndef _PDS_ADMINQ_H_
#define _PDS_ADMINQ_H_
#define PDS_EVENT_RESET(x) (x)
struct pds_core_reset_event {
\tint state;
};
enum pds_event { PDS_EVENT_LINK };
union pds_core_adminq_cmd {
\tint opcode;
};
int pdsc_register_notify(struct notifier_block *nb);
typedef int (*pds_cb)(void *);
#endif
";

    /// A header that sorts before the source defining what it declares
    /// (`.` before `_`), with a prototype wrapped over two lines and a
    /// version-guarded shim of kernel APIs.
    const MODULE_ADMINQ_HEADER: &str = "\
#ifndef _PDSC_ADMINQ_H_
#define _PDSC_ADMINQ_H_
int pdsc_adminq_post(struct pdsc *pdsc,
\t\t     union pds_core_adminq_cmd *cmd);
#if LINUX_VERSION_CODE < KERNEL_VERSION(6, 2, 0)
#define timer_delete del_timer
static inline int pci_iov_vf_id(struct pci_dev *pdev)
{
\treturn 0;
}
struct ethtool_coalesce {
\tint rx;
};
#endif
static inline int pdsc_in_both(void)
{
\treturn 0;
}
#endif
";

    const MODULE_ADMINQ_SOURCE: &str = "\
int pdsc_adminq_post(struct pdsc *pdsc,
\t\t     union pds_core_adminq_cmd *cmd)
{
\treturn 0;
}
MODULE_PARM_DESC(lats_enable,
\t\t \"x\");
TRACE_EVENT(pdsc_adminq,
\tTP_PROTO(int x)
);
int pdsc_in_both(void)
{
\treturn 1;
}
";

    /// A compatibility header, unguarded, re-implementing kernel APIs.
    const MODULE_KCOMPAT: &str = "\
#define devlink_register _kc_devlink_register
static inline void *vcalloc(size_t n, size_t size)
{
\treturn NULL;
}
";

    struct Trees {
        _module: tempfile::TempDir,
        _kernel: tempfile::TempDir,
        module_dir: PathBuf,
        module_commit: String,
        kernel: ReferenceRepo,
    }

    fn trees() -> Trees {
        let module = tempfile::tempdir().unwrap();
        let module_commit = repo_with(
            module.path(),
            &[
                ("core/core.c", MODULE_CORE),
                ("include/linux/pds/pds_adminq.h", MODULE_HEADER),
                ("vfio/dirty.c", "void pds_vfio_dirty_reset(void) {}\n"),
                ("core/adminq.h", MODULE_ADMINQ_HEADER),
                ("core/adminq_post.c", MODULE_ADMINQ_SOURCE),
                ("core/kcompat.h", MODULE_KCOMPAT),
                ("Makefile", "obj-m += core/\n"),
            ],
        );
        let kernel_dir = tempfile::tempdir().unwrap();
        let kernel_commit = repo_with(
            kernel_dir.path(),
            &[
                ("drivers/net/ethernet/amd/pds_core/core.c", MODULE_CORE),
                ("include/linux/pds/pds_adminq.h", MODULE_HEADER),
                ("include/linux/kvm_host.h", "struct kvm {\n};\n"),
            ],
        );
        let kernel = reference(
            "kernel",
            ReferenceKind::Kernel,
            kernel_dir.path(),
            &kernel_commit,
        );
        Trees {
            module_dir: module.path().to_path_buf(),
            _module: module,
            _kernel: kernel_dir,
            module_commit,
            kernel,
        }
    }

    #[test]
    fn every_tree_is_listed_at_its_commit() {
        let t = trees();
        let index = TreeIndex::build(
            &t.module_dir,
            &t.module_commit,
            std::slice::from_ref(&t.kernel),
        );
        let kernel = LocationRepo::Reference("kernel".to_string());

        assert_eq!(
            index.contains_path(&LocationRepo::Review, "vfio/dirty.c"),
            Some(true)
        );
        assert_eq!(
            index.contains_path(&LocationRepo::Review, "Makefile"),
            Some(true)
        );
        assert_eq!(
            index.contains_path(
                &LocationRepo::Review,
                "drivers/net/ethernet/amd/pds_core/core.c"
            ),
            Some(false)
        );
        assert_eq!(
            index.contains_path(&kernel, "include/linux/kvm_host.h"),
            Some(true)
        );
        assert_eq!(index.contains_path(&kernel, "vfio/dirty.c"), Some(false));
        assert_eq!(
            index.paths(&LocationRepo::Review).map(HashSet::len),
            Some(7)
        );

        let shared: Vec<&LocationRepo> = index
            .repos_with_path("include/linux/pds/pds_adminq.h")
            .collect();
        assert_eq!(shared, vec![&LocationRepo::Review, &kernel]);
        let kernel_only: Vec<&LocationRepo> =
            index.repos_with_path("include/linux/kvm_host.h").collect();
        assert_eq!(kernel_only, vec![&kernel]);
    }

    #[test]
    fn a_tree_that_cannot_be_listed_is_absent_not_empty() {
        let t = trees();
        let missing = reference(
            "rdma-core",
            ReferenceKind::Source,
            &t.module_dir,
            &"f".repeat(40),
        );
        let index = TreeIndex::build(&t.module_dir, "unknown", &[t.kernel.clone(), missing]);

        assert_eq!(index.paths(&LocationRepo::Review), None);
        assert_eq!(
            index.contains_path(&LocationRepo::Review, "vfio/dirty.c"),
            None
        );
        assert_eq!(index.review_carries("vfio/dirty.c"), None);
        let rdma = LocationRepo::Reference("rdma-core".to_string());
        assert_eq!(index.contains_path(&rdma, "anything"), None);
        // Definitions need the review tree's listing to size the scan.
        assert!(!index.has_definitions());
        let kernel = LocationRepo::Reference("kernel".to_string());
        assert_eq!(
            index.contains_path(&kernel, "include/linux/kvm_host.h"),
            Some(true)
        );
    }

    #[test]
    fn a_commit_that_is_not_hex_never_reaches_git() {
        assert!(checked_commit("--output=/tmp/x").is_err());
        assert!(checked_commit("").is_err());
        assert!(checked_commit("HEAD").is_err());
        assert_eq!(checked_commit("8168ea0b4af1"), Ok("8168ea0b4af1"));
    }

    #[test]
    fn the_module_definitions_map_to_their_first_line() {
        let t = trees();
        let index = TreeIndex::build(
            &t.module_dir,
            &t.module_commit,
            std::slice::from_ref(&t.kernel),
        );
        assert!(index.has_definitions());

        let at = |file: &str, line: u32| {
            Some(Definition {
                file: file.to_string(),
                line,
            })
        };
        assert_eq!(
            index.definition("pdsc_notify").cloned(),
            at("core/core.c", 7)
        );
        assert_eq!(
            index.definition("pdsc_from_dev").cloned(),
            at("core/core.c", 16)
        );
        assert_eq!(
            index.definition("pds_vfio_dirty_reset").cloned(),
            at("vfio/dirty.c", 1)
        );
        let header = "include/linux/pds/pds_adminq.h";
        assert_eq!(index.definition("_PDS_ADMINQ_H_").cloned(), at(header, 2));
        assert_eq!(index.definition("PDS_EVENT_RESET").cloned(), at(header, 3));
        assert_eq!(
            index.definition("pds_core_reset_event").cloned(),
            at(header, 4)
        );
        assert_eq!(index.definition("pds_event").cloned(), at(header, 7));
        assert_eq!(
            index.definition("pds_core_adminq_cmd").cloned(),
            at(header, 8)
        );
        assert_eq!(
            index.definition("pdsc_notify").map(ToString::to_string),
            Some("core/core.c:7".to_string())
        );

        // Declarations, macro calls and typedefs define nothing.
        for name in [
            "pdsc_register_notify",
            "BLOCKING_NOTIFIER_HEAD",
            "MODULE_LICENSE",
            "EXPORT_SYMBOL_GPL",
            "pds_cb",
            "if",
            "kvm",
        ] {
            assert_eq!(index.definition(name), None, "{name}");
        }
    }

    #[test]
    fn shims_wrapped_prototypes_and_macro_calls_define_nothing() {
        let t = trees();
        let index = TreeIndex::build(
            &t.module_dir,
            &t.module_commit,
            std::slice::from_ref(&t.kernel),
        );
        let found = |name: &str| index.definition(name).map(ToString::to_string);

        // The body, not the wrapped prototype in the header sorting first.
        assert_eq!(
            found("pdsc_adminq_post"),
            Some("core/adminq_post.c:1".to_string())
        );
        // A source file's definition is preferred over a header's.
        assert_eq!(
            found("pdsc_in_both"),
            Some("core/adminq_post.c:11".to_string())
        );
        // The include guard does not make the header conditional.
        assert_eq!(
            found("_PDSC_ADMINQ_H_"),
            Some("core/adminq.h:2".to_string())
        );

        for name in [
            // Shims guarded by a kernel version.
            "timer_delete",
            "pci_iov_vf_id",
            "ethtool_coalesce",
            // An unguarded compatibility header.
            "devlink_register",
            "vcalloc",
            // Macro calls spanning lines.
            "MODULE_PARM_DESC",
            "TRACE_EVENT",
            "TP_PROTO",
        ] {
            assert_eq!(found(name), None, "{name}");
        }
    }

    #[test]
    fn a_head_defines_only_when_a_brace_follows() {
        let scan = |lines: &[&str]| {
            let mut scan = DefinitionScan::default();
            for (i, text) in lines.iter().enumerate() {
                scan.line("a.c", i as u32 + 1, text);
            }
            let mut names: Vec<String> = scan.definitions.into_keys().collect();
            names.sort();
            names
        };
        assert_eq!(scan(&["int f(int a,", "{"]), vec!["f"]);
        assert_eq!(
            scan(&["int f(int a,", "int g(void);", "{"]),
            Vec::<String>::new()
        );
        assert_eq!(scan(&["int f(int a,"]), Vec::<String>::new());
        assert_eq!(
            scan(&["int f(void)", "__releases(x)", "{"]),
            Vec::<String>::new()
        );
        assert_eq!(
            scan(&[
                "#ifdef CONFIG_X",
                "int f(void)",
                "{",
                "#endif",
                "int g(void)",
                "{"
            ]),
            vec!["g"]
        );
        // A source file has no include guard: its #ifndef is a condition.
        assert_eq!(
            scan(&["#ifndef A", "#define A", "#define B 1", "#endif"]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn definitions_need_a_kernel_reference() {
        let t = trees();
        let source = ReferenceRepo {
            kind: ReferenceKind::Source,
            ..t.kernel.clone()
        };
        let index = TreeIndex::build(&t.module_dir, &t.module_commit, &[source]);
        assert!(!index.has_definitions());
        assert_eq!(index.definition("pdsc_notify"), None);
        assert_eq!(
            index.contains_path(&LocationRepo::Review, "vfio/dirty.c"),
            Some(true)
        );
    }

    #[test]
    fn reference_listings_are_shared_by_commit() {
        let t = trees();
        let first = TreeIndex::build(
            &t.module_dir,
            &t.module_commit,
            std::slice::from_ref(&t.kernel),
        );
        let second = TreeIndex::build(
            &t.module_dir,
            &t.module_commit,
            std::slice::from_ref(&t.kernel),
        );
        let kernel = LocationRepo::Reference("kernel".to_string());
        let shared = |index: &TreeIndex| {
            index
                .paths
                .iter()
                .find(|(repo, _)| *repo == kernel)
                .map(|(_, set)| set.clone())
                .unwrap()
        };
        assert!(Arc::ptr_eq(&shared(&first), &shared(&second)));
    }

    #[tokio::test]
    async fn nothing_is_built_without_references() {
        let t = trees();
        let index = TreeIndex::build_for_patch(
            t.module_dir.clone(),
            t.module_commit.clone(),
            None,
            Vec::new(),
        )
        .await;
        assert!(index.is_none());

        let index = TreeIndex::build_for_patch(
            t.module_dir.clone(),
            t.module_commit.clone(),
            None,
            vec![t.kernel.clone()],
        )
        .await
        .unwrap();
        assert_eq!(
            index.contains_path(&LocationRepo::Review, "vfio/dirty.c"),
            Some(true)
        );
        assert!(format!("{index:?}").contains("review: 7 paths"));
    }

    #[test]
    fn the_matcher_reads_column_zero_c() {
        use ColumnZero::*;
        for (line, kind) in [
            ("int foo(void)", Head("foo")),
            ("static int __init foo_init(void)", Head("foo_init")),
            ("static inline struct x *bar(int a,", Head("bar")),
            ("int one_line(void) { return 9; }", Defines("one_line")),
            ("int proto(void);", Other),
            ("static void (*cb)(void)", Other),
            ("MODULE_AUTHOR(\"x\");", Other),
            ("MODULE_PARM_DESC(x,", Other),
            ("typedef int (fn)(void)", Other),
            ("__attribute__((x)) int f(void)", Other),
            ("#define FOO 1", Macro("FOO")),
            ("# define FOO_BAR(x) x", Macro("FOO_BAR")),
            ("#defined", Other),
            ("#include <x.h>", Other),
            (
                "#ifndef _X_H_",
                OpenConditional {
                    ifndef: Some("_X_H_"),
                },
            ),
            (
                "#if LINUX_VERSION_CODE < 1",
                OpenConditional { ifndef: None },
            ),
            ("#  ifdef CONFIG_X", OpenConditional { ifndef: None }),
            ("#else", Other),
            ("#endif /* X */", CloseConditional),
            ("{", OpenBrace),
            ("struct foo {", Defines("foo")),
            ("struct foo{", Defines("foo")),
            ("union u {", Defines("u")),
            ("enum {", Other),
            ("struct foo;", Other),
            ("struct foo bar = {", Other),
            ("struct foo *make_foo(void)", Head("make_foo")),
            ("1abc(x)", Other),
        ] {
            assert_eq!(ColumnZero::classify(line), kind, "{line}");
        }
    }

    #[test]
    fn grep_records_parse_with_colons_in_paths() {
        let out = b"abc:dir/a:b.c\x003\x00int f(void)\nabc:dir/a:b.c\x004\x00{\nabc:x.h\x00nope\x00int g(void) {\nzzz:y.c\x001\x00int h(void) {\nabc:z.c\x009\x00int f(void) {\n";
        let defs = parse_grep_definitions(out, "abc", Vec::new());
        assert_eq!(defs.len(), 1);
        assert_eq!(
            defs.get("f"),
            Some(&Definition {
                file: "dir/a:b.c".to_string(),
                line: 3
            })
        );
    }

    /// A module whose top-level Kbuild builds its fwctl core backport only
    /// for kernels without fwctl, as driver-b does, and whose Makefile
    /// keeps the Kbuild half of an external module build under
    /// `ifneq ($(KERNELRELEASE),)`.
    fn conditional_module() -> (tempfile::TempDir, String) {
        let module = tempfile::tempdir().unwrap();
        let commit = repo_with(
            module.path(),
            &[
                (
                    "Kbuild",
                    "obj-$(CONFIG_PDS_CORE) += core/\n\
                     ifeq ($(CONFIG_FWCTL),)\n  \
                     obj-$(CONFIG_FWCTL) += fwctl/ \\\n\t./extra/\n\
                     else\n  \
                     obj-$(CONFIG_FWCTL_PDS) += fwctl/pds/\n\
                     endif\n\
                     obj-m += after/\n",
                ),
                (
                    "Makefile",
                    "ifneq ($(KERNELRELEASE),)\nobj-m += vfio/\nelse\nall:\n\t$(MAKE)\nendif\n",
                ),
                ("core/Makefile", "ifdef CONFIG_X\nobj-y += sub/\nendif\n"),
                ("core/core.c", "int pdsc_notify(void)\n{\n}\n"),
                ("core/sub/sub.c", "int sub_fn(void)\n{\n}\n"),
                ("fwctl/main.c", "int fwctl_register(void)\n{\n}\n"),
                ("fwctl/pds/main.c", "int pdsc_fwctl_probe(void)\n{\n}\n"),
                ("extra/x.c", "int extra_fn(void)\n{\n}\n"),
                ("after/a.c", "int after_fn(void)\n{\n}\n"),
                ("vfio/pci.c", "int pds_vfio_probe(void)\n{\n}\n"),
            ],
        );
        (module, commit)
    }

    #[test]
    fn directories_built_only_under_a_config_test_define_nothing() {
        let (module, commit) = conditional_module();
        let build_files = grep_lines(module.path(), &commit, "", &BUILD_FILE_PATHSPECS).unwrap();
        assert_eq!(
            parse_conditional_dirs(&build_files, &commit),
            vec!["fwctl/", "extra/", "fwctl/pds/", "core/sub/"]
        );

        let definitions = scan_definitions(module.path(), &commit).unwrap();
        for name in ["pdsc_notify", "after_fn", "pds_vfio_probe"] {
            assert!(definitions.contains_key(name), "{name}");
        }
        for name in ["fwctl_register", "pdsc_fwctl_probe", "extra_fn", "sub_fn"] {
            assert!(!definitions.contains_key(name), "{name}");
        }
    }

    #[test]
    fn a_build_file_scan_tracks_conditionals_and_continuations() {
        let mut scan = BuildFileScan::default();
        let lines = [
            ("obj-m += a/", None),
            ("ifneq ($(KERNELRELEASE),)", None),
            ("obj-m += b/", None),
            ("else ifeq ($(CONFIG_X),y)", None),
            ("obj-y += c/ \\", Some(" c/ \\")),
            ("d/", Some("d/")),
            ("all:", None),
            ("endif", None),
            ("obj-m += e/", None),
        ];
        for (line, values) in lines {
            assert_eq!(scan.line(line), values, "{line}");
        }
        assert_eq!(
            obj_dirs("sub", " ./x/ y.o $(Z)/ w/").collect::<Vec<_>>(),
            vec!["sub/x/", "sub/w/"]
        );
    }

    /// A grep.column setting adds a column field to every git grep record
    /// unless the scan turns it off.
    #[test]
    fn a_grep_column_setting_does_not_hide_definitions() {
        let t = trees();
        git(&t.module_dir, &["config", "grep.column", "true"]);
        let definitions = scan_definitions(&t.module_dir, &t.module_commit).unwrap();
        assert_eq!(
            definitions.get("pdsc_notify").map(ToString::to_string),
            Some("core/core.c:7".to_string())
        );
    }

    /// A patch that deletes one file, in a series whose next patch adds
    /// another: both are paths a location in the review can name.
    #[test]
    fn the_review_tree_holds_the_parent_and_series_tip_paths() {
        let t = trees();
        git(&t.module_dir, &["rm", "-q", "vfio/dirty.c"]);
        git(
            &t.module_dir,
            &["-c", "commit.gpgsign=false", "commit", "-qm", "delete"],
        );
        let patch = git(&t.module_dir, &["rev-parse", "HEAD"]);
        fs::write(t.module_dir.join("core/later.c"), "int later;\n").unwrap();
        git(&t.module_dir, &["add", "--all"]);
        git(
            &t.module_dir,
            &["-c", "commit.gpgsign=false", "commit", "-qm", "add"],
        );
        let tip = git(&t.module_dir, &["rev-parse", "HEAD"]);

        let refs = std::slice::from_ref(&t.kernel);
        let index = TreeIndex::build_in_series(&t.module_dir, &patch, Some(&tip), refs);
        for path in ["vfio/dirty.c", "core/later.c", "core/core.c"] {
            assert_eq!(
                index.contains_path(&LocationRepo::Review, path),
                Some(true),
                "{path}"
            );
        }
        // Only the patch's own tree is what the module ships at this patch.
        assert_eq!(index.review_carries("core/core.c"), Some(true));
        assert_eq!(index.review_carries("vfio/dirty.c"), Some(false));
        assert_eq!(index.review_carries("core/later.c"), Some(false));

        // A root commit has no parent, and a tip that cannot be listed
        // leaves the patch's own paths.
        let index = TreeIndex::build_in_series(
            &t.module_dir,
            &t.module_commit,
            Some(&"f".repeat(40)),
            refs,
        );
        assert_eq!(
            index.paths(&LocationRepo::Review).map(HashSet::len),
            Some(7)
        );
    }
}
