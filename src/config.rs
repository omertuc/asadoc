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
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) const CONFIG_FILE: &str = "asadoc.yaml";

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

#[derive(Deserialize, Default, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Links {
    /// e.g. <https://github.com/org/repo/blob/main>/
    pub repo: Option<String>,
    /// e.g. <https://github.com/org/docs/blob/main>/ (default for GitHub docs
    /// repos: the fetched commit)
    pub docs: Option<String>,
}

pub(crate) struct AsadocConfig {
    /// The git checkout the config file is in: where marked code is looked for
    pub repo_root: PathBuf,
    pub docs: Docs,
    pub assemblies: Vec<String>,
    pub ignore_dir: PathBuf,
    pub exclude: Vec<String>,
    pub links: Links,
}

impl AsadocConfig {
    /// Loads `path`, or the nearest `asadoc.yaml` from the working directory up.
    pub(crate) fn load(path: Option<&Path>, docs_override: Option<&Path>) -> Result<Self> {
        let path = match path {
            Some(path) => path.to_path_buf(),
            None => find_config().context("looking for the config file")?,
        };
        let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let raw: RawConfig = serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let dir = path
            .canonicalize()
            .with_context(|| format!("resolving {}", path.display()))?
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .to_path_buf();
        let docs = match docs_override {
            Some(docs_dir) => local_docs(docs_dir).context("opening the --docs checkout")?,
            None => configured_docs(&path, &dir, &raw.docs.asciidoc)?,
        };
        let links = Links {
            docs: raw.links.docs.or_else(|| docs.default_link_base()),
            ..raw.links
        };
        let repo_root = git_root(&dir).context("finding the repo the config file is in")?;
        let ignore_dir = dir.join(raw.ignore_dir);
        // The ignore directory holds doc content, not code to match
        let ignore_exclude = ignore_dir
            .strip_prefix(&repo_root)
            .ok()
            .map(|relative| format!("{}/", relative.display()));
        Ok(Self {
            repo_root,
            docs,
            assemblies: raw.docs.asciidoc.assemblies,
            ignore_dir,
            exclude: raw.exclude.into_iter().chain(ignore_exclude).collect(),
            links,
        })
    }
}

/// The docs the config file at `path` (in `dir`) points to
fn configured_docs(path: &Path, dir: &Path, asciidoc: &RawAsciidoc) -> Result<Docs> {
    match (&asciidoc.path, &asciidoc.git, &asciidoc.reference) {
        (Some(local_path), None, None) => {
            local_docs(&dir.join(local_path)).context("opening the configured docs checkout")
        }
        (None, Some(git), Some(reference)) => git_docs(dir, git, reference),
        (None, Some(_), None) => bail!(
            "{}: `docs.asciidoc.git` needs a `ref` (branch, tag or commit)",
            path.display()
        ),
        _ => bail!(
            "{}: `docs.asciidoc` needs either `path`, or `git` and `ref`",
            path.display()
        ),
    }
}

/// A local docs checkout at `root`
fn local_docs(root: &Path) -> Result<Docs> {
    if !root.is_dir() {
        bail!("docs checkout not found at {}", root.display());
    }
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving the docs checkout {}", root.display()))?;
    Ok(Docs::Local(root))
}

/// The docs git repository `git` (a URL, or a path relative to `dir`) at `reference`
fn git_docs(dir: &Path, git: &str, reference: &str) -> Result<Docs> {
    let local_repo = dir.join(git);
    let url = if local_repo.is_dir() {
        local_repo
            .canonicalize()
            .with_context(|| format!("resolving the docs repository {git}"))?
            .display()
            .to_string()
    } else {
        git.to_owned()
    };
    let git_docs = GitDocs::open(&url, reference).with_context(|| format!("opening {url} at {reference}"))?;
    Ok(Docs::Git(git_docs))
}

fn find_config() -> Result<PathBuf> {
    let working_dir = env::current_dir().context("getting the working directory")?;
    working_dir
        .ancestors()
        .map(|dir| dir.join(CONFIG_FILE))
        .find(|candidate| candidate.is_file())
        .with_context(|| format!("no {CONFIG_FILE} in this directory or any parent (or pass --config)"))
}

fn git_root(dir: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("--show-toplevel")
        .current_dir(dir)
        .output()
        .context("running git rev-parse")?;
    if !output.status.success() {
        bail!("{} isn't inside a git checkout", dir.display());
    }
    let root = String::from_utf8(output.stdout).context("reading git's output")?;
    Ok(PathBuf::from(root.trim()))
}
