//! `asadoc.yaml`: where the docs are, which of them to check, and where the
//! directory of ignored doc blocks is. Paths are relative to the config file.
//!
//! ```yaml
//! docs:
//!   asciidoc:                  # the docs' format (the only one, for now)
//!     git: https://github.com/openshift/openshift-docs
//!     ref: main                # branch, tag or commit
//!     # or, instead of git and ref, a local checkout: path: ../openshift-docs
//!     assemblies: [...]
//! ```

use crate::source::{Docs, GitDocs};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const CONFIG_FILE: &str = "asadoc.yaml";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    docs: RawDocs,
    /// Directory of doc blocks that don't come from this repo
    #[serde(default = "default_ignore_dir")]
    ignore_dir: PathBuf,
    /// Repo paths (prefixes) never scanned for markers
    #[serde(default)]
    exclude: Vec<String>,
    /// Base URLs for "source" links in the UI
    #[serde(default)]
    links: Links,
}

/// The docs, keyed by format
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDocs {
    asciidoc: RawAsciidoc,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAsciidoc {
    /// A local docs checkout
    path: Option<PathBuf>,
    /// A docs git repository (URL, or local path), read at `ref`
    git: Option<String>,
    #[serde(rename = "ref")]
    reference: Option<String>,
    /// Assemblies (relative to the docs root) whose code blocks must come
    /// from this repo
    assemblies: Vec<String>,
}

fn default_ignore_dir() -> PathBuf {
    PathBuf::from(".asadoc-ignore")
}

#[derive(Deserialize, Default, Clone, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Links {
    /// e.g. https://github.com/org/repo/blob/main/
    pub repo: Option<String>,
    /// e.g. https://github.com/org/docs/blob/main/ (default for GitHub docs
    /// repos: the fetched commit)
    pub docs: Option<String>,
}

pub struct Config {
    /// The git checkout the config file is in: where marked code is looked for
    pub repo_root: PathBuf,
    pub docs: Docs,
    pub assemblies: Vec<String>,
    pub ignore_dir: PathBuf,
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
        let dir = path.canonicalize()?.parent().unwrap_or(Path::new("/")).to_path_buf();
        let asciidoc = raw.docs.asciidoc;
        let local = |root: PathBuf| -> Result<Docs> {
            if !root.is_dir() {
                bail!("docs checkout not found at {}", root.display());
            }
            Ok(Docs::Local(root.canonicalize()?))
        };
        let docs = match (docs_override, asciidoc.path, asciidoc.git, asciidoc.reference) {
            (Some(d), ..) => local(d.to_path_buf())?,
            (None, Some(p), None, None) => local(dir.join(p))?,
            (None, None, Some(git), Some(reference)) => {
                // A local repository, relative to the config file like other paths
                let url = if dir.join(&git).is_dir() { dir.join(&git).canonicalize()?.display().to_string() } else { git };
                Docs::Git(GitDocs::open(&url, &reference)?)
            }
            (None, None, Some(_), None) => bail!("{}: `docs.asciidoc.git` needs a `ref` (branch, tag or commit)", path.display()),
            _ => bail!("{}: `docs.asciidoc` needs either `path`, or `git` and `ref`", path.display()),
        };
        let mut links = raw.links;
        if links.docs.is_none() {
            links.docs = docs.default_link_base();
        }
        let repo_root = git_root(&dir)?;
        let ignore_dir = dir.join(raw.ignore_dir);
        // The ignore directory holds doc content, not code to match
        let mut exclude = raw.exclude;
        if let Ok(rel) = ignore_dir.strip_prefix(&repo_root) {
            exclude.push(format!("{}/", rel.display()));
        }
        Ok(Config {
            repo_root,
            docs,
            assemblies: asciidoc.assemblies,
            ignore_dir,
            exclude,
            links,
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
