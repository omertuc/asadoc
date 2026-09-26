//! The ignore directory (`.asadoc-ignore/` by default): doc blocks that don't
//! come from the repo. One file per ignored block content, verbatim, in a
//! subdirectory named after the reason:
//!
//! ```text
//! .asadoc-ignore/
//!   example-output/nw-dpf-worker-machineconfig--terminal-005.txt
//!   manual-command/nw-dpf-management-cluster-setup--terminal-002.txt
//!   no-repo-source/…
//! ```
//!
//! File names are only names (taken from a block that had the content); a doc
//! block is ignored when its content is exactly a file's content.

use anyhow::{Context, Result};
use std::fs;
use std::io::ErrorKind;
use std::iter;
use std::path::{Path, PathBuf};

pub(crate) const IGNORE_REASONS: &[&str] = &["example-output", "manual-command", "no-repo-source"];

#[derive(Clone)]
pub(crate) struct IgnoredEntry {
    pub reason: String,
    pub path: PathBuf,
    pub content: String,
}

pub(crate) struct IgnoredBlocks {
    ignore_dir: PathBuf,
    pub entries: Vec<IgnoredEntry>,
}

impl IgnoredBlocks {
    pub(crate) fn load(ignore_dir: &Path) -> Result<Self> {
        let entries = IGNORE_REASONS
            .iter()
            .map(|reason| {
                load_reason(ignore_dir, reason).with_context(|| format!("loading the blocks ignored as {reason}"))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        Ok(Self {
            ignore_dir: ignore_dir.to_path_buf(),
            entries,
        })
    }

    pub(crate) fn reason_of(&self, content: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|entry| entry.content == content)
            .map(|entry| entry.reason.as_str())
    }

    /// Ignores `content` for `reason`, in a file named after `block_ref`,
    /// replacing it (and `replacing_content`, if given) wherever else it's ignored
    pub(crate) fn ignore(
        &mut self,
        content: &str,
        reason: &str,
        block_ref: &str,
        replacing_content: Option<&str>,
    ) -> Result<()> {
        self.unignore(content)
            .context("removing where the content was ignored before")?;
        if let Some(replaced_content) = replacing_content {
            self.unignore(replaced_content)
                .context("removing the content this replaces")?;
        }
        let reason_dir = self.ignore_dir.join(reason);
        fs::create_dir_all(&reason_dir).with_context(|| format!("creating {}", reason_dir.display()))?;
        let block_path = unused_path(&reason_dir, &block_ref.replace('/', "--"))?;
        fs::write(&block_path, content).with_context(|| format!("writing {}", block_path.display()))?;
        self.entries.push(IgnoredEntry {
            reason: reason.to_owned(),
            path: block_path,
            content: content.to_owned(),
        });
        Ok(())
    }

    /// Stops ignoring `content`: removes every file holding it
    pub(crate) fn unignore(&mut self, content: &str) -> Result<()> {
        self.entries
            .iter()
            .filter(|entry| entry.content == content)
            .try_for_each(|entry| {
                fs::remove_file(&entry.path).with_context(|| format!("removing {}", entry.path.display()))
            })?;
        self.entries.retain(|entry| entry.content != content);
        Ok(())
    }
}

/// The entries ignored for `reason`, in file name order
fn load_reason(ignore_dir: &Path, reason: &str) -> Result<Vec<IgnoredEntry>> {
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
    block_paths.retain(|block_path| block_path.is_file());
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
            })
        })
        .collect()
}

/// `{file_stem}.txt` in `reason_dir`, or `{file_stem}-2.txt`, `{file_stem}-3.txt`… when that's taken
fn unused_path(reason_dir: &Path, file_stem: &str) -> Result<PathBuf> {
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
        ignored_blocks.ignore("$ oc get nodes\n", "manual-command", "mod/terminal-001", None)?;
        ignored_blocks.ignore("  indented\n\nno trailing", "example-output", "mod/terminal-002", None)?;
        // Same content again moves it; a name taken by other content gets a suffix
        ignored_blocks.ignore("$ oc get nodes\n", "example-output", "mod/terminal-002", None)?;
        let mut reloaded_blocks = IgnoredBlocks::load(&ignore_dir)?;
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
