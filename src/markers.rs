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
//! remove-text: "<regex>"                remove every match of <regex> (it may span lines)
//! remove-blank-lines                    drop lines that are empty or only whitespace
//! unindent-common                       remove the indentation all non-blank lines share
//! reindent: <from> -> <to>              turn each <from> spaces of leading indentation into <to>
//! param: "<text>"                       (code side only) <text> is a placeholder: it matches anything
//!                                       without whitespace in the doc
//! comment: "<text>"                     a note for people; changes nothing
//! TODO: "<text>"                        a note of work left on this marker, listed by `asadoc check`
//! ```
//!
//! `remove-text`'s regex is written raw: a `\` is kept as it is, so `\s` means
//! whitespace, and only `\"` is needed for a quote.
//!
//! In `param` and `remove-lines-starting-with`, `*` stands for any text
//! without whitespace: `param: "<*>"` makes every `<NAME>` in the code a
//! placeholder, without the marker spelling any of them out. A placeholder's
//! value has no whitespace, except for placeholders from a `**` wildcard
//! (`param: "<**>"`), whose value is anything on the line.

use crate::matching::SpacedPlaceholders;
use crate::re::capture_group_text;
use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use serde::Serialize;
use std::collections::BTreeMap;
use std::iter::once;

pub(crate) const MARKER_PREFIX: &str = "@docs-as-code:";

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
        "remove-prefix"
        | "remove-suffix"
        | "strip-line-prefix"
        | "remove-lines-starting-with"
        | "remove-text"
        | "param"
        | "comment"
        | "TODO" => OptionKind::Text,
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
    /// A `comment` or `TODO`: for people, not for comparing
    pub(crate) fn is_note(&self) -> bool {
        matches!(self.key.as_str(), "comment" | "TODO")
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
            Some(OptionValue::Text(text)) if self.key == "remove-text" => format!(": {}", quote_raw(text)),
            Some(OptionValue::Text(text)) => format!(": {}", quote(text)),
        };
        format!("{side}{}{value}", self.key)
    }
}

fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Quotes a regex the way [`OptionCursor::raw_quoted_value`] reads it back:
/// backslashes as they are, a bare `"` escaped
fn quote_raw(regex: &str) -> String {
    let mut quoted = String::from("\"");
    let mut characters = regex.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => {
                quoted.push('\\');
                quoted.extend(characters.next());
            }
            '"' => quoted.push_str("\\\""),
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
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

    /// `"regex"`, kept as written: `\` and the character after it are both kept
    /// (so `\"` doesn't end it)
    fn raw_quoted_value(&mut self, key: &str) -> Result<String> {
        if !self.eat('"') {
            bail!("the value of \"{key}\" must be quoted");
        }
        let mut value = String::new();
        loop {
            match self.peek() {
                None => bail!("unterminated value for \"{key}\""),
                Some('"') => {
                    self.position += 1;
                    return Ok(value);
                }
                Some('\\') => {
                    value.push('\\');
                    self.position += 1;
                    value.push(
                        self.peek()
                            .with_context(|| format!("unterminated value for \"{key}\""))?,
                    );
                }
                Some(character) => value.push(character),
            }
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
    if side == OptionSide::Doc && matches!(key.as_str(), "param" | "comment" | "TODO") {
        bail!("\"{key}\" only applies to the repo side");
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
    let value = if key == "remove-text" {
        cursor.raw_quoted_value(key)?
    } else {
        cursor.quoted_value(key)?
    };
    if value.is_empty() {
        bail!("\"{key}\" needs a non-empty value");
    }
    if key == "remove-text" {
        compile_option_regex(key, &value)?;
    }
    cursor.skip_whitespace();
    if key == "param" && cursor.remaining().starts_with("->") {
        bail!("\"param\" takes only the placeholder, e.g. param: \"<NODES_MTU>\"");
    }
    Ok(OptionValue::Text(value))
}

fn compile_option_regex(key: &str, pattern: &str) -> Result<fancy_regex::Regex> {
    fancy_regex::Regex::new(pattern).with_context(|| format!("\"{key}\" has an invalid regex \"{pattern}\""))
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

/// A `TODO` on a marker
#[derive(Clone, Debug, Serialize)]
pub(crate) struct MarkerTodo {
    /// 1-based line number of the marker line it's on
    pub line: usize,
    /// The section whose start marker has it; None on a file marker
    pub section: Option<String>,
    pub text: String,
}

#[derive(Default, Debug)]
pub(crate) struct Markers {
    pub file: Option<MarkerHeader>,
    pub sections: BTreeMap<String, MarkedSection>,
    pub problems: Vec<String>,
    pub todos: Vec<MarkerTodo>,
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
    let todo_line = regex!(r"\|\s*TODO\s*:")?;
    markers.todos = markers
        .file
        .iter()
        .map(|header| (header, None))
        .chain(
            markers
                .sections
                .values()
                .map(|section| (&section.header, Some(section.name.as_str()))),
        )
        .flat_map(|(header, section)| header_todos(header, section, &lines, todo_line))
        .collect();
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

/// A marker's `TODO`s, each on the marker line it's written on
fn header_todos(header: &MarkerHeader, section: Option<&str>, lines: &[&str], todo_line: &Regex) -> Vec<MarkerTodo> {
    let todo_lines: Vec<usize> = (header.first_line..=header.last_line)
        .filter(|line_number| lines.get(line_number - 1).is_some_and(|line| todo_line.is_match(line)))
        .collect();
    header
        .options
        .iter()
        .filter(|option| option.key == "TODO")
        .enumerate()
        .map(|(todo_index, option)| MarkerTodo {
            line: todo_lines.get(todo_index).copied().unwrap_or(header.first_line),
            section: section.map(str::to_owned),
            text: option.text_value().to_owned(),
        })
        .collect()
}

/// `text` without the markers of its marked code: the file marker (`section`
/// None) or a section's start and end markers, option lines included
pub(crate) fn remove_markers(text: &str, section: Option<&str>) -> Result<String> {
    let markers = parse_markers(text)?;
    let marker_lines: Vec<usize> = match section {
        None => {
            let header = markers.file.as_ref().context("the file isn't marked whole")?;
            (header.first_line..=header.last_line).collect()
        }
        Some(name) => {
            let section = markers
                .sections
                .get(name)
                .with_context(|| format!("the file has no section \"{name}\""))?;
            (section.header.first_line..=section.header.last_line)
                .chain([section.end_line])
                .collect()
        }
    };
    Ok(text
        .split_inclusive('\n')
        .enumerate()
        .filter(|(line_index, _)| !marker_lines.contains(&(line_index + 1)))
        .map(|(_, line)| line)
        .collect())
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

/// The regex for a value with `*` wildcards; None when it has none. Each `*`
/// stands for the shortest text without whitespace that the text after it
/// allows (all of it, at the value's end)
fn wildcard_regex(value: &str) -> Result<Option<Regex>> {
    if !value.contains('*') {
        return Ok(None);
    }
    let pattern = value
        .replace("**", "*")
        .split('*')
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join(r"\S+?");
    let pattern = match pattern.strip_suffix(r"\S+?") {
        Some(before_last_wildcard) => format!(r"{before_last_wildcard}\S+"),
        None => pattern,
    };
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
        "remove-text" => remove_text(&lines, value),
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

/// Removes every match of `pattern`, which may span lines
fn remove_text(lines: &[String], pattern: &str) -> Result<Vec<String>> {
    let regex = compile_option_regex("remove-text", pattern)?;
    let text = lines.join("\n");
    if !regex.is_match(&text).context("matching \"remove-text\"")? {
        bail!("\"remove-text\" doesn't match anything: \"{pattern}\"");
    }
    let mut removed = String::new();
    let mut kept_from = 0;
    for found in regex.find_iter(&text) {
        let found = found.context("matching \"remove-text\"")?;
        removed.push_str(text.get(kept_from..found.start()).unwrap_or_default());
        kept_from = found.end();
    }
    removed.push_str(text.get(kept_from..).unwrap_or_default());
    Ok(removed.split('\n').map(str::to_owned).collect())
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
    options.iter().any(|option| option.key != "param" && !option.is_note())
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

/// The placeholders whose value may have whitespace: those a `**` wildcard
/// stands for
pub(crate) fn spaced_placeholders(options: &[MarkerOption], content: &str) -> Result<SpacedPlaceholders> {
    let mut spaced = SpacedPlaceholders::new();
    for option in options
        .iter()
        .filter(|option| option.key == "param" && option.text_value().contains("**"))
    {
        spaced.extend(param_names(option.text_value(), content)?);
    }
    Ok(spaced)
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
    fn removes_text_by_regex() -> Result<()> {
        let options = parse_options(r#"| remove-text: " +#.*" | remove-text: "\n\s*// drop\n""#)?;
        let content = apply_options("a: 1 # note\n  // drop\nb: \"#x\"\n", &options)?;
        assert_eq!(content, "a: 1b: \"#x\"\n");
        assert!(apply_options("a\n", &parse_options(r#"| remove-text: "zz""#)?).is_err());
        assert!(parse_options(r#"| remove-text: "(""#).is_err());
        let quoted = parse_options(r#"| remove-text: "\"\d+\"""#)?;
        assert_eq!(quoted[0].text_value(), r#"\"\d+\""#);
        assert_eq!(parse_options(&format!("| {}", quoted[0].marker_text()))?, quoted);
        Ok(())
    }

    #[test]
    fn double_wildcards_allow_spaced_values() -> Result<()> {
        let options = parse_options(r#"| param: "${**}" | param: "<*>""#)?;
        let content = "a: ${A} <B>\nb: ${C}\n";
        assert_eq!(placeholders(&options, content)?, ["${A}", "${C}", "<B>"]);
        let spaced = spaced_placeholders(&options, content)?;
        assert_eq!(spaced, SpacedPlaceholders::from(["${A}".to_owned(), "${C}".to_owned()]));
        Ok(())
    }

    #[test]
    fn notes_change_nothing_and_todos_are_listed() -> Result<()> {
        let text = "# @docs-as-code: start section \"s\" | comment: \"why | this\"\n#   | TODO: \"use a file\"\n#   | unindent-common | TODO: \"simplify the docs\"\n  a\n# @docs-as-code: end section \"s\"\n";
        let markers = parse_markers(text)?;
        assert!(markers.problems.is_empty(), "{:?}", markers.problems);
        let options = &markers.sections["s"].header.options;
        assert_eq!(options[0].text_value(), "why | this");
        assert_eq!(apply_options("  a\n", &options[..2])?, "  a\n");
        let todos: Vec<_> = markers
            .todos
            .iter()
            .map(|todo| (todo.line, todo.section.as_deref(), todo.text.as_str()))
            .collect();
        assert_eq!(
            todos,
            [(2, Some("s"), "use a file"), (3, Some("s"), "simplify the docs")]
        );
        assert!(parse_options(r#"| doc TODO: "x""#).is_err());
        Ok(())
    }

    #[test]
    fn removes_a_section_s_markers() -> Result<()> {
        let text =
            "a\n# @docs-as-code: start section \"s\"\n#   | TODO: \"x\"\nb\n# @docs-as-code: end section \"s\"\nc";
        assert_eq!(remove_markers(text, Some("s"))?, "a\nb\nc");
        assert!(remove_markers(text, Some("t")).is_err());
        assert!(remove_markers(text, None).is_err());
        assert_eq!(remove_markers("# @docs-as-code: file\nx\n", None)?, "x\n");
        Ok(())
    }

    #[test]
    fn wildcards_stand_for_any_text_without_whitespace() -> Result<()> {
        let options = parse_options(r#"| param: "<*>""#)?;
        let content = "name: <interface-name> <a>-<b>\n";
        assert_eq!(placeholders(&options, content)?, ["<interface-name>", "<a>", "<b>"]);
        let options = parse_options(r#"| remove-lines-starting-with: "--*""#)?;
        assert_eq!(apply_options("--set-x\nkeep\n", &options)?, "keep\n");
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
