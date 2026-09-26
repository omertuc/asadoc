//! Lightbulbs: simple repo-side changes that make candidate code match a doc
//! block exactly: marking a file or part of one, adding or removing a leading
//! `---` or trailing newlines, and, when the doc's shell prompts are in the
//! way, adding `doc strip-line-prefix: "$ "` to the marker. Anything more
//! involved is left to a person or an AI assistant.

use crate::docs::Block;
use crate::markers::{self, MarkerOption, Markers, Side};
use crate::matching::match_content;
use crate::repo::{MarkedCode, can_hold_markers};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::fs;
use std::iter;
use std::path::Path;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum Fix {
    MarkFile,
    MarkSection {
        name: String,
        line: usize,
        lines: usize,
        options: Vec<MarkerOption>,
    },
    RemoveDashes,
    AddDashes,
    TrailingNewline {
        newlines: usize,
        had: usize,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Plan {
    pub fixes: Vec<Fix>,
    /// Options for the doc side, added to the marker
    pub doc_options: Vec<MarkerOption>,
}

/// What a candidate is: marked code, or an unmarked file
pub(crate) struct Candidate<'a> {
    pub file: &'a str,
    pub section: Option<&'a str>,
    /// Marked code with its options applied; None for an unmarked file
    pub marked: Option<&'a MarkedCode>,
    pub text: &'a str,
    pub markers: &'a Markers,
}

pub(crate) fn prompt_option() -> MarkerOption {
    MarkerOption::text("strip-line-prefix", "$ ", Side::Doc)
}

/// The leading YAML document marker, made to agree with the doc
fn normalize_dashes(doc: &str, content: &str) -> (String, Option<Fix>) {
    let doc_dashes = doc.starts_with("---\n");
    if !doc_dashes && let Some(rest) = content.strip_prefix("---\n") {
        (rest.to_owned(), Some(Fix::RemoveDashes))
    } else if doc_dashes && !content.starts_with("---\n") {
        (format!("---\n{content}"), Some(Fix::AddDashes))
    } else {
        (content.to_owned(), None)
    }
}

/// The number of newlines `text` ends with
fn trailing_newlines(text: &str) -> usize {
    text.len() - text.trim_end_matches('\n').len()
}

/// Trailing newlines, made to agree with the doc
fn normalize_trailing_newlines(doc: &str, content: String) -> (String, Option<Fix>) {
    let doc_trail = trailing_newlines(doc);
    let content_trail = trailing_newlines(&content);
    if doc_trail == content_trail {
        return (content, None);
    }
    let fixed = format!("{}{}", content.trim_end_matches('\n'), "\n".repeat(doc_trail));
    let fix = Fix::TrailingNewline {
        newlines: doc_trail,
        had: content_trail,
    };
    (fixed, Some(fix))
}

/// Leading YAML document marker and trailing newlines, made to agree with the doc
fn normalize(doc: &str, content: &str) -> (String, Vec<Fix>) {
    let (dashed, dashes_fix) = normalize_dashes(doc, content);
    let (normalized, newlines_fix) = normalize_trailing_newlines(doc, dashed);
    (normalized, dashes_fix.into_iter().chain(newlines_fix).collect())
}

fn content_plan(
    doc: &str,
    content: &str,
    prior: Vec<Fix>,
    can_normalize: bool,
    declared: &[String],
) -> Result<Option<Vec<Fix>>> {
    let (normalized, fixes) = if can_normalize {
        normalize(doc, content)
    } else {
        (content.to_owned(), vec![])
    };
    if prior.is_empty() && fixes.is_empty() {
        return Ok(None);
    }
    if match_content(&normalized, declared, doc)
        .context("matching the fixed code against the doc")?
        .is_none()
    {
        return Ok(None);
    }
    Ok(Some(prior.into_iter().chain(fixes).collect()))
}

fn section_name(markers: &Markers, block: &Block) -> String {
    let base = &block.module;
    let mut name = base.clone();
    let mut suffix = 2;
    while markers.sections.contains_key(&name) {
        name = format!("{base}-{suffix}");
        suffix += 1;
    }
    name
}

fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

/// The doc block as a stretch of lines in an unmarked file: consecutive code
/// lines equal to it, skipping comment lines in between (only when the doc has
/// no comments of its own). Marking that stretch as a section is the fix.
fn portion_plan(block: &Block, doc: &str, text: &str, markers: &Markers) -> Option<Vec<Fix>> {
    let doc_lines: Vec<&str> = doc.strip_suffix('\n').unwrap_or(doc).split('\n').collect();
    if doc_lines.len() < 3 && doc.len() < 40 {
        return None;
    }
    let skip_comments = !doc_lines.iter().any(|line| is_comment(line));
    // (index, line) of the lines to compare
    let code: Vec<(usize, &str)> = text
        .split('\n')
        .enumerate()
        .filter(|(_, line)| !(skip_comments && is_comment(line)))
        .collect();
    let window = code.windows(doc_lines.len()).find(|window| {
        window
            .iter()
            .zip(&doc_lines)
            .all(|((_, code_line), doc_line)| code_line == doc_line)
    })?;
    let (first, _) = window.first()?;
    let (last, _) = window.last()?;
    let (first, last) = (first + 1, last + 1);
    // Already a section: that section is a candidate of its own
    if markers
        .sections
        .values()
        .any(|section| first > section.header.last_line && last < section.end_line)
    {
        return None;
    }
    let lines = last - first + 1;
    let options = if lines > window.len() {
        vec![MarkerOption::text("remove-lines-starting-with", "#", Side::Repo)]
    } else {
        vec![]
    };
    Some(vec![Fix::MarkSection {
        name: section_name(markers, block),
        line: first,
        lines,
        options,
    }])
}

/// Marking the whole of an unmarked file, with its content normalized if need be
fn whole_file_plan(doc: &str, file_text: &str) -> Result<Option<Vec<Fix>>> {
    // Marked, the file's last line ends with a newline even if the file doesn't
    let text = if file_text.is_empty() || file_text.ends_with('\n') {
        file_text.to_owned()
    } else {
        format!("{file_text}\n")
    };
    if text == doc {
        return Ok(Some(vec![Fix::MarkFile]));
    }
    content_plan(doc, &text, vec![Fix::MarkFile], true, &[])
}

fn plan_against(block: &Block, doc: &str, cand: &Candidate<'_>) -> Result<Option<Vec<Fix>>> {
    if let Some(code) = cand.marked {
        return content_plan(
            doc,
            &code.content,
            vec![],
            !markers::has_shaping_options(&code.options),
            &code.placeholders,
        )
        .context("fixing the marked code");
    }
    if !can_hold_markers(cand.file) || cand.markers.file.is_some() {
        return Ok(None);
    }
    if cand.markers.sections.is_empty()
        && let Some(fixes) = whole_file_plan(doc, cand.text).context("marking the whole file")?
    {
        return Ok(Some(fixes));
    }
    Ok(portion_plan(block, doc, cand.text, cand.markers))
}

/// The lightbulb for a candidate, if any. When shell prompts also stand in the
/// way, it adds `doc strip-line-prefix: "$ "` to the marker.
pub(crate) fn plan_for(block: &Block, cand: &Candidate<'_>) -> Result<Option<Plan>> {
    let declared = cand.marked.map_or(&[][..], |code| code.doc_options.as_slice());
    if !declared.is_empty() {
        // Doc options that don't fit the block leave nothing to fix
        let Ok(doc) = markers::apply_options(&block.content, declared) else {
            return Ok(None);
        };
        return Ok(plan_against(block, &doc, cand)
            .context("planning against the doc with its declared options")?
            .map(|fixes| Plan {
                fixes,
                doc_options: vec![],
            }));
    }
    if let Some(fixes) = plan_against(block, &block.content, cand).context("planning against the doc")? {
        return Ok(Some(Plan {
            fixes,
            doc_options: vec![],
        }));
    }
    if !block.content.lines().any(|line| line.starts_with("$ ")) {
        return Ok(None);
    }
    let Ok(doc) = markers::apply_options(&block.content, &[prompt_option()]) else {
        return Ok(None);
    };
    Ok(plan_against(block, &doc, cand)
        .context("planning against the doc without its shell prompts")?
        .map(|fixes| Plan {
            fixes,
            doc_options: vec![prompt_option()],
        }))
}

const fn lines_word(count: usize) -> &'static str {
    if count == 1 { "line" } else { "lines" }
}

/// The section later fixes apply to, once `fix` is done
fn section_after(fix: &Fix, section: Option<String>) -> Option<String> {
    match fix {
        Fix::MarkSection { name, .. } => Some(name.clone()),
        _ => section,
    }
}

fn describe_trailing_newline(newlines: usize, had: usize, place: &str) -> String {
    if had == 0 {
        format!("add the missing newline at the end of {place}")
    } else if newlines < had {
        let count = had - newlines;
        format!(
            "remove the {count} extra blank {} at the end of {place}",
            lines_word(count)
        )
    } else {
        let count = newlines - had;
        format!("add {count} blank {} at the end of {place}", lines_word(count))
    }
}

/// One fix's changes to `file` (and `section`, if any), in words
fn describe_fix(fix: &Fix, file: &str, section: Option<&str>) -> Vec<String> {
    let place = match section {
        Some(name) => format!("section \"{name}\""),
        None => "the file".to_owned(),
    };
    match fix {
        Fix::MarkFile => vec![format!(
            "add a `# {}` line at the top of {file}, marking the whole file",
            markers::file_marker()
        )],
        Fix::MarkSection {
            name,
            line,
            lines,
            options,
        } => iter::once(format!(
            "add start and end markers around lines {line}-{} of {file}, making them section \"{name}\"",
            line + lines.saturating_sub(1)
        ))
        .chain(
            options
                .iter()
                .map(|option| format!("add `| {}` to its start marker", option.format())),
        )
        .collect(),
        Fix::RemoveDashes => vec![format!("remove the `---` line at the top of {place}")],
        Fix::AddDashes => vec![format!("add a `---` line at the top of {place}")],
        Fix::TrailingNewline { newlines, had } => vec![describe_trailing_newline(*newlines, *had, &place)],
    }
}

/// A lightbulb's changes to `file` (and its section, if any), in words
pub(crate) fn describe(plan: &Plan, file: &str, section: Option<&str>) -> Vec<String> {
    let fix_steps = plan
        .fixes
        .iter()
        .scan(section.map(str::to_owned), |section, fix| {
            let steps = describe_fix(fix, file, section.as_deref());
            *section = section_after(fix, section.take());
            Some(steps)
        })
        .flatten();
    let option_steps = plan
        .doc_options
        .iter()
        .map(|option| format!("add `| {}` to the marker", option.format()));
    fix_steps.chain(option_steps).collect()
}

/// `text` split at byte `at`
fn split(text: &str, at: usize) -> Result<(&str, &str)> {
    text.split_at_checked(at)
        .with_context(|| format!("byte {at} is outside the text or inside a character"))
}

/// `text` without the bytes `from..to`: (before, removed, after)
fn split3(text: &str, from: usize, to: usize) -> Result<(&str, &str, &str)> {
    let (before, rest) = split(text, from).context("splitting at the start of the range")?;
    let length = to.checked_sub(from).context("range ends before it starts")?;
    let (removed, after) = split(rest, length).context("splitting at the end of the range")?;
    Ok((before, removed, after))
}

/// Rewrites the marked lines of section `name`, keeping its markers where they are
fn edit_section(text: &str, found: &Markers, name: &str, edit: impl Fn(&str) -> String) -> Result<String> {
    let section = found
        .sections
        .get(name)
        .with_context(|| format!("section \"{name}\" not found"))?;
    let (before, region, after) = split3(text, section.from, section.to).context("locating the section")?;
    Ok(format!("{before}{}{after}", edit(region)))
}

/// Rewrites the whole file around its file marker, keeping the marker where it is
fn edit_marked_file(text: &str, found: &Markers, edit: impl Fn(&str) -> String) -> Result<String> {
    let header = found.file.as_ref().context("file marker not found")?;
    let (before, marker, after) =
        split3(text, header.marker_from, header.marker_to).context("locating the file marker")?;
    let edited = edit(&format!("{before}{after}"));
    // Back where it was: after the same number of lines
    let at = edited
        .split_inclusive('\n')
        .take(header.start_line - 1)
        .map(str::len)
        .sum::<usize>();
    let (before, after) = split(&edited, at).context("locating the file marker in the edited text")?;
    Ok(format!("{before}{marker}{after}"))
}

/// Rewrites the marked lines of a section (or the whole file around its file
/// marker), keeping the markers where they are
fn edit_region(text: &str, section: Option<&str>, edit: impl Fn(&str) -> String) -> Result<String> {
    let found = markers::parse_markers(text).context("finding the markers")?;
    match section {
        Some(name) => edit_section(text, &found, name, edit),
        None => edit_marked_file(text, &found, edit),
    }
}

/// Adds options to a section's start marker (or the file marker) as continuation lines
fn extend_marker(text: &str, section: Option<&str>, options: &[MarkerOption]) -> Result<String> {
    let found = markers::parse_markers(text).context("finding the markers")?;
    let header = match section {
        Some(name) => found.sections.get(name).map(|section| section.header.clone()),
        None => found.file,
    }
    .context("marker not found")?;
    let (_, marker, after) = split3(text, header.marker_from, header.marker_to).context("locating the marker")?;
    let before = text.get(..header.marker_to).context("marker ends outside the text")?;
    let indent: String = marker
        .chars()
        .take_while(|character| *character == ' ' || *character == '\t')
        .collect();
    let separator = if before.ends_with('\n') { "" } else { "\n" };
    Ok(format!(
        "{before}{separator}{}{after}",
        markers::continuation_lines(&indent, options)
    ))
}

/// Adds a file marker (with the doc options) at the top of `text`, after any shebang line
fn mark_file(text: &str, doc_options: &[MarkerOption]) -> Result<String> {
    let at = if text.starts_with("#!") {
        text.find('\n').map_or(text.len(), |newline| newline + 1)
    } else {
        0
    };
    let head = format!(
        "# {}\n{}",
        markers::file_marker(),
        markers::continuation_lines("", doc_options)
    );
    let (before, after) = split(text, at).context("placing the file marker")?;
    Ok(format!("{before}{head}{after}"))
}

/// Surrounds the `count` lines from (1-based) `line` with markers making them section `name`
fn mark_section(text: &str, name: &str, line: usize, count: usize, options: &[MarkerOption]) -> Result<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let split_lines = line.checked_sub(1).and_then(|at| {
        let (before, rest) = lines.split_at_checked(at)?;
        let (marked, after) = rest.split_at_checked(count)?;
        Some((before, rest, marked, after))
    });
    let Some((before, rest, marked, after)) = split_lines else {
        bail!("the {count} lines from line {line} aren't all in the file");
    };
    let indent: String = rest
        .first()
        .map(|first| {
            first
                .chars()
                .take_while(|character| character.is_whitespace())
                .collect()
        })
        .unwrap_or_default();
    let start = iter::once(format!("{indent}# {}", markers::section_start(name))).chain(
        options
            .iter()
            .map(|option| format!("{indent}#   | {}", option.format())),
    );
    let end = format!("{indent}# {}", markers::section_end(name));
    let owned = |lines: &[&str]| lines.iter().map(ToString::to_string).collect::<Vec<_>>();
    Ok(owned(before)
        .into_iter()
        .chain(start)
        .chain(owned(marked))
        .chain(iter::once(end))
        .chain(owned(after))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Applies one fix to `text`, in `section` (if any)
fn apply_fix(text: &str, section: Option<&str>, fix: &Fix, doc_options: &[MarkerOption]) -> Result<String> {
    match fix {
        Fix::MarkFile => mark_file(text, doc_options).context("marking the file"),
        Fix::MarkSection {
            name,
            line,
            lines,
            options,
        } => {
            let all: Vec<MarkerOption> = options.iter().chain(doc_options).cloned().collect();
            mark_section(text, name, *line, *lines, &all).with_context(|| format!("marking section \"{name}\""))
        }
        Fix::RemoveDashes => edit_region(text, section, |region| {
            region.strip_prefix("---\n").unwrap_or(region).to_owned()
        })
        .context("removing the leading ---"),
        Fix::AddDashes => edit_region(text, section, |region| format!("---\n{region}")).context("adding a leading ---"),
        Fix::TrailingNewline { newlines, .. } => edit_region(text, section, |region| {
            format!("{}{}", region.trim_end_matches('\n'), "\n".repeat(*newlines))
        })
        .context("fixing the trailing newlines"),
    }
}

/// Applies a lightbulb's changes to `file` (and its section, if any)
pub(crate) fn apply(root: &Path, file: &str, section: Option<&str>, plan: &Plan) -> Result<()> {
    let path = root.join(file);
    let original = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let (fixed, section) = plan.fixes.iter().try_fold(
        (original, section.map(str::to_owned)),
        |(text, section), fix| -> Result<_> {
            let text = apply_fix(&text, section.as_deref(), fix, &plan.doc_options)?;
            Ok((text, section_after(fix, section)))
        },
    )?;
    let options_placed = plan
        .fixes
        .iter()
        .any(|fix| matches!(fix, Fix::MarkFile | Fix::MarkSection { .. }));
    let text = if !plan.doc_options.is_empty() && !options_placed {
        extend_marker(&fixed, section.as_deref(), &plan.doc_options).context("adding the doc options")?
    } else {
        fixed
    };
    fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
