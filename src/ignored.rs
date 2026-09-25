//! The ignored list: doc blocks that don't come from the repo, by exact
//! content, grouped by reason.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

pub const REASONS: &[&str] = &["example-output", "manual-command", "no-repo-source"];

const HEADER: &str = "# Doc code blocks that don't come from this repo, by exact content, so no repo
# code needs to match them. Managed by asadoc (asadoc serve); see its README.
#   example-output   sample output shown to the reader
#   manual-command   a command too simple or doc-specific to track
#   no-repo-source   content with no counterpart in this repo
";

/// reason → contents, in the order they were added
#[derive(Default, Clone)]
pub struct Ignored(pub BTreeMap<String, Vec<String>>);

impl Ignored {
    pub fn load(path: &Path) -> Result<Ignored> {
        if !path.exists() {
            return Ok(Ignored::default());
        }
        let text = std::fs::read_to_string(path)?;
        let raw: Option<BTreeMap<String, Vec<String>>> =
            serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Ignored(raw.unwrap_or_default()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut out = format!("{HEADER}\n");
        for reason in REASONS {
            let Some(contents) = self.0.get(*reason).filter(|c| !c.is_empty()) else { continue };
            out.push_str(&format!("{reason}:\n"));
            for c in contents {
                out.push_str(&literal_item(c));
            }
        }
        // Write a new file and move it into place, so a failure never leaves it truncated
        let tmp = path.with_extension("yaml.tmp");
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn reason_of(&self, content: &str) -> Option<&str> {
        REASONS.iter().copied().find(|r| self.0.get(*r).is_some_and(|c| c.iter().any(|x| x == content)))
    }

    /// Ignores `content` for `reason`, replacing it (and `replacing`, if given) anywhere else
    pub fn ignore(&mut self, content: &str, reason: &str, replacing: Option<&str>) {
        self.remove(content);
        if let Some(r) = replacing {
            self.remove(r);
        }
        self.0.entry(reason.to_string()).or_default().push(content.to_string());
    }

    pub fn remove(&mut self, content: &str) {
        for list in self.0.values_mut() {
            list.retain(|c| c != content);
        }
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        REASONS.iter().flat_map(|r| self.0.get(*r).into_iter().flatten().map(move |c| (*r, c.as_str())))
    }
}

/// A YAML sequence item holding `content` as a literal block scalar
fn literal_item(content: &str) -> String {
    let body = content.strip_suffix('\n').unwrap_or(content);
    let trailing = content.len() - content.trim_end_matches('\n').len();
    let chomp = match trailing {
        0 => "-",
        1 => "",
        _ => "+",
    };
    // An explicit indentation indicator when the first line starts with a space
    let indicator = if body.starts_with(' ') { "2" } else { "" };
    let mut out = format!("  - |{indicator}{chomp}\n");
    for line in body.trim_end_matches('\n').split('\n') {
        out.push_str(if line.is_empty() { "\n" } else { "    " });
        if !line.is_empty() {
            out.push_str(line);
            out.push('\n');
        }
    }
    for _ in 1..trailing.max(1) {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_awkward_content() {
        let dir = std::env::temp_dir().join(format!("asadoc-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ignored.yaml");
        let mut ig = Ignored::default();
        for c in ["$ oc get nodes\n", "  indented\nnext\n", "no newline", "blank\n\ninside\n", "trailing\n\n", "quote: \"x\" # y\n"] {
            ig.ignore(c, "manual-command", None);
        }
        ig.save(&path).unwrap();
        let back = Ignored::load(&path).unwrap();
        assert_eq!(back.0["manual-command"], ig.0["manual-command"]);
        std::fs::remove_dir_all(dir).ok();
    }
}

