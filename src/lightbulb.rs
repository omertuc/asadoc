//! Lightbulbs: simple repo-side changes that make candidate code match a doc
//! block exactly — marking a file or part of one, a leading `---`, trailing
//! newlines, and the doc's shell prompts. Anything more involved is left to a
//! person or an AI assistant.

use crate::docs::Block;
use crate::markers::{self, MarkerOption, Side};
use crate::matching::match_content;
use crate::repo::{self, can_hold_markers};
use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Fix {
    MarkFile,
    MarkSection { name: String, line: usize, lines: usize, options: Vec<MarkerOption> },
    RemoveDashes,
    AddDashes,
    TrailingNewline { newlines: usize, had: usize },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub fixes: Vec<Fix>,
    /// Options for the doc side, added to the marker
    pub doc_options: Vec<MarkerOption>,
}

/// What a candidate is: marked code, or an unmarked file
pub struct Candidate<'a> {
    pub file: &'a str,
    pub section: Option<&'a str>,
    /// Marked code with its options applied; None for an unmarked file
    pub marked: Option<&'a repo::MarkedCode>,
    pub text: &'a str,
    pub markers: &'a markers::Markers,
}

pub fn prompt_option() -> MarkerOption {
    MarkerOption::text("strip-line-prefix", "$ ", Side::Doc)
}

/// Leading YAML document marker and trailing newlines, made to agree with the doc
fn normalize(doc: &str, content: &str) -> (String, Vec<Fix>) {
    let mut fixes = Vec::new();
    let mut out = content.to_string();
    let doc_dashes = doc.starts_with("---\n");
    if !doc_dashes && out.starts_with("---\n") {
        out = out[4..].to_string();
        fixes.push(Fix::RemoveDashes);
    } else if doc_dashes && !out.starts_with("---\n") {
        out = format!("---\n{out}");
        fixes.push(Fix::AddDashes);
    }
    let doc_trail = doc.len() - doc.trim_end_matches('\n').len();
    let out_trail = out.len() - out.trim_end_matches('\n').len();
    if doc_trail != out_trail {
        out = format!("{}{}", out.trim_end_matches('\n'), "\n".repeat(doc_trail));
        fixes.push(Fix::TrailingNewline { newlines: doc_trail, had: out_trail });
    }
    (out, fixes)
}

fn content_plan(doc: &str, content: &str, mut pre: Vec<Fix>, can_normalize: bool, declared: &[String]) -> Option<Vec<Fix>> {
    let (out, fixes) = if can_normalize { normalize(doc, content) } else { (content.to_string(), vec![]) };
    if pre.is_empty() && fixes.is_empty() {
        return None;
    }
    match_content(&out, declared, doc)?;
    pre.extend(fixes);
    Some(pre)
}

fn section_name(markers: &markers::Markers, block: &Block) -> String {
    let base = block.module.clone();
    let mut name = base.clone();
    let mut n = 2;
    while markers.sections.contains_key(&name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    name
}

/// The doc block as a stretch of lines in an unmarked file: consecutive code
/// lines equal to it, skipping comment lines in between (only when the doc has
/// no comments of its own). Marking that stretch as a section is the fix.
fn portion_plan(block: &Block, doc: &str, text: &str, markers: &markers::Markers) -> Option<Vec<Fix>> {
    let doc_lines: Vec<&str> = doc.strip_suffix('\n').unwrap_or(doc).split('\n').collect();
    if doc_lines.len() < 3 && doc.len() < 40 {
        return None;
    }
    let is_comment = |l: &str| l.trim_start().starts_with('#');
    let skip_comments = !doc_lines.iter().any(|l| is_comment(l));
    let lines: Vec<&str> = text.split('\n').collect();
    let code: Vec<usize> = (0..lines.len()).filter(|&i| !(skip_comments && is_comment(lines[i]))).collect();
    for window in code.windows(doc_lines.len()) {
        if !window.iter().zip(&doc_lines).all(|(&i, d)| lines[i] == *d) {
            continue;
        }
        let (first, last) = (window[0] + 1, window[window.len() - 1] + 1);
        // Already a section: that section is a candidate of its own
        if markers.sections.values().any(|s| first > s.header.last_line && last < s.end_line) {
            return None;
        }
        let options = if last - first + 1 > window.len() {
            vec![MarkerOption::text("remove-lines-starting-with", "#", Side::Repo)]
        } else {
            vec![]
        };
        return Some(vec![Fix::MarkSection { name: section_name(markers, block), line: first, lines: last - first + 1, options }]);
    }
    None
}

fn plan_against(block: &Block, doc: &str, cand: &Candidate) -> Option<Vec<Fix>> {
    if let Some(code) = cand.marked {
        return content_plan(doc, &code.content, vec![], !markers::has_shaping_options(&code.options), &code.placeholders);
    }
    if !can_hold_markers(cand.file) || cand.markers.file.is_some() {
        return None;
    }
    if cand.markers.sections.is_empty() {
        let plan = if cand.text == doc { Some(vec![Fix::MarkFile]) } else { content_plan(doc, cand.text, vec![Fix::MarkFile], true, &[]) };
        if plan.is_some() {
            return plan;
        }
    }
    portion_plan(block, doc, cand.text, cand.markers)
}

/// The lightbulb for a candidate, if any. When shell prompts also stand in the
/// way, it adds `doc strip-line-prefix: "$ "` to the marker.
pub fn plan_for(block: &Block, cand: &Candidate) -> Option<Plan> {
    let declared = cand.marked.map(|c| c.doc_options.as_slice()).unwrap_or(&[]);
    if !declared.is_empty() {
        let doc = markers::apply_options(&block.content, declared).ok()?;
        return plan_against(block, &doc, cand).map(|fixes| Plan { fixes, doc_options: vec![] });
    }
    if let Some(fixes) = plan_against(block, &block.content, cand) {
        return Some(Plan { fixes, doc_options: vec![] });
    }
    if !block.content.lines().any(|l| l.starts_with("$ ")) {
        return None;
    }
    let doc = markers::apply_options(&block.content, &[prompt_option()]).ok()?;
    plan_against(block, &doc, cand).map(|fixes| Plan { fixes, doc_options: vec![prompt_option()] })
}

/// Rewrites the marked lines of a section (or the whole file around its file
/// marker), keeping the markers where they are
fn edit_region(text: &str, section: Option<&str>, edit: impl Fn(&str) -> String) -> Result<String> {
    let found = markers::parse_markers(text);
    if let Some(name) = section {
        let s = found.sections.get(name).context("section not found")?;
        return Ok(format!("{}{}{}", &text[..s.from], edit(&text[s.from..s.to]), &text[s.to..]));
    }
    let h = found.file.context("file marker not found")?;
    let marker = &text[h.marker_from..h.marker_to];
    let edited = edit(&format!("{}{}", &text[..h.marker_from], &text[h.marker_to..]));
    let lines: Vec<&str> = edited.split('\n').collect();
    let before = h.start_line - 1;
    let at = if before == 0 { 0 } else { lines[..before].join("\n").len() + 1 };
    Ok(format!("{}{}{}", &edited[..at.min(edited.len())], marker, &edited[at.min(edited.len())..]))
}

/// Adds options to a section's start marker (or the file marker) as continuation lines
fn extend_marker(text: &str, section: Option<&str>, options: &[MarkerOption]) -> Result<String> {
    let found = markers::parse_markers(text);
    let h = match section {
        Some(name) => found.sections.get(name).map(|s| s.header.clone()),
        None => found.file.clone(),
    }
    .context("marker not found")?;
    let indent: String = text[h.marker_from..].chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let sep = if text[..h.marker_to].ends_with('\n') { "" } else { "\n" };
    Ok(format!("{}{sep}{}{}", &text[..h.marker_to], markers::continuation_lines(&indent, options), &text[h.marker_to..]))
}

/// Applies a lightbulb's changes to `file` (and its section, if any)
pub fn apply(root: &Path, file: &str, section: Option<&str>, plan: &Plan) -> Result<()> {
    let path = root.join(file);
    let mut text = std::fs::read_to_string(&path)?;
    let mut section = section.map(str::to_string);
    let mut options_placed = false;
    for fix in &plan.fixes {
        match fix {
            Fix::MarkFile => {
                let at = if text.starts_with("#!") { text.find('\n').map(|i| i + 1).unwrap_or(text.len()) } else { 0 };
                let head = format!("# {}\n{}", markers::file_marker(), markers::continuation_lines("", &plan.doc_options));
                text.insert_str(at, &head);
                options_placed = true;
            }
            Fix::MarkSection { name, line, lines: count, options } => {
                let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
                let indent: String = lines[line - 1].chars().take_while(|c| c.is_whitespace()).collect();
                let all: Vec<MarkerOption> = options.iter().chain(&plan.doc_options).cloned().collect();
                lines.insert(line - 1 + count, format!("{indent}# {}", markers::section_end(name)));
                let mut head = vec![format!("{indent}# {}", markers::section_start(name))];
                head.extend(all.iter().map(|o| format!("{indent}#   | {}", o.format())));
                lines.splice(line - 1..line - 1, head);
                text = lines.join("\n");
                section = Some(name.clone());
                options_placed = true;
            }
            Fix::RemoveDashes => text = edit_region(&text, section.as_deref(), |s| s.strip_prefix("---\n").unwrap_or(s).to_string())?,
            Fix::AddDashes => text = edit_region(&text, section.as_deref(), |s| format!("---\n{s}"))?,
            Fix::TrailingNewline { newlines, .. } => {
                text = edit_region(&text, section.as_deref(), |s| format!("{}{}", s.trim_end_matches('\n'), "\n".repeat(*newlines)))?
            }
        }
    }
    if !plan.doc_options.is_empty() && !options_placed {
        text = extend_marker(&text, section.as_deref(), &plan.doc_options)?;
    }
    std::fs::write(&path, text)?;
    Ok(())
}
