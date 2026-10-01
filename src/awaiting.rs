//! The awaiting-doc-fix directory (`.asadoc/awaiting-doc-fix/` by default):
//! doc blocks that are out of date because the code changed, and that wait for
//! the docs to catch up. Each subdirectory is one doc fix, with a `README.md`
//! saying what the docs need to change (and where that change is tracked), and
//! one file per doc block content awaiting it, verbatim:
//!
//! ```text
//! .asadoc/awaiting-doc-fix/
//!   pull-secret-type/README.md
//!   pull-secret-type/nw-dpf-creating-hcp-secrets--terminal-002.txt
//! ```
//!
//! A block awaiting a doc fix doesn't fail `asadoc check`, but is still
//! reported, with how it differs from its closest code. An entry only covers
//! the content the block had: once the docs change, the block is checked
//! against the code again, and the entry is reported as no longer needed.

use crate::ignored::{self, DESCRIPTION_FILE_NAME};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

/// A change the docs need: a subdirectory of the awaiting-doc-fix directory
#[derive(Clone, Debug, Serialize)]
pub(crate) struct DocFix {
    pub name: String,
    /// What the docs need to change, from its `README.md` (empty without one)
    pub description: String,
}

#[derive(Clone, Debug)]
pub(crate) struct AwaitingEntry {
    /// The doc fix it awaits
    pub fix: String,
    pub path: PathBuf,
    pub content: String,
}

pub(crate) struct AwaitingDocFixes {
    dir: PathBuf,
    pub fixes: Vec<DocFix>,
    pub entries: Vec<AwaitingEntry>,
}

impl AwaitingDocFixes {
    pub(crate) fn load(dir: &Path) -> Result<Self> {
        let fixes: Vec<DocFix> = ignored::load_reason_dirs(dir)
            .context("listing the doc fixes")?
            .into_iter()
            .map(|reason| DocFix {
                name: reason.name,
                description: reason.description,
            })
            .collect();
        let entries = fixes
            .iter()
            .map(|fix| {
                ignored::load_reason(dir, &fix.name)
                    .with_context(|| format!("loading the blocks awaiting the doc fix {}", fix.name))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .map(|entry| AwaitingEntry {
                fix: entry.reason,
                path: entry.path,
                content: entry.content,
            })
            .collect();
        Ok(Self {
            dir: dir.to_path_buf(),
            fixes,
            entries,
        })
    }

    /// The doc fix content awaits, if any
    pub(crate) fn fix_of(&self, content: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|entry| entry.content == content)
            .map(|entry| entry.fix.as_str())
    }

    /// Adds the doc fix `name`, whose `README.md` says `description`
    pub(crate) fn add_fix(&mut self, name: &str, description: &str) -> Result<()> {
        if !ignored::is_valid_reason_name(name) {
            bail!("{name:?} can't be a doc fix's name: use lowercase letters, digits and dashes");
        }
        if self.fixes.iter().any(|fix| fix.name == name) {
            bail!("the doc fix {name} already exists");
        }
        let description = description.trim();
        if description.is_empty() {
            bail!("the doc fix {name} needs a description of what the docs need to change");
        }
        let fix_dir = self.dir.join(name);
        fs::create_dir_all(&fix_dir).with_context(|| format!("creating {}", fix_dir.display()))?;
        let description_path = fix_dir.join(DESCRIPTION_FILE_NAME);
        fs::write(&description_path, format!("{description}\n"))
            .with_context(|| format!("writing {}", description_path.display()))?;
        self.fixes.push(DocFix {
            name: name.to_owned(),
            description: description.to_owned(),
        });
        Ok(())
    }

    /// Makes `content` await the doc fix `fix`, in a file named after
    /// `block_ref`, instead of any other fix it awaited; returns the file
    pub(crate) fn await_fix(&mut self, content: &str, fix: &str, block_ref: &str) -> Result<PathBuf> {
        if !self.fixes.iter().any(|known_fix| known_fix.name == fix) {
            bail!("no doc fix {fix:?} in {}", self.dir.display());
        }
        if let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.content == content && entry.fix == fix)
        {
            return Ok(entry.path.clone());
        }
        self.stop_awaiting(content)
            .context("removing where the content awaited another doc fix")?;
        let fix_dir = self.dir.join(fix);
        let block_path = ignored::unused_path(&fix_dir, &block_ref.replace(['/', ':'], "--"))?;
        fs::write(&block_path, content).with_context(|| format!("writing {}", block_path.display()))?;
        self.entries.push(AwaitingEntry {
            fix: fix.to_owned(),
            path: block_path.clone(),
            content: content.to_owned(),
        });
        Ok(block_path)
    }

    /// Stops `content` awaiting a doc fix: removes every file holding it, and
    /// the directory of a fix nothing awaits anymore
    pub(crate) fn stop_awaiting(&mut self, content: &str) -> Result<()> {
        let mut emptied_fixes = vec![];
        for entry in self.entries.iter().filter(|entry| entry.content == content) {
            fs::remove_file(&entry.path).with_context(|| format!("removing {}", entry.path.display()))?;
            emptied_fixes.push(entry.fix.clone());
        }
        self.entries.retain(|entry| entry.content != content);
        emptied_fixes.retain(|fix| !self.entries.iter().any(|entry| entry.fix == *fix));
        for fix in &emptied_fixes {
            let fix_dir = self.dir.join(fix);
            fs::remove_dir_all(&fix_dir).with_context(|| format!("removing {}", fix_dir.display()))?;
        }
        self.fixes.retain(|known_fix| !emptied_fixes.contains(&known_fix.name));
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::panic_in_result_fn, reason = "assertions are how tests fail")]
mod tests {
    use super::*;
    use std::{env, process};

    #[test]
    fn awaits_and_stops_awaiting_by_content() -> Result<()> {
        let dir = env::temp_dir().join(format!("asadoc-awaiting-test-{}", process::id()));
        if dir.exists() {
            fs::remove_dir_all(&dir).context("clearing the test directory")?;
        }
        let mut awaiting = AwaitingDocFixes::load(&dir)?;
        assert!(awaiting.await_fix("x", "unheard-of", "mod/terminal-000").is_err());
        assert!(awaiting.add_fix("Bad name", "Whatever").is_err());
        assert!(awaiting.add_fix("no-description", "  \n").is_err());
        awaiting.add_fix("secret-type", "  The docs should use dockerconfigjson\n")?;
        awaiting.add_fix("flavor", "The DPUFlavor needs more lines")?;
        assert!(awaiting.add_fix("flavor", "Again").is_err());
        awaiting.await_fix("$ oc create secret\n", "secret-type", "mod/terminal-001")?;
        awaiting.await_fix("kind: DPUFlavor\n", "flavor", "mod/yaml-001")?;
        // Awaiting another fix moves it, emptying (and so removing) the first
        awaiting.await_fix("$ oc create secret\n", "flavor", "mod/terminal-001")?;
        assert!(!dir.join("secret-type").exists());
        let reloaded = AwaitingDocFixes::load(&dir)?;
        assert_eq!(reloaded.fix_of("$ oc create secret\n"), Some("flavor"));
        assert_eq!(reloaded.fix_of("kind: DPUFlavor\n"), Some("flavor"));
        assert_eq!(reloaded.fix_of("other"), None);
        let fixes: Vec<_> = reloaded
            .fixes
            .iter()
            .map(|fix| (fix.name.as_str(), fix.description.as_str()))
            .collect();
        assert_eq!(fixes, [("flavor", "The DPUFlavor needs more lines")]);
        let mut reloaded = reloaded;
        reloaded.stop_awaiting("kind: DPUFlavor\n")?;
        assert!(dir.join("flavor/README.md").exists());
        reloaded.stop_awaiting("$ oc create secret\n")?;
        assert!(!dir.join("flavor").exists());
        assert!(AwaitingDocFixes::load(&dir)?.entries.is_empty());
        fs::remove_dir_all(dir).context("cleaning up the test directory")?;
        Ok(())
    }
}
