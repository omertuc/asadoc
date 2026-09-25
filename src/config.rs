//! `asadoc.yaml`: where the docs are, which of them to check, and where the
//! list of ignored doc blocks lives. Paths are relative to the config file.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const CONFIG_FILE: &str = "asadoc.yaml";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    /// The docs checkout
    docs: PathBuf,
    /// AsciiDoc assemblies (relative to the docs checkout) whose code blocks
    /// must come from this repo
    assemblies: Vec<String>,
    /// Doc blocks that don't come from this repo
    #[serde(default = "default_ignored")]
    ignored: PathBuf,
    /// Repo paths (prefixes) never scanned for markers
    #[serde(default)]
    exclude: Vec<String>,
    /// Base URLs for "source" links in the UI
    #[serde(default)]
    links: Links,
}

fn default_ignored() -> PathBuf {
    PathBuf::from("asadoc-ignored.yaml")
}

#[derive(Deserialize, Default, Clone, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Links {
    /// e.g. https://github.com/org/repo/blob/main/
    pub repo: Option<String>,
    /// e.g. https://github.com/org/docs/blob/main/
    pub docs: Option<String>,
}

pub struct Config {
    /// The git checkout the config file is in: where marked code is looked for
    pub repo_root: PathBuf,
    pub docs_root: PathBuf,
    pub assemblies: Vec<String>,
    pub ignored_file: PathBuf,
    pub exclude: Vec<String>,
    pub links: Links,
}

impl Config {
    /// Loads `path`, or the nearest `asadoc.yaml` from the working directory up.
    pub fn load(path: Option<&Path>, docs_override: Option<&Path>) -> Result<Config> {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => find_config()?,
        };
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let raw: RawConfig = serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let dir = path.parent().unwrap_or(Path::new(".")).canonicalize()?;
        let docs_root = match docs_override {
            Some(d) => d.to_path_buf(),
            None => dir.join(&raw.docs),
        };
        if !docs_root.is_dir() {
            bail!("docs checkout not found at {} (set `docs` in {} or pass --docs)", docs_root.display(), path.display());
        }
        Ok(Config {
            repo_root: git_root(&dir)?,
            docs_root: docs_root.canonicalize()?,
            assemblies: raw.assemblies,
            ignored_file: dir.join(raw.ignored),
            exclude: raw.exclude,
            links: raw.links,
        })
    }
}

fn find_config() -> Result<PathBuf> {
    let mut dir = std::env::current_dir()?;
    loop {
        let candidate = dir.join(CONFIG_FILE);
        if candidate.is_file() {
            return Ok(candidate);
        }
        if !dir.pop() {
            bail!("no {CONFIG_FILE} in this directory or any parent (or pass --config)");
        }
    }
}

fn git_root(dir: &Path) -> Result<PathBuf> {
    let out = Command::new("git").arg("rev-parse").arg("--show-toplevel").current_dir(dir).output()?;
    if !out.status.success() {
        bail!("{} isn't inside a git checkout", dir.display());
    }
    Ok(PathBuf::from(String::from_utf8(out.stdout)?.trim()))
}
