//! Comparing marked code with doc blocks: exactly, except where the code has
//! placeholders (declared with `param`), which match any value on their line,
//! the same value wherever the same placeholder appears.

use anyhow::{Context, Result};
use fancy_regex::Regex;
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fmt::Write;

/// The value each placeholder took
pub(crate) type PlaceholderValues = BTreeMap<String, String>;

struct PlaceholderPattern {
    regex: Regex,
    /// (placeholder, capture group)
    placeholder_groups: Vec<(String, String)>,
}

/// A regex finding any of the placeholders, longest first
fn placeholder_finder(placeholders: &[&String]) -> Result<Regex> {
    let mut longest_first = placeholders.to_vec();
    longest_first.sort_by_key(|placeholder| Reverse(placeholder.len()));
    let alternation = longest_first
        .iter()
        .map(|placeholder| fancy_regex::escape(placeholder).into_owned())
        .collect::<Vec<_>>()
        .join("|");
    Regex::new(&format!("({alternation})")).context("compiling the placeholder regex")
}

impl PlaceholderPattern {
    fn new(content: &str, placeholders: &[&String]) -> Result<Self> {
        let finder = placeholder_finder(placeholders)?;
        let mut placeholder_groups: Vec<(String, String)> = Vec::new();
        let mut regex_source = String::from(r"\A");
        let mut consumed_until = 0;
        for placeholder_match in finder.find_iter(content) {
            let placeholder_match = placeholder_match.context("finding placeholders")?;
            let text_before_placeholder = content
                .get(consumed_until..placeholder_match.start())
                .context("placeholder match isn't on character boundaries")?;
            regex_source.push_str(&fancy_regex::escape(text_before_placeholder));
            let placeholder = placeholder_match.as_str();
            if let Some((_, group_name)) = placeholder_groups
                .iter()
                .find(|(known_placeholder, _)| known_placeholder == placeholder)
            {
                write!(regex_source, r"\k<{group_name}>").context("writing a placeholder backreference")?;
            } else {
                let group_name = format!("p{}", placeholder_groups.len());
                write!(regex_source, r"(?<{group_name}>[^\n]*?)").context("writing a placeholder group")?;
                placeholder_groups.push((placeholder.to_owned(), group_name));
            }
            consumed_until = placeholder_match.end();
        }
        let text_after_placeholders = content
            .get(consumed_until..)
            .context("placeholder match isn't on character boundaries")?;
        regex_source.push_str(&fancy_regex::escape(text_after_placeholders));
        regex_source.push_str(r"\z");
        Ok(Self {
            regex: Regex::new(&regex_source).context("compiling the pattern for the code")?,
            placeholder_groups,
        })
    }

    fn captures(&self, doc: &str) -> Result<Option<PlaceholderValues>> {
        let Some(captures) = self.regex.captures(doc).context("matching the code's pattern")? else {
            return Ok(None);
        };
        Ok(Some(
            self.placeholder_groups
                .iter()
                .map(|(placeholder, group_name)| {
                    (
                        placeholder.clone(),
                        captures
                            .name(group_name)
                            .map(|capture| capture.as_str().to_owned())
                            .unwrap_or_default(),
                    )
                })
                .collect(),
        ))
    }

    /// The first of `doc_lines` from `start` on that this matches: its index
    /// and the values it gives
    fn first_match(&self, doc_lines: &[&str], start: usize) -> Result<Option<(usize, PlaceholderValues)>> {
        doc_lines
            .iter()
            .enumerate()
            .skip(start)
            .find_map(|(doc_line_index, doc_line)| {
                self.captures(doc_line)
                    .map(|captured| captured.map(|values| (doc_line_index, values)))
                    .transpose()
            })
            .transpose()
    }
}

/// Matches doc text against some code with placeholders. Compiled once, used
/// against every doc block.
pub(crate) struct Matcher {
    content: String,
    whole_pattern: Option<PlaceholderPattern>,
    /// For each line of the content with placeholders: its pattern (for filling
    /// placeholders in for display)
    line_patterns: Vec<Option<PlaceholderPattern>>,
}

impl Matcher {
    pub(crate) fn new(content: &str, placeholders: &[String]) -> Result<Self> {
        let used_placeholders: Vec<&String> = placeholders
            .iter()
            .filter(|placeholder| content.contains(placeholder.as_str()))
            .collect();
        let line_patterns = content
            .split('\n')
            .map(|line| {
                let placeholders_on_line: Vec<&String> = used_placeholders
                    .iter()
                    .copied()
                    .filter(|placeholder| line.contains(placeholder.as_str()))
                    .collect();
                if placeholders_on_line.is_empty() {
                    Ok(None)
                } else {
                    PlaceholderPattern::new(line, &placeholders_on_line)
                        .map(Some)
                        .with_context(|| format!("building the pattern for line {line:?}"))
                }
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            content: content.to_owned(),
            whole_pattern: if used_placeholders.is_empty() {
                None
            } else {
                Some(
                    PlaceholderPattern::new(content, &used_placeholders)
                        .context("building the pattern for the whole code")?,
                )
            },
            line_patterns,
        })
    }

    /// Whether the code matches `doc`, and if so the value each placeholder took
    pub(crate) fn matches(&self, doc: &str) -> Result<Option<PlaceholderValues>> {
        match &self.whole_pattern {
            None => Ok((self.content == doc).then(PlaceholderValues::new)),
            Some(whole_pattern) => whole_pattern.captures(doc),
        }
    }

    /// For showing a diff: the code with placeholders replaced by the doc's
    /// values. Code and doc are walked in order: each line with placeholders
    /// takes its values from the next doc line of the same shape, after the
    /// doc lines already matched.
    pub(crate) fn fill(&self, doc: &str) -> Result<String> {
        if self.whole_pattern.is_none() {
            return Ok(self.content.clone());
        }
        let doc_lines: Vec<&str> = doc.split('\n').collect();
        let mut first_unmatched_doc_line = 0;
        let mut filled_lines = Vec::new();
        for (line, line_pattern) in self.content.split('\n').zip(&self.line_patterns) {
            let (filled_line, matched_at) =
                fill_line(line, line_pattern.as_ref(), &doc_lines, first_unmatched_doc_line)?;
            if let Some(matched_doc_line) = matched_at {
                first_unmatched_doc_line = matched_doc_line + 1;
            }
            filled_lines.push(filled_line);
        }
        Ok(filled_lines.join("\n"))
    }
}

/// One line of code for `Matcher::fill`, with the index of the doc line
/// (from `start` on) it matched, if any
fn fill_line(
    line: &str,
    line_pattern: Option<&PlaceholderPattern>,
    doc_lines: &[&str],
    start: usize,
) -> Result<(String, Option<usize>)> {
    let Some(line_pattern) = line_pattern else {
        let matched_at = doc_lines
            .iter()
            .enumerate()
            .skip(start)
            .find(|(_, doc_line)| **doc_line == line)
            .map(|(doc_line_index, _)| doc_line_index);
        return Ok((line.to_owned(), matched_at));
    };
    Ok(match line_pattern.first_match(doc_lines, start)? {
        Some((doc_line_index, values)) => {
            let filled_line = values
                .iter()
                .fold(line.to_owned(), |partly_filled, (placeholder, value)| {
                    partly_filled.replace(placeholder.as_str(), value)
                });
            (filled_line, Some(doc_line_index))
        }
        None => (line.to_owned(), None),
    })
}

/// One-off matching (for content that isn't compiled ahead of time)
pub(crate) fn match_content(content: &str, placeholders: &[String], doc: &str) -> Result<Option<PlaceholderValues>> {
    if !placeholders
        .iter()
        .any(|placeholder| content.contains(placeholder.as_str()))
    {
        return Ok((content == doc).then(PlaceholderValues::new));
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
        assert_eq!(match_content("same\n", &[], "same\n")?, Some(PlaceholderValues::new()));
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
