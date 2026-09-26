//! Lightbulbs: simple repo-side changes that make candidate code match a doc
//! block exactly: marking a file or part of one, adding or removing a leading
//! `---` or trailing newlines, and, when the doc's shell prompts are in the
//! way, adding `doc strip-line-prefix: "$ "` to the marker. Anything more
//! involved is left to a person or an AI assistant.

use crate::docs::DocBlock;
use crate::markers::{self, MarkerOption, Markers, OptionSide};
use crate::matching::match_content;
use crate::repo::{MarkedCode, can_hold_markers};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::fs;
use std::iter;
use std::path::Path;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum LightbulbFix {
    MarkFile,
    MarkSection {
        name: String,
        #[serde(rename = "line")]
        first_line: usize,
        #[serde(rename = "lines")]
        line_count: usize,
        options: Vec<MarkerOption>,
    },
    RemoveDashes,
    AddDashes,
    TrailingNewline {
        #[serde(rename = "newlines")]
        wanted_newlines: usize,
        #[serde(rename = "had")]
        had_newlines: usize,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LightbulbPlan {
    pub fixes: Vec<LightbulbFix>,
    /// Options for the doc side, added to the marker
    pub doc_options: Vec<MarkerOption>,
}

/// What a candidate is: marked code, or an unmarked file
pub(crate) struct Candidate<'a> {
    pub file: &'a str,
    pub section: Option<&'a str>,
    /// Marked code with its options applied; None for an unmarked file
    pub marked_code: Option<&'a MarkedCode>,
    pub file_text: &'a str,
    pub markers: &'a Markers,
}

pub(crate) fn strip_prompt_option() -> MarkerOption {
    MarkerOption::text("strip-line-prefix", "$ ", OptionSide::Doc)
}

/// The leading YAML document marker, made to agree with the doc
fn normalize_dashes(doc_code: &str, repo_code: &str) -> (String, Option<LightbulbFix>) {
    let doc_has_dashes = doc_code.starts_with("---\n");
    if !doc_has_dashes && let Some(without_dashes) = repo_code.strip_prefix("---\n") {
        (without_dashes.to_owned(), Some(LightbulbFix::RemoveDashes))
    } else if doc_has_dashes && !repo_code.starts_with("---\n") {
        (format!("---\n{repo_code}"), Some(LightbulbFix::AddDashes))
    } else {
        (repo_code.to_owned(), None)
    }
}

/// The number of newlines `text` ends with
fn trailing_newlines(text: &str) -> usize {
    text.len() - text.trim_end_matches('\n').len()
}

/// Trailing newlines, made to agree with the doc
fn normalize_trailing_newlines(doc_code: &str, repo_code: String) -> (String, Option<LightbulbFix>) {
    let doc_newlines = trailing_newlines(doc_code);
    let repo_newlines = trailing_newlines(&repo_code);
    if doc_newlines == repo_newlines {
        return (repo_code, None);
    }
    let fixed_code = format!("{}{}", repo_code.trim_end_matches('\n'), "\n".repeat(doc_newlines));
    let newlines_fix = LightbulbFix::TrailingNewline {
        wanted_newlines: doc_newlines,
        had_newlines: repo_newlines,
    };
    (fixed_code, Some(newlines_fix))
}

/// Leading YAML document marker and trailing newlines, made to agree with the doc
fn normalize(doc_code: &str, repo_code: &str) -> (String, Vec<LightbulbFix>) {
    let (dashes_normalized, dashes_fix) = normalize_dashes(doc_code, repo_code);
    let (normalized, newlines_fix) = normalize_trailing_newlines(doc_code, dashes_normalized);
    (normalized, dashes_fix.into_iter().chain(newlines_fix).collect())
}

fn content_plan(
    doc_code: &str,
    repo_code: &str,
    prior_fixes: Vec<LightbulbFix>,
    can_normalize: bool,
    placeholders: &[String],
) -> Result<Option<Vec<LightbulbFix>>> {
    let (normalized, normalize_fixes) = if can_normalize {
        normalize(doc_code, repo_code)
    } else {
        (repo_code.to_owned(), vec![])
    };
    if prior_fixes.is_empty() && normalize_fixes.is_empty() {
        return Ok(None);
    }
    if match_content(&normalized, placeholders, doc_code)
        .context("matching the fixed code against the doc")?
        .is_none()
    {
        return Ok(None);
    }
    Ok(Some(prior_fixes.into_iter().chain(normalize_fixes).collect()))
}

/// A section name, after the block's module, that no section in `markers` has yet
fn unused_section_name(markers: &Markers, block: &DocBlock) -> String {
    let module_name = &block.module;
    let mut section_name = module_name.clone();
    let mut suffix = 2;
    while markers.sections.contains_key(&section_name) {
        section_name = format!("{module_name}-{suffix}");
        suffix += 1;
    }
    section_name
}

fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

/// The doc block as a stretch of lines in an unmarked file: consecutive code
/// lines equal to it, skipping comment lines in between (only when the doc has
/// no comments of its own). Marking that stretch as a section is the fix.
fn portion_plan(block: &DocBlock, doc_code: &str, file_text: &str, markers: &Markers) -> Option<Vec<LightbulbFix>> {
    let doc_lines: Vec<&str> = doc_code.strip_suffix('\n').unwrap_or(doc_code).split('\n').collect();
    if doc_lines.len() < 3 && doc_code.len() < 40 {
        return None;
    }
    let skip_comments = !doc_lines.iter().any(|doc_line| is_comment(doc_line));
    // (index, line) of the lines to compare
    let compared_lines: Vec<(usize, &str)> = file_text
        .split('\n')
        .enumerate()
        .filter(|(_, code_line)| !(skip_comments && is_comment(code_line)))
        .collect();
    let matching_window = compared_lines.windows(doc_lines.len()).find(|candidate_window| {
        candidate_window
            .iter()
            .zip(&doc_lines)
            .all(|((_, code_line), doc_line)| code_line == doc_line)
    })?;
    let (first_index, _) = matching_window.first()?;
    let (last_index, _) = matching_window.last()?;
    let (first_line, last_line) = (first_index + 1, last_index + 1);
    // Already a section: that section is a candidate of its own
    if markers
        .sections
        .values()
        .any(|section| first_line > section.header.last_line && last_line < section.end_line)
    {
        return None;
    }
    let line_count = last_line - first_line + 1;
    let options = if line_count > matching_window.len() {
        vec![MarkerOption::text("remove-lines-starting-with", "#", OptionSide::Repo)]
    } else {
        vec![]
    };
    Some(vec![LightbulbFix::MarkSection {
        name: unused_section_name(markers, block),
        first_line,
        line_count,
        options,
    }])
}

/// Marking the whole of an unmarked file, with its content normalized if need be
fn whole_file_plan(doc_code: &str, file_text: &str) -> Result<Option<Vec<LightbulbFix>>> {
    // Marked, the file's last line ends with a newline even if the file doesn't
    let marked_text = if file_text.is_empty() || file_text.ends_with('\n') {
        file_text.to_owned()
    } else {
        format!("{file_text}\n")
    };
    if marked_text == doc_code {
        return Ok(Some(vec![LightbulbFix::MarkFile]));
    }
    content_plan(doc_code, &marked_text, vec![LightbulbFix::MarkFile], true, &[])
}

fn plan_against(block: &DocBlock, doc_code: &str, candidate: &Candidate<'_>) -> Result<Option<Vec<LightbulbFix>>> {
    if let Some(marked_code) = candidate.marked_code {
        return content_plan(
            doc_code,
            &marked_code.content,
            vec![],
            !markers::has_shaping_options(&marked_code.options),
            &marked_code.placeholders,
        )
        .context("fixing the marked code");
    }
    if !can_hold_markers(candidate.file) || candidate.markers.file.is_some() {
        return Ok(None);
    }
    if candidate.markers.sections.is_empty()
        && let Some(fixes) = whole_file_plan(doc_code, candidate.file_text).context("marking the whole file")?
    {
        return Ok(Some(fixes));
    }
    Ok(portion_plan(block, doc_code, candidate.file_text, candidate.markers))
}

/// The lightbulb for a candidate, if any. When shell prompts also stand in the
/// way, it adds `doc strip-line-prefix: "$ "` to the marker.
pub(crate) fn plan_for(block: &DocBlock, candidate: &Candidate<'_>) -> Result<Option<LightbulbPlan>> {
    let declared_doc_options = candidate
        .marked_code
        .map_or(&[][..], |marked_code| marked_code.doc_options.as_slice());
    if !declared_doc_options.is_empty() {
        // Doc options that don't fit the block leave nothing to fix
        let Ok(doc_code) = markers::apply_options(&block.content, declared_doc_options) else {
            return Ok(None);
        };
        return Ok(plan_against(block, &doc_code, candidate)
            .context("planning against the doc with its declared options")?
            .map(|fixes| LightbulbPlan {
                fixes,
                doc_options: vec![],
            }));
    }
    if let Some(fixes) = plan_against(block, &block.content, candidate).context("planning against the doc")? {
        return Ok(Some(LightbulbPlan {
            fixes,
            doc_options: vec![],
        }));
    }
    if !block.content.lines().any(|doc_line| doc_line.starts_with("$ ")) {
        return Ok(None);
    }
    let Ok(doc_code) = markers::apply_options(&block.content, &[strip_prompt_option()]) else {
        return Ok(None);
    };
    Ok(plan_against(block, &doc_code, candidate)
        .context("planning against the doc without its shell prompts")?
        .map(|fixes| LightbulbPlan {
            fixes,
            doc_options: vec![strip_prompt_option()],
        }))
}

const fn lines_word(line_count: usize) -> &'static str {
    if line_count == 1 { "line" } else { "lines" }
}

/// The section later fixes apply to, once `fix` is done
fn section_after(fix: &LightbulbFix, section: Option<String>) -> Option<String> {
    match fix {
        LightbulbFix::MarkSection { name, .. } => Some(name.clone()),
        _ => section,
    }
}

fn describe_trailing_newline(wanted_newlines: usize, had_newlines: usize, place: &str) -> String {
    if had_newlines == 0 {
        format!("add the missing newline at the end of {place}")
    } else if wanted_newlines < had_newlines {
        let extra_count = had_newlines - wanted_newlines;
        format!(
            "remove the {extra_count} extra blank {} at the end of {place}",
            lines_word(extra_count)
        )
    } else {
        let missing_count = wanted_newlines - had_newlines;
        format!(
            "add {missing_count} blank {} at the end of {place}",
            lines_word(missing_count)
        )
    }
}

/// One fix's changes to `file` (and `section`, if any), in words
fn describe_fix(fix: &LightbulbFix, file: &str, section: Option<&str>) -> Vec<String> {
    let place = match section {
        Some(section_name) => format!("section \"{section_name}\""),
        None => "the file".to_owned(),
    };
    match fix {
        LightbulbFix::MarkFile => vec![format!(
            "add a `# {}` line at the top of {file}, marking the whole file",
            markers::file_marker()
        )],
        LightbulbFix::MarkSection {
            name,
            first_line,
            line_count,
            options,
        } => iter::once(format!(
            "add start and end markers around lines {first_line}-{} of {file}, making them section \"{name}\"",
            first_line + line_count.saturating_sub(1)
        ))
        .chain(
            options
                .iter()
                .map(|option| format!("add `| {}` to its start marker", option.marker_text())),
        )
        .collect(),
        LightbulbFix::RemoveDashes => vec![format!("remove the `---` line at the top of {place}")],
        LightbulbFix::AddDashes => vec![format!("add a `---` line at the top of {place}")],
        LightbulbFix::TrailingNewline {
            wanted_newlines,
            had_newlines,
        } => vec![describe_trailing_newline(*wanted_newlines, *had_newlines, &place)],
    }
}

/// A lightbulb's changes to `file` (and its section, if any), in words
pub(crate) fn describe(plan: &LightbulbPlan, file: &str, section: Option<&str>) -> Vec<String> {
    let fix_steps = plan
        .fixes
        .iter()
        .scan(section.map(str::to_owned), |current_section, fix| {
            let steps = describe_fix(fix, file, current_section.as_deref());
            *current_section = section_after(fix, current_section.take());
            Some(steps)
        })
        .flatten();
    let option_steps = plan
        .doc_options
        .iter()
        .map(|option| format!("add `| {}` to the marker", option.marker_text()));
    fix_steps.chain(option_steps).collect()
}

/// `text` split at byte `byte_offset`
fn split_at_byte(text: &str, byte_offset: usize) -> Result<(&str, &str)> {
    text.split_at_checked(byte_offset)
        .with_context(|| format!("byte {byte_offset} is outside the text or inside a character"))
}

/// `text` without the bytes `range_start..range_end`: (before, removed, after)
fn split_around_range(text: &str, range_start: usize, range_end: usize) -> Result<(&str, &str, &str)> {
    let (before, rest) = split_at_byte(text, range_start).context("splitting at the start of the range")?;
    let range_length = range_end
        .checked_sub(range_start)
        .context("range ends before it starts")?;
    let (removed, after) = split_at_byte(rest, range_length).context("splitting at the end of the range")?;
    Ok((before, removed, after))
}

/// Rewrites the marked lines of section `section_name`, keeping its markers where they are
fn edit_section(
    text: &str,
    found_markers: &Markers,
    section_name: &str,
    rewrite: impl Fn(&str) -> String,
) -> Result<String> {
    let section = found_markers
        .sections
        .get(section_name)
        .with_context(|| format!("section \"{section_name}\" not found"))?;
    let (before, marked_region, after) =
        split_around_range(text, section.content_from, section.content_to).context("locating the section")?;
    Ok(format!("{before}{}{after}", rewrite(marked_region)))
}

/// Rewrites the whole file around its file marker, keeping the marker where it is
fn edit_marked_file(text: &str, found_markers: &Markers, rewrite: impl Fn(&str) -> String) -> Result<String> {
    let file_header = found_markers.file.as_ref().context("file marker not found")?;
    let (before, marker_text, after) =
        split_around_range(text, file_header.marker_from, file_header.marker_to).context("locating the file marker")?;
    let edited = rewrite(&format!("{before}{after}"));
    // Back where it was: after the same number of lines
    let marker_offset = edited
        .split_inclusive('\n')
        .take(file_header.first_line - 1)
        .map(str::len)
        .sum::<usize>();
    let (before, after) =
        split_at_byte(&edited, marker_offset).context("locating the file marker in the edited text")?;
    Ok(format!("{before}{marker_text}{after}"))
}

/// Rewrites the marked lines of a section (or the whole file around its file
/// marker), keeping the markers where they are
fn edit_region(text: &str, section: Option<&str>, rewrite: impl Fn(&str) -> String) -> Result<String> {
    let found_markers = markers::parse_markers(text).context("finding the markers")?;
    match section {
        Some(section_name) => edit_section(text, &found_markers, section_name, rewrite),
        None => edit_marked_file(text, &found_markers, rewrite),
    }
}

/// Adds options to a section's start marker (or the file marker) as continuation lines
fn extend_marker(text: &str, section: Option<&str>, options: &[MarkerOption]) -> Result<String> {
    let found_markers = markers::parse_markers(text).context("finding the markers")?;
    let marker_header = match section {
        Some(section_name) => found_markers
            .sections
            .get(section_name)
            .map(|found_section| found_section.header.clone()),
        None => found_markers.file,
    }
    .context("marker not found")?;
    let (_, marker_text, after) =
        split_around_range(text, marker_header.marker_from, marker_header.marker_to).context("locating the marker")?;
    let through_marker = text
        .get(..marker_header.marker_to)
        .context("marker ends outside the text")?;
    let indent: String = marker_text
        .chars()
        .take_while(|character| *character == ' ' || *character == '\t')
        .collect();
    let separator = if through_marker.ends_with('\n') { "" } else { "\n" };
    Ok(format!(
        "{through_marker}{separator}{}{after}",
        markers::continuation_lines(&indent, options)
    ))
}

/// Adds a file marker (with the doc options) at the top of `text`, after any shebang line
fn mark_file(text: &str, doc_options: &[MarkerOption]) -> Result<String> {
    let marker_offset = if text.starts_with("#!") {
        text.find('\n').map_or(text.len(), |newline_offset| newline_offset + 1)
    } else {
        0
    };
    let marker_lines = format!(
        "# {}\n{}",
        markers::file_marker(),
        markers::continuation_lines("", doc_options)
    );
    let (before, after) = split_at_byte(text, marker_offset).context("placing the file marker")?;
    Ok(format!("{before}{marker_lines}{after}"))
}

/// Surrounds the `line_count` lines from (1-based) `first_line` with markers making them section `section_name`
fn mark_section(
    text: &str,
    section_name: &str,
    first_line: usize,
    line_count: usize,
    options: &[MarkerOption],
) -> Result<String> {
    let text_lines: Vec<&str> = text.split('\n').collect();
    let split_lines = first_line.checked_sub(1).and_then(|line_count_before_section| {
        let (lines_before_section, lines_from_section_start) =
            text_lines.split_at_checked(line_count_before_section)?;
        let (section_lines, lines_after_section) = lines_from_section_start.split_at_checked(line_count)?;
        Some((
            lines_before_section,
            lines_from_section_start,
            section_lines,
            lines_after_section,
        ))
    });
    let Some((lines_before_section, lines_from_section_start, section_lines, lines_after_section)) = split_lines else {
        bail!("the {line_count} lines from line {first_line} aren't all in the file");
    };
    let indent: String = lines_from_section_start
        .first()
        .map(|first_marked_line| {
            first_marked_line
                .chars()
                .take_while(|character| character.is_whitespace())
                .collect()
        })
        .unwrap_or_default();
    let start_marker_lines = iter::once(format!("{indent}# {}", markers::section_start_marker(section_name))).chain(
        options
            .iter()
            .map(|option| format!("{indent}#   | {}", option.marker_text())),
    );
    let end_marker_line = format!("{indent}# {}", markers::section_end_marker(section_name));
    let to_owned_lines = |borrowed_lines: &[&str]| borrowed_lines.iter().map(ToString::to_string).collect::<Vec<_>>();
    Ok(to_owned_lines(lines_before_section)
        .into_iter()
        .chain(start_marker_lines)
        .chain(to_owned_lines(section_lines))
        .chain(iter::once(end_marker_line))
        .chain(to_owned_lines(lines_after_section))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Applies one fix to `text`, in `section` (if any)
fn apply_fix(text: &str, section: Option<&str>, fix: &LightbulbFix, doc_options: &[MarkerOption]) -> Result<String> {
    match fix {
        LightbulbFix::MarkFile => mark_file(text, doc_options).context("marking the file"),
        LightbulbFix::MarkSection {
            name,
            first_line,
            line_count,
            options,
        } => {
            let all_options: Vec<MarkerOption> = options.iter().chain(doc_options).cloned().collect();
            mark_section(text, name, *first_line, *line_count, &all_options)
                .with_context(|| format!("marking section \"{name}\""))
        }
        LightbulbFix::RemoveDashes => edit_region(text, section, |region| {
            region.strip_prefix("---\n").unwrap_or(region).to_owned()
        })
        .context("removing the leading ---"),
        LightbulbFix::AddDashes => {
            edit_region(text, section, |region| format!("---\n{region}")).context("adding a leading ---")
        }
        LightbulbFix::TrailingNewline { wanted_newlines, .. } => edit_region(text, section, |region| {
            format!("{}{}", region.trim_end_matches('\n'), "\n".repeat(*wanted_newlines))
        })
        .context("fixing the trailing newlines"),
    }
}

/// Applies a lightbulb's changes to `file` (and its section, if any)
pub(crate) fn apply(repo_root: &Path, file: &str, section: Option<&str>, plan: &LightbulbPlan) -> Result<()> {
    let file_path = repo_root.join(file);
    let original_text = fs::read_to_string(&file_path).with_context(|| format!("reading {}", file_path.display()))?;
    let (fixed_text, final_section) = plan.fixes.iter().try_fold(
        (original_text, section.map(str::to_owned)),
        |(text, current_section), fix| -> Result<_> {
            let text = apply_fix(&text, current_section.as_deref(), fix, &plan.doc_options)?;
            Ok((text, section_after(fix, current_section)))
        },
    )?;
    let options_placed = plan
        .fixes
        .iter()
        .any(|fix| matches!(fix, LightbulbFix::MarkFile | LightbulbFix::MarkSection { .. }));
    let final_text = if !plan.doc_options.is_empty() && !options_placed {
        extend_marker(&fixed_text, final_section.as_deref(), &plan.doc_options).context("adding the doc options")?
    } else {
        fixed_text
    };
    fs::write(&file_path, final_text).with_context(|| format!("writing {}", file_path.display()))?;
    Ok(())
}
