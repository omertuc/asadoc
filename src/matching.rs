//! Comparing marked code with doc blocks: exactly, except where the code has
//! placeholders (declared with `param`), which match any value on their line,
//! the same value wherever the same placeholder appears.

use fancy_regex::Regex;
use std::collections::BTreeMap;

/// The value each placeholder took
pub type Values = BTreeMap<String, String>;

struct Pattern {
    regex: Regex,
    /// (placeholder, capture group)
    groups: Vec<(String, String)>,
}

impl Pattern {
    fn new(content: &str, placeholders: &[&String]) -> Option<Pattern> {
        let mut by_length = placeholders.to_vec();
        by_length.sort_by_key(|p| std::cmp::Reverse(p.len()));
        let alternation = by_length.iter().map(|p| fancy_regex::escape(p).into_owned()).collect::<Vec<_>>().join("|");
        let splitter = Regex::new(&format!("({alternation})")).ok()?;
        let mut groups: Vec<(String, String)> = Vec::new();
        let mut src = String::from(r"\A");
        let mut last = 0;
        for m in splitter.find_iter(content).flatten() {
            src.push_str(&fancy_regex::escape(&content[last..m.start()]));
            let p = m.as_str();
            match groups.iter().find(|(q, _)| q == p) {
                Some((_, g)) => src.push_str(&format!(r"\k<{g}>")),
                None => {
                    let g = format!("p{}", groups.len());
                    src.push_str(&format!(r"(?<{g}>[^\n]*?)"));
                    groups.push((p.to_string(), g));
                }
            }
            last = m.end();
        }
        src.push_str(&fancy_regex::escape(&content[last..]));
        src.push_str(r"\z");
        Some(Pattern { regex: Regex::new(&src).ok()?, groups })
    }

    fn captures(&self, doc: &str) -> Option<Values> {
        let caps = self.regex.captures(doc).ok()??;
        Some(self.groups.iter().map(|(p, g)| (p.clone(), caps.name(g).map(|m| m.as_str().to_string()).unwrap_or_default())).collect())
    }
}

/// Matches doc text against some code with placeholders. Compiled once, used
/// against every doc block.
pub struct Matcher {
    content: String,
    whole: Option<Pattern>,
    /// For each line of the content with placeholders: its pattern (for filling
    /// placeholders in for display)
    lines: Vec<Option<Pattern>>,
}

impl Matcher {
    pub fn new(content: &str, placeholders: &[String]) -> Matcher {
        let used: Vec<&String> = placeholders.iter().filter(|p| content.contains(p.as_str())).collect();
        let lines = content
            .split('\n')
            .map(|line| {
                let on_line: Vec<&String> = used.iter().copied().filter(|p| line.contains(p.as_str())).collect();
                if on_line.is_empty() { None } else { Pattern::new(line, &on_line) }
            })
            .collect();
        Matcher {
            content: content.to_string(),
            whole: if used.is_empty() { None } else { Pattern::new(content, &used) },
            lines,
        }
    }

    /// Whether the code matches `doc`, and if so the value each placeholder took
    pub fn matches(&self, doc: &str) -> Option<Values> {
        match &self.whole {
            None => (self.content == doc).then(Values::new),
            Some(p) => p.captures(doc),
        }
    }

    /// For showing a diff: the code with placeholders replaced by the doc's
    /// values. Code and doc are walked in order: each line with placeholders
    /// takes its values from the next doc line of the same shape, after the
    /// doc lines already matched.
    pub fn fill(&self, doc: &str) -> String {
        if self.whole.is_none() {
            return self.content.clone();
        }
        let doc_lines: Vec<&str> = doc.split('\n').collect();
        let mut next = 0;
        let mut out = Vec::new();
        for (line, pattern) in self.content.split('\n').zip(&self.lines) {
            match pattern {
                Some(p) => match (next..doc_lines.len()).find_map(|i| p.captures(doc_lines[i]).map(|v| (i, v))) {
                    Some((i, values)) => {
                        next = i + 1;
                        out.push(values.iter().fold(line.to_string(), |l, (ph, v)| l.replace(ph.as_str(), v)));
                    }
                    None => out.push(line.to_string()),
                },
                None => {
                    if let Some(i) = (next..doc_lines.len()).find(|&i| doc_lines[i] == line) {
                        next = i + 1;
                    }
                    out.push(line.to_string());
                }
            }
        }
        out.join("\n")
    }
}

/// One-off matching (for content that isn't compiled ahead of time)
pub fn match_content(content: &str, placeholders: &[String], doc: &str) -> Option<Values> {
    if !placeholders.iter().any(|p| content.contains(p.as_str())) {
        return (content == doc).then(Values::new);
    }
    Matcher::new(content, placeholders).matches(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_match_consistently() {
        let ph = vec!["<A>".to_string()];
        assert_eq!(match_content("x: <A>\ny: <A>\n", &ph, "x: 1\ny: 1\n").unwrap()["<A>"], "1");
        assert!(match_content("x: <A>\ny: <A>\n", &ph, "x: 1\ny: 2\n").is_none());
        assert!(match_content("x: <A>\n", &ph, "x: 1\n2\n").is_none());
        assert_eq!(match_content("same\n", &[], "same\n"), Some(Values::new()));
    }

    #[test]
    fn fills_placeholders_for_display() {
        let ph = vec!["${V}".to_string()];
        assert_eq!(Matcher::new("a ${V}\nb", &ph).fill("c\na 4.22\n"), "a 4.22\nb");
        // In order: the second `end:` takes the second value
        let ph = vec!["<A>".to_string(), "<B>".to_string()];
        assert_eq!(Matcher::new("end: <A>\nx\nend: <B>\n", &ph).fill("end: 1\nx\nend: 45\n"), "end: 1\nx\nend: 45\n");
    }
}
