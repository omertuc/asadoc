//! The ignore directory (`.asadoc/ignore/` by default): doc blocks that don't
//! come from the repo. Each subdirectory is a reason to ignore blocks, with a
//! `README.md` saying what it means, and one file per ignored block content,
//! verbatim:
//!
//! ```text
//! .asadoc/ignore/
//!   example-output/README.md
//!   example-output/nw-dpf-worker-machineconfig--terminal-005.txt
//!   manual-command/README.md
//!   manual-command/nw-dpf-management-cluster-setup--terminal-002.txt
//! ```
//!
//! File names are only names (taken from a block that had the content); a doc
//! block is ignored when its content is exactly a file's content.
//!
//! The other code sources' ignore directories count too: a block ignored in
//! any of them is ignored. Only this repo's is changed.

use crate::config::{AsadocConfig, CodeSource};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::fs;
use std::io::ErrorKind;
use std::iter;
use std::path::{Path, PathBuf};

/// Reasons always offered, with what they mean when their directory has no `README.md`
const PRESET_REASONS: &[(&str, &str)] = &[
    (
        "example-output",
        "Docs often contain code blocks that simply show an example output of a terminal command. These of course usually have no correspondence in a repo so should be ignored.",
    ),
    (
        "manual-command",
        "Docs often ask the user to run a command. If this command is long and complicated, maybe we also already have it in the code repo, so it should be marked and matched. But if it's simple (e.g. 'kubectl get pods') or specific to the docs, it should probably be ignored.",
    ),
    (
        "no-repo-source",
        "Some code blocks in the docs don't have a counterpart in the code repo, so they should be ignored.",
    ),
];

/// The file in a reason's directory that describes the reason
pub(crate) const DESCRIPTION_FILE_NAME: &str = "README.md";

/// A reason to ignore blocks: a subdirectory of the ignore directory
#[derive(Clone, Debug, Serialize)]
pub(crate) struct IgnoreReason {
    pub name: String,
    /// What the reason means, from its `README.md` (empty without one)
    pub description: String,
}

#[derive(Clone)]
pub(crate) struct IgnoredEntry {
    pub reason: String,
    /// In this repo, on disk; in another code source, relative to its root
    pub path: PathBuf,
    pub content: String,
    /// The code source it's in, when not this repo
    pub source: Option<String>,
}

pub(crate) struct IgnoredBlocks {
    ignore_dir: PathBuf,
    pub reasons: Vec<IgnoreReason>,
    pub entries: Vec<IgnoredEntry>,
}

impl IgnoredBlocks {
    /// This repo's ignore directory, and every other code source's
    pub(crate) fn load_all(config: &AsadocConfig) -> Result<Self> {
        let mut ignored_blocks =
            Self::load(&config.ignore_dir).with_context(|| format!("loading {}", config.ignore_dir.display()))?;
        for code_source in config.code.iter().skip(1) {
            ignored_blocks
                .add_other_source(code_source)
                .with_context(|| format!("loading the ignore directory of {}", code_source.describe()))?;
        }
        Ok(ignored_blocks)
    }

    /// Adds what another code source ignores, and its reasons
    fn add_other_source(&mut self, code_source: &CodeSource) -> Result<()> {
        let Some(ignore_dir) = &code_source.ignore_dir else {
            return Ok(());
        };
        let dir_prefix = format!("{ignore_dir}/");
        // (path, reason, file name) of each file directly in a reason's directory
        let mut files: Vec<(String, String, String)> = code_source
            .tree
            .list_files()?
            .into_iter()
            .filter_map(|(path, _)| {
                let (reason, file_name) = path.strip_prefix(&dir_prefix)?.split_once('/')?;
                let is_entry = is_valid_reason_name(reason) && !file_name.contains('/') && !file_name.starts_with('.');
                is_entry.then(|| (path.clone(), reason.to_owned(), file_name.to_owned()))
            })
            .collect();
        files.sort();
        let paths: Vec<String> = files.iter().map(|(path, ..)| path.clone()).collect();
        let texts = code_source.tree.read_all(&paths)?;
        for ((path, reason, file_name), text) in files.into_iter().zip(texts) {
            let Some(text) = text else { continue };
            if file_name == DESCRIPTION_FILE_NAME {
                self.add_known_reason(&reason, text.trim());
            } else {
                self.add_known_reason(&reason, "");
                self.entries.push(IgnoredEntry {
                    reason,
                    path: PathBuf::from(path),
                    content: text,
                    source: code_source.name.clone(),
                });
            }
        }
        Ok(())
    }

    /// Knows of the reason `name`, meaning `description` unless it already has a meaning
    fn add_known_reason(&mut self, name: &str, description: &str) {
        match self.reasons.iter_mut().find(|reason| reason.name == name) {
            Some(reason) if reason.description.is_empty() => description.clone_into(&mut reason.description),
            Some(_) => {}
            None => self.reasons.push(IgnoreReason {
                name: name.to_owned(),
                description: description.to_owned(),
            }),
        }
    }

    pub(crate) fn load(ignore_dir: &Path) -> Result<Self> {
        let reasons = load_reasons(ignore_dir).context("listing the reasons")?;
        let entries = reasons
            .iter()
            .map(|reason| {
                load_reason(ignore_dir, &reason.name)
                    .with_context(|| format!("loading the blocks ignored as {}", reason.name))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        Ok(Self {
            ignore_dir: ignore_dir.to_path_buf(),
            reasons,
            entries,
        })
    }

    /// Adds the reason `name`, meaning `description`
    pub(crate) fn add_reason(&mut self, name: &str, description: &str) -> Result<()> {
        if !is_valid_reason_name(name) {
            bail!("{name:?} can't be a reason: use lowercase letters, digits and dashes");
        }
        if self.reasons.iter().any(|reason| reason.name == name) {
            bail!("the reason {name} already exists");
        }
        let description = description.trim();
        if description.is_empty() {
            bail!("the reason {name} needs a description");
        }
        let reason_dir = self.ignore_dir.join(name);
        fs::create_dir_all(&reason_dir).with_context(|| format!("creating {}", reason_dir.display()))?;
        let description_path = reason_dir.join(DESCRIPTION_FILE_NAME);
        fs::write(&description_path, format!("{description}\n"))
            .with_context(|| format!("writing {}", description_path.display()))?;
        self.reasons.push(IgnoreReason {
            name: name.to_owned(),
            description: description.to_owned(),
        });
        Ok(())
    }

    pub(crate) fn reason_of(&self, content: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|entry| entry.content == content)
            .map(|entry| entry.reason.as_str())
    }

    /// Ignores `content` for `reason`, in a file named after `block_ref`,
    /// replacing it (and `replacing_content`, if given) wherever else this repo ignores it
    pub(crate) fn ignore(
        &mut self,
        content: &str,
        reason: &str,
        block_ref: &str,
        replacing_content: Option<&str>,
    ) -> Result<()> {
        if !self.reasons.iter().any(|known_reason| known_reason.name == reason) {
            bail!("no reason {reason:?} in {}", self.ignore_dir.display());
        }
        self.remove_from_this_repo(content)
            .context("removing where the content was ignored before")?;
        if let Some(replaced_content) = replacing_content {
            self.remove_from_this_repo(replaced_content)
                .context("removing the content this replaces")?;
        }
        let reason_dir = self.ignore_dir.join(reason);
        fs::create_dir_all(&reason_dir).with_context(|| format!("creating {}", reason_dir.display()))?;
        self.describe_here(reason).context("copying the reason's description")?;
        let block_path = unused_path(&reason_dir, &block_ref.replace(['/', ':'], "--"))?;
        fs::write(&block_path, content).with_context(|| format!("writing {}", block_path.display()))?;
        self.entries.push(IgnoredEntry {
            reason: reason.to_owned(),
            path: block_path,
            content: content.to_owned(),
            source: None,
        });
        Ok(())
    }

    /// Gives a reason only other code sources had a description in this repo too
    fn describe_here(&self, reason: &str) -> Result<()> {
        let description_path = self.ignore_dir.join(reason).join(DESCRIPTION_FILE_NAME);
        let description = self
            .reasons
            .iter()
            .find(|known_reason| known_reason.name == reason)
            .map_or("", |known_reason| known_reason.description.as_str());
        let is_preset = PRESET_REASONS.iter().any(|(preset_name, _)| *preset_name == reason);
        if is_preset || description.is_empty() || description_path.exists() {
            return Ok(());
        }
        fs::write(&description_path, format!("{description}\n"))
            .with_context(|| format!("writing {}", description_path.display()))
    }

    /// Stops ignoring `content`: removes every file in this repo holding it.
    /// Fails when another code source ignores it too, as only that repo can
    /// stop ignoring it
    pub(crate) fn unignore(&mut self, content: &str) -> Result<()> {
        if let Some(other_entry) = self
            .entries
            .iter()
            .find(|entry| entry.content == content && entry.source.is_some())
        {
            bail!(
                "{} ignores it too, in {}; stop ignoring it there",
                other_entry.source.as_deref().unwrap_or_default(),
                other_entry.path.display()
            );
        }
        self.remove_from_this_repo(content)
    }

    fn remove_from_this_repo(&mut self, content: &str) -> Result<()> {
        self.entries
            .iter()
            .filter(|entry| entry.content == content && entry.source.is_none())
            .try_for_each(|entry| {
                fs::remove_file(&entry.path).with_context(|| format!("removing {}", entry.path.display()))
            })?;
        self.entries
            .retain(|entry| entry.content != content || entry.source.is_some());
        Ok(())
    }
}

/// Lowercase letters, digits and dashes, starting with a letter or digit
pub(crate) fn is_valid_reason_name(name: &str) -> bool {
    name.chars()
        .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-')
        && !name.is_empty()
        && !name.starts_with('-')
}

/// The preset reasons, then the other subdirectories of `ignore_dir`, in name
/// order, with their descriptions
fn load_reasons(ignore_dir: &Path) -> Result<Vec<IgnoreReason>> {
    let mut reasons = load_reason_dirs(ignore_dir)?;
    for (preset_index, (preset_name, preset_description)) in PRESET_REASONS.iter().enumerate() {
        let preset = match reasons.iter().position(|reason| reason.name == *preset_name) {
            Some(dir_index) => reasons.remove(dir_index),
            None => IgnoreReason {
                name: (*preset_name).to_owned(),
                description: String::new(),
            },
        };
        let description = if preset.description.is_empty() {
            (*preset_description).to_owned()
        } else {
            preset.description
        };
        reasons.insert(preset_index, IgnoreReason { description, ..preset });
    }
    Ok(reasons)
}

/// The subdirectories of `ignore_dir`, in name order, with their descriptions
pub(crate) fn load_reason_dirs(ignore_dir: &Path) -> Result<Vec<IgnoreReason>> {
    let ignore_dir_listing = match fs::read_dir(ignore_dir) {
        Ok(ignore_dir_listing) => ignore_dir_listing,
        // Nothing ignored yet
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("listing {}", ignore_dir.display())),
    };
    let mut reason_dirs = ignore_dir_listing
        .map(|dir_entry| {
            dir_entry
                .map(|dir_entry| dir_entry.path())
                .with_context(|| format!("listing {}", ignore_dir.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    reason_dirs.retain(|reason_dir| reason_dir.is_dir());
    reason_dirs.sort();
    reason_dirs
        .into_iter()
        .filter_map(|reason_dir| {
            let name = reason_dir.file_name()?.to_str()?.to_owned();
            (!name.starts_with('.')).then_some((reason_dir, name))
        })
        .map(|(reason_dir, name)| {
            let description_path = reason_dir.join(DESCRIPTION_FILE_NAME);
            let description = match fs::read_to_string(&description_path) {
                Ok(description) => description.trim().to_owned(),
                Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
                Err(error) => return Err(error).with_context(|| format!("reading {}", description_path.display())),
            };
            Ok(IgnoreReason { name, description })
        })
        .collect()
}

/// The entries ignored for `reason`, in file name order
pub(crate) fn load_reason(ignore_dir: &Path, reason: &str) -> Result<Vec<IgnoredEntry>> {
    let reason_dir = ignore_dir.join(reason);
    let reason_dir_listing = match fs::read_dir(&reason_dir) {
        Ok(reason_dir_listing) => reason_dir_listing,
        // No block ignored for this reason
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("listing {}", reason_dir.display())),
    };
    let mut block_paths = reason_dir_listing
        .map(|dir_entry| {
            dir_entry
                .map(|dir_entry| dir_entry.path())
                .with_context(|| format!("listing {}", reason_dir.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    block_paths.retain(|block_path| {
        block_path.is_file()
            && block_path
                .file_name()
                .is_some_and(|file_name| file_name != DESCRIPTION_FILE_NAME)
    });
    block_paths.sort();
    block_paths
        .into_iter()
        .map(|block_path| {
            let content =
                fs::read_to_string(&block_path).with_context(|| format!("reading {}", block_path.display()))?;
            Ok(IgnoredEntry {
                reason: reason.to_owned(),
                path: block_path,
                content,
                source: None,
            })
        })
        .collect()
}

/// `{file_stem}.txt` in `reason_dir`, or `{file_stem}-2.txt`, `{file_stem}-3.txt`… when that's taken
pub(crate) fn unused_path(reason_dir: &Path, file_stem: &str) -> Result<PathBuf> {
    iter::once(reason_dir.join(format!("{file_stem}.txt")))
        .chain((2..=u64::MAX).map(|suffix| reason_dir.join(format!("{file_stem}-{suffix}.txt"))))
        .find(|candidate_path| !candidate_path.exists())
        .with_context(|| format!("no unused file name for {file_stem} in {}", reason_dir.display()))
}

#[cfg(test)]
#[allow(clippy::panic_in_result_fn, reason = "assertions are how tests fail")]
mod tests {
    use super::*;
    use std::{env, process};

    #[test]
    fn ignores_and_unignores_by_content() -> Result<()> {
        let ignore_dir = env::temp_dir().join(format!("asadoc-ignore-test-{}", process::id()));
        if ignore_dir.exists() {
            fs::remove_dir_all(&ignore_dir).context("clearing the test directory")?;
        }
        let mut ignored_blocks = IgnoredBlocks::load(&ignore_dir)?;
        assert!(
            ignored_blocks
                .ignore("x", "unheard-of", "mod/terminal-000", None)
                .is_err()
        );
        assert!(ignored_blocks.add_reason("Bad name", "Whatever").is_err());
        assert!(ignored_blocks.add_reason("example-output", "Again").is_err());
        ignored_blocks.add_reason("from-upstream", "  Copied from upstream docs\n")?;
        ignored_blocks.ignore("$ oc get nodes\n", "manual-command", "mod/terminal-001", None)?;
        ignored_blocks.ignore("  indented\n\nno trailing", "example-output", "mod/terminal-002", None)?;
        // Same content again moves it; a name taken by other content gets a suffix
        ignored_blocks.ignore("$ oc get nodes\n", "example-output", "mod/terminal-002", None)?;
        fs::write(ignore_dir.join("manual-command/README.md"), "Typed by hand\n").context("describing a preset")?;
        let mut reloaded_blocks = IgnoredBlocks::load(&ignore_dir)?;
        let reasons: Vec<_> = reloaded_blocks
            .reasons
            .iter()
            .map(|reason| (reason.name.as_str(), reason.description.as_str()))
            .collect();
        assert_eq!(
            reasons,
            [
                (
                    "example-output",
                    "Docs often contain code blocks that simply show an example output of a terminal command. These of course usually have no correspondence in a repo so should be ignored."
                ),
                ("manual-command", "Typed by hand"),
                (
                    "no-repo-source",
                    "Some code blocks in the docs don't have a counterpart in the code repo, so they should be ignored."
                ),
                ("from-upstream", "Copied from upstream docs"),
            ]
        );
        assert_eq!(reloaded_blocks.reason_of("$ oc get nodes\n"), Some("example-output"));
        assert_eq!(
            reloaded_blocks.reason_of("  indented\n\nno trailing"),
            Some("example-output")
        );
        assert!(ignore_dir.join("example-output/mod--terminal-002-2.txt").exists());
        reloaded_blocks.unignore("$ oc get nodes\n")?;
        assert_eq!(IgnoredBlocks::load(&ignore_dir)?.entries.len(), 1);
        fs::remove_dir_all(ignore_dir).context("cleaning up the test directory")?;
        Ok(())
    }
}
