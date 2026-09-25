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
use std::path::{Path, PathBuf};

pub const REASONS: &[&str] = &["example-output", "manual-command", "no-repo-source"];

#[derive(Clone)]
pub struct Entry {
    pub reason: String,
    pub path: PathBuf,
    pub content: String,
}

pub struct Ignored {
    dir: PathBuf,
    pub entries: Vec<Entry>,
}

impl Ignored {
    pub fn load(dir: &Path) -> Result<Ignored> {
        let mut entries = Vec::new();
        for reason in REASONS {
            let sub = dir.join(reason);
            let Ok(read) = std::fs::read_dir(&sub) else { continue };
            let mut paths: Vec<PathBuf> = read.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_file()).collect();
            paths.sort();
            for path in paths {
                let content = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
                entries.push(Entry { reason: reason.to_string(), path, content });
            }
        }
        Ok(Ignored { dir: dir.to_path_buf(), entries })
    }

    pub fn reason_of(&self, content: &str) -> Option<&str> {
        self.entries.iter().find(|e| e.content == content).map(|e| e.reason.as_str())
    }

    /// Ignores `content` for `reason`, in a file named after `block_ref`,
    /// replacing it (and `replacing`, if given) wherever else it's ignored
    pub fn ignore(&mut self, content: &str, reason: &str, block_ref: &str, replacing: Option<&str>) -> Result<()> {
        self.remove(content)?;
        if let Some(r) = replacing {
            self.remove(r)?;
        }
        let sub = self.dir.join(reason);
        std::fs::create_dir_all(&sub)?;
        let base = block_ref.replace('/', "--");
        let mut path = sub.join(format!("{base}.txt"));
        let mut n = 2;
        while path.exists() {
            path = sub.join(format!("{base}-{n}.txt"));
            n += 1;
        }
        std::fs::write(&path, content)?;
        self.entries.push(Entry { reason: reason.to_string(), path, content: content.to_string() });
        Ok(())
    }

    /// Stops ignoring `content`: removes every file holding it
    pub fn remove(&mut self, content: &str) -> Result<()> {
        for e in self.entries.iter().filter(|e| e.content == content) {
            std::fs::remove_file(&e.path).with_context(|| format!("removing {}", e.path.display()))?;
        }
        self.entries.retain(|e| e.content != content);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_and_unignores_by_content() {
        let dir = std::env::temp_dir().join(format!("asadoc-ignore-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut ig = Ignored::load(&dir).unwrap();
        ig.ignore("$ oc get nodes\n", "manual-command", "mod/terminal-001", None).unwrap();
        ig.ignore("  indented\n\nno trailing", "example-output", "mod/terminal-002", None).unwrap();
        // Same content again moves it; a name taken by other content gets a suffix
        ig.ignore("$ oc get nodes\n", "example-output", "mod/terminal-002", None).unwrap();
        let back = Ignored::load(&dir).unwrap();
        assert_eq!(back.reason_of("$ oc get nodes\n"), Some("example-output"));
        assert_eq!(back.reason_of("  indented\n\nno trailing"), Some("example-output"));
        assert!(dir.join("example-output/mod--terminal-002-2.txt").exists());
        let mut back = back;
        back.remove("$ oc get nodes\n").unwrap();
        assert_eq!(Ignored::load(&dir).unwrap().entries.len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }
}
