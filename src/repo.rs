//! The repo side: marked code, found by scanning the repo's files for markers.

use crate::config::AsadocConfig;
use crate::markers::{self, MarkedSection, MarkerHeader, MarkerOption, Markers, OptionSide};
use crate::matching::Matcher;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

/// Files whose comments start with `#`, so they can carry markers
const HASH_COMMENT_EXTENSIONS: &[&str] = &[
    "", "yaml", "yml", "sh", "bash", "conf", "cfg", "env", "py", "ini", "toml",
];
const OTHER_TEXT_EXTENSIONS: &[&str] = &["txt", "j2", "tpl", "template"];
const MAX_SCANNED_FILE_SIZE: u64 = 256 * 1024;

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

/// Text files tracked (or untracked but not ignored) in the repo, minus excluded paths
fn repo_files(config: &AsadocConfig) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(&config.repo_root)
        .output()
        .context("running git ls-files")?;
    if !output.status.success() {
        bail!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    listing
        .lines()
        .filter(|file| !file.is_empty() && !file.split('/').any(|path_component| path_component == "node_modules"))
        .filter(|file| {
            !config
                .exclude
                .iter()
                .any(|excluded| file.starts_with(excluded.as_str()))
        })
        .filter(|file| can_hold_markers(file) || OTHER_TEXT_EXTENSIONS.contains(&extension(file)))
        .map(|file| Ok(is_small_file(&config.repo_root.join(file))?.then(|| file.to_owned())))
        .filter_map(Result::transpose)
        .collect()
}

/// Whether `path` is a regular file small enough to scan; false when it's gone
fn is_small_file(path: &Path) -> Result<bool> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() && metadata.len() < MAX_SCANNED_FILE_SIZE),
        // Deleted, but still in the index
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

/// A repo file's text; None when it's gone or isn't text
pub(crate) fn read_repo_file(repo_root: &Path, file: &str) -> Result<Option<String>> {
    let path = repo_root.join(file);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::InvalidData) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// A marked file, or a marked section of a file
#[derive(Clone)]
pub(crate) struct MarkedCode {
    /// `path` or `path#section`
    pub id: String,
    pub file: String,
    pub section: Option<String>,
    /// The marked lines with the code-side options applied
    pub content: String,
    pub options: Vec<MarkerOption>,
    pub doc_options: Vec<MarkerOption>,
    pub placeholders: Vec<String>,
    /// 1-based range of the marked lines (sections only)
    pub line_range: Option<(usize, usize)>,
    /// 1-based marker line numbers
    pub marker_lines: Vec<usize>,
    /// The content, compiled for matching against doc blocks
    pub matcher: Arc<Matcher>,
}

/// A repo file, for finding code to mark
pub(crate) struct RepoFile {
    pub path: String,
    pub text: String,
    pub markers: Markers,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct MarkerProblem {
    pub file: String,
    pub message: String,
}

pub(crate) struct RepoScan {
    pub marked: Vec<MarkedCode>,
    pub files: Vec<RepoFile>,
    pub problems: Vec<MarkerProblem>,
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
    };
    for file in repo_files(config).context("listing the repo's files")? {
        let Some(text) = read_repo_file(&config.repo_root, &file)? else {
            continue;
        };
        scan_file(&mut scan, file, text)?;
    }
    Ok(scan)
}

/// Adds a file's marked code, and the problems with its markers, to `scan`
fn scan_file(scan: &mut RepoScan, file: String, text: String) -> Result<()> {
    let parsed_markers = markers::parse_markers(&text).with_context(|| format!("finding markers in {file}"))?;
    scan.problems
        .extend(parsed_markers.problems.iter().map(|message| MarkerProblem {
            file: file.clone(),
            message: message.clone(),
        }));
    if let Some(header) = &parsed_markers.file {
        add_marked(scan, &file, marked_file(&text, header)?)
            .with_context(|| format!("reading the file marked in {file}"))?;
    }
    for section in parsed_markers.sections.values() {
        add_marked(scan, &file, marked_section(&text, section)?)
            .with_context(|| format!("reading section \"{}\" of {file}", section.name))?;
    }
    scan.files.push(RepoFile {
        path: file,
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
fn add_marked(scan: &mut RepoScan, file: &str, marked: UnappliedMarkedCode<'_>) -> Result<()> {
    let (repo_side_options, doc_side_options): (Vec<_>, Vec<_>) = marked
        .options
        .iter()
        .cloned()
        .partition(|option| option.side == OptionSide::Repo);
    match markers::apply_options(&marked.unapplied_content, &repo_side_options) {
        Ok(content) => {
            let placeholders =
                markers::placeholders(&repo_side_options, &content).context("finding the placeholders")?;
            let matcher = Matcher::new(&content, &placeholders).context("compiling the code for matching")?;
            scan.marked.push(MarkedCode {
                matcher: Arc::new(matcher),
                id: code_id(file, marked.section),
                file: file.to_owned(),
                section: marked.section.map(str::to_owned),
                content,
                placeholders,
                options: repo_side_options,
                doc_options: doc_side_options,
                line_range: marked.line_range,
                marker_lines: marked.marker_lines,
            });
        }
        Err(error) => scan.problems.push(MarkerProblem {
            file: file.to_owned(),
            message: match marked.section {
                Some(section) => format!("section \"{section}\": {error}"),
                None => format!("file marker: {error}"),
            },
        }),
    }
    Ok(())
}
