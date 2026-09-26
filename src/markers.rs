//! Markers in repo files, in `#` comments:
//!
//! ```text
//! # @docs-as-code: file [| <option>]...
//! # @docs-as-code: start section "<name>" [| <option>]...
//! # @docs-as-code: end section "<name>"
//! ```
//!
//! Options may also continue on the comment lines right after a file or start
//! marker that begin with `|`. They apply, in the order written, to the marked
//! code; prefixed with `doc`, to the doc blocks compared with it instead:
//!
//! ```text
//! remove-prefix: "<text>"               strip <text> from the start of the first line (after its indentation)
//! remove-suffix: "<text>"               strip <text> from the end of the last line
//! strip-line-prefix: "<text>"           strip <text> from the start of every line that has it
//! remove-lines-starting-with: "<text>"  drop lines whose text (after indentation) starts with <text>
//! remove-blank-lines                    drop lines that are empty or only whitespace
//! unindent-common                       remove the indentation all non-blank lines share
//! reindent: <from> -> <to>              turn each <from> spaces of leading indentation into <to>
//! param: "<text>"                       (code side only) <text> is a placeholder: it matches any value
//!                                       on its line in the doc, the same value everywhere it appears
//! ```
//!
//! In `param` and `remove-lines-starting-with`, `*` stands for any name
//! (letters, digits and `_`): `param: "<*>"` makes every `<NAME>` in the code a
//! placeholder, without the marker spelling any of them out.

use crate::re::capture_group_text;
use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use serde::Serialize;
use std::collections::BTreeMap;
use std::iter::once;

pub(crate) const MARKER_PREFIX: &str = "@docs-as-code:";

pub(crate) fn file_marker() -> String {
    format!("{MARKER_PREFIX} file")
}
pub(crate) fn section_start_marker(name: &str) -> String {
    format!("{MARKER_PREFIX} start section \"{name}\"")
}
pub(crate) fn section_end_marker(name: &str) -> String {
    format!("{MARKER_PREFIX} end section \"{name}\"")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum OptionSide {
    Repo,
    Doc,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub(crate) enum OptionValue {
    Text(String),
    Reindent { from: usize, to: usize },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct MarkerOption {
    pub key: String,
    pub value: Option<OptionValue>,
    pub side: OptionSide,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OptionKind {
    Text,
    Flag,
    Numbers,
}

fn option_kind(key: &str) -> Option<OptionKind> {
    Some(match key {
        "remove-prefix" | "remove-suffix" | "strip-line-prefix" | "remove-lines-starting-with" | "param" => {
            OptionKind::Text
        }
        "unindent-common" | "remove-blank-lines" => OptionKind::Flag,
        "reindent" => OptionKind::Numbers,
        _ => return None,
    })
}

impl MarkerOption {
    pub(crate) fn text(key: &str, value: &str, side: OptionSide) -> Self {
        Self {
            key: key.to_owned(),
            value: Some(OptionValue::Text(value.to_owned())),
            side,
        }
    }
    pub(crate) fn text_value(&self) -> &str {
        match &self.value {
            Some(OptionValue::Text(text)) => text,
            _ => "",
        }
    }
    /// As written on a marker
    pub(crate) fn marker_text(&self) -> String {
        let side = if self.side == OptionSide::Doc { "doc " } else { "" };
        let value = match &self.value {
            None => String::new(),
            Some(OptionValue::Reindent { from, to }) => format!(": {from} -> {to}"),
            Some(OptionValue::Text(text)) => format!(": {}", quote(text)),
        };
        format!("{side}{}{value}", self.key)
    }
}

fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Reads option text a character at a time
struct OptionCursor {
    chars: Vec<char>,
    position: usize,
}

impl OptionCursor {
    fn new(text: &str) -> Self {
        Self {
            chars: text.chars().collect(),
            position: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.position).copied()
    }

    fn eat(&mut self, expected: char) -> bool {
        let found = self.peek() == Some(expected);
        if found {
            self.position += 1;
        }
        found
    }

    fn eat_str(&mut self, expected: &str) -> bool {
        let found = self.remaining().starts_with(expected);
        if found {
            self.position += expected.chars().count();
        }
        found
    }

    fn take_while(&mut self, predicate: impl Fn(char) -> bool) -> String {
        let taken: String = self
            .chars
            .iter()
            .skip(self.position)
            .copied()
            .take_while(|character| predicate(*character))
            .collect();
        self.position += taken.chars().count();
        taken
    }

    fn skip_whitespace(&mut self) {
        self.take_while(char::is_whitespace);
    }

    fn at_end(&self) -> bool {
        self.peek().is_none()
    }

    fn remaining(&self) -> String {
        self.chars.get(self.position..).unwrap_or_default().iter().collect()
    }

    /// `"value"`, with `\` escaping the next character
    fn quoted_value(&mut self, key: &str) -> Result<String> {
        if !self.eat('"') {
            bail!("the value of \"{key}\" must be quoted");
        }
        let mut value = String::new();
        loop {
            let character = match self.peek() {
                None => bail!("unterminated value for \"{key}\""),
                Some('"') => {
                    self.position += 1;
                    return Ok(value);
                }
                Some('\\') => {
                    self.position += 1;
                    self.peek()
                        .with_context(|| format!("unterminated value for \"{key}\""))?
                }
                Some(character) => character,
            };
            value.push(character);
            self.position += 1;
        }
    }

    fn number(&mut self) -> Option<usize> {
        self.take_while(|character| character.is_ascii_digit()).parse().ok()
    }
}

/// `| key: "value" | flag ...`
fn parse_options(options_text: &str) -> Result<Vec<MarkerOption>> {
    let mut cursor = OptionCursor::new(options_text);
    let mut options = Vec::new();
    loop {
        cursor.skip_whitespace();
        if cursor.at_end() {
            return Ok(options);
        }
        options.push(parse_option(&mut cursor)?);
    }
}

/// `| [doc] key[: value]`
fn parse_option(cursor: &mut OptionCursor) -> Result<MarkerOption> {
    if !cursor.eat('|') {
        bail!("expected \"|\" before \"{}\"", cursor.remaining());
    }
    cursor.skip_whitespace();
    let side = parse_side(cursor);
    let key = cursor.take_while(|character| character.is_ascii_alphanumeric() || character == '-' || character == '_');
    if key.is_empty() {
        bail!("expected an option name at \"{}\"", cursor.remaining());
    }
    let kind = option_kind(&key).with_context(|| format!("unknown option \"{key}\""))?;
    if side == OptionSide::Doc && key == "param" {
        bail!("\"param\" only applies to the repo side");
    }
    cursor.skip_whitespace();
    let value = match kind {
        OptionKind::Flag => {
            if cursor.peek() == Some(':') {
                bail!("\"{key}\" takes no value");
            }
            None
        }
        OptionKind::Text => {
            expect_value(cursor, &key)?;
            Some(parse_text_value(cursor, &key)?)
        }
        OptionKind::Numbers => {
            expect_value(cursor, &key)?;
            Some(parse_reindent(cursor, &key)?)
        }
    };
    Ok(MarkerOption { key, value, side })
}

/// `doc ` before an option's name makes it apply to the doc side
fn parse_side(cursor: &mut OptionCursor) -> OptionSide {
    let on_doc = cursor
        .remaining()
        .strip_prefix("doc")
        .is_some_and(|after_doc| after_doc.starts_with(char::is_whitespace));
    if !on_doc {
        return OptionSide::Repo;
    }
    cursor.eat_str("doc");
    cursor.skip_whitespace();
    OptionSide::Doc
}

/// The `:` between an option's name and its value
fn expect_value(cursor: &mut OptionCursor, key: &str) -> Result<()> {
    if !cursor.eat(':') {
        bail!("\"{key}\" needs a value");
    }
    cursor.skip_whitespace();
    Ok(())
}

fn parse_text_value(cursor: &mut OptionCursor, key: &str) -> Result<OptionValue> {
    let value = cursor.quoted_value(key)?;
    if value.is_empty() {
        bail!("\"{key}\" needs a non-empty value");
    }
    cursor.skip_whitespace();
    if key == "param" && cursor.remaining().starts_with("->") {
        bail!("\"param\" takes only the placeholder, e.g. param: \"<NODES_MTU>\"");
    }
    Ok(OptionValue::Text(value))
}

/// `<from> -> <to>`
fn parse_reindent(cursor: &mut OptionCursor, key: &str) -> Result<OptionValue> {
    let usage_error = || anyhow!("\"{key}\" takes \"<from> -> <to>\", e.g. \"{key}: 4 -> 2\"");
    let from = cursor.number().filter(|spaces| *spaces > 0).ok_or_else(usage_error)?;
    cursor.skip_whitespace();
    if !cursor.eat_str("->") {
        return Err(usage_error());
    }
    cursor.skip_whitespace();
    let to = cursor.number().ok_or_else(usage_error)?;
    Ok(OptionValue::Reindent { from, to })
}

/// A file marker or a section's start marker, with its continuation lines
#[derive(Clone, Debug)]
pub(crate) struct MarkerHeader {
    /// Byte range of the marker lines (continuations included)
    pub marker_from: usize,
    pub marker_to: usize,
    /// 1-based line numbers of the first and last marker line
    pub first_line: usize,
    pub last_line: usize,
    pub options: Vec<MarkerOption>,
}

#[derive(Clone, Debug)]
pub(crate) struct MarkedSection {
    pub name: String,
    pub header: MarkerHeader,
    /// Byte range of the marked lines
    pub content_from: usize,
    pub content_to: usize,
    pub end_line: usize,
}

#[derive(Default, Debug)]
pub(crate) struct Markers {
    pub file: Option<MarkerHeader>,
    pub sections: BTreeMap<String, MarkedSection>,
    pub problems: Vec<String>,
}

const CONTINUATION_PATTERN: &str = r"^\s*#\s*(\|.*)$";
const START_PATTERN: &str = r#"^start section "([^"]+)"(.*)$"#;
const END_PATTERN: &str = r#"^end section "([^"]+)"$"#;
const FILE_PATTERN: &str = r"^file(\s.*|)$";

struct MarkerPatterns {
    continuation: &'static Regex,
    start: &'static Regex,
    end: &'static Regex,
    file: &'static Regex,
}

/// Sections whose start marker was seen but not yet their end
type OpenSections = BTreeMap<String, MarkerHeader>;

/// Where a marker's lines (continuations included) are in the text
struct MarkerSpan {
    /// Byte range of the marker lines
    marker_from: usize,
    marker_to: usize,
    /// Byte offset of the last marker line
    last_line_start: usize,
    /// 1-based line numbers of the first and last marker line
    first_line: usize,
    last_line: usize,
}

impl MarkerSpan {
    const fn header(&self, options: Vec<MarkerOption>) -> MarkerHeader {
        MarkerHeader {
            marker_from: self.marker_from,
            marker_to: self.marker_to,
            first_line: self.first_line,
            last_line: self.last_line,
            options,
        }
    }
}

pub(crate) fn parse_markers(text: &str) -> Result<Markers> {
    let patterns = MarkerPatterns {
        continuation: regex!(CONTINUATION_PATTERN)?,
        start: regex!(START_PATTERN)?,
        end: regex!(END_PATTERN)?,
        file: regex!(FILE_PATTERN)?,
    };
    let lines: Vec<&str> = text.split('\n').collect();
    let line_starts = line_starts(&lines);
    let line_start_offset = |line_index: usize| {
        line_starts
            .get(line_index)
            .copied()
            .with_context(|| format!("line {} is past the end of the text", line_index + 1))
    };

    let mut markers = Markers::default();
    let mut open_sections = OpenSections::new();
    let mut line_index = 0;
    while let Some(line) = lines.get(line_index) {
        let Some((_, after_marker)) = line.split_once(MARKER_PREFIX) else {
            line_index += 1;
            continue;
        };
        let (marker_text, last_line_index) = marker_body(&lines, line_index, after_marker, patterns.continuation)?;
        let last_line_start = line_start_offset(last_line_index)?;
        let span = MarkerSpan {
            marker_from: line_start_offset(line_index)?,
            marker_to: (last_line_start + lines.get(last_line_index).map_or(0, |last_line| last_line.len()) + 1)
                .min(text.len()),
            last_line_start,
            first_line: line_index + 1,
            last_line: last_line_index + 1,
        };
        if let Err(error) = record_marker(&marker_text, &span, &patterns, &mut markers, &mut open_sections) {
            markers.problems.push(format!("line {}: {error}", span.first_line));
        }
        line_index = last_line_index + 1;
    }
    markers.problems.extend(
        open_sections
            .into_iter()
            .map(|(name, header)| format!("line {}: section \"{name}\" has no end marker", header.first_line)),
    );
    if let Some(header) = &markers.file
        && !markers.sections.is_empty()
    {
        markers.problems.push(format!(
            "line {}: a file marked whole can't also have sections (its marked content would include their markers)",
            header.first_line
        ));
    }
    Ok(markers)
}

/// The byte offset each line starts at
fn line_starts(lines: &[&str]) -> Vec<usize> {
    lines
        .iter()
        .scan(0, |next_start, line| {
            let start = *next_start;
            *next_start += line.len() + 1;
            Some(start)
        })
        .collect()
}

/// A marker's text after [`MARKER_PREFIX`], joined with its continuation lines (for
/// file and start markers), and the index of its last line
fn marker_body(
    lines: &[&str],
    first_line_index: usize,
    after_marker: &str,
    continuation_pattern: &Regex,
) -> Result<(String, usize)> {
    let marker_text = after_marker.trim();
    if !(marker_text.starts_with("file") || marker_text.starts_with("start section")) {
        return Ok((marker_text.to_owned(), first_line_index));
    }
    let continuation_texts = lines
        .iter()
        .skip(first_line_index + 1)
        .take_while(|line| !line.contains(MARKER_PREFIX))
        .map_while(|line| continuation_pattern.captures(line))
        .map(|captures| capture_group_text(&captures, 1))
        .collect::<Result<Vec<_>>>()?;
    let last_line_index = first_line_index + continuation_texts.len();
    Ok((
        once(marker_text)
            .chain(continuation_texts)
            .collect::<Vec<_>>()
            .join(" "),
        last_line_index,
    ))
}

/// Records a marker in `markers` (or `open_sections`, for a section's start); errors
/// with what's wrong with it
fn record_marker(
    marker_text: &str,
    span: &MarkerSpan,
    patterns: &MarkerPatterns,
    markers: &mut Markers,
    open_sections: &mut OpenSections,
) -> Result<()> {
    if let Some(captures) = patterns.file.captures(marker_text) {
        markers.file = Some(span.header(parse_options(capture_group_text(&captures, 1)?)?));
    } else if let Some(captures) = patterns.start.captures(marker_text) {
        let name = capture_group_text(&captures, 1)?.to_owned();
        let header = span.header(parse_options(capture_group_text(&captures, 2)?)?);
        if open_sections.contains_key(&name) || markers.sections.contains_key(&name) {
            bail!("section \"{name}\" is defined twice");
        }
        open_sections.insert(name, header);
    } else if let Some(captures) = patterns.end.captures(marker_text) {
        let name = capture_group_text(&captures, 1)?.to_owned();
        let header = open_sections
            .remove(&name)
            .with_context(|| format!("end of section \"{name}\" without a start"))?;
        markers.sections.insert(
            name.clone(),
            MarkedSection {
                name,
                content_from: header.marker_to,
                header,
                content_to: span.last_line_start,
                end_line: span.last_line,
            },
        );
    } else {
        bail!("unrecognized marker \"{marker_text}\"");
    }
    Ok(())
}

/// What `*` stands for in a wildcard value
const WILDCARD_NAME_PATTERN: &str = "[A-Za-z0-9_]+";

/// The regex for a value with `*` wildcards; None when it has none
fn wildcard_regex(value: &str) -> Result<Option<Regex>> {
    if !value.contains('*') {
        return Ok(None);
    }
    let pattern = value
        .split('*')
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join(WILDCARD_NAME_PATTERN);
    Regex::new(&pattern)
        .map(Some)
        .with_context(|| format!("compiling the pattern for \"{value}\""))
}

fn indentation_width(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Content with options applied; errors when an option doesn't fit it
pub(crate) fn apply_options(raw_content: &str, options: &[MarkerOption]) -> Result<String> {
    if options.is_empty() {
        return Ok(raw_content.to_owned());
    }
    let (without_trailing_newline, has_trailing_newline) = raw_content
        .strip_suffix('\n')
        .map_or((raw_content, false), |without_trailing_newline| {
            (without_trailing_newline, true)
        });
    let lines = options.iter().try_fold(
        without_trailing_newline.split('\n').map(str::to_owned).collect(),
        apply_option,
    )?;
    Ok(lines.join("\n") + if has_trailing_newline { "\n" } else { "" })
}

fn apply_option(lines: Vec<String>, option: &MarkerOption) -> Result<Vec<String>> {
    let value = option.text_value();
    match option.key.as_str() {
        "remove-prefix" => remove_prefix(lines, value),
        "remove-suffix" => remove_suffix(lines, value),
        "strip-line-prefix" => Ok(strip_line_prefix(lines, value)),
        "remove-lines-starting-with" => remove_lines_starting_with(lines, value),
        "remove-blank-lines" => Ok(lines.into_iter().filter(|line| !line.trim().is_empty()).collect()),
        "unindent-common" => unindent_common(&lines),
        "reindent" => match option.value {
            Some(OptionValue::Reindent { from, to }) => reindent(&lines, from, to),
            _ => Ok(lines),
        },
        "param" => {
            check_param_appears(&lines, value)?;
            Ok(lines)
        }
        _ => Ok(lines),
    }
}

fn remove_prefix(mut lines: Vec<String>, prefix: &str) -> Result<Vec<String>> {
    let first_line = lines.first_mut().context("the code has no first line")?;
    let (indentation, unprefixed) = first_line
        .split_at_checked(indentation_width(first_line))
        .context("the indentation ends inside a character")?;
    let Some(unprefixed) = unprefixed.strip_prefix(prefix) else {
        bail!("the first line doesn't start with \"{prefix}\"");
    };
    *first_line = format!("{indentation}{unprefixed}");
    Ok(lines)
}

fn remove_suffix(mut lines: Vec<String>, suffix: &str) -> Result<Vec<String>> {
    let last_line = lines.last_mut().context("the code has no last line")?;
    let Some(kept_length) = last_line.strip_suffix(suffix).map(str::len) else {
        bail!("the last line doesn't end with \"{suffix}\"");
    };
    last_line.truncate(kept_length);
    Ok(lines)
}

fn strip_line_prefix(lines: Vec<String>, prefix: &str) -> Vec<String> {
    lines
        .into_iter()
        .map(|line| match line.strip_prefix(prefix) {
            Some(unprefixed) => unprefixed.to_owned(),
            None => line,
        })
        .collect()
}

fn remove_lines_starting_with(lines: Vec<String>, line_start: &str) -> Result<Vec<String>> {
    let wildcard_pattern = wildcard_regex(line_start)?;
    Ok(lines
        .into_iter()
        .filter(|line| {
            let unindented = line.trim_start();
            match &wildcard_pattern {
                Some(wildcard_pattern) => wildcard_pattern
                    .find(unindented)
                    .is_none_or(|wildcard_match| wildcard_match.start() != 0),
                None => !unindented.starts_with(line_start),
            }
        })
        .collect())
}

fn unindent_common(lines: &[String]) -> Result<Vec<String>> {
    let common_indentation = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| indentation_width(line))
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|line| {
            line.get(common_indentation.min(indentation_width(line))..)
                .map(str::to_owned)
                .with_context(|| {
                    format!("can't unindent {line:?}: lines are indented with different kinds of whitespace")
                })
        })
        .collect()
}

fn reindent(lines: &[String], from: usize, to: usize) -> Result<Vec<String>> {
    lines
        .iter()
        .map(|line| {
            let unindented = line.trim_start_matches(' ');
            let indentation = line.len() - unindented.len();
            let (indent_steps, leftover_spaces) = indentation
                .checked_div(from)
                .zip(indentation.checked_rem(from))
                .context("\"reindent\" can't convert from 0 spaces")?;
            Ok(format!(
                "{}{unindented}",
                " ".repeat(indent_steps * to + leftover_spaces)
            ))
        })
        .collect()
}

fn check_param_appears(lines: &[String], param: &str) -> Result<()> {
    let code = lines.join("\n");
    let appears = match wildcard_regex(param)? {
        Some(pattern) => pattern.is_match(&code),
        None => code.contains(param),
    };
    if !appears {
        bail!("param \"{param}\" doesn't appear in the code");
    }
    Ok(())
}

/// Options that change the text (so whitespace fixes can't be worked out on the result)
pub(crate) fn has_shaping_options(options: &[MarkerOption]) -> bool {
    options.iter().any(|option| option.key != "param")
}

/// The placeholders `param` options declare in `content`: each wildcard
/// `param` stands for every name matching it there
pub(crate) fn placeholders(options: &[MarkerOption], content: &str) -> Result<Vec<String>> {
    let names_per_param = options
        .iter()
        .filter(|option| option.key == "param")
        .map(|option| param_names(option.text_value(), content))
        .collect::<Result<Vec<_>>>()?;
    Ok(names_per_param
        .into_iter()
        .flatten()
        .fold(Vec::new(), |mut unique_names, name| {
            if !unique_names.contains(&name) {
                unique_names.push(name);
            }
            unique_names
        }))
}

/// The names a `param` value stands for in `content`
fn param_names(param: &str, content: &str) -> Result<Vec<String>> {
    Ok(match wildcard_regex(param)? {
        Some(pattern) => pattern
            .find_iter(content)
            .map(|name_match| name_match.as_str().to_owned())
            .collect(),
        None => vec![param.to_owned()],
    })
}

/// Options as continuation lines under a marker indented by `indent`
pub(crate) fn continuation_lines(indent: &str, options: &[MarkerOption]) -> String {
    options
        .iter()
        .map(|option| format!("{indent}#   | {}\n", option.marker_text()))
        .collect::<Vec<_>>()
        .concat()
}

#[cfg(test)]
#[allow(clippy::panic_in_result_fn, reason = "assertions are how tests fail")]
mod tests {
    use super::*;

    #[test]
    fn parses_multiline_markers_and_options() -> Result<()> {
        let text = "x\n    # @docs-as-code: start section \"s\"\n    #   | doc strip-line-prefix: \"$ \"\n    #   | remove-prefix: \"if \" | unindent-common\n    #   | reindent: 4 -> 2 | param: \"<A>\"\n    if a \\\n        b <A>\n    # @docs-as-code: end section \"s\"\n";
        let markers = parse_markers(text)?;
        assert!(markers.problems.is_empty(), "{:?}", markers.problems);
        let section = &markers.sections["s"];
        assert_eq!(
            (section.header.first_line, section.header.last_line, section.end_line),
            (2, 5, 8)
        );
        assert_eq!(section.header.options.len(), 5);
        assert_eq!(section.header.options[0].side, OptionSide::Doc);
        let repo_options: Vec<_> = section
            .header
            .options
            .iter()
            .filter(|option| option.side == OptionSide::Repo)
            .cloned()
            .collect();
        let raw_section = text
            .get(section.content_from..section.content_to)
            .context("section outside the text")?;
        assert_eq!(apply_options(raw_section, &repo_options)?, "a \\\n  b <A>\n");
        Ok(())
    }

    #[test]
    fn wildcards_stand_for_names() -> Result<()> {
        let options = parse_options(r#"| param: "<*>" | remove-lines-starting-with: "<*>" | remove-blank-lines"#)?;
        let content = apply_options("a: <X>\n\n  <POOL>\nb: <Y> <X>\n", &options)?;
        assert_eq!(content, "a: <X>\nb: <Y> <X>\n");
        assert_eq!(placeholders(&options, &content)?, ["<X>", "<Y>"]);
        assert!(apply_options("no names\n", &parse_options(r#"| param: "<*>""#)?).is_err());
        Ok(())
    }

    #[test]
    fn reports_problems() -> Result<()> {
        let markers = parse_markers(
            "# @docs-as-code: start section \"a\" | frob\n# @docs-as-code: end section \"b\"\n# @docs-as-code: start section \"c\" | param: \"x\" -> \"y\"\n",
        )?;
        assert_eq!(markers.problems.len(), 3, "{:?}", markers.problems);
        Ok(())
    }
}
