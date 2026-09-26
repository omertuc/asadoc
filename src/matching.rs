//! Comparing marked code with doc blocks: exactly, except where the code has
//! placeholders (declared with `param`), which match any value on their line,
//! the same value wherever the same placeholder appears.

use anyhow::{Context, Result};
use fancy_regex::Regex;
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fmt::Write;

/// The value each placeholder took
pub(crate) type Values = BTreeMap<String, String>;

struct Pattern {
    regex: Regex,
    /// (placeholder, capture group)
    groups: Vec<(String, String)>,
}

/// A regex finding any of the placeholders, longest first
fn placeholder_finder(placeholders: &[&String]) -> Result<Regex> {
    let mut by_length = placeholders.to_vec();
    by_length.sort_by_key(|placeholder| Reverse(placeholder.len()));
    let alternation = by_length
        .iter()
        .map(|placeholder| fancy_regex::escape(placeholder).into_owned())
        .collect::<Vec<_>>()
        .join("|");
    Regex::new(&format!("({alternation})")).context("compiling the placeholder regex")
}

impl Pattern {
    fn new(content: &str, placeholders: &[&String]) -> Result<Self> {
        let finder = placeholder_finder(placeholders)?;
        let mut groups: Vec<(String, String)> = Vec::new();
        let mut source = String::from(r"\A");
        let mut consumed = 0;
        for found in finder.find_iter(content) {
            let found = found.context("finding placeholders")?;
            let between = content
                .get(consumed..found.start())
                .context("placeholder match isn't on character boundaries")?;
            source.push_str(&fancy_regex::escape(between));
            let placeholder = found.as_str();
            if let Some((_, group)) = groups.iter().find(|(known, _)| known == placeholder) {
                write!(source, r"\k<{group}>").context("writing a placeholder backreference")?;
            } else {
                let group = format!("p{}", groups.len());
                write!(source, r"(?<{group}>[^\n]*?)").context("writing a placeholder group")?;
                groups.push((placeholder.to_owned(), group));
            }
            consumed = found.end();
        }
        let tail = content
            .get(consumed..)
            .context("placeholder match isn't on character boundaries")?;
        source.push_str(&fancy_regex::escape(tail));
        source.push_str(r"\z");
        Ok(Self {
            regex: Regex::new(&source).context("compiling the pattern for the code")?,
            groups,
        })
    }

    fn captures(&self, doc: &str) -> Result<Option<Values>> {
        let Some(captures) = self.regex.captures(doc).context("matching the code's pattern")? else {
            return Ok(None);
        };
        Ok(Some(
            self.groups
                .iter()
                .map(|(placeholder, group)| {
                    (
                        placeholder.clone(),
                        captures
                            .name(group)
                            .map(|capture| capture.as_str().to_owned())
                            .unwrap_or_default(),
                    )
                })
                .collect(),
        ))
    }

    /// The first of `doc_lines` from `start` on that this matches: its index
    /// and the values it gives
    fn first_match(&self, doc_lines: &[&str], start: usize) -> Result<Option<(usize, Values)>> {
        doc_lines
            .iter()
            .enumerate()
            .skip(start)
            .find_map(|(index, doc_line)| {
                self.captures(doc_line)
                    .map(|values| values.map(|values| (index, values)))
                    .transpose()
            })
            .transpose()
    }
}

/// Matches doc text against some code with placeholders. Compiled once, used
/// against every doc block.
pub(crate) struct Matcher {
    content: String,
    whole: Option<Pattern>,
    /// For each line of the content with placeholders: its pattern (for filling
    /// placeholders in for display)
    lines: Vec<Option<Pattern>>,
}

impl Matcher {
    pub(crate) fn new(content: &str, placeholders: &[String]) -> Result<Self> {
        let used: Vec<&String> = placeholders
            .iter()
            .filter(|placeholder| content.contains(placeholder.as_str()))
            .collect();
        let lines = content
            .split('\n')
            .map(|line| {
                let on_line: Vec<&String> = used
                    .iter()
                    .copied()
                    .filter(|placeholder| line.contains(placeholder.as_str()))
                    .collect();
                if on_line.is_empty() {
                    Ok(None)
                } else {
                    Pattern::new(line, &on_line)
                        .map(Some)
                        .with_context(|| format!("building the pattern for line {line:?}"))
                }
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            content: content.to_owned(),
            whole: if used.is_empty() {
                None
            } else {
                Some(Pattern::new(content, &used).context("building the pattern for the whole code")?)
            },
            lines,
        })
    }

    /// Whether the code matches `doc`, and if so the value each placeholder took
    pub(crate) fn matches(&self, doc: &str) -> Result<Option<Values>> {
        match &self.whole {
            None => Ok((self.content == doc).then(Values::new)),
            Some(pattern) => pattern.captures(doc),
        }
    }

    /// For showing a diff: the code with placeholders replaced by the doc's
    /// values. Code and doc are walked in order: each line with placeholders
    /// takes its values from the next doc line of the same shape, after the
    /// doc lines already matched.
    pub(crate) fn fill(&self, doc: &str) -> Result<String> {
        if self.whole.is_none() {
            return Ok(self.content.clone());
        }
        let doc_lines: Vec<&str> = doc.split('\n').collect();
        // The first doc line not matched yet
        let mut next = 0;
        let mut filled = Vec::new();
        for (line, pattern) in self.content.split('\n').zip(&self.lines) {
            let (filled_line, matched_at) = fill_line(line, pattern.as_ref(), &doc_lines, next)?;
            if let Some(index) = matched_at {
                next = index + 1;
            }
            filled.push(filled_line);
        }
        Ok(filled.join("\n"))
    }
}

/// One line of code for `Matcher::fill`, with the index of the doc line
/// (from `start` on) it matched, if any
fn fill_line(
    line: &str,
    pattern: Option<&Pattern>,
    doc_lines: &[&str],
    start: usize,
) -> Result<(String, Option<usize>)> {
    let Some(pattern) = pattern else {
        let matched_at = doc_lines
            .iter()
            .enumerate()
            .skip(start)
            .find(|(_, doc_line)| **doc_line == line)
            .map(|(index, _)| index);
        return Ok((line.to_owned(), matched_at));
    };
    Ok(match pattern.first_match(doc_lines, start)? {
        Some((index, values)) => {
            let filled = values.iter().fold(line.to_owned(), |filled, (placeholder, value)| {
                filled.replace(placeholder.as_str(), value)
            });
            (filled, Some(index))
        }
        None => (line.to_owned(), None),
    })
}

/// One-off matching (for content that isn't compiled ahead of time)
pub(crate) fn match_content(content: &str, placeholders: &[String], doc: &str) -> Result<Option<Values>> {
    if !placeholders
        .iter()
        .any(|placeholder| content.contains(placeholder.as_str()))
    {
        return Ok((content == doc).then(Values::new));
    }
    Matcher::new(content, placeholders)
        .context("compiling the content for matching")?
        .matches(doc)
}

#[cfg(test)]
#[allow(clippy::panic_in_result_fn, reason = "assertions are how tests fail")]
mod tests {
    use super::*;

    #[test]
    fn placeholders_match_consistently() -> Result<()> {
        let placeholders = vec!["<A>".to_owned()];
        let values = match_content("x: <A>\ny: <A>\n", &placeholders, "x: 1\ny: 1\n")?.context("no match")?;
        assert_eq!(values["<A>"], "1");
        assert!(match_content("x: <A>\ny: <A>\n", &placeholders, "x: 1\ny: 2\n")?.is_none());
        assert!(match_content("x: <A>\n", &placeholders, "x: 1\n2\n")?.is_none());
        assert_eq!(match_content("same\n", &[], "same\n")?, Some(Values::new()));
        Ok(())
    }

    #[test]
    fn fills_placeholders_for_display() -> Result<()> {
        let placeholders = vec!["${V}".to_owned()];
        assert_eq!(
            Matcher::new("a ${V}\nb", &placeholders)?.fill("c\na 4.22\n")?,
            "a 4.22\nb"
        );
        // In order: the second `end:` takes the second value
        let placeholders = vec!["<A>".to_owned(), "<B>".to_owned()];
        assert_eq!(
            Matcher::new("end: <A>\nx\nend: <B>\n", &placeholders)?.fill("end: 1\nx\nend: 45\n")?,
            "end: 1\nx\nend: 45\n"
        );
        Ok(())
    }
}
