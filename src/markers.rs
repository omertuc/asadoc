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

use crate::re::group;
use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use serde::Serialize;
use std::collections::BTreeMap;
use std::iter::once;

pub(crate) const MARKER: &str = "@docs-as-code:";

pub(crate) fn file_marker() -> String {
    format!("{MARKER} file")
}
pub(crate) fn section_start(name: &str) -> String {
    format!("{MARKER} start section \"{name}\"")
}
pub(crate) fn section_end(name: &str) -> String {
    format!("{MARKER} end section \"{name}\"")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Side {
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
    pub side: Side,
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
    pub(crate) fn text(key: &str, value: &str, side: Side) -> Self {
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
    pub(crate) fn format(&self) -> String {
        let side = if self.side == Side::Doc { "doc " } else { "" };
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
struct Cursor {
    chars: Vec<char>,
    at: usize,
}

impl Cursor {
    fn new(text: &str) -> Self {
        Self {
            chars: text.chars().collect(),
            at: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn eat(&mut self, expected: char) -> bool {
        let found = self.peek() == Some(expected);
        if found {
            self.at += 1;
        }
        found
    }

    fn eat_str(&mut self, expected: &str) -> bool {
        let found = self.rest().starts_with(expected);
        if found {
            self.at += expected.chars().count();
        }
        found
    }

    fn take_while(&mut self, predicate: impl Fn(char) -> bool) -> String {
        let taken: String = self
            .chars
            .iter()
            .skip(self.at)
            .copied()
            .take_while(|next| predicate(*next))
            .collect();
        self.at += taken.chars().count();
        taken
    }

    fn skip_ws(&mut self) {
        self.take_while(char::is_whitespace);
    }

    fn at_end(&self) -> bool {
        self.peek().is_none()
    }

    fn rest(&self) -> String {
        self.chars.get(self.at..).unwrap_or_default().iter().collect()
    }

    /// `"value"`, with `\` escaping the next character
    fn quoted(&mut self, key: &str) -> Result<String> {
        if !self.eat('"') {
            bail!("the value of \"{key}\" must be quoted");
        }
        let mut value = String::new();
        loop {
            let next = match self.peek() {
                None => bail!("unterminated value for \"{key}\""),
                Some('"') => {
                    self.at += 1;
                    return Ok(value);
                }
                Some('\\') => {
                    self.at += 1;
                    self.peek()
                        .with_context(|| format!("unterminated value for \"{key}\""))?
                }
                Some(next) => next,
            };
            value.push(next);
            self.at += 1;
        }
    }

    fn number(&mut self) -> Option<usize> {
        self.take_while(|next| next.is_ascii_digit()).parse().ok()
    }
}

/// `| key: "value" | flag ...`
fn parse_options(rest: &str) -> Result<Vec<MarkerOption>> {
    let mut cursor = Cursor::new(rest);
    let mut options = Vec::new();
    loop {
        cursor.skip_ws();
        if cursor.at_end() {
            return Ok(options);
        }
        options.push(parse_option(&mut cursor)?);
    }
}

/// `| [doc] key[: value]`
fn parse_option(cursor: &mut Cursor) -> Result<MarkerOption> {
    if !cursor.eat('|') {
        bail!("expected \"|\" before \"{}\"", cursor.rest());
    }
    cursor.skip_ws();
    let side = parse_side(cursor);
    let key = cursor.take_while(|next| next.is_ascii_alphanumeric() || next == '-' || next == '_');
    if key.is_empty() {
        bail!("expected an option name at \"{}\"", cursor.rest());
    }
    let kind = option_kind(&key).with_context(|| format!("unknown option \"{key}\""))?;
    if side == Side::Doc && key == "param" {
        bail!("\"param\" only applies to the repo side");
    }
    cursor.skip_ws();
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
fn parse_side(cursor: &mut Cursor) -> Side {
    let on_doc = cursor
        .rest()
        .strip_prefix("doc")
        .is_some_and(|after| after.starts_with(char::is_whitespace));
    if !on_doc {
        return Side::Repo;
    }
    cursor.eat_str("doc");
    cursor.skip_ws();
    Side::Doc
}

/// The `:` between an option's name and its value
fn expect_value(cursor: &mut Cursor, key: &str) -> Result<()> {
    if !cursor.eat(':') {
        bail!("\"{key}\" needs a value");
    }
    cursor.skip_ws();
    Ok(())
}

fn parse_text_value(cursor: &mut Cursor, key: &str) -> Result<OptionValue> {
    let value = cursor.quoted(key)?;
    if value.is_empty() {
        bail!("\"{key}\" needs a non-empty value");
    }
    cursor.skip_ws();
    if key == "param" && cursor.rest().starts_with("->") {
        bail!("\"param\" takes only the placeholder, e.g. param: \"<NODES_MTU>\"");
    }
    Ok(OptionValue::Text(value))
}

/// `<from> -> <to>`
fn parse_reindent(cursor: &mut Cursor, key: &str) -> Result<OptionValue> {
    let usage = || anyhow!("\"{key}\" takes \"<from> -> <to>\", e.g. \"{key}: 4 -> 2\"");
    let from = cursor.number().filter(|spaces| *spaces > 0).ok_or_else(usage)?;
    cursor.skip_ws();
    if !cursor.eat_str("->") {
        return Err(usage());
    }
    cursor.skip_ws();
    let to = cursor.number().ok_or_else(usage)?;
    Ok(OptionValue::Reindent { from, to })
}

/// A file marker or a section's start marker, with its continuation lines
#[derive(Clone, Debug)]
pub(crate) struct Header {
    /// Byte range of the marker lines (continuations included)
    pub marker_from: usize,
    pub marker_to: usize,
    /// 1-based line numbers of the first and last marker line
    pub start_line: usize,
    pub last_line: usize,
    pub options: Vec<MarkerOption>,
}

#[derive(Clone, Debug)]
pub(crate) struct Section {
    pub name: String,
    pub header: Header,
    /// Byte range of the marked lines
    pub from: usize,
    pub to: usize,
    pub end_line: usize,
}

#[derive(Default, Debug)]
pub(crate) struct Markers {
    pub file: Option<Header>,
    pub sections: BTreeMap<String, Section>,
    pub problems: Vec<String>,
}

const CONTINUATION: &str = r"^\s*#\s*(\|.*)$";
const START: &str = r#"^start section "([^"]+)"(.*)$"#;
const END: &str = r#"^end section "([^"]+)"$"#;
const FILE: &str = r"^file(\s.*|)$";

struct MarkerPatterns {
    continuation: &'static Regex,
    start: &'static Regex,
    end: &'static Regex,
    file: &'static Regex,
}

/// Sections whose start marker was seen but not yet their end
type OpenSections = BTreeMap<String, Header>;

/// Where a marker's lines (continuations included) are in the text
struct MarkerSpan {
    /// Byte range of the marker lines
    from: usize,
    to: usize,
    /// Byte offset of the last marker line
    last_line_start: usize,
    /// 1-based line numbers of the first and last marker line
    first_line: usize,
    last_line: usize,
}

impl MarkerSpan {
    const fn header(&self, options: Vec<MarkerOption>) -> Header {
        Header {
            marker_from: self.from,
            marker_to: self.to,
            start_line: self.first_line,
            last_line: self.last_line,
            options,
        }
    }
}

pub(crate) fn parse_markers(text: &str) -> Result<Markers> {
    let patterns = MarkerPatterns {
        continuation: regex!(CONTINUATION)?,
        start: regex!(START)?,
        end: regex!(END)?,
        file: regex!(FILE)?,
    };
    let lines: Vec<&str> = text.split('\n').collect();
    let line_starts = line_starts(&lines);
    let start_of = |index: usize| {
        line_starts
            .get(index)
            .copied()
            .with_context(|| format!("line {} is past the end of the text", index + 1))
    };

    let mut markers = Markers::default();
    let mut open = OpenSections::new();
    let mut index = 0;
    while let Some(line) = lines.get(index) {
        let Some((_, after)) = line.split_once(MARKER) else {
            index += 1;
            continue;
        };
        let (body, last) = marker_body(&lines, index, after, patterns.continuation)?;
        let last_line_start = start_of(last)?;
        let span = MarkerSpan {
            from: start_of(index)?,
            to: (last_line_start + lines.get(last).map_or(0, |last_line| last_line.len()) + 1).min(text.len()),
            last_line_start,
            first_line: index + 1,
            last_line: last + 1,
        };
        if let Err(error) = record_marker(&body, &span, &patterns, &mut markers, &mut open) {
            markers.problems.push(format!("line {}: {error}", span.first_line));
        }
        index = last + 1;
    }
    markers.problems.extend(
        open.into_iter()
            .map(|(name, header)| format!("line {}: section \"{name}\" has no end marker", header.start_line)),
    );
    if let Some(header) = &markers.file
        && !markers.sections.is_empty()
    {
        markers.problems.push(format!(
            "line {}: a file marked whole can't also have sections (its marked content would include their markers)",
            header.start_line
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

/// A marker's text after [`MARKER`], joined with its continuation lines (for
/// file and start markers), and the index of its last line
fn marker_body(lines: &[&str], first: usize, after: &str, continuation: &Regex) -> Result<(String, usize)> {
    let body = after.trim();
    if !(body.starts_with("file") || body.starts_with("start section")) {
        return Ok((body.to_owned(), first));
    }
    let continued = lines
        .iter()
        .skip(first + 1)
        .take_while(|next| !next.contains(MARKER))
        .map_while(|next| continuation.captures(next))
        .map(|captures| group(&captures, 1))
        .collect::<Result<Vec<_>>>()?;
    let last = first + continued.len();
    Ok((once(body).chain(continued).collect::<Vec<_>>().join(" "), last))
}

/// Records a marker in `markers` (or `open`, for a section's start); errors
/// with what's wrong with it
fn record_marker(
    body: &str,
    span: &MarkerSpan,
    patterns: &MarkerPatterns,
    markers: &mut Markers,
    open: &mut OpenSections,
) -> Result<()> {
    if let Some(captures) = patterns.file.captures(body) {
        markers.file = Some(span.header(parse_options(group(&captures, 1)?)?));
    } else if let Some(captures) = patterns.start.captures(body) {
        let name = group(&captures, 1)?.to_owned();
        let header = span.header(parse_options(group(&captures, 2)?)?);
        if open.contains_key(&name) || markers.sections.contains_key(&name) {
            bail!("section \"{name}\" is defined twice");
        }
        open.insert(name, header);
    } else if let Some(captures) = patterns.end.captures(body) {
        let name = group(&captures, 1)?.to_owned();
        let header = open
            .remove(&name)
            .with_context(|| format!("end of section \"{name}\" without a start"))?;
        markers.sections.insert(
            name.clone(),
            Section {
                name,
                from: header.marker_to,
                header,
                to: span.last_line_start,
                end_line: span.last_line,
            },
        );
    } else {
        bail!("unrecognized marker \"{body}\"");
    }
    Ok(())
}

/// What `*` stands for in a wildcard value
const NAME: &str = "[A-Za-z0-9_]+";

/// The regex for a value with `*` wildcards; None when it has none
fn wildcard(value: &str) -> Result<Option<Regex>> {
    if !value.contains('*') {
        return Ok(None);
    }
    let pattern = value.split('*').map(regex::escape).collect::<Vec<_>>().join(NAME);
    Regex::new(&pattern)
        .map(Some)
        .with_context(|| format!("compiling the pattern for \"{value}\""))
}

fn leading_ws(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Content with options applied; errors when an option doesn't fit it
pub(crate) fn apply_options(raw: &str, options: &[MarkerOption]) -> Result<String> {
    if options.is_empty() {
        return Ok(raw.to_owned());
    }
    let (body, trailing) = raw.strip_suffix('\n').map_or((raw, false), |body| (body, true));
    let lines = options
        .iter()
        .try_fold(body.split('\n').map(str::to_owned).collect(), apply_option)?;
    Ok(lines.join("\n") + if trailing { "\n" } else { "" })
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
    let first = lines.first_mut().context("the code has no first line")?;
    let (indent, rest) = first
        .split_at_checked(leading_ws(first))
        .context("the indentation ends inside a character")?;
    let Some(rest) = rest.strip_prefix(prefix) else {
        bail!("the first line doesn't start with \"{prefix}\"");
    };
    *first = format!("{indent}{rest}");
    Ok(lines)
}

fn remove_suffix(mut lines: Vec<String>, suffix: &str) -> Result<Vec<String>> {
    let last = lines.last_mut().context("the code has no last line")?;
    let Some(kept) = last.strip_suffix(suffix).map(str::len) else {
        bail!("the last line doesn't end with \"{suffix}\"");
    };
    last.truncate(kept);
    Ok(lines)
}

fn strip_line_prefix(lines: Vec<String>, prefix: &str) -> Vec<String> {
    lines
        .into_iter()
        .map(|line| match line.strip_prefix(prefix) {
            Some(rest) => rest.to_owned(),
            None => line,
        })
        .collect()
}

fn remove_lines_starting_with(lines: Vec<String>, start: &str) -> Result<Vec<String>> {
    let pattern = wildcard(start)?;
    Ok(lines
        .into_iter()
        .filter(|line| {
            let text = line.trim_start();
            match &pattern {
                Some(pattern) => pattern.find(text).is_none_or(|found| found.start() != 0),
                None => !text.starts_with(start),
            }
        })
        .collect())
}

fn unindent_common(lines: &[String]) -> Result<Vec<String>> {
    let common = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| leading_ws(line))
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|line| {
            line.get(common.min(leading_ws(line))..)
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
            let rest = line.trim_start_matches(' ');
            let indent = line.len() - rest.len();
            let (whole, part) = indent
                .checked_div(from)
                .zip(indent.checked_rem(from))
                .context("\"reindent\" can't convert from 0 spaces")?;
            Ok(format!("{}{rest}", " ".repeat(whole * to + part)))
        })
        .collect()
}

fn check_param_appears(lines: &[String], param: &str) -> Result<()> {
    let code = lines.join("\n");
    let found = match wildcard(param)? {
        Some(pattern) => pattern.is_match(&code),
        None => code.contains(param),
    };
    if !found {
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
    let declared = options
        .iter()
        .filter(|option| option.key == "param")
        .map(|option| param_names(option.text_value(), content))
        .collect::<Result<Vec<_>>>()?;
    Ok(declared.into_iter().flatten().fold(Vec::new(), |mut found, name| {
        if !found.contains(&name) {
            found.push(name);
        }
        found
    }))
}

/// The names a `param` value stands for in `content`
fn param_names(param: &str, content: &str) -> Result<Vec<String>> {
    Ok(match wildcard(param)? {
        Some(pattern) => pattern
            .find_iter(content)
            .map(|found| found.as_str().to_owned())
            .collect(),
        None => vec![param.to_owned()],
    })
}

/// Options as continuation lines under a marker indented by `indent`
pub(crate) fn continuation_lines(indent: &str, options: &[MarkerOption]) -> String {
    options
        .iter()
        .map(|option| format!("{indent}#   | {}\n", option.format()))
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
            (section.header.start_line, section.header.last_line, section.end_line),
            (2, 5, 8)
        );
        assert_eq!(section.header.options.len(), 5);
        assert_eq!(section.header.options[0].side, Side::Doc);
        let repo: Vec<_> = section
            .header
            .options
            .iter()
            .filter(|option| option.side == Side::Repo)
            .cloned()
            .collect();
        let raw = text.get(section.from..section.to).context("section outside the text")?;
        assert_eq!(apply_options(raw, &repo)?, "a \\\n  b <A>\n");
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
