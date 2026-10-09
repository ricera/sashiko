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
//! finding and neither reads a key that no workflow emits. It also splits a
//! finding's `severity_explanation` into the rubric sections the web page
//! shows, so a summary of an earlier finding can say what it leads to.

use crate::db::Severity;
use crate::prior_revisions::{PriorLink, PriorRelation};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::io;
use std::sync::LazyLock;
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
    /// The finding of an earlier revision of the pull request that this one
    /// repeats or that the fix for it caused, read by the rule of
    /// `PriorLink::from_json`, which the web page's tag also follows.
    pub prior: Option<PriorLink>,
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
            prior: finding.get("prior").and_then(PriorLink::from_json),
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

/// Writes one finding as
/// `  [Severity] title (speculative) (preexisting) (repeat of v3)`, with its
/// location on an indented second line when it has one. The tags use the web
/// page's order and wording.
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
    if let Some(prior) = &line.prior {
        write!(out, " {}", prior_tag(prior))?;
    }
    writeln!(out)?;
    if let Some(location) = &line.location {
        writeln!(out, "    {location}")?;
    }
    Ok(())
}

/// The tag of a finding linked to an earlier revision's finding, worded as
/// the web page words it: "(repeat of v3)" or "(caused by the fix for v3)".
/// The label is made one line, because the link only trims it and a model
/// wrote it, so a newline or an escape sequence in it would otherwise reach
/// the terminal.
fn prior_tag(prior: &PriorLink) -> String {
    let relation = match prior.relation {
        PriorRelation::Repeat => "repeat of",
        PriorRelation::FixRegression => "caused by the fix for",
    };
    format!("({relation} {})", one_line(&prior.revision))
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
pub(crate) fn sanitize_text(text: &str) -> String {
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
pub(crate) fn clip_title(problem: &str) -> String {
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
pub(crate) fn clip_chars(text: String, max_chars: usize, marker: &str) -> String {
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

/// One location of a finding, as the one-line views and the summaries of
/// earlier findings read it.
///
/// Only a non-negative integer up to 2^53 - 1 is a line number, and an
/// integral float such as 12.0 counts as 12; a string such as "45", a
/// negative number, a fraction and a larger number are dropped, because the
/// verification schema asks for an integer and a guessed conversion could
/// print a line the model never named. The web page applies the same rule
/// when it lists a finding's locations. A line is kept only with the file it
/// is a line of. The file and function are sanitised like a title, since
/// both come from the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindingLocation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_or_symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}

impl FindingLocation {
    /// Reads one entry of a finding's `locations`. Returns `None` for an
    /// entry that is not an object or names neither a file nor a function,
    /// so the caller moves on to the next location.
    pub fn from_json(location: &Value) -> Option<Self> {
        let text_of = |key: &str| {
            location
                .get(key)
                .and_then(Value::as_str)
                .map(sanitize_text)
                .filter(|text| !text.is_empty())
        };
        let file = text_of("file");
        let function_or_symbol = text_of("function_or_symbol");
        if file.is_none() && function_or_symbol.is_none() {
            return None;
        }
        let line = file
            .as_ref()
            .and_then(|_| line_number(location.get("line")));
        Some(Self {
            file,
            function_or_symbol,
            line,
        })
    }
}

/// Formats a location as `file:line (function)`, dropping absent parts.
impl std::fmt::Display for FindingLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.file, &self.function_or_symbol) {
            (Some(file), function) => {
                write!(f, "{file}")?;
                if let Some(line) = self.line {
                    write!(f, ":{line}")?;
                }
                match function {
                    Some(function) => write!(f, " ({function})"),
                    None => Ok(()),
                }
            }
            (None, Some(function)) => write!(f, "{function}"),
            (None, None) => Ok(()),
        }
    }
}

/// Formats one location as `file:line (function)` (see [`FindingLocation`]),
/// or `None` for an entry that names neither a file nor a function.
fn format_location(location: &Value) -> Option<String> {
    FindingLocation::from_json(location).map(|location| location.to_string())
}

/// A step of the severity rubric that a severity explanation may label: the
/// kernel rubric's Consequence, Triggering path and Reachability, and the
/// Sashiko rubric's Blast radius and Likelihood headings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalibrationLabel {
    Consequence,
    TriggeringPath,
    Reachability,
    BlastRadius,
    Likelihood,
}

impl CalibrationLabel {
    /// Every spelling of a label, without its qualifier, with the step it
    /// names. The Sashiko headings have a short and a long spelling, and
    /// both name one step. The prompt tests read it to find the labels a
    /// rule names.
    pub(crate) const SPELLINGS: [(&'static str, Self); 7] = [
        ("Consequence", Self::Consequence),
        ("Triggering path", Self::TriggeringPath),
        ("Reachability", Self::Reachability),
        ("Blast radius", Self::BlastRadius),
        ("Blast radius and irreversibility", Self::BlastRadius),
        ("Likelihood", Self::Likelihood),
        ("Likelihood and preconditions", Self::Likelihood),
    ];

    /// Names the step a label's spelling stands for, without its qualifier.
    fn from_spelling(spelling: &str) -> Option<Self> {
        Self::SPELLINGS
            .iter()
            .find(|(written, _)| *written == spelling)
            .map(|&(_, label)| label)
    }
}

/// One labelled section of a severity explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrationSection {
    /// The rubric step the label names.
    pub label: CalibrationLabel,
    /// The label as the model wrote it, with its qualifier ("Triggering path
    /// (verified, not speculative)") and without bold markup, a list number
    /// or the colon. The web page shows this text.
    pub written: String,
    /// The trimmed text between this label and the next one.
    pub body: String,
}

/// A severity explanation split at its rubric labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeverityCalibration {
    /// The trimmed text before the first label; the whole text when no label
    /// is found.
    pub preamble: String,
    /// The labelled sections in the order they appear, without the sections
    /// whose body is empty.
    pub sections: Vec<CalibrationSection>,
}

/// The rubric label rule of `parseSeverityCalibration` in static/index.html,
/// written the same way so that the two can be compared line by line. A label
/// is case-sensitive, ends in a colon, may carry a qualifier before the colon
/// or be written in bold ("**Label**:" or "**Label:**"), and counts only at a
/// boundary: the start of the text, the start of a line, or after '.', ';' or
/// ')' and spaces, each optionally followed by a bullet marker or a list
/// number. Group 1 is the boundary, groups 2 and 4 the label with its
/// qualifier (bold and plain form), groups 3 and 5 the label's spelling.
///
/// The page writes `\d`, which is ASCII in JavaScript, so this pattern writes
/// `[0-9]`; Rust's `\d` would also match other scripts' digits. One difference
/// remains: JavaScript counts a qualifier's 1 to 80 and 0 to 47 characters in
/// UTF-16 code units and this pattern counts code points, so a qualifier
/// holding characters outside the Basic Multilingual Plane, such as emoji, can
/// fit here and not on the page. The model writes qualifiers in English, so
/// the difference is recorded rather than emulated.
static CALIBRATION_LABEL: LazyLock<Regex> = LazyLock::new(|| {
    let labels = "(?:Consequence|Triggering path|Reachability\
                  |Blast radius(?: and irreversibility)?|Likelihood(?: and preconditions)?)";
    let qualifier = r"(?:[ \t]*\([^()\n]{1,80}\)|,?[ \t]+[a-z][^:;.\n()]{0,47})?";
    let full = format!("(({labels}){qualifier})");
    let list_number = r"[0-9]+[.)][ \t]+";
    let marker = format!(r"(?:[-*][ \t]+|{list_number})");
    let boundary = format!(r"(^{marker}?|\n[ \t]*{marker}?|[.;)][ \t]+(?:{list_number})?)");
    let pattern = format!(r"{boundary}(?:\*\*{full}(?::\*\*|\*\*:)|{full}:)");
    // The pattern is assembled from the constant strings above and nothing
    // else, so it either always compiles or never does, and the splitter's
    // tests compile it: a pattern that does not compile fails them, so it
    // cannot pass the checks a change must pass before it lands.
    Regex::new(&pattern).expect("the calibration label pattern is a valid constant regex")
});

/// Trims what JavaScript's `String.prototype.trim` trims, so the preamble and
/// the bodies match the page's. That set is Rust's whitespace without U+0085
/// (NEXT LINE), plus U+FEFF (ZERO WIDTH NO-BREAK SPACE).
fn trim_like_page(text: &str) -> &str {
    text.trim_matches(|c: char| c == '\u{FEFF}' || (c.is_whitespace() && c != '\u{85}'))
}

/// Where one label opens its section within the trimmed explanation.
struct LabelMark<'a> {
    label: CalibrationLabel,
    written: &'a str,
    /// Where the previous text ends: the start of the boundary, or just after
    /// it when the boundary is a ')' that belongs to the previous text.
    start: usize,
    /// Where this label's body starts, just after its colon.
    body_start: usize,
}

/// Finds the first occurrence of each label spelling in `whole`. Repeats are
/// keyed by spelling, as on the page, so "Blast radius:" and "Blast radius
/// and irreversibility:" each open a section of their own.
fn label_marks(whole: &str) -> Vec<LabelMark<'_>> {
    let mut seen = HashSet::new();
    CALIBRATION_LABEL
        .captures_iter(whole)
        .filter_map(|caps| {
            let spelling = caps.get(3).or_else(|| caps.get(5))?.as_str();
            if !seen.insert(spelling) {
                return None;
            }
            let matched = caps.get(0)?;
            // A closing parenthesis ends the previous text, so it stays
            // there; the other boundary characters are only separators.
            let keep = usize::from(caps.get(1)?.as_str().starts_with(')'));
            Some(LabelMark {
                // The pattern's label alternation holds exactly the spellings
                // from_spelling names, so this lookup always succeeds.
                label: CalibrationLabel::from_spelling(spelling)?,
                written: caps.get(2).or_else(|| caps.get(4))?.as_str(),
                start: matched.start() + keep,
                body_start: matched.end(),
            })
        })
        .collect()
}

/// Splits a severity explanation at its rubric labels, by the rule the web
/// page's `parseSeverityCalibration` applies (see `CALIBRATION_LABEL`), and
/// gives `None` for empty or whitespace-only text. Only the first occurrence
/// of each label opens a section; a later one stays in the body it appears
/// in. Text with no label is all preamble, so nothing the model wrote is lost.
pub fn split_severity_explanation(text: &str) -> Option<SeverityCalibration> {
    let whole = trim_like_page(text);
    if whole.is_empty() {
        return None;
    }
    let marks = label_marks(whole);
    let preamble_end = marks.first().map_or(whole.len(), |mark| mark.start);
    let sections = marks
        .iter()
        .enumerate()
        .map(|(i, mark)| {
            let end = marks.get(i + 1).map_or(whole.len(), |next| next.start);
            CalibrationSection {
                label: mark.label,
                written: mark.written.to_string(),
                body: trim_like_page(&whole[mark.body_start..end]).to_string(),
            }
        })
        .filter(|section| !section.body.is_empty())
        .collect();
    Some(SeverityCalibration {
        preamble: trim_like_page(&whole[..preamble_end]).to_string(),
        sections,
    })
}

/// What a one-line summary says a finding leads to: the body of the
/// Consequence section, else of the Blast radius section (the Sashiko
/// rubric's nearest step), else the preamble. The text is made one line, with
/// each character `is_unsafe_for_terminal` names and each run of whitespace
/// becoming one space, because it comes from a model and is shown inside
/// another prompt's block, where a line break could start a line that reads
/// as part of the block's own frame. It is then clipped after at most `chars`
/// code points at the last word boundary and ended with "…". `None` when the
/// explanation is empty or the chosen text has nothing visible.
pub fn consequence_of(text: &str, chars: usize) -> Option<String> {
    let calibration = split_severity_explanation(text)?;
    let body_of = |label: CalibrationLabel| {
        calibration
            .sections
            .iter()
            .find(|section| section.label == label)
            .map(|section| section.body.as_str())
    };
    let said = body_of(CalibrationLabel::Consequence)
        .or_else(|| body_of(CalibrationLabel::BlastRadius))
        .unwrap_or(&calibration.preamble);
    let line = one_line(said);
    (!line.is_empty()).then(|| clip_on_word(line, chars))
}

/// `text` made one line: each character `is_unsafe_for_terminal` names and
/// each run of whitespace becomes one space, with none at either end. Empty
/// when nothing visible is left.
pub(crate) fn one_line(text: &str) -> String {
    text.split(|c: char| c.is_whitespace() || is_unsafe_for_terminal(c))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Cuts one-line `text` after at most `max_chars` code points, at the last
/// space within them so no word is split, and ends a cut text with "…". A
/// first word longer than the limit is cut inside it, since there is no
/// earlier boundary. Text within the limit is returned as it is.
fn clip_on_word(text: String, max_chars: usize) -> String {
    let Some((cut, next)) = text.char_indices().nth(max_chars) else {
        return text;
    };
    let kept = &text[..cut];
    let kept = match kept.rfind(' ') {
        Some(space) if next != ' ' => &kept[..space],
        _ => kept,
    };
    format!("{}…", kept.trim_end())
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

    /// The split as the UI verifier states it: the preamble, then each
    /// section as its written label and its body.
    fn split(text: &str) -> Option<(String, Vec<(String, String)>)> {
        split_severity_explanation(text).map(|calibration| {
            let sections = calibration
                .sections
                .into_iter()
                .map(|section| (section.written, section.body))
                .collect();
            (calibration.preamble, sections)
        })
    }

    /// One verifier case: the explanation, its preamble, and its sections as
    /// (written label, body).
    type PageCase = (
        &'static str,
        &'static str,
        &'static [(&'static str, &'static str)],
    );

    /// The verifier's `parseSeverityCalibration` cases
    /// (findingCalibrationLabelChecks, findingCalibrationShapeChecks and
    /// findingCalibrationProseChecks in scripts/verify_stage_progress_ui.mjs),
    /// copied as (explanation, preamble, [(label, body)]). The page's parser
    /// and the verifier both point here, so a change to the rule or to a
    /// case on one side is made on the other, and a change to the rule that
    /// alters any of these splits fails the table of the side it was not
    /// made on.
    const PAGE_CASES: &[PageCase] = &[
        // Labels.
        (
            "Consequence: a leak. Triggering path: a caller passes Y. Reachability: ioctl.",
            "",
            &[
                ("Consequence", "a leak"),
                ("Triggering path", "a caller passes Y"),
                ("Reachability", "ioctl."),
            ],
        ),
        (
            "Blast radius and irreversibility: one queue. Likelihood and preconditions: rare.",
            "",
            &[
                ("Blast radius and irreversibility", "one queue"),
                ("Likelihood and preconditions", "rare."),
            ],
        ),
        (
            "Blast radius: one queue.\nLikelihood: rare.",
            "",
            &[("Blast radius", "one queue."), ("Likelihood", "rare.")],
        ),
        (
            "- **Blast radius and irreversibility**: data loss.\n- **Likelihood and preconditions**: needs root.",
            "",
            &[
                ("Blast radius and irreversibility", "data loss."),
                ("Likelihood and preconditions", "needs root."),
            ],
        ),
        (
            "**Consequence:** a. **Triggering path:** b",
            "",
            &[("Consequence", "a"), ("Triggering path", "b")],
        ),
        (
            "**Consequence:** a.\n**Triggering path:** b",
            "",
            &[("Consequence", "a."), ("Triggering path", "b")],
        ),
        (
            "Consequence: a.\nTriggering path: b.",
            "",
            &[("Consequence", "a."), ("Triggering path", "b.")],
        ),
        (
            "Consequence: a.\n\nReachability: c",
            "",
            &[("Consequence", "a."), ("Reachability", "c")],
        ),
        (
            "Consequence: a.  Triggering path: b",
            "",
            &[("Consequence", "a"), ("Triggering path", "b")],
        ),
        (
            "It breaks. Consequence: a leak.",
            "It breaks",
            &[("Consequence", "a leak.")],
        ),
        (
            "Consequence: a.\nConsequence: b.",
            "",
            &[("Consequence", "a.\nConsequence: b.")],
        ),
        (
            "Consequence:\nTriggering path: b",
            "",
            &[("Triggering path", "b")],
        ),
        (
            "Blast radius: a. Blast radius and irreversibility: b.",
            "",
            &[
                ("Blast radius", "a"),
                ("Blast radius and irreversibility", "b."),
            ],
        ),
        // Shapes the deployments' explanations take.
        (
            "1. Consequence: a.\n2. Triggering path: b.\n3. Reachability: c.",
            "",
            &[
                ("Consequence", "a."),
                ("Triggering path", "b."),
                ("Reachability", "c."),
            ],
        ),
        (
            "x. 1. Consequence: a. 2. Triggering path: b.",
            "x",
            &[("Consequence", "a"), ("Triggering path", "b.")],
        ),
        (
            "Consequence: a.\nTriggering path (verified, not speculative): b",
            "",
            &[
                ("Consequence", "a."),
                ("Triggering path (verified, not speculative)", "b"),
            ],
        ),
        (
            "Consequence: a.\nTriggering path verified in code: b",
            "",
            &[
                ("Consequence", "a."),
                ("Triggering path verified in code", "b"),
            ],
        ),
        ("Consequence first: a", "", &[("Consequence first", "a")]),
        (
            "Consequence: it leaves it at 1. Triggering path: b",
            "",
            &[
                ("Consequence", "it leaves it at 1"),
                ("Triggering path", "b"),
            ],
        ),
        (
            "Consequence: crash (null deref) Triggering path: x",
            "",
            &[
                ("Consequence", "crash (null deref)"),
                ("Triggering path", "x"),
            ],
        ),
        (
            "It breaks (badly) Consequence: a",
            "It breaks (badly)",
            &[("Consequence", "a")],
        ),
        (
            "* Consequence: a.\n* Reachability: c.",
            "",
            &[("Consequence", "a."), ("Reachability", "c.")],
        ),
        // Prose.
        (
            "Consequence: nothing, because reachability is speculative here.",
            "",
            &[(
                "Consequence",
                "nothing, because reachability is speculative here.",
            )],
        ),
        (
            "Not lowered for reachability: x",
            "Not lowered for reachability: x",
            &[],
        ),
        (
            "Consequence: a. consequence: x",
            "",
            &[("Consequence", "a. consequence: x")],
        ),
        (
            "Consequence: a, the Reachability: x",
            "",
            &[("Consequence", "a, the Reachability: x")],
        ),
        ("**Consequence: x", "**Consequence: x", &[]),
        ("Consequence**: x", "Consequence**: x", &[]),
        (
            "Consequence: a.\nTriggering path Verified: b",
            "",
            &[("Consequence", "a.\nTriggering path Verified: b")],
        ),
        (
            "Consequence is bad. Reachability is fine.",
            "Consequence is bad. Reachability is fine.",
            &[],
        ),
        (
            "1. Condition Y is met.\n2. The buffer leaks.",
            "1. Condition Y is met.\n2. The buffer leaks.",
            &[],
        ),
    ];

    #[test]
    fn the_splitter_agrees_with_the_page_on_every_verifier_case() {
        for (text, preamble, sections) in PAGE_CASES {
            let expected = sections
                .iter()
                .map(|(label, body)| (label.to_string(), body.to_string()))
                .collect::<Vec<_>>();
            assert_eq!(
                split(text),
                Some((preamble.to_string(), expected)),
                "{text:?}"
            );
        }
    }

    #[test]
    fn empty_and_whitespace_explanations_split_into_nothing() {
        for text in ["", "   \n\t", "\u{FEFF} \u{2028}"] {
            assert_eq!(split_severity_explanation(text), None, "{text:?}");
        }
    }

    #[test]
    fn the_explanation_is_trimmed_the_way_the_page_trims_it() {
        // JavaScript's trim removes U+FEFF and keeps U+0085, the reverse of
        // Rust's str::trim.
        assert_eq!(
            split("\u{FEFF}Consequence: a\u{FEFF}"),
            Some((String::new(), vec![("Consequence".into(), "a".into())]))
        );
        assert_eq!(
            split("\u{85}x\u{85}"),
            Some(("\u{85}x\u{85}".into(), vec![]))
        );
    }

    #[test]
    fn each_spelling_names_its_rubric_step() {
        let text = "1. Consequence first: a. 2. Triggering path (verified): b.\n\
                    - **Reachability**: c.\nBlast radius and irreversibility: d.\n\
                    Likelihood: e.";
        let labels = split_severity_explanation(text)
            .expect("the text is not empty")
            .sections
            .iter()
            .map(|section| section.label)
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            [
                CalibrationLabel::Consequence,
                CalibrationLabel::TriggeringPath,
                CalibrationLabel::Reachability,
                CalibrationLabel::BlastRadius,
                CalibrationLabel::Likelihood,
            ]
        );
        assert_eq!(
            CalibrationLabel::from_spelling("Blast radius"),
            Some(CalibrationLabel::BlastRadius)
        );
        assert_eq!(
            CalibrationLabel::from_spelling("Likelihood and preconditions"),
            Some(CalibrationLabel::Likelihood)
        );
        assert_eq!(CalibrationLabel::from_spelling("consequence"), None);
    }

    /// The splitter's pattern and the spelling table list the same labels,
    /// so every spelling the table holds opens a section of its own step.
    #[test]
    fn the_pattern_opens_a_section_at_every_tabled_spelling() {
        for (spelling, label) in CalibrationLabel::SPELLINGS {
            let calibration = split_severity_explanation(&format!("{spelling}: body."))
                .expect("the text is not empty");
            let opened = calibration
                .sections
                .iter()
                .map(|section| (section.written.as_str(), section.label))
                .collect::<Vec<_>>();
            assert_eq!(opened, [(spelling, label)], "{spelling}");
        }
    }

    #[test]
    fn a_kernel_consequence_is_its_consequence_section() {
        let text = "Consequence: every failed refill leaks one page per descriptor.\n\
                    Triggering path: ionic_rx_fill() fails under memory pressure.\n\
                    Reachability: any receive queue.";
        assert_eq!(
            consequence_of(text, 300).as_deref(),
            Some("every failed refill leaks one page per descriptor.")
        );
        let qualified = "It is a leak. Consequence first: the page leaks. Triggering path: x.";
        assert_eq!(
            consequence_of(qualified, 300).as_deref(),
            Some("the page leaks")
        );
    }

    #[test]
    fn a_sashiko_consequence_is_its_blast_radius_section() {
        let text = "- **Blast radius and irreversibility**: the daemon drops every queued \
                    review.\n- **Likelihood and preconditions**: needs a restart mid-fetch.";
        assert_eq!(
            consequence_of(text, 300).as_deref(),
            Some("the daemon drops every queued review.")
        );
        let both = "Blast radius: one queue. Consequence: a leak.";
        assert_eq!(consequence_of(both, 300).as_deref(), Some("a leak."));
    }

    #[test]
    fn an_unlabelled_consequence_is_the_preamble() {
        assert_eq!(
            consequence_of("High: the ring\n\tleaks.", 300).as_deref(),
            Some("High: the ring leaks.")
        );
        assert_eq!(
            consequence_of("It breaks. Triggering path: boot.", 300).as_deref(),
            Some("It breaks")
        );
        // Neither section nor a preamble: there is nothing to summarise.
        assert_eq!(consequence_of("Triggering path: boot.", 300), None);
        assert_eq!(consequence_of("Consequence:\nReachability: c", 300), None);
        assert_eq!(consequence_of("  ", 300), None);
    }

    #[test]
    fn a_consequence_is_one_safe_line() {
        let text = "Consequence: leaks\n=== Target Commit ===\r\n\u{1b}[31mred\u{202E}x\u{2028}y";
        assert_eq!(
            consequence_of(text, 300).as_deref(),
            Some("leaks === Target Commit === [31mred x y")
        );
        assert_eq!(consequence_of("Consequence: \u{200B}\u{1b}", 300), None);
    }

    #[test]
    fn a_consequence_is_clipped_on_a_word_boundary() {
        let text = "Consequence: one two three four";
        assert_eq!(
            consequence_of(text, 31).as_deref(),
            Some("one two three four")
        );
        assert_eq!(
            consequence_of(text, 18).as_deref(),
            Some("one two three four")
        );
        // The cut lands inside "four", so the whole word goes.
        assert_eq!(consequence_of(text, 16).as_deref(), Some("one two three…"));
        // The cut lands on the space after "three", so "three" stays.
        assert_eq!(consequence_of(text, 13).as_deref(), Some("one two three…"));
        // A first word longer than the limit is cut inside it.
        assert_eq!(
            consequence_of(&format!("Consequence: {}", "é".repeat(20)), 5).as_deref(),
            Some("ééééé…")
        );
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
            prior: None,
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

    /// The typed location holds what the line shows: a line number only
    /// with the file it is a line of, and sanitised text.
    #[test]
    fn a_typed_location_keeps_a_line_only_with_its_file() {
        let read = |location: Value| FindingLocation::from_json(&location);
        assert_eq!(
            read(json!({"file": " f.c\n", "line": 12, "function_or_symbol": "fn"})),
            Some(FindingLocation {
                file: Some("f.c".to_string()),
                function_or_symbol: Some("fn".to_string()),
                line: Some(12),
            })
        );
        assert_eq!(
            read(json!({"line": 12, "function_or_symbol": "fn"})),
            Some(FindingLocation {
                file: None,
                function_or_symbol: Some("fn".to_string()),
                line: None,
            })
        );
        assert_eq!(read(json!({"line": 12})), None);
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

    /// A link to the earlier finding "rx page leak" listed under v3, with
    /// `relation` and the head of that revision.
    fn prior_link(relation: &str) -> Value {
        json!({
            "relation": relation,
            "revision": "v3",
            "earlier_problem": "rx page leak",
            "revision_head": "1a2b3c4d5e6f",
        })
    }

    #[test]
    fn a_line_reads_its_link_to_an_earlier_finding() {
        let line = FindingLine::from_json(&json!({"prior": prior_link("fix_regression")}));
        assert_eq!(
            line.prior,
            Some(PriorLink {
                relation: PriorRelation::FixRegression,
                revision: "v3".to_string(),
                earlier_problem: "rx page leak".to_string(),
                revision_head: Some("1a2b3c4d5e6f".to_string()),
            })
        );
        let headless = FindingLine::from_json(&json!({
            "prior": {"relation": "repeat", "revision": "v3", "earlier_problem": "leak",
                      "revision_head": "1a2b3c"},
        }));
        assert_eq!(
            headless
                .prior
                .map(|link| (link.relation, link.revision_head)),
            Some((PriorRelation::Repeat, None))
        );
    }

    #[test]
    fn a_malformed_link_reads_as_none() {
        for prior in [
            json!({"relation": "duplicate", "revision": "v3", "earlier_problem": "leak"}),
            json!({"relation": "Repeat", "revision": "v3", "earlier_problem": "leak"}),
            json!({"relation": "repeat", "revision": " \n ", "earlier_problem": "leak"}),
            json!({"relation": "repeat", "revision": "v".repeat(33), "earlier_problem": "leak"}),
            json!({"relation": "repeat", "revision": "v3", "earlier_problem": 7}),
            json!({"relation": "repeat", "revision": "v3", "headline": "leak"}),
            json!("repeat of v3"),
            json!(["repeat", "v3", "leak"]),
            Value::Null,
        ] {
            let line = FindingLine::from_json(
                &json!({"severity": "High", "problem": "t", "prior": prior}),
            );
            assert_eq!(line.prior, None, "{prior}");
            assert_eq!(written(&line), "  [High] t\n", "{prior}");
        }
    }

    #[test]
    fn a_linked_line_prints_its_tag_in_the_page_wording() {
        let tagged = |relation: &str| {
            written(&FindingLine::from_json(&json!({
                "severity": "Medium",
                "problem": "title",
                "prior": prior_link(relation),
            })))
        };
        assert_eq!(tagged("repeat"), "  [Medium] title (repeat of v3)\n");
        assert_eq!(
            tagged("fix_regression"),
            "  [Medium] title (caused by the fix for v3)\n"
        );
    }

    #[test]
    fn the_earlier_finding_tag_comes_after_the_others() {
        let line = FindingLine::from_json(&json!({
            "severity": "High",
            "problem": "title",
            "speculative": true,
            "preexisting": true,
            "prior": prior_link("repeat"),
            "locations": [{"file": "f.c", "line": 3}],
        }));
        assert_eq!(
            written(&line),
            "  [High] title (speculative) (preexisting) (repeat of v3)\n    f.c:3\n"
        );
    }

    #[test]
    fn an_earlier_revision_label_prints_as_one_safe_line() {
        let line = FindingLine::from_json(&json!({
            "severity": "Low",
            "problem": "title",
            "prior": {"relation": "repeat", "revision": "  v3\n (fixup)\u{1b}[31m\u{202E} ",
                      "earlier_problem": "leak"},
        }));
        assert_eq!(
            written(&line),
            "  [Low] title (repeat of v3 (fixup) [31m)\n"
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
