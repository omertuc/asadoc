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

pub(crate) const CONFIG_FILE_NAME: &str = "asadoc.yaml";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAsadocConfig {
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
    pub(crate) fn load(config_path: Option<&Path>, docs_override: Option<&Path>) -> Result<Self> {
        let config_path = match config_path {
            Some(given_path) => given_path.to_path_buf(),
            None => find_config().context("looking for the config file")?,
        };
        let config_text =
            fs::read_to_string(&config_path).with_context(|| format!("reading {}", config_path.display()))?;
        let raw_config: RawAsadocConfig =
            serde_yaml::from_str(&config_text).with_context(|| format!("parsing {}", config_path.display()))?;
        let config_dir = config_path
            .canonicalize()
            .with_context(|| format!("resolving {}", config_path.display()))?
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .to_path_buf();
        let docs = match docs_override {
            Some(docs_dir) => local_docs(docs_dir).context("opening the --docs checkout")?,
            None => configured_docs(&config_path, &config_dir, &raw_config.docs.asciidoc)?,
        };
        let links = Links {
            docs: raw_config.links.docs.or_else(|| docs.default_link_base()),
            ..raw_config.links
        };
        let repo_root = git_root(&config_dir).context("finding the repo the config file is in")?;
        let ignore_dir = config_dir.join(raw_config.ignore_dir);
        // The ignore directory holds doc content, not code to match
        let ignore_dir_exclude = ignore_dir
            .strip_prefix(&repo_root)
            .ok()
            .map(|relative_ignore_dir| format!("{}/", relative_ignore_dir.display()));
        Ok(Self {
            repo_root,
            docs,
            assemblies: raw_config.docs.asciidoc.assemblies,
            ignore_dir,
            exclude: raw_config.exclude.into_iter().chain(ignore_dir_exclude).collect(),
            links,
        })
    }
}

/// The docs the config file at `config_path` (in `config_dir`) points to
fn configured_docs(config_path: &Path, config_dir: &Path, raw_asciidoc: &RawAsciidoc) -> Result<Docs> {
    match (&raw_asciidoc.path, &raw_asciidoc.git, &raw_asciidoc.reference) {
        (Some(local_path), None, None) => {
            local_docs(&config_dir.join(local_path)).context("opening the configured docs checkout")
        }
        (None, Some(git_repo), Some(reference)) => git_docs(config_dir, git_repo, reference),
        (None, Some(_), None) => bail!(
            "{}: `docs.asciidoc.git` needs a `ref` (branch, tag or commit)",
            config_path.display()
        ),
        _ => bail!(
            "{}: `docs.asciidoc` needs either `path`, or `git` and `ref`",
            config_path.display()
        ),
    }
}

/// A local docs checkout at `docs_root`
fn local_docs(docs_root: &Path) -> Result<Docs> {
    if !docs_root.is_dir() {
        bail!("docs checkout not found at {}", docs_root.display());
    }
    let docs_root = docs_root
        .canonicalize()
        .with_context(|| format!("resolving the docs checkout {}", docs_root.display()))?;
    Ok(Docs::Local(docs_root))
}

/// The docs git repository `git_repo` (a URL, or a path relative to `config_dir`) at `reference`
fn git_docs(config_dir: &Path, git_repo: &str, reference: &str) -> Result<Docs> {
    let local_repo = config_dir.join(git_repo);
    let repo_url = if local_repo.is_dir() {
        local_repo
            .canonicalize()
            .with_context(|| format!("resolving the docs repository {git_repo}"))?
            .display()
            .to_string()
    } else {
        git_repo.to_owned()
    };
    let git_docs = GitDocs::open(&repo_url, reference).with_context(|| format!("opening {repo_url} at {reference}"))?;
    Ok(Docs::Git(git_docs))
}

fn find_config() -> Result<PathBuf> {
    let working_dir = env::current_dir().context("getting the working directory")?;
    working_dir
        .ancestors()
        .map(|ancestor_dir| ancestor_dir.join(CONFIG_FILE_NAME))
        .find(|candidate_path| candidate_path.is_file())
        .with_context(|| format!("no {CONFIG_FILE_NAME} in this directory or any parent (or pass --config)"))
}

fn git_root(inside_dir: &Path) -> Result<PathBuf> {
    let rev_parse_output = Command::new("git")
        .arg("rev-parse")
        .arg("--show-toplevel")
        .current_dir(inside_dir)
        .output()
        .context("running git rev-parse")?;
    if !rev_parse_output.status.success() {
        bail!("{} isn't inside a git checkout", inside_dir.display());
    }
    let toplevel = String::from_utf8(rev_parse_output.stdout).context("reading git's output")?;
    Ok(PathBuf::from(toplevel.trim()))
}
