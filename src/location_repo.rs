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

//! The repository a finding's location is in.
//!
//! A review with reference repositories attached reads code from more than one
//! tree, and a path such as `include/linux/pds/pds_adminq.h` can exist in the
//! module under review and in the kernel it builds against. A location names
//! its tree with a `"repo"` key that takes the tools' own vocabulary:
//! [`REPO_ARG_REVIEW`] or an attached reference's name. The model writes in the
//! location exactly what it passed to the tool that read the code, so the two
//! cannot drift apart.
//!
//! [`LocationRepo`] is the parsed form. It is built only by
//! [`LocationRepo::parse`] against the references attached to the patch, so an
//! unknown name is a parse error carrying the message a stage validator hands
//! back to the model, never a third state.
//!
//! [`normalize_location`] accepts the older prose habit of writing
//! `kernel:path` or `kernel:path:line` into `file`. It runs before validation and again before a
//! location is stored, and it does nothing when no reference is attached, so a
//! review without references sees no change.

use crate::toolbox::{REPO_ARG, REPO_ARG_REVIEW, ReferenceRepo};
use serde_json::Value;
use std::fmt;

/// The key a location names its repository under. The tools' argument name,
/// so a location and the tool call that read its code use the same word.
pub const LOCATION_REPO_KEY: &str = REPO_ARG;

/// The key holding a location's path.
pub(crate) const LOCATION_FILE_KEY: &str = "file";

/// A warning a deciding stage's reduce writes on a location it had to accept
/// although the location does not match the trees it names. Only the reduce
/// sets one, once the stage's retry budget is spent; a copy the model wrote is
/// removed first ([`strip_location_flags`]), so a flag always means the check
/// failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationFlag {
    /// The location cites a reference's copy of a file, function or struct
    /// the code under review carries itself.
    RepoConflict,
    /// The location's file is not in the repository it names.
    UnverifiedPath,
}

impl LocationFlag {
    /// Every flag, for stripping.
    pub const ALL: [Self; 2] = [Self::RepoConflict, Self::UnverifiedPath];

    /// The key the flag is stored under, with the value `true`.
    pub fn key(self) -> &'static str {
        match self {
            Self::RepoConflict => "repo_conflict",
            Self::UnverifiedPath => "unverified_path",
        }
    }

    /// Writes the flag onto a location object.
    pub fn set(self, location: &mut Value) {
        if let Some(obj) = location.as_object_mut() {
            obj.insert(self.key().to_string(), Value::Bool(true));
        }
    }
}

/// Removes every [`LocationFlag`] key from a location. The flags record a
/// check's verdict, so a model-written copy is never kept. Returns whether
/// anything was removed.
pub fn strip_location_flags(location: &mut Value) -> bool {
    let Some(obj) = location.as_object_mut() else {
        return false;
    };
    let mut removed = false;
    for flag in LocationFlag::ALL {
        removed |= obj.remove(flag.key()).is_some();
    }
    removed
}

/// The key holding a location's line.
const LOCATION_LINE_KEY: &str = "line";

/// The most characters of a rejected name quoted back in an error. The name
/// is model output, and a runaway value must not swell the retry prompt.
const MAX_QUOTED_NAME_CHARS: usize = 64;

/// Which attached repository a location is in.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LocationRepo {
    /// The code under review.
    Review,
    /// An attached reference repository, by its name.
    Reference(String),
}

impl LocationRepo {
    /// Reads a location's `repo` value against the references attached to the
    /// patch. Only [`REPO_ARG_REVIEW`] and an attached reference's exact name
    /// are accepted; a dormant or misspelled name is rejected rather than
    /// guessed at, since a guess would check the location in the wrong tree.
    pub fn parse(name: &str, references: &[ReferenceRepo]) -> Result<Self, UnknownLocationRepo> {
        if name == REPO_ARG_REVIEW {
            return Ok(Self::Review);
        }
        if references.iter().any(|r| r.name == name) {
            return Ok(Self::Reference(name.to_string()));
        }
        Err(UnknownLocationRepo {
            given: name.to_string(),
            attached: attached_names(references),
        })
    }

    /// The name as the tools and the location spell it.
    pub fn name(&self) -> &str {
        match self {
            Self::Review => REPO_ARG_REVIEW,
            Self::Reference(name) => name,
        }
    }

    /// Whether the location is in the code under review.
    pub fn is_review(&self) -> bool {
        matches!(self, Self::Review)
    }
}

impl fmt::Display for LocationRepo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A location's `repo` names no attached repository.
///
/// Its `Display` is the tail of a validator message: the caller prefixes the
/// location it is about, e.g. `location 3 of concern C2 `, and the text goes on
/// to name every valid value and the fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownLocationRepo {
    given: String,
    attached: Vec<String>,
}

impl fmt::Display for UnknownLocationRepo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let given: String = self.given.chars().take(MAX_QUOTED_NAME_CHARS).collect();
        write!(
            f,
            "has {LOCATION_REPO_KEY} {given:?} but {}",
            valid_values_and_fix(&self.attached)
        )
    }
}

impl std::error::Error for UnknownLocationRepo {}

/// The end of every message about a location's `repo`: the values it can take
/// for this patch, and what to do.
fn valid_values_and_fix(attached: &[String]) -> String {
    let quoted: Vec<String> = attached.iter().map(|n| format!("\"{n}\"")).collect();
    let list = match quoted.as_slice() {
        [only] => format!("the only attached repository is {only}"),
        [rest @ .., last] => {
            format!(
                "the attached repositories are {} and {last}",
                rest.join(", ")
            )
        }
        [] => String::new(),
    };
    format!("{list}; set \"{LOCATION_REPO_KEY}\" to the one you read the code in")
}

/// Why a location does not name an attached repository.
///
/// Like [`UnknownLocationRepo`], its `Display` is the tail of a validator
/// message, and the caller prefixes the location it is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocationRepoError {
    /// The location is not a JSON object, so it has no keys at all.
    NotAnObject,
    /// The location has no `repo` string, and its `file` carries no attached
    /// repository's prefix either.
    Missing { attached: Vec<String> },
    /// The location's `repo` names no attached repository.
    Unknown(UnknownLocationRepo),
}

impl fmt::Display for LocationRepoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnObject => write!(
                f,
                "is not an object; write each location as an object that opens with \
                 \"{LOCATION_REPO_KEY}\""
            ),
            Self::Missing { attached } => write!(
                f,
                "has no \"{LOCATION_REPO_KEY}\" string but {}",
                valid_values_and_fix(attached)
            ),
            Self::Unknown(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for LocationRepoError {}

/// Reads which attached repository a location names.
///
/// The location is read through [`normalize_location`] first, so a
/// `kernel:path` file is accepted exactly as the reduce will store it. A
/// prefix that conflicts with `repo` is left to the checks that compare a path
/// with the tree it names; here `repo` alone decides.
pub fn read_location_repo(
    location: &Value,
    references: &[ReferenceRepo],
) -> Result<LocationRepo, LocationRepoError> {
    if !location.is_object() {
        return Err(LocationRepoError::NotAnObject);
    }
    let mut normalized = location.clone();
    normalize_location(&mut normalized, references);
    match normalized.get(LOCATION_REPO_KEY).and_then(Value::as_str) {
        Some(name) => LocationRepo::parse(name, references).map_err(LocationRepoError::Unknown),
        None => Err(LocationRepoError::Missing {
            attached: attached_names(references),
        }),
    }
}

/// Every value a location's `repo` can take for this patch, the code under
/// review first.
fn attached_names(references: &[ReferenceRepo]) -> Vec<String> {
    std::iter::once(REPO_ARG_REVIEW.to_string())
        .chain(references.iter().map(|r| r.name.clone()))
        .collect()
}

/// Splits a `file` written as `<name>:<path>` when `<name>` is an attached
/// repository, returning the name and the path.
pub(crate) fn split_repo_prefix<'a>(
    file: &'a str,
    references: &[ReferenceRepo],
) -> Option<(&'a str, &'a str)> {
    let (name, path) = file.split_once(':')?;
    if path.is_empty() {
        return None;
    }
    let attached = name == REPO_ARG_REVIEW || references.iter().any(|r| r.name == name);
    attached.then_some((name, path))
}

/// Splits a trailing `:<line>` or `:<first>-<last>` off a path written in
/// the prose form `repo:file:line`, returning the path and its first line.
/// A path whose tail after the last colon is not such a number is returned
/// whole.
pub(crate) fn split_line_suffix(path: &str) -> (&str, Option<u64>) {
    let Some((file, tail)) = path.rsplit_once(':') else {
        return (path, None);
    };
    let is_number = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let first = match tail.split_once('-') {
        Some((first, last)) if is_number(first) && is_number(last) => first,
        None if is_number(tail) => tail,
        _ => return (path, None),
    };
    match first.parse() {
        Ok(line) if !file.is_empty() => (file, Some(line)),
        _ => (path, None),
    }
}

/// Moves a `<name>:<path>` prefix in a location's `file` into its `repo`.
///
/// The prompt's prose rule "cite repo:file:line" predates the key, and models
/// carry the prefix into `file`. Rejecting it would cost a stage retry over a
/// spelling, so it is accepted:
///
/// - `repo` absent: the prefix becomes `repo` and `file` keeps the path.
/// - `repo` equal to the prefix: the prefix is stripped.
/// - `repo` different from the prefix: both are left as written, so the
///   validator reports the conflict rather than this helper picking a side.
///
/// A prefixed `file` written in full, `kernel:path:42` or `kernel:path:42-48`,
/// also loses its line suffix, which no tree's path carries; the first line
/// moves into `line` when the location has none.
///
/// A prefix that names no attached repository is not a prefix (a path can
/// contain a colon), and nothing is done when no reference is attached, so a
/// review without references keeps its locations byte for byte. Returns
/// whether the location changed.
pub fn normalize_location(location: &mut Value, references: &[ReferenceRepo]) -> bool {
    if references.is_empty() {
        return false;
    }
    let Some(obj) = location.as_object_mut() else {
        return false;
    };
    let Some(file) = obj.get(LOCATION_FILE_KEY).and_then(Value::as_str) else {
        return false;
    };
    let Some((name, path)) = split_repo_prefix(file, references) else {
        return false;
    };
    let (path, line) = split_line_suffix(path);
    let (name, path) = (name.to_string(), path.to_string());
    match obj.get(LOCATION_REPO_KEY) {
        None | Some(Value::Null) => {
            obj.insert(LOCATION_REPO_KEY.to_string(), Value::from(name));
        }
        Some(Value::String(repo)) if *repo == name => {}
        Some(_) => return false,
    }
    obj.insert(LOCATION_FILE_KEY.to_string(), Value::from(path));
    if let Some(line) = line
        && obj.get(LOCATION_LINE_KEY).is_none_or(Value::is_null)
    {
        obj.insert(LOCATION_LINE_KEY.to_string(), Value::from(line));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ReferenceKind;
    use serde_json::json;
    use std::path::PathBuf;

    fn reference(name: &str) -> ReferenceRepo {
        ReferenceRepo {
            name: name.to_string(),
            kind: if name == "kernel" {
                ReferenceKind::Kernel
            } else {
                ReferenceKind::Source
            },
            path: PathBuf::from(format!("/refs/{name}")),
            revision: "HEAD".to_string(),
            commit: "0".repeat(40),
            description: None,
            applies_to: None,
        }
    }

    fn kernel() -> Vec<ReferenceRepo> {
        vec![reference("kernel")]
    }

    #[test]
    fn review_and_attached_names_parse() {
        let refs = vec![reference("kernel"), reference("rdma-core")];
        assert_eq!(
            LocationRepo::parse("review", &refs),
            Ok(LocationRepo::Review)
        );
        assert_eq!(
            LocationRepo::parse("kernel", &refs),
            Ok(LocationRepo::Reference("kernel".to_string()))
        );
        let rdma = LocationRepo::parse("rdma-core", &refs).unwrap();
        assert_eq!(rdma.name(), "rdma-core");
        assert_eq!(rdma.to_string(), "rdma-core");
        assert!(!rdma.is_review());
        assert!(LocationRepo::Review.is_review());
        assert_eq!(LocationRepo::Review.to_string(), "review");
    }

    #[test]
    fn an_unknown_name_names_every_valid_value_and_the_fix() {
        let err = LocationRepo::parse("linux", &kernel()).unwrap_err();
        assert_eq!(
            format!("location 3 of concern C2 {err}"),
            "location 3 of concern C2 has repo \"linux\" but the attached repositories are \
             \"review\" and \"kernel\"; set \"repo\" to the one you read the code in"
        );

        let refs = vec![reference("kernel"), reference("rdma-core")];
        let err = LocationRepo::parse("Kernel", &refs).unwrap_err();
        assert_eq!(
            err.to_string(),
            "has repo \"Kernel\" but the attached repositories are \"review\", \"kernel\" \
             and \"rdma-core\"; set \"repo\" to the one you read the code in"
        );
    }

    #[test]
    fn without_references_only_review_parses() {
        assert_eq!(LocationRepo::parse("review", &[]), Ok(LocationRepo::Review));
        let err = LocationRepo::parse("kernel", &[]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "has repo \"kernel\" but the only attached repository is \"review\"; set \
             \"repo\" to the one you read the code in"
        );
    }

    #[test]
    fn a_rejected_name_is_quoted_escaped_and_clipped() {
        let err = LocationRepo::parse("a\"b\nc", &kernel()).unwrap_err();
        assert!(
            err.to_string().starts_with("has repo \"a\\\"b\\nc\" but"),
            "{err}"
        );

        let long = "x".repeat(500);
        let err = LocationRepo::parse(&long, &kernel())
            .unwrap_err()
            .to_string();
        assert!(err.contains(&format!("\"{}\"", "x".repeat(MAX_QUOTED_NAME_CHARS))));
        assert!(!err.contains(&"x".repeat(MAX_QUOTED_NAME_CHARS + 1)));
    }

    #[test]
    fn a_prefix_without_repo_moves_into_repo() {
        let mut loc = json!({"file": "kernel:include/linux/kvm_host.h", "line": 7});
        assert!(normalize_location(&mut loc, &kernel()));
        assert_eq!(
            loc,
            json!({"repo": "kernel", "file": "include/linux/kvm_host.h", "line": 7})
        );

        let mut loc = json!({"file": "review:vfio/dirty.c", "repo": null});
        assert!(normalize_location(&mut loc, &kernel()));
        assert_eq!(loc, json!({"repo": "review", "file": "vfio/dirty.c"}));
    }

    #[test]
    fn a_prose_line_suffix_moves_into_line() {
        let mut loc = json!({"file": "kernel:include/linux/kref.h:42"});
        assert!(normalize_location(&mut loc, &kernel()));
        assert_eq!(
            loc,
            json!({"repo": "kernel", "file": "include/linux/kref.h", "line": 42})
        );

        let mut loc = json!({"file": "kernel:mm/slab.c:42-48", "line": null});
        assert!(normalize_location(&mut loc, &kernel()));
        assert_eq!(
            loc,
            json!({"repo": "kernel", "file": "mm/slab.c", "line": 42})
        );

        // A line already set is kept; the suffix is only dropped.
        let mut loc = json!({"repo": "kernel", "file": "kernel:mm/slab.c:42", "line": 40});
        assert!(normalize_location(&mut loc, &kernel()));
        assert_eq!(
            loc,
            json!({"repo": "kernel", "file": "mm/slab.c", "line": 40})
        );

        // A tail that is not a line number stays part of the path.
        for (file, path) in [
            ("kernel:dir/a:b.c", "dir/a:b.c"),
            ("kernel:mm/slab.c:", "mm/slab.c:"),
            ("kernel:mm/slab.c:4-", "mm/slab.c:4-"),
            ("kernel::42", ":42"),
        ] {
            let mut loc = json!({ "file": file });
            assert!(normalize_location(&mut loc, &kernel()), "{file}");
            assert_eq!(loc, json!({"repo": "kernel", "file": path}), "{file}");
        }
    }

    #[test]
    fn a_prefix_equal_to_repo_is_stripped() {
        let mut loc = json!({"repo": "kernel", "file": "kernel:mm/slab.c"});
        assert!(normalize_location(&mut loc, &kernel()));
        assert_eq!(loc, json!({"repo": "kernel", "file": "mm/slab.c"}));
    }

    #[test]
    fn a_prefix_conflicting_with_repo_is_left_for_the_validator() {
        let original = json!({"repo": "review", "file": "kernel:mm/slab.c"});
        let mut loc = original.clone();
        assert!(!normalize_location(&mut loc, &kernel()));
        assert_eq!(loc, original);

        let original = json!({"repo": 3, "file": "kernel:mm/slab.c"});
        let mut loc = original.clone();
        assert!(!normalize_location(&mut loc, &kernel()));
        assert_eq!(loc, original);
    }

    #[test]
    fn locations_without_an_attached_prefix_are_untouched() {
        for original in [
            json!({"file": "vfio/dirty.c"}),
            json!({"repo": "kernel", "file": "mm/slab.c"}),
            json!({"file": "linux:mm/slab.c"}),
            json!({"file": "kernel:"}),
            json!({"file": "C:\\path"}),
            json!({"file": 12}),
            json!({"line": 4}),
            json!("kernel:mm/slab.c"),
        ] {
            let mut loc = original.clone();
            assert!(!normalize_location(&mut loc, &kernel()), "{original}");
            assert_eq!(loc, original);
        }
    }

    #[test]
    fn nothing_changes_without_references() {
        let original = json!({"file": "review:vfio/dirty.c"});
        let mut loc = original.clone();
        assert!(!normalize_location(&mut loc, &[]));
        assert_eq!(loc, original);
    }

    #[test]
    fn a_location_is_read_through_normalization() {
        let refs = kernel();
        let read = |loc: Value| read_location_repo(&loc, &refs);
        assert_eq!(
            read(json!({"repo": "review", "file": "vfio/dirty.c"})),
            Ok(LocationRepo::Review)
        );
        assert_eq!(
            read(json!({"file": "kernel:include/linux/kref.h"})),
            Ok(LocationRepo::Reference("kernel".to_string()))
        );
        assert_eq!(
            read(json!({"repo": null, "file": "review:vfio/dirty.c"})),
            Ok(LocationRepo::Review)
        );
        // A conflicting prefix is not this check's business: repo decides.
        assert_eq!(
            read(json!({"repo": "review", "file": "kernel:mm/slab.c"})),
            Ok(LocationRepo::Review)
        );
    }

    #[test]
    fn a_location_without_a_usable_repo_names_the_fix() {
        let refs = kernel();
        for loc in [
            json!({"file": "vfio/dirty.c"}),
            json!({"repo": null, "file": "vfio/dirty.c"}),
            json!({"repo": 3, "file": "kernel:mm/slab.c"}),
            json!({"file": "linux:mm/slab.c"}),
        ] {
            let err = read_location_repo(&loc, &refs).unwrap_err();
            assert_eq!(
                err.to_string(),
                "has no \"repo\" string but the attached repositories are \"review\" and \
                 \"kernel\"; set \"repo\" to the one you read the code in",
                "{loc}"
            );
        }

        let err =
            read_location_repo(&json!({"repo": "linux", "file": "mm/slab.c"}), &refs).unwrap_err();
        assert!(matches!(err, LocationRepoError::Unknown(_)));
        assert!(
            err.to_string().starts_with("has repo \"linux\" but"),
            "{err}"
        );

        let err = read_location_repo(&json!("kernel:mm/slab.c"), &refs).unwrap_err();
        assert_eq!(err, LocationRepoError::NotAnObject);
        assert_eq!(
            err.to_string(),
            "is not an object; write each location as an object that opens with \"repo\""
        );
    }

    #[test]
    fn flags_are_set_and_stripped_by_key() {
        let mut loc = json!({"repo": "kernel", "file": "mm/slab.c"});
        LocationFlag::RepoConflict.set(&mut loc);
        LocationFlag::UnverifiedPath.set(&mut loc);
        assert_eq!(loc["repo_conflict"], json!(true));
        assert_eq!(loc["unverified_path"], json!(true));

        assert!(strip_location_flags(&mut loc));
        assert_eq!(loc, json!({"repo": "kernel", "file": "mm/slab.c"}));
        assert!(!strip_location_flags(&mut loc));

        // A model-written value of any type is a flag all the same.
        let mut loc = json!({"file": "a.c", "repo_conflict": "no", "unverified_path": null});
        assert!(strip_location_flags(&mut loc));
        assert_eq!(loc, json!({"file": "a.c"}));

        let mut not_object = json!("a.c");
        LocationFlag::RepoConflict.set(&mut not_object);
        assert!(!strip_location_flags(&mut not_object));
        assert_eq!(not_object, json!("a.c"));
    }
}
