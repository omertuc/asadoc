//! The repo side: marked code, found by scanning the repo's files for markers.

use crate::config::Config;
use crate::markers::{self, Markers, MarkerOption, Side};
use crate::matching::Matcher;
use std::sync::Arc;
use serde::Serialize;
use std::path::Path;
use std::process::Command;

/// Files whose comments start with `#`, so they can carry markers
const HASH_COMMENT_EXT: &[&str] = &["", "yaml", "yml", "sh", "bash", "conf", "cfg", "env", "py", "ini", "toml"];
const OTHER_TEXT_EXT: &[&str] = &["txt", "j2", "tpl", "template"];
const MAX_SIZE: u64 = 256 * 1024;

fn ext(file: &str) -> &str {
    let name = file.rsplit('/').next().unwrap_or(file);
    match name.rfind('.') {
        Some(i) if i > 0 => &name[i + 1..],
        _ => "",
    }
}

fn is_makefile(file: &str) -> bool {
    file.rsplit('/').next() == Some("Makefile")
}

pub fn can_hold_markers(file: &str) -> bool {
    HASH_COMMENT_EXT.contains(&ext(file)) || is_makefile(file)
}

/// Text files tracked (or untracked but not ignored) in the repo, minus excluded paths
pub fn repo_files(config: &Config) -> Vec<String> {
    let Ok(out) = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(&config.repo_root)
        .output()
    else {
        return vec![];
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|f| !f.is_empty() && !f.split('/').any(|p| p == "node_modules"))
        .filter(|f| !config.exclude.iter().any(|x| f.starts_with(x.as_str())))
        .filter(|f| can_hold_markers(f) || OTHER_TEXT_EXT.contains(&ext(f)))
        .filter(|f| std::fs::metadata(config.repo_root.join(f)).map(|m| m.is_file() && m.len() < MAX_SIZE).unwrap_or(false))
        .map(str::to_string)
        .collect()
}

pub fn read(root: &Path, file: &str) -> Option<String> {
    std::fs::read_to_string(root.join(file)).ok()
}

/// A marked file, or a marked section of a file
#[derive(Clone)]
pub struct MarkedCode {
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
    pub lines: Option<(usize, usize)>,
    /// 1-based marker line numbers
    pub marker_lines: Vec<usize>,
    /// The content, compiled for matching against doc blocks
    pub matcher: Arc<Matcher>,
}

/// A repo file, for finding code to mark
pub struct RepoFile {
    pub file: String,
    pub text: String,
    pub markers: Markers,
}

#[derive(Clone, Debug, Serialize)]
pub struct Problem {
    pub file: String,
    pub message: String,
}

pub struct Scan {
    pub marked: Vec<MarkedCode>,
    pub files: Vec<RepoFile>,
    pub problems: Vec<Problem>,
}

pub fn code_id(file: &str, section: Option<&str>) -> String {
    match section {
        Some(s) => format!("{file}#{s}"),
        None => file.to_string(),
    }
}

pub fn scan(config: &Config) -> Scan {
    let mut scan = Scan { marked: vec![], files: vec![], problems: vec![] };
    for file in repo_files(config) {
        let Some(text) = read(&config.repo_root, &file) else { continue };
        let found = markers::parse_markers(&text);
        for p in &found.problems {
            scan.problems.push(Problem { file: file.clone(), message: p.clone() });
        }
        let mut add = |section: Option<&str>, raw: String, options: &[MarkerOption], lines, marker_lines| {
            let (code_side, doc_side): (Vec<_>, Vec<_>) = options.iter().cloned().partition(|o| o.side == Side::Repo);
            match markers::apply_options(&raw, &code_side) {
                Ok(content) => scan.marked.push(MarkedCode {
                    matcher: Arc::new(Matcher::new(&content, &markers::placeholders(&code_side))),
                    id: code_id(&file, section),
                    file: file.clone(),
                    section: section.map(str::to_string),
                    content,
                    placeholders: markers::placeholders(&code_side),
                    options: code_side,
                    doc_options: doc_side,
                    lines,
                    marker_lines,
                }),
                Err(e) => scan.problems.push(Problem {
                    file: file.clone(),
                    message: match section {
                        Some(s) => format!("section \"{s}\": {e}"),
                        None => format!("file marker: {e}"),
                    },
                }),
            }
        };
        if let Some(h) = &found.file {
            let raw = format!("{}{}", &text[..h.marker_from], &text[h.marker_to..]);
            add(None, raw, &h.options, None, (h.start_line..=h.last_line).collect());
        }
        for s in found.sections.values() {
            let mut marker_lines: Vec<usize> = (s.header.start_line..=s.header.last_line).collect();
            marker_lines.push(s.end_line);
            add(Some(&s.name), text[s.from..s.to].to_string(), &s.header.options, Some((s.header.last_line + 1, s.end_line - 1)), marker_lines);
        }
        scan.files.push(RepoFile { file, text, markers: found });
    }
    scan
}
