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

pub(crate) const REASONS: &[&str] = &["example-output", "manual-command", "no-repo-source"];

#[derive(Clone)]
pub(crate) struct Entry {
    pub reason: String,
    pub path: PathBuf,
    pub content: String,
}

pub(crate) struct Ignored {
    dir: PathBuf,
    pub entries: Vec<Entry>,
}

impl Ignored {
    pub(crate) fn load(dir: &Path) -> Result<Self> {
        let entries = REASONS
            .iter()
            .map(|reason| load_reason(dir, reason).with_context(|| format!("loading the blocks ignored as {reason}")))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        Ok(Self {
            dir: dir.to_path_buf(),
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
    /// replacing it (and `replacing`, if given) wherever else it's ignored
    pub(crate) fn ignore(
        &mut self,
        content: &str,
        reason: &str,
        block_ref: &str,
        replacing: Option<&str>,
    ) -> Result<()> {
        self.remove(content)
            .context("removing where the content was ignored before")?;
        if let Some(replaced) = replacing {
            self.remove(replaced).context("removing the content this replaces")?;
        }
        let reason_dir = self.dir.join(reason);
        fs::create_dir_all(&reason_dir).with_context(|| format!("creating {}", reason_dir.display()))?;
        let path = unused_path(&reason_dir, &block_ref.replace('/', "--"))?;
        fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
        self.entries.push(Entry {
            reason: reason.to_owned(),
            path,
            content: content.to_owned(),
        });
        Ok(())
    }

    /// Stops ignoring `content`: removes every file holding it
    pub(crate) fn remove(&mut self, content: &str) -> Result<()> {
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
fn load_reason(dir: &Path, reason: &str) -> Result<Vec<Entry>> {
    let reason_dir = dir.join(reason);
    let listing = match fs::read_dir(&reason_dir) {
        Ok(listing) => listing,
        // No block ignored for this reason
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("listing {}", reason_dir.display())),
    };
    let mut paths = listing
        .map(|dir_entry| {
            dir_entry
                .map(|dir_entry| dir_entry.path())
                .with_context(|| format!("listing {}", reason_dir.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    paths.retain(|path| path.is_file());
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let content = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            Ok(Entry {
                reason: reason.to_owned(),
                path,
                content,
            })
        })
        .collect()
}

/// `{base}.txt` in `dir`, or `{base}-2.txt`, `{base}-3.txt`… when that's taken
fn unused_path(dir: &Path, base: &str) -> Result<PathBuf> {
    iter::once(dir.join(format!("{base}.txt")))
        .chain((2..=u64::MAX).map(|suffix| dir.join(format!("{base}-{suffix}.txt"))))
        .find(|path| !path.exists())
        .with_context(|| format!("no unused file name for {base} in {}", dir.display()))
}

#[cfg(test)]
#[allow(clippy::panic_in_result_fn, reason = "assertions are how tests fail")]
mod tests {
    use super::*;
    use std::{env, process};

    #[test]
    fn ignores_and_unignores_by_content() -> Result<()> {
        let dir = env::temp_dir().join(format!("asadoc-ignore-test-{}", process::id()));
        if dir.exists() {
            fs::remove_dir_all(&dir).context("clearing the test directory")?;
        }
        let mut ignored = Ignored::load(&dir)?;
        ignored.ignore("$ oc get nodes\n", "manual-command", "mod/terminal-001", None)?;
        ignored.ignore("  indented\n\nno trailing", "example-output", "mod/terminal-002", None)?;
        // Same content again moves it; a name taken by other content gets a suffix
        ignored.ignore("$ oc get nodes\n", "example-output", "mod/terminal-002", None)?;
        let mut reloaded = Ignored::load(&dir)?;
        assert_eq!(reloaded.reason_of("$ oc get nodes\n"), Some("example-output"));
        assert_eq!(reloaded.reason_of("  indented\n\nno trailing"), Some("example-output"));
        assert!(dir.join("example-output/mod--terminal-002-2.txt").exists());
        reloaded.remove("$ oc get nodes\n")?;
        assert_eq!(Ignored::load(&dir)?.entries.len(), 1);
        fs::remove_dir_all(dir).context("cleaning up the test directory")?;
        Ok(())
    }
}
