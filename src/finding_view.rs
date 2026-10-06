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

//! A typed, one-line view of a finding from a review's output JSON.
//!
//! The verification stage has no validator, so a finding's keys are
//! free-form JSON. This module reads them once, with one rule per key, so
//! that `sashiko review` and `sashiko-cli` print the same line for the same
//! finding and neither reads a key that no workflow emits.

use crate::db::Severity;
use serde_json::Value;
use std::io;
use termcolor::{Color, ColorSpec, WriteColor};

/// How long a finding title may be before a one-line view clips it. Matches
/// FINDING_TITLE_MAX_CHARS in static/index.html.
pub const TITLE_MAX_CHARS: usize = 160;

/// The title shown for a finding whose `problem` is missing or empty.
const NO_DESCRIPTION: &str = "(no description)";

/// The line printed once after a list in which no finding says whether it is
/// speculative. The web page prints the same sentence.
pub const NOT_MARKED_NOTE: &str = "These findings do not say whether they are speculative.";

/// What a one-line view shows for one finding of a review's output JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingLine {
    /// The parsed `severity`; `Unknown` for a missing or unrecognised value,
    /// read by the same rule as the severity counts (see `parse_severity`).
    pub severity: Severity,
    /// The trimmed `problem`, clipped at `TITLE_MAX_CHARS` code points.
    pub title: String,
    /// `None` when the key is absent or not a JSON boolean, so a finding the
    /// model did not mark is never read as one it verified.
    pub speculative: Option<bool>,
    /// Only a JSON `true` counts, as in the severity counts.
    pub preexisting: bool,
    /// The first location that names something, as `file:line (function)`.
    pub location: Option<String>,
}

impl FindingLine {
    /// Reads one finding. A non-object finding reads as an empty one:
    /// Unknown, "(no description)", nothing else.
    pub fn from_json(finding: &Value) -> Self {
        let severity = finding
            .get("severity")
            .and_then(Value::as_str)
            .map_or(Severity::Unknown, parse_severity);
        let problem = finding.get("problem").and_then(Value::as_str);
        Self {
            severity,
            title: clip_title(problem.unwrap_or("")),
            speculative: finding.get("speculative").and_then(Value::as_bool),
            preexisting: finding.get("preexisting").and_then(Value::as_bool) == Some(true),
            location: finding
                .get("locations")
                .and_then(Value::as_array)
                .and_then(|locations| locations.iter().find_map(format_location)),
        }
    }
}

/// True when no finding carries a boolean `speculative` key. Every review
/// stored before the key existed is such a list, and so is a run where the
/// model left the key out of every finding.
pub fn none_marked_speculative(findings: &[Value]) -> bool {
    !findings
        .iter()
        .any(|finding| finding.get("speculative").is_some_and(Value::is_boolean))
}

/// Writes one finding as `  [Severity] title (speculative) (preexisting)`,
/// with its location on an indented second line when it has one. The tags
/// use the web page's order and wording.
pub fn write_finding_line(out: &mut impl WriteColor, line: &FindingLine) -> io::Result<()> {
    write!(out, "  ")?;
    out.set_color(ColorSpec::new().set_fg(Some(severity_color(line.severity))))?;
    write!(out, "[{}]", line.severity)?;
    out.reset()?;
    write!(out, " {}", line.title)?;
    if line.speculative == Some(true) {
        write!(out, " (speculative)")?;
    }
    if line.preexisting {
        write!(out, " (preexisting)")?;
    }
    writeln!(out)?;
    if let Some(location) = &line.location {
        writeln!(out, "    {location}")?;
    }
    Ok(())
}

/// Writes `NOT_MARKED_NOTE` as one dim line when `findings` is a non-empty
/// list in which no finding says whether it is speculative. Without it, an
/// old review's untagged findings would look verified.
pub fn write_not_marked_note(out: &mut impl WriteColor, findings: &[Value]) -> io::Result<()> {
    if findings.is_empty() || !none_marked_speculative(findings) {
        return Ok(());
    }
    out.set_color(ColorSpec::new().set_dimmed(true))?;
    write!(out, "  {NOT_MARKED_NOTE}")?;
    out.reset()?;
    writeln!(out)
}

/// Reads a severity by the rule the severity counts and the exit code use:
/// one of the four names, compared without ASCII case and without trimming.
/// `Severity::from_str` also accepts prefixes such as "High (capped)", which
/// the counts skip, so a label read that way could print a red `[High]` for a
/// finding that no count includes and that does not fail the exit code.
fn parse_severity(text: &str) -> Severity {
    [
        Severity::Critical,
        Severity::High,
        Severity::Medium,
        Severity::Low,
    ]
    .into_iter()
    .find(|severity| text.eq_ignore_ascii_case(severity.as_str()))
    .unwrap_or(Severity::Unknown)
}

/// The colour both CLIs give a severity label.
fn severity_color(severity: Severity) -> Color {
    match severity {
        Severity::Critical | Severity::High => Color::Red,
        Severity::Medium => Color::Yellow,
        Severity::Low => Color::Cyan,
        Severity::Unknown => Color::White,
    }
}

/// Trims `problem` and clips it at `TITLE_MAX_CHARS` code points, so a
/// multibyte character is never split. A clipped title loses its trailing
/// whitespace and ends in "…".
fn clip_title(problem: &str) -> String {
    let problem = problem.trim();
    if problem.is_empty() {
        return NO_DESCRIPTION.to_string();
    }
    match problem.char_indices().nth(TITLE_MAX_CHARS) {
        Some((cut, _)) => format!("{}…", problem[..cut].trim_end()),
        None => problem.to_string(),
    }
}

/// Formats one location as `file:line (function)`, dropping absent parts.
/// Returns `None` for an entry that is not an object or names neither a file
/// nor a function, so the caller moves on to the next location. Only a JSON
/// non-negative integer is a line number; a string such as "45", a negative
/// number or a float such as 12.0 is dropped, because the verification
/// schema asks for an integer and a guessed conversion could print a line the
/// model never named. The web page must apply the same rule when it lists a
/// finding's locations.
fn format_location(location: &Value) -> Option<String> {
    let text_of = |key: &str| {
        location
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    };
    let file = text_of("file");
    let function = text_of("function_or_symbol");
    let line = location.get("line").and_then(Value::as_u64);
    match (file, function) {
        (Some(file), function) => {
            let mut text = file.to_string();
            if let Some(line) = line {
                text.push_str(&format!(":{line}"));
            }
            if let Some(function) = function {
                text.push_str(&format!(" ({function})"));
            }
            Some(text)
        }
        (None, Some(function)) => Some(function.to_string()),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use termcolor::Buffer;

    fn written(line: &FindingLine) -> String {
        let mut buffer = Buffer::no_color();
        write_finding_line(&mut buffer, line).expect("a buffer accepts every write");
        String::from_utf8(buffer.into_inner()).expect("the line is UTF-8")
    }

    fn location_of(locations: Value) -> Option<String> {
        FindingLine::from_json(&json!({ "locations": locations })).location
    }

    #[test]
    fn a_short_problem_is_the_title_and_an_empty_one_is_no_description() {
        let line = FindingLine::from_json(&json!({"problem": "  mm: leak in f()  "}));
        assert_eq!(line.title, "mm: leak in f()");
        for finding in [json!({"problem": "   "}), json!({}), json!({"problem": 7})] {
            assert_eq!(FindingLine::from_json(&finding).title, NO_DESCRIPTION);
        }
    }

    #[test]
    fn a_long_problem_is_clipped_at_the_limit() {
        let line = FindingLine::from_json(&json!({"problem": "x".repeat(400)}));
        assert_eq!(line.title, format!("{}…", "x".repeat(TITLE_MAX_CHARS)));
        let exact = "y".repeat(TITLE_MAX_CHARS);
        assert_eq!(
            FindingLine::from_json(&json!({"problem": exact})).title,
            exact
        );
    }

    #[test]
    fn a_clip_never_splits_a_multibyte_character() {
        // Each "é" is two bytes, so a byte-based cut would land mid-character.
        let problem = "é".repeat(400);
        let title = FindingLine::from_json(&json!({"problem": problem})).title;
        assert_eq!(title, format!("{}…", "é".repeat(TITLE_MAX_CHARS)));
    }

    #[test]
    fn a_clip_drops_the_trailing_space_before_the_ellipsis() {
        let problem = format!("{} {}", "a".repeat(TITLE_MAX_CHARS - 1), "b".repeat(50));
        let title = FindingLine::from_json(&json!({"problem": problem})).title;
        assert_eq!(title, format!("{}…", "a".repeat(TITLE_MAX_CHARS - 1)));
    }

    #[test]
    fn severity_parses_in_any_case_and_falls_back_to_unknown() {
        for (severity, expected) in [
            (json!("critical"), Severity::Critical),
            (json!("High"), Severity::High),
            (json!("MEDIUM"), Severity::Medium),
            (json!("bogus"), Severity::Unknown),
            (json!(3), Severity::Unknown),
            // The counts and the exit code match the whole name, so a label
            // must not read these as High either.
            (json!("High (capped)"), Severity::Unknown),
            (json!("high "), Severity::Unknown),
            (json!("highest"), Severity::Unknown),
        ] {
            let line = FindingLine::from_json(&json!({"severity": severity}));
            assert_eq!(line.severity, expected, "{severity}");
        }
        assert_eq!(
            FindingLine::from_json(&json!({})).severity,
            Severity::Unknown
        );
    }

    #[test]
    fn speculative_keeps_not_marked_apart_from_false() {
        for (finding, expected) in [
            (json!({"speculative": true}), Some(true)),
            (json!({"speculative": false}), Some(false)),
            (json!({}), None),
            (json!({"speculative": "true"}), None),
        ] {
            assert_eq!(
                FindingLine::from_json(&finding).speculative,
                expected,
                "{finding}"
            );
        }
    }

    #[test]
    fn only_a_json_true_is_preexisting() {
        for (finding, expected) in [
            (json!({"preexisting": true}), true),
            (json!({"preexisting": false}), false),
            (json!({"preexisting": "true"}), false),
            (json!({}), false),
        ] {
            assert_eq!(
                FindingLine::from_json(&finding).preexisting,
                expected,
                "{finding}"
            );
        }
    }

    #[test]
    fn a_non_object_finding_reads_as_an_empty_one() {
        let empty = FindingLine {
            severity: Severity::Unknown,
            title: NO_DESCRIPTION.to_string(),
            speculative: None,
            preexisting: false,
            location: None,
        };
        for finding in [Value::Null, json!("junk"), json!(4), json!([1])] {
            assert_eq!(FindingLine::from_json(&finding), empty, "{finding}");
        }
    }

    #[test]
    fn none_marked_speculative_holds_until_one_finding_has_a_boolean() {
        assert!(none_marked_speculative(&[
            json!({}),
            json!({"speculative": "true"})
        ]));
        assert!(!none_marked_speculative(&[
            json!({}),
            json!({"speculative": false})
        ]));
        assert!(!none_marked_speculative(&[json!({"speculative": true})]));
    }

    #[test]
    fn a_location_drops_its_absent_parts() {
        let full = json!([{"file": "f.c", "line": 12, "function_or_symbol": "fn"}]);
        assert_eq!(location_of(full).as_deref(), Some("f.c:12 (fn)"));
        let no_line = json!([{"file": "f.c", "line": null, "function_or_symbol": "fn"}]);
        assert_eq!(location_of(no_line).as_deref(), Some("f.c (fn)"));
        let no_function = json!([{"file": "f.c", "line": 12}]);
        assert_eq!(location_of(no_function).as_deref(), Some("f.c:12"));
        let function_only = json!([{"file": "", "line": 12, "function_or_symbol": "fn"}]);
        assert_eq!(location_of(function_only).as_deref(), Some("fn"));
    }

    #[test]
    fn a_line_number_must_be_a_non_negative_integer() {
        for line in [json!("45"), json!(-1), json!(12.0)] {
            let locations = json!([{"file": "f.c", "line": line, "function_or_symbol": "fn"}]);
            assert_eq!(
                location_of(locations).as_deref(),
                Some("f.c (fn)"),
                "{line}"
            );
        }
    }

    #[test]
    fn a_location_that_names_nothing_gives_none() {
        assert_eq!(location_of(json!([])), None);
        assert_eq!(location_of(json!(["f.c:12"])), None);
        assert_eq!(location_of(json!([{"line": 12}])), None);
        assert_eq!(location_of(json!("f.c")), None);
        assert_eq!(FindingLine::from_json(&json!({})).location, None);
    }

    #[test]
    fn the_first_location_that_names_something_is_shown() {
        let locations = json!(["junk", {"file": "a.c"}, {"file": "b.c"}]);
        assert_eq!(location_of(locations).as_deref(), Some("a.c"));
    }

    #[test]
    fn a_line_carries_its_tags_and_its_location() {
        let speculative = FindingLine::from_json(&json!({
            "severity": "High",
            "problem": "title",
            "speculative": true,
            "locations": [{"file": "f.c", "line": 12, "function_or_symbol": "fn"}],
        }));
        assert_eq!(
            written(&speculative),
            "  [High] title (speculative)\n    f.c:12 (fn)\n"
        );
        let preexisting = FindingLine::from_json(&json!({
            "severity": "low",
            "problem": "title",
            "preexisting": true,
        }));
        assert_eq!(written(&preexisting), "  [Low] title (preexisting)\n");
    }

    #[test]
    fn both_tags_follow_the_web_page_order() {
        let line = FindingLine::from_json(&json!({
            "severity": "High",
            "problem": "title",
            "speculative": true,
            "preexisting": true,
        }));
        assert_eq!(
            written(&line),
            "  [High] title (speculative) (preexisting)\n"
        );
    }

    #[test]
    fn each_severity_keeps_its_colour() {
        for (severity, expected) in [
            (Severity::Critical, Color::Red),
            (Severity::High, Color::Red),
            (Severity::Medium, Color::Yellow),
            (Severity::Low, Color::Cyan),
            (Severity::Unknown, Color::White),
        ] {
            assert_eq!(severity_color(severity), expected, "{severity}");
        }
    }

    #[test]
    fn the_label_is_coloured_and_the_note_is_dim_on_a_terminal() {
        let mut buffer = Buffer::ansi();
        let line = FindingLine::from_json(&json!({"severity": "High", "problem": "title"}));
        write_finding_line(&mut buffer, &line).expect("a buffer accepts every write");
        write_not_marked_note(&mut buffer, &[json!({})]).expect("a buffer accepts every write");
        let text = String::from_utf8(buffer.into_inner()).expect("the output is UTF-8");
        // termcolor writes a foreground colour as ESC[38;5;Nm or ESC[3Nm and
        // dim as ESC[2m; red is colour 1 in both forms.
        let red_label = ["\x1b[31m[High]", "\x1b[38;5;1m[High]"];
        assert!(
            red_label.iter().any(|label| text.contains(label)),
            "{text:?}"
        );
        assert!(
            text.contains(&format!("\x1b[2m  {NOT_MARKED_NOTE}")),
            "{text:?}"
        );
    }

    #[test]
    fn a_line_not_marked_speculative_prints_no_tag() {
        for speculative in [json!(false), Value::Null] {
            let line = FindingLine::from_json(&json!({
                "severity": "Medium",
                "problem": "title",
                "speculative": speculative,
            }));
            assert_eq!(written(&line), "  [Medium] title\n", "{speculative}");
        }
    }

    #[test]
    fn the_not_marked_note_is_written_once_and_only_when_none_is_marked() {
        let note = |findings: &[Value]| {
            let mut buffer = Buffer::no_color();
            write_not_marked_note(&mut buffer, findings).expect("a buffer accepts every write");
            String::from_utf8(buffer.into_inner()).expect("the note is UTF-8")
        };
        assert_eq!(
            note(&[json!({}), json!({})]),
            format!("  {NOT_MARKED_NOTE}\n")
        );
        assert_eq!(note(&[json!({}), json!({"speculative": false})]), "");
        assert_eq!(note(&[]), "");
    }
}
