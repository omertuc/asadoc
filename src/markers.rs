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
//! unindent-common                       remove the indentation all non-blank lines share
//! reindent: <from> -> <to>              turn each <from> spaces of leading indentation into <to>
//! param: "<text>"                       (code side only) <text> is a placeholder: it matches any value
//!                                       on its line in the doc, the same value everywhere it appears
//! ```

use anyhow::{Result, anyhow, bail};
use regex::Regex;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::LazyLock;

pub const MARKER: &str = "@docs-as-code:";

pub fn file_marker() -> String {
    format!("{MARKER} file")
}
pub fn section_start(name: &str) -> String {
    format!("{MARKER} start section \"{name}\"")
}
pub fn section_end(name: &str) -> String {
    format!("{MARKER} end section \"{name}\"")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Repo,
    Doc,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum OptionValue {
    Text(String),
    Reindent { from: usize, to: usize },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MarkerOption {
    pub key: String,
    pub value: Option<OptionValue>,
    pub side: Side,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Flag,
    Numbers,
}

fn option_kind(key: &str) -> Option<Kind> {
    Some(match key {
        "remove-prefix" | "remove-suffix" | "strip-line-prefix" | "remove-lines-starting-with" | "param" => Kind::Text,
        "unindent-common" => Kind::Flag,
        "reindent" => Kind::Numbers,
        _ => return None,
    })
}

impl MarkerOption {
    pub fn text(key: &str, value: &str, side: Side) -> MarkerOption {
        MarkerOption { key: key.to_string(), value: Some(OptionValue::Text(value.to_string())), side }
    }
    pub fn text_value(&self) -> &str {
        match &self.value {
            Some(OptionValue::Text(t)) => t,
            _ => "",
        }
    }
    /// As written on a marker
    pub fn format(&self) -> String {
        let side = if self.side == Side::Doc { "doc " } else { "" };
        let value = match &self.value {
            None => String::new(),
            Some(OptionValue::Reindent { from, to }) => format!(": {from} -> {to}"),
            Some(OptionValue::Text(t)) => format!(": {}", quote(t)),
        };
        format!("{side}{}{value}", self.key)
    }
}

pub fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `| key: "value" | flag ...`
fn parse_options(rest: &str) -> Result<Vec<MarkerOption>> {
    let chars: Vec<char> = rest.chars().collect();
    let mut i = 0;
    let mut options = Vec::new();
    let ws = |i: &mut usize| {
        while *i < chars.len() && chars[*i].is_whitespace() {
            *i += 1;
        }
    };
    let rest_at = |i: usize| chars[i..].iter().collect::<String>();
    let quoted = |i: &mut usize, key: &str| -> Result<String> {
        if chars.get(*i) != Some(&'"') {
            bail!("the value of \"{key}\" must be quoted");
        }
        *i += 1;
        let mut value = String::new();
        while *i < chars.len() && chars[*i] != '"' {
            if chars[*i] == '\\' && *i + 1 < chars.len() {
                *i += 1;
            }
            value.push(chars[*i]);
            *i += 1;
        }
        if chars.get(*i) != Some(&'"') {
            bail!("unterminated value for \"{key}\"");
        }
        *i += 1;
        Ok(value)
    };
    let number = |i: &mut usize| -> Option<usize> {
        let start = *i;
        while *i < chars.len() && chars[*i].is_ascii_digit() {
            *i += 1;
        }
        chars[start..*i].iter().collect::<String>().parse().ok()
    };
    loop {
        ws(&mut i);
        if i >= chars.len() {
            return Ok(options);
        }
        if chars[i] != '|' {
            bail!("expected \"|\" before \"{}\"", rest_at(i));
        }
        i += 1;
        ws(&mut i);
        let mut side = Side::Repo;
        if rest_at(i).starts_with("doc") && chars.get(i + 3).is_some_and(|c| c.is_whitespace()) {
            side = Side::Doc;
            i += 3;
            ws(&mut i);
        }
        let start = i;
        while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '-' || chars[i] == '_') {
            i += 1;
        }
        let key: String = chars[start..i].iter().collect();
        if key.is_empty() {
            bail!("expected an option name at \"{}\"", rest_at(start));
        }
        let kind = option_kind(&key).ok_or_else(|| anyhow!("unknown option \"{key}\""))?;
        if side == Side::Doc && key == "param" {
            bail!("\"param\" only applies to the repo side");
        }
        ws(&mut i);
        if kind == Kind::Flag {
            if chars.get(i) == Some(&':') {
                bail!("\"{key}\" takes no value");
            }
            options.push(MarkerOption { key, value: None, side });
            continue;
        }
        if chars.get(i) != Some(&':') {
            bail!("\"{key}\" needs a value");
        }
        i += 1;
        ws(&mut i);
        if kind == Kind::Text {
            let value = quoted(&mut i, &key)?;
            if value.is_empty() {
                bail!("\"{key}\" needs a non-empty value");
            }
            ws(&mut i);
            if key == "param" && rest_at(i).starts_with("->") {
                bail!("\"param\" takes only the placeholder, e.g. param: \"<NODES_MTU>\"");
            }
            options.push(MarkerOption { key, value: Some(OptionValue::Text(value)), side });
        } else {
            let bad = || anyhow!("\"{key}\" takes \"<from> -> <to>\", e.g. \"{key}: 4 -> 2\"");
            let from = number(&mut i).filter(|n| *n > 0).ok_or_else(bad)?;
            ws(&mut i);
            if !rest_at(i).starts_with("->") {
                return Err(bad());
            }
            i += 2;
            ws(&mut i);
            let to = number(&mut i).ok_or_else(bad)?;
            options.push(MarkerOption { key, value: Some(OptionValue::Reindent { from, to }), side });
        }
    }
}

/// A file marker or a section's start marker, with its continuation lines
#[derive(Clone, Debug)]
pub struct Header {
    /// Byte range of the marker lines (continuations included)
    pub marker_from: usize,
    pub marker_to: usize,
    /// 1-based line numbers of the first and last marker line
    pub start_line: usize,
    pub last_line: usize,
    pub options: Vec<MarkerOption>,
}

#[derive(Clone, Debug)]
pub struct Section {
    pub name: String,
    pub header: Header,
    /// Byte range of the marked lines
    pub from: usize,
    pub to: usize,
    pub end_line: usize,
}

#[derive(Default, Debug)]
pub struct Markers {
    pub file: Option<Header>,
    pub sections: BTreeMap<String, Section>,
    pub problems: Vec<String>,
}

static CONTINUATION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*#\s*(\|.*)$").unwrap());
static START: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^start section "([^"]+)"(.*)$"#).unwrap());
static END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^end section "([^"]+)"$"#).unwrap());
static FILE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^file(\s.*|)$").unwrap());

pub fn parse_markers(text: &str) -> Markers {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut starts = Vec::with_capacity(lines.len());
    let mut pos = 0;
    for l in &lines {
        starts.push(pos);
        pos += l.len() + 1;
    }
    let end_of = |i: usize| (starts[i] + lines[i].len() + 1).min(text.len());

    let mut markers = Markers::default();
    let mut open: BTreeMap<String, (Header, usize)> = BTreeMap::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(at) = lines[i].find(MARKER) else {
            i += 1;
            continue;
        };
        let first = i;
        let mut body = lines[i][at + MARKER.len()..].trim().to_string();
        if body.starts_with("file") || body.starts_with("start section") {
            while i + 1 < lines.len() && !lines[i + 1].contains(MARKER) {
                let Some(c) = CONTINUATION.captures(lines[i + 1]) else { break };
                body.push(' ');
                body.push_str(&c[1]);
                i += 1;
            }
        }
        let header = |options| Header {
            marker_from: starts[first],
            marker_to: end_of(i),
            start_line: first + 1,
            last_line: i + 1,
            options,
        };
        let result: Result<()> = (|| {
            if let Some(c) = FILE.captures(&body) {
                markers.file = Some(header(parse_options(&c[1])?));
            } else if let Some(c) = START.captures(&body) {
                let name = c[1].to_string();
                let h = header(parse_options(&c[2])?);
                if open.contains_key(&name) || markers.sections.contains_key(&name) {
                    bail!("section \"{name}\" is defined twice");
                }
                let from = h.marker_to;
                open.insert(name, (h, from));
            } else if let Some(c) = END.captures(&body) {
                let name = c[1].to_string();
                let (h, from) = open.remove(&name).ok_or_else(|| anyhow!("end of section \"{name}\" without a start"))?;
                markers.sections.insert(name.clone(), Section { name, header: h, from, to: starts[i], end_line: i + 1 });
            } else {
                bail!("unrecognized marker \"{body}\"");
            }
            Ok(())
        })();
        if let Err(e) = result {
            markers.problems.push(format!("line {}: {e}", first + 1));
        }
        i += 1;
    }
    for (name, (h, _)) in open {
        markers.problems.push(format!("line {}: section \"{name}\" has no end marker", h.start_line));
    }
    markers
}

fn leading_ws(l: &str) -> usize {
    l.len() - l.trim_start().len()
}

/// Content with options applied; errors when an option doesn't fit it
pub fn apply_options(raw: &str, options: &[MarkerOption]) -> Result<String> {
    if options.is_empty() {
        return Ok(raw.to_string());
    }
    let trailing = raw.ends_with('\n');
    let body = if trailing { &raw[..raw.len() - 1] } else { raw };
    let mut lines: Vec<String> = body.split('\n').map(str::to_string).collect();
    for opt in options {
        let v = opt.text_value();
        match opt.key.as_str() {
            "remove-prefix" => {
                let indent = leading_ws(&lines[0]);
                if !lines[0][indent..].starts_with(v) {
                    bail!("the first line doesn't start with \"{v}\"");
                }
                lines[0] = format!("{}{}", &lines[0][..indent], &lines[0][indent + v.len()..]);
            }
            "remove-suffix" => {
                let last = lines.last_mut().unwrap();
                if !last.ends_with(v) {
                    bail!("the last line doesn't end with \"{v}\"");
                }
                last.truncate(last.len() - v.len());
            }
            "strip-line-prefix" => {
                for l in &mut lines {
                    if let Some(rest) = l.strip_prefix(v) {
                        *l = rest.to_string();
                    }
                }
            }
            "remove-lines-starting-with" => lines.retain(|l| !l.trim_start().starts_with(v)),
            "unindent-common" => {
                let n = lines.iter().filter(|l| !l.trim().is_empty()).map(|l| leading_ws(l)).min().unwrap_or(0);
                for l in &mut lines {
                    let cut = n.min(leading_ws(l));
                    *l = l[cut..].to_string();
                }
            }
            "reindent" => {
                let Some(OptionValue::Reindent { from, to }) = opt.value else { continue };
                for l in &mut lines {
                    let n = l.len() - l.trim_start_matches(' ').len();
                    *l = format!("{}{}", " ".repeat(n / from * to + n % from), &l[n..]);
                }
            }
            "param" => {
                if !lines.join("\n").contains(v) {
                    bail!("param \"{v}\" doesn't appear in the code");
                }
            }
            _ => {}
        }
    }
    Ok(lines.join("\n") + if trailing { "\n" } else { "" })
}

/// Options that change the text (so whitespace fixes can't be worked out on the result)
pub fn has_shaping_options(options: &[MarkerOption]) -> bool {
    options.iter().any(|o| o.key != "param")
}

pub fn placeholders(options: &[MarkerOption]) -> Vec<String> {
    options.iter().filter(|o| o.key == "param").map(|o| o.text_value().to_string()).collect()
}

/// Options as continuation lines under a marker indented by `indent`
pub fn continuation_lines(indent: &str, options: &[MarkerOption]) -> String {
    options.iter().map(|o| format!("{indent}#   | {}\n", o.format())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiline_markers_and_options() {
        let text = "x\n    # @docs-as-code: start section \"s\"\n    #   | doc strip-line-prefix: \"$ \"\n    #   | remove-prefix: \"if \" | unindent-common\n    #   | reindent: 4 -> 2 | param: \"<A>\"\n    if a \\\n        b <A>\n    # @docs-as-code: end section \"s\"\n";
        let m = parse_markers(text);
        assert!(m.problems.is_empty(), "{:?}", m.problems);
        let s = &m.sections["s"];
        assert_eq!((s.header.start_line, s.header.last_line, s.end_line), (2, 5, 8));
        assert_eq!(s.header.options.len(), 5);
        assert_eq!(s.header.options[0].side, Side::Doc);
        let repo: Vec<_> = s.header.options.iter().filter(|o| o.side == Side::Repo).cloned().collect();
        assert_eq!(apply_options(&text[s.from..s.to], &repo).unwrap(), "a \\\n  b <A>\n");
    }

    #[test]
    fn reports_problems() {
        let m = parse_markers("# @docs-as-code: start section \"a\" | frob\n# @docs-as-code: end section \"b\"\n# @docs-as-code: start section \"c\" | param: \"x\" -> \"y\"\n");
        assert_eq!(m.problems.len(), 3, "{:?}", m.problems);
    }
}
