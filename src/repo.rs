//! The repo side: marked code, found in the files of every code source (this
//! repo, and each `[[code]]`) that contain markers.

use crate::config::{AsadocConfig, CodeSource};
use crate::markers::{self, MarkedSection, MarkerHeader, MarkerOption, Markers, OptionSide};
use crate::matching::{Matcher, SpacedPlaceholders};
use anyhow::{Context, Result};
use serde::Serialize;
use std::sync::Arc;

/// Files whose comments start with `#`, so they can carry markers
const HASH_COMMENT_EXTENSIONS: &[&str] = &[
    "", "yaml", "yml", "sh", "bash", "conf", "cfg", "env", "py", "ini", "toml",
];

fn extension(file: &str) -> &str {
    let file_name = file.rsplit('/').next().unwrap_or(file);
    match file_name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => extension,
        _ => "",
    }
}

fn is_makefile(file: &str) -> bool {
    file.rsplit('/').next() == Some("Makefile")
}

pub(crate) fn can_hold_markers(file: &str) -> bool {
    HASH_COMMENT_EXTENSIONS.contains(&extension(file)) || is_makefile(file)
}

/// The files of a code source with markers in them, with their text
fn files_with_markers(code_source: &CodeSource) -> Result<Vec<(String, String)>> {
    code_source.tree.files_containing(markers::MARKER_PREFIX, |file| {
        can_hold_markers(file)
            && !code_source
                .ignore_dir_prefix
                .as_ref()
                .is_some_and(|ignore_dir_prefix| file.starts_with(ignore_dir_prefix.as_str()))
    })
}

/// A marked file, or a marked section of a file
#[derive(Clone)]
pub(crate) struct MarkedCode {
    /// `file` or `file#section`
    pub id: String,
    /// The file's name among all the code sources (see `CodeSource::qualify`)
    pub file: String,
    /// Index into `AsadocConfig::code`
    pub source_index: usize,
    pub section: Option<String>,
    /// The marked lines with the code-side options applied
    pub content: String,
    pub options: Vec<MarkerOption>,
    pub doc_options: Vec<MarkerOption>,
    pub placeholders: Vec<String>,
    /// The regex each placeholder's value must match, where its `param` gives one
    pub spaced_placeholders: SpacedPlaceholders,
    /// 1-based range of the marked lines (sections only)
    pub line_range: Option<(usize, usize)>,
    /// 1-based marker line numbers
    pub marker_lines: Vec<usize>,
    /// The content, compiled for matching against doc blocks
    pub matcher: Arc<Matcher>,
}

/// A repo file with markers, for finding more code to mark in it
pub(crate) struct RepoFile {
    /// Its name among all the code sources (see `CodeSource::qualify`)
    pub path: String,
    /// Index into `AsadocConfig::code`
    pub source_index: usize,
    pub text: String,
    pub markers: Markers,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct MarkerProblem {
    pub file: String,
    pub message: String,
}

/// A `TODO` on a marker in this repo
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Todo {
    pub file: String,
    /// The id of the marked code it's on (see `code_id`)
    pub code_id: String,
    /// 1-based line number of the marker line it's on
    pub line: usize,
    pub text: String,
}

pub(crate) struct RepoScan {
    pub marked: Vec<MarkedCode>,
    pub files: Vec<RepoFile>,
    /// This repo's (the other code sources' are theirs to report)
    pub problems: Vec<MarkerProblem>,
    /// This repo's, like `problems`
    pub todos: Vec<Todo>,
}

fn code_id(file: &str, section: Option<&str>) -> String {
    match section {
        Some(section) => format!("{file}#{section}"),
        None => file.to_owned(),
    }
}

/// A marked file or section, before its code-side options are applied
struct UnappliedMarkedCode<'a> {
    section: Option<&'a str>,
    unapplied_content: String,
    options: &'a [MarkerOption],
    line_range: Option<(usize, usize)>,
    marker_lines: Vec<usize>,
}

pub(crate) fn scan(config: &AsadocConfig) -> Result<RepoScan> {
    let mut scan = RepoScan {
        marked: vec![],
        files: vec![],
        problems: vec![],
        todos: vec![],
    };
    for (source_index, code_source) in config.code.iter().enumerate() {
        let files = files_with_markers(code_source).context("finding the files with markers")?;
        for (file, text) in files {
            scan_file(&mut scan, source_index, code_source.qualify(&file), text)
                .with_context(|| format!("scanning {}", code_source.describe()))?;
        }
    }
    Ok(scan)
}

/// Adds a file's marked code, and the problems with its markers, to `scan`
fn scan_file(scan: &mut RepoScan, source_index: usize, file: String, text: String) -> Result<()> {
    let parsed_markers = markers::parse_markers(&text).with_context(|| format!("finding markers in {file}"))?;
    let mut marker_problems: Vec<MarkerProblem> = parsed_markers
        .problems
        .iter()
        .map(|message| MarkerProblem {
            file: file.clone(),
            message: message.clone(),
        })
        .collect();
    if let Some(header) = &parsed_markers.file {
        add_marked(
            scan,
            &mut marker_problems,
            source_index,
            &file,
            marked_file(&text, header)?,
        )
        .with_context(|| format!("reading the file marked in {file}"))?;
    }
    for section in parsed_markers.sections.values() {
        add_marked(
            scan,
            &mut marker_problems,
            source_index,
            &file,
            marked_section(&text, section)?,
        )
        .with_context(|| format!("reading section \"{}\" of {file}", section.name))?;
    }
    if source_index == 0 {
        scan.problems.extend(marker_problems);
        scan.todos.extend(parsed_markers.todos.iter().map(|todo| Todo {
            file: file.clone(),
            code_id: code_id(&file, todo.section.as_deref()),
            line: todo.line,
            text: todo.text.clone(),
        }));
    }
    scan.files.push(RepoFile {
        path: file,
        source_index,
        text,
        markers: parsed_markers,
    });
    Ok(())
}

/// The whole file, minus its file marker
fn marked_file<'a>(text: &str, header: &'a MarkerHeader) -> Result<UnappliedMarkedCode<'a>> {
    let before_marker = text
        .get(..header.marker_from)
        .context("file marker starts outside the file")?;
    let after_marker = text
        .get(header.marker_to..)
        .context("file marker ends outside the file")?;
    // Marked content is whole lines, like a doc block's: a file's last
    // line counts as ending with a newline even when the file doesn't
    let without_marker = format!("{before_marker}{after_marker}");
    let unapplied_content = if without_marker.is_empty() || without_marker.ends_with('\n') {
        without_marker
    } else {
        without_marker + "\n"
    };
    Ok(UnappliedMarkedCode {
        section: None,
        unapplied_content,
        options: &header.options,
        line_range: None,
        marker_lines: (header.first_line..=header.last_line).collect(),
    })
}

/// The lines between a section's markers
fn marked_section<'a>(text: &str, section: &'a MarkedSection) -> Result<UnappliedMarkedCode<'a>> {
    let unapplied_content = text
        .get(section.content_from..section.content_to)
        .with_context(|| format!("section \"{}\" is outside the file", section.name))?;
    Ok(UnappliedMarkedCode {
        section: Some(&section.name),
        unapplied_content: unapplied_content.to_owned(),
        options: &section.header.options,
        line_range: Some((section.header.last_line + 1, section.end_line - 1)),
        marker_lines: (section.header.first_line..=section.header.last_line)
            .chain([section.end_line])
            .collect(),
    })
}

/// Adds the marked code to `scan`, or a problem when its options can't be applied
fn add_marked(
    scan: &mut RepoScan,
    marker_problems: &mut Vec<MarkerProblem>,
    source_index: usize,
    file: &str,
    marked: UnappliedMarkedCode<'_>,
) -> Result<()> {
    let (repo_side_options, doc_side_options): (Vec<_>, Vec<_>) = marked
        .options
        .iter()
        .filter(|option| !option.is_note())
        .cloned()
        .partition(|option| option.side == OptionSide::Repo);
    match markers::apply_options(&marked.unapplied_content, &repo_side_options) {
        Ok(content) => {
            let placeholders =
                markers::placeholders(&repo_side_options, &content).context("finding the placeholders")?;
            let spaced_placeholders = markers::spaced_placeholders(&repo_side_options, &content)
                .context("finding the placeholders whose values may have spaces")?;
            let matcher = Matcher::new(&content, &placeholders, &spaced_placeholders)
                .context("compiling the code for matching")?;
            scan.marked.push(MarkedCode {
                matcher: Arc::new(matcher),
                id: code_id(file, marked.section),
                file: file.to_owned(),
                source_index,
                section: marked.section.map(str::to_owned),
                content,
                placeholders,
                spaced_placeholders,
                options: repo_side_options,
                doc_options: doc_side_options,
                line_range: marked.line_range,
                marker_lines: marked.marker_lines,
            });
        }
        Err(error) => marker_problems.push(MarkerProblem {
            file: file.to_owned(),
            message: match marked.section {
                Some(section) => format!("section \"{section}\": {error}"),
                None => format!("file marker: {error}"),
            },
        }),
    }
    Ok(())
}
