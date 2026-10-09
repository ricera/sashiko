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

/// The largest integer a JSON number can name without rounding (2^53 - 1),
/// the same bound as `Number.MAX_SAFE_INTEGER` on the web page.
const MAX_SAFE_LINE_INT: u64 = 9_007_199_254_740_991;

/// Reports whether a character must not reach a terminal as it is: a control
/// character such as newline, tab or ESC, a line or paragraph separator, a
/// zero-width or directional mark (U+200B to U+200F), or a bidirectional
/// embedding, override or isolate (U+202A to U+202E and U+2066 to U+2069). The
/// last two groups can make a bidi-aware terminal draw text in another order
/// than it was written, so a file name or a title could read differently from
/// what it is.
pub(crate) fn is_unsafe_for_terminal(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{2028}' | '\u{2029}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
        )
}

/// Replaces every character for which `is_unsafe_for_terminal` holds with a
/// single space and trims the result. Finding text comes from the model and
/// is printed to a terminal, so such a character must neither split one
/// finding over several lines, nor reach the terminal as an escape sequence,
/// nor reorder the text around it.
fn sanitize_text(text: &str) -> String {
    text.chars()
        .map(|c| if is_unsafe_for_terminal(c) { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Reads a line number: a JSON non-negative integer, or an integral
/// non-negative number written as a float such as 12.0. A string, a negative
/// number, a fraction and anything above 2^53 - 1 give `None`, whether the
/// number was written as an integer or as a float.
fn line_number(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    if let Some(line) = value.as_u64() {
        // serde_json keeps an integer literal exact up to u64::MAX, but the web
        // page rounds anything above 2^53 - 1, so both apply the same bound.
        return Some(line).filter(|line| *line <= MAX_SAFE_LINE_INT);
    }
    value
        .as_f64()
        .filter(|line| line.fract() == 0.0 && (0.0..=MAX_SAFE_LINE_INT as f64).contains(line))
        // The filter proves the value is a non-negative integer below 2^53,
        // so the conversion is exact.
        .map(|line| line as u64)
}

/// Sanitises `problem` (see `sanitize_text`) and clips it at
/// `TITLE_MAX_CHARS` code points, so a multibyte character is never split.
/// A clipped title loses its trailing whitespace and ends in "…".
fn clip_title(problem: &str) -> String {
    let problem = sanitize_text(problem);
    if problem.is_empty() {
        return NO_DESCRIPTION.to_string();
    }
    clip_chars(problem, TITLE_MAX_CHARS, "…")
}

/// Cuts `text` after `max_chars` code points, so a multibyte character is
/// never split, and ends a cut text with `marker` after dropping the
/// whitespace the cut left at its end. Text within the limit is returned as
/// it is.
fn clip_chars(text: String, max_chars: usize, marker: &str) -> String {
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}{marker}", text[..cut].trim_end()),
        None => text,
    }
}

/// Reduces text from another program, such as a command's error output, to
/// one line that is safe to log and to show to an operator: the first line
/// that still has visible text once `sanitize_text` has replaced each
/// character `is_unsafe_for_terminal` names with a space, cut after
/// `max_chars` code points and then ended with "...".
///
/// Only the first line is kept because a second line in a log entry would
/// read as an entry of its own. A carriage return or a Unicode line separator
/// inside that line becomes a space like any other unsafe character, so it
/// can neither return the cursor over the text before it nor start a line.
/// Blank lines are skipped so that leading blank output does not hide the
/// message after it; text with no visible character gives "".
pub fn sanitize_one_line(text: &str, max_chars: usize) -> String {
    let line = text
        .split('\n')
        .map(sanitize_text)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    clip_chars(line, max_chars, "...")
}

/// Formats one location as `file:line (function)`, dropping absent parts.
/// Returns `None` for an entry that is not an object or names neither a file
/// nor a function, so the caller moves on to the next location. Only a
/// non-negative integer up to 2^53 - 1 is a line number, and an integral
/// float such as 12.0 counts as 12; a string such as "45", a negative number,
/// a fraction and a larger number are dropped, because the verification
/// schema asks for an integer and a guessed conversion could print a line the
/// model never named. The web page applies the same rule when it lists a
/// finding's locations. The file and function are sanitised like a title.
fn format_location(location: &Value) -> Option<String> {
    let text_of = |key: &str| {
        location
            .get(key)
            .and_then(Value::as_str)
            .map(sanitize_text)
            .filter(|text| !text.is_empty())
    };
    let file = text_of("file");
    let function = text_of("function_or_symbol");
    let line = line_number(location.get("line"));
    match (file, function) {
        (Some(mut text), function) => {
            if let Some(line) = line {
                text.push_str(&format!(":{line}"));
            }
            if let Some(function) = function {
                text.push_str(&format!(" ({function})"));
            }
            Some(text)
        }
        (None, Some(function)) => Some(function),
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
        for line in [
            json!("45"),
            json!(-1),
            json!(12.5),
            json!(1e22),
            json!(9_007_199_254_740_992.0),
        ] {
            let locations = json!([{"file": "f.c", "line": line, "function_or_symbol": "fn"}]);
            assert_eq!(
                location_of(locations).as_deref(),
                Some("f.c (fn)"),
                "{line}"
            );
        }
    }

    #[test]
    fn an_integer_literal_above_the_safe_bound_is_not_a_line_number() {
        // serde_json keeps these exact as u64, so they reach the integer path
        // that the float cases above never take.
        for text in [
            "9007199254740992",
            "9007199254740993",
            "18446744073709551615",
        ] {
            let line: Value = serde_json::from_str(text).expect("the text is a JSON number");
            assert!(line.as_u64().is_some(), "{text}");
            let locations = json!([{"file": "f.c", "line": line}]);
            assert_eq!(location_of(locations).as_deref(), Some("f.c"), "{text}");
        }
        let largest: Value = serde_json::from_str("9007199254740991").expect("a JSON number");
        let locations = json!([{"file": "f.c", "line": largest}]);
        assert_eq!(
            location_of(locations).as_deref(),
            Some("f.c:9007199254740991")
        );
    }

    #[test]
    fn bidi_and_format_characters_are_replaced_like_control_characters() {
        let locations =
            json!([{"file": "a\u{202E}c.", "function_or_symbol": "f\u{2066}g\u{200B}h"}]);
        assert_eq!(location_of(locations).as_deref(), Some("a c. (f g h)"));
        let line = FindingLine::from_json(&json!({"problem": "\u{202E}gnirts\u{2028}two"}));
        assert_eq!(line.title, "gnirts two");
    }

    #[test]
    fn an_integral_float_is_a_line_number() {
        for (line, expected) in [
            (json!(12.0), "f.c:12"),
            (json!(0.0), "f.c:0"),
            (json!(0), "f.c:0"),
            (json!(9_007_199_254_740_991.0), "f.c:9007199254740991"),
        ] {
            let locations = json!([{"file": "f.c", "line": line}]);
            assert_eq!(location_of(locations).as_deref(), Some(expected), "{line}");
        }
    }

    #[test]
    fn padded_and_blank_location_parts_are_trimmed_or_dropped() {
        let padded = json!([{"file": "  a.c  ", "function_or_symbol": "  "}]);
        assert_eq!(location_of(padded).as_deref(), Some("a.c"));
        let both = json!([{"file": "  a.c ", "line": 3, "function_or_symbol": " fn "}]);
        assert_eq!(location_of(both).as_deref(), Some("a.c:3 (fn)"));
        let blank_file = json!([{"file": " \t ", "function_or_symbol": " fn "}]);
        assert_eq!(location_of(blank_file).as_deref(), Some("fn"));
    }

    #[test]
    fn a_title_with_a_newline_prints_as_one_line() {
        let line = FindingLine::from_json(&json!({
            "severity": "High",
            "problem": "first\nsecond\tthird\r\n",
            "speculative": true,
        }));
        assert_eq!(line.title, "first second third");
        assert_eq!(
            written(&line),
            "  [High] first second third (speculative)\n"
        );
    }

    #[test]
    fn an_escape_sequence_in_a_location_never_reaches_the_terminal() {
        let locations = json!([{"file": "\u{1b}[2Ja.c", "function_or_symbol": "f\u{1b}[0m"}]);
        let location = location_of(locations).expect("the location names a file");
        assert!(!location.contains('\u{1b}'), "{location:?}");
        assert_eq!(location, "[2Ja.c (f [0m)");
        let line = FindingLine::from_json(&json!({
            "problem": "\u{1b}[31mred",
            "locations": [{"file": "\u{1b}[2Ja.c"}],
        }));
        assert!(!written(&line).contains('\u{1b}'));
    }

    #[test]
    fn the_clip_counts_code_points_after_sanitising() {
        // Leading control characters are trimmed before the count, so the
        // title keeps the full limit of visible characters.
        let problem = format!("\n\u{1b}{}", "é".repeat(400));
        let title = FindingLine::from_json(&json!({"problem": problem})).title;
        assert_eq!(title, format!("{}…", "é".repeat(TITLE_MAX_CHARS)));
        // An interior control character is one space and counts as one.
        let problem = format!("{}\n{}", "a".repeat(TITLE_MAX_CHARS - 2), "b".repeat(50));
        let title = FindingLine::from_json(&json!({"problem": problem})).title;
        assert_eq!(title, format!("{} b…", "a".repeat(TITLE_MAX_CHARS - 2)));
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
    fn one_line_is_the_first_visible_line_made_safe() {
        assert_eq!(
            sanitize_one_line("error: not found\nhint: try again\n", 200),
            "error: not found"
        );
        assert_eq!(sanitize_one_line("\n \t\n\u{200B}\nreal\n", 200), "real");
        // ESC, a bidi override, a lone carriage return and a line separator
        // inside the line each become one space; a newline ends the line.
        assert_eq!(
            sanitize_one_line("\u{1b}[31mbad\u{202E}x\ry\u{2028}z\r\nforged entry", 200),
            "[31mbad x y z"
        );
        assert_eq!(sanitize_one_line("", 200), "");
        assert_eq!(sanitize_one_line("\n\u{1b}\n", 200), "");
    }

    #[test]
    fn one_line_is_cut_on_code_points_and_marked() {
        assert_eq!(
            sanitize_one_line(&"é".repeat(1000), 200),
            format!("{}...", "é".repeat(200))
        );
        let exact = "x".repeat(200);
        assert_eq!(sanitize_one_line(&exact, 200), exact);
        let spaced = format!("{} {}", "a".repeat(199), "b".repeat(10));
        assert_eq!(
            sanitize_one_line(&spaced, 200),
            format!("{}...", "a".repeat(199))
        );
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
