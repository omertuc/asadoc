//! `.asadoc/config.toml`: where the docs are, which of them to check, and where
//! the directory of ignored doc blocks is. Paths are relative to the config
//! file's directory (`.asadoc/`).
//!
//! ```toml
//! [[docs]]                     # one per docs source
//! name = "openshift"           # names its blocks; needed when there are several
//! git = "https://github.com/openshift/openshift-docs"
//! ref = "main"                 # branch, tag or commit
//! # or, instead of git and ref, a local checkout: path = "../../openshift-docs"
//!
//! [docs.asciidoc]              # the docs' format (the only one, for now)
//! assemblies = [...]
//!
//! [[code]]                     # other repos with marked code, besides this one
//! name = "installer"           # prefixes its files' names
//! git = "https://github.com/org/installer"
//! ref = "main"                 # or path = "../../installer"
//! ```

use crate::links::ExternalLinks;
use crate::source::{GitTree, Tree};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::iter;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

pub(crate) const CONFIG_FILE_NAME: &str = ".asadoc/config.toml";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAsadocConfig {
    docs: Vec<RawDocs>,
    /// Other repos with marked code
    #[serde(default)]
    code: Vec<RawCode>,
    /// Directory of doc blocks that don't come from this repo
    #[serde(default = "default_ignore_dir")]
    ignore_dir: PathBuf,
    /// Repo paths (prefixes) never scanned for markers
    #[serde(default)]
    exclude: Vec<String>,
    /// This repo's; see `RawDocs::external_link_format`
    #[serde(rename = "external-link-format")]
    external_link_format: Option<String>,
}

/// A docs source: where the docs are, and their format's settings (keyed by format)
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDocs {
    /// Names the source's blocks and assemblies; needed when there are several sources
    name: Option<String>,
    /// A local docs checkout
    path: Option<PathBuf>,
    /// A docs git repository (URL, or local path), read at `ref`
    git: Option<String>,
    #[serde(rename = "ref")]
    reference: Option<String>,
    /// How the review UI links to its files on the web (see `links`): a kind
    /// of host, a template, or `none`; detected for public hosts when omitted
    #[serde(rename = "external-link-format")]
    external_link_format: Option<String>,
    asciidoc: RawAsciidoc,
}

/// Another repo with marked code
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCode {
    /// Prefixes the names of its files, `<name>:<path>`
    name: String,
    /// A local checkout
    path: Option<PathBuf>,
    /// A git repository (URL, or local path), read at `ref`
    git: Option<String>,
    #[serde(rename = "ref")]
    reference: Option<String>,
    /// See `RawDocs::external_link_format`
    #[serde(rename = "external-link-format")]
    external_link_format: Option<String>,
}

/// What asadoc reads of another code repo's own config: what it ignores and
/// excludes. The rest is its business.
#[derive(Deserialize)]
struct RawOtherConfig {
    #[serde(default = "default_ignore_dir")]
    ignore_dir: PathBuf,
    #[serde(default)]
    exclude: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAsciidoc {
    /// Assemblies (relative to the docs root) whose code blocks must come
    /// from this repo
    assemblies: Vec<String>,
}

fn default_ignore_dir() -> PathBuf {
    PathBuf::from("ignore")
}

/// One docs source
pub(crate) struct Docs {
    /// Set when there are several sources: the prefix of its block references
    /// and assembly ids, `<name>:`
    pub name: Option<String>,
    pub source: Tree,
    pub assemblies: Vec<String>,
    /// How to link to the docs' files on the web
    pub links: Option<ExternalLinks>,
}

impl Docs {
    /// A block reference or assembly id, as it's known among all the sources
    pub(crate) fn qualify(&self, local_name: &str) -> String {
        match &self.name {
            Some(name) => format!("{name}:{local_name}"),
            None => local_name.to_owned(),
        }
    }

    /// For people: the source's name (when it has one) and where the docs are
    pub(crate) fn describe(&self) -> String {
        match &self.name {
            Some(name) => format!("{name}: {}", self.source.describe()),
            None => self.source.describe(),
        }
    }
}

/// A repo where marked code is looked for
pub(crate) struct CodeSource {
    /// None for this repo (the one the config file is in); its files' names
    /// are prefixed with it, `<name>:<path>`
    pub name: Option<String>,
    pub tree: Tree,
    /// Paths (prefixes) never scanned for markers
    pub exclude: Vec<String>,
    /// For another repo: its ignore directory, relative to its root (this
    /// repo's is `AsadocConfig::ignore_dir`)
    pub ignore_dir: Option<String>,
    /// How to link to its files on the web
    pub links: Option<ExternalLinks>,
}

impl CodeSource {
    /// A file's name among all the code sources
    pub(crate) fn qualify(&self, path: &str) -> String {
        match &self.name {
            Some(name) => format!("{name}:{path}"),
            None => path.to_owned(),
        }
    }

    /// For people: the source's name (when it has one) and where the code is
    pub(crate) fn describe(&self) -> String {
        match &self.name {
            Some(name) => format!("{name}: {}", self.tree.describe()),
            None => self.tree.describe(),
        }
    }
}

pub(crate) struct AsadocConfig {
    pub docs: Vec<Docs>,
    /// This repo, then each `[[code]]`
    pub code: Vec<CodeSource>,
    /// This repo's ignore directory
    pub ignore_dir: PathBuf,
}

impl AsadocConfig {
    /// Loads `path`, or the nearest `.asadoc/config.toml` from the working
    /// directory up. `docs_overrides` and `code_overrides` are `--docs` and
    /// `--code` arguments: local checkouts to read instead of the configured ones.
    pub(crate) fn load(
        config_path: Option<&Path>,
        docs_overrides: &[String],
        code_overrides: &[String],
    ) -> Result<Self> {
        let config_path = match config_path {
            Some(given_path) => given_path.to_path_buf(),
            None => find_config().context("looking for the config file")?,
        };
        let config_text =
            fs::read_to_string(&config_path).with_context(|| format!("reading {}", config_path.display()))?;
        let raw_config: RawAsadocConfig =
            toml::from_str(&config_text).with_context(|| format!("parsing {}", config_path.display()))?;
        let config_dir = config_path
            .canonicalize()
            .with_context(|| format!("resolving {}", config_path.display()))?
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .to_path_buf();
        check_docs_names(&raw_config.docs).with_context(|| format!("in {}", config_path.display()))?;
        let override_dirs = override_dirs(&raw_config.docs, docs_overrides)?;
        let is_several = raw_config.docs.len() > 1;
        let docs = raw_config
            .docs
            .into_iter()
            .zip(override_dirs)
            .map(|(raw_docs, override_dir)| {
                let source = match override_dir {
                    Some(docs_dir) => local_tree(&docs_dir, "docs").context("opening the --docs checkout")?,
                    None => configured_docs(&config_path, &config_dir, &raw_docs)?,
                };
                Ok(Docs {
                    links: external_links(raw_docs.external_link_format.as_deref(), &source)
                        .with_context(|| format!("in {}", config_path.display()))?,
                    name: raw_docs.name.filter(|_| is_several),
                    source,
                    assemblies: raw_docs.asciidoc.assemblies,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let repo_root = git_root(&config_dir).context("finding the repo the config file is in")?;
        let ignore_dir = config_dir.join(raw_config.ignore_dir);
        // The ignore directory holds doc content, not code to match
        let ignore_dir_exclude = ignore_dir
            .strip_prefix(&repo_root)
            .ok()
            .map(|relative_ignore_dir| format!("{}/", relative_ignore_dir.display()));
        let this_repo_tree = Tree::Local(repo_root);
        let this_repo = CodeSource {
            name: None,
            links: external_links(raw_config.external_link_format.as_deref(), &this_repo_tree)
                .with_context(|| format!("in {}", config_path.display()))?,
            tree: this_repo_tree,
            exclude: raw_config.exclude.into_iter().chain(ignore_dir_exclude).collect(),
            ignore_dir: None,
        };
        let other_code = other_code_sources(&config_path, &config_dir, raw_config.code, code_overrides)?;
        Ok(Self {
            docs,
            code: iter::once(this_repo).chain(other_code).collect(),
            ignore_dir,
        })
    }

    /// The code source a file name (as `CodeSource::qualify` makes it) is in,
    /// and the file's path in it
    pub(crate) fn code_file<'a>(&self, file: &'a str) -> Result<(&CodeSource, &'a str)> {
        let named_source = file.split_once(':').and_then(|(name, path)| {
            self.code
                .iter()
                .find(|code_source| code_source.name.as_deref() == Some(name))
                .map(|code_source| (code_source, path))
        });
        match named_source {
            Some(named_source) => Ok(named_source),
            None => Ok((self.code.first().context("no code sources")?, file)),
        }
    }

    /// Where on disk to change a file (by its name among all the code sources)
    pub(crate) fn writable_code_file(&self, file: &str) -> Result<PathBuf> {
        let (code_source, path) = self.code_file(file)?;
        let root = code_source.tree.local_root().with_context(|| {
            format!(
                "{file} is in {}, read from git; change it in that repo",
                code_source.describe()
            )
        })?;
        Ok(root.join(path))
    }
}

/// The `[[code]]` sources, each with what its own config (if any) says it
/// ignores and excludes
fn other_code_sources(
    config_path: &Path,
    config_dir: &Path,
    all_raw_code: Vec<RawCode>,
    code_overrides: &[String],
) -> Result<Vec<CodeSource>> {
    check_code_names(&all_raw_code).with_context(|| format!("in {}", config_path.display()))?;
    let override_dirs = code_override_dirs(&all_raw_code, code_overrides)?;
    all_raw_code
        .into_iter()
        .zip(override_dirs)
        .map(|(raw_code, override_dir)| {
            let which_code = format!("code {:?}", raw_code.name);
            let tree = match (override_dir, &raw_code.path, &raw_code.git, &raw_code.reference) {
                (Some(code_dir), ..) => local_tree(&code_dir, "code").context("opening the --code checkout")?,
                (None, Some(local_path), None, None) => local_tree(&config_dir.join(local_path), "code")
                    .with_context(|| format!("opening the checkout of {which_code}"))?,
                (None, None, Some(git_repo), Some(reference)) => git_tree(config_dir, git_repo, reference)?,
                (None, None, Some(_), None) => bail!(
                    "{}: {which_code}: `git` needs a `ref` (branch, tag or commit)",
                    config_path.display()
                ),
                _ => bail!(
                    "{}: {which_code} needs either `path`, or `git` and `ref`",
                    config_path.display()
                ),
            };
            let other_config = other_config(&tree).with_context(|| format!("reading the config of {which_code}"))?;
            let ignore_dir = normalized_relative_path(&Path::new(".asadoc").join(&other_config.ignore_dir));
            Ok(CodeSource {
                links: external_links(raw_code.external_link_format.as_deref(), &tree)
                    .with_context(|| format!("in {}: {which_code}", config_path.display()))?,
                exclude: other_config
                    .exclude
                    .into_iter()
                    .chain(ignore_dir.as_ref().map(|ignore_dir| format!("{ignore_dir}/")))
                    .collect(),
                ignore_dir,
                name: Some(raw_code.name),
                tree,
            })
        })
        .collect()
}

/// How to link to a docs or code source's files, per its `external-link-format`
fn external_links(setting: Option<&str>, tree: &Tree) -> Result<Option<ExternalLinks>> {
    ExternalLinks::new(setting, tree.remote()?.as_deref(), tree.commit()?.as_deref())
}

/// What another code repo's own config says (the defaults when it has none)
fn other_config(tree: &Tree) -> Result<RawOtherConfig> {
    let Some(config_text) = tree.read(CONFIG_FILE_NAME)? else {
        return Ok(RawOtherConfig {
            ignore_dir: default_ignore_dir(),
            exclude: vec![],
        });
    };
    toml::from_str(&config_text).with_context(|| format!("parsing its {CONFIG_FILE_NAME}"))
}

/// `path` without `.` and `..`, as a `/`-separated string; None when it leaves
/// the directory it's relative to
fn normalized_relative_path(path: &Path) -> Option<String> {
    let mut components: Vec<&str> = vec![];
    for component in path.components() {
        match component {
            Component::Normal(name) => components.push(name.to_str()?),
            Component::CurDir => {}
            Component::ParentDir => {
                components.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(components.join("/"))
}

/// Each `[[code]]` needs a distinct name
fn check_code_names(all_raw_code: &[RawCode]) -> Result<()> {
    let mut seen_names = HashSet::new();
    for raw_code in all_raw_code {
        if !is_valid_source_name(&raw_code.name) {
            bail!(
                "bad code name {:?}: use letters, digits, `-`, `_` and `.`",
                raw_code.name
            );
        }
        if !seen_names.insert(raw_code.name.as_str()) {
            bail!("two `[[code]]` are named {:?}", raw_code.name);
        }
    }
    Ok(())
}

/// The `--code <name>=<dir>` checkout for each `[[code]]`, if any
fn code_override_dirs(all_raw_code: &[RawCode], code_overrides: &[String]) -> Result<Vec<Option<PathBuf>>> {
    let mut override_dirs = vec![None; all_raw_code.len()];
    for code_override in code_overrides {
        let code_index = code_override.split_once('=').and_then(|(name, code_dir)| {
            all_raw_code
                .iter()
                .position(|raw_code| raw_code.name == name)
                .map(|code_index| (code_index, code_dir))
        });
        let Some((code_index, code_dir)) = code_index else {
            let names: Vec<&str> = all_raw_code.iter().map(|raw_code| raw_code.name.as_str()).collect();
            bail!(
                "--code {code_override}: say which code source as `--code <name>=<dir>` (one of: {})",
                names.join(", ")
            );
        };
        if let Some(override_dir) = override_dirs.get_mut(code_index) {
            *override_dir = Some(PathBuf::from(code_dir));
        }
    }
    Ok(override_dirs)
}

/// With several docs sources, each needs a distinct name
fn check_docs_names(all_raw_docs: &[RawDocs]) -> Result<()> {
    if all_raw_docs.is_empty() {
        bail!("no `[[docs]]`: the config needs at least one docs source");
    }
    if let Some(bad_name) = all_raw_docs
        .iter()
        .filter_map(|raw_docs| raw_docs.name.as_deref())
        .find(|name| !is_valid_source_name(name))
    {
        bail!("bad docs name {bad_name:?}: use letters, digits, `-`, `_` and `.`");
    }
    if all_raw_docs.len() == 1 {
        return Ok(());
    }
    let mut seen_names = HashSet::new();
    for raw_docs in all_raw_docs {
        let Some(name) = raw_docs.name.as_deref() else {
            bail!("with several `[[docs]]`, each needs a `name`: it prefixes the references of its doc blocks");
        };
        if !seen_names.insert(name) {
            bail!("two `[[docs]]` are named {name:?}");
        }
    }
    Ok(())
}

fn is_valid_source_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
}

/// The `--docs` checkout for each docs source, if any: `<dir>` with a single
/// source, `<name>=<dir>` for any
fn override_dirs(all_raw_docs: &[RawDocs], docs_overrides: &[String]) -> Result<Vec<Option<PathBuf>>> {
    let mut override_dirs = vec![None; all_raw_docs.len()];
    for docs_override in docs_overrides {
        let named_override = docs_override.split_once('=').and_then(|(name, docs_dir)| {
            all_raw_docs
                .iter()
                .position(|raw_docs| raw_docs.name.as_deref() == Some(name))
                .map(|docs_index| (docs_index, docs_dir))
        });
        let (docs_index, docs_dir) = match named_override {
            Some(named_override) => named_override,
            None if all_raw_docs.len() == 1 => (0, docs_override.as_str()),
            None => {
                let names: Vec<&str> = all_raw_docs
                    .iter()
                    .filter_map(|raw_docs| raw_docs.name.as_deref())
                    .collect();
                bail!(
                    "--docs {docs_override}: with several docs sources, say which as `--docs <name>=<dir>` (one of {})",
                    names.join(", ")
                );
            }
        };
        if let Some(override_dir) = override_dirs.get_mut(docs_index) {
            *override_dir = Some(PathBuf::from(docs_dir));
        }
    }
    Ok(override_dirs)
}

/// The docs the config file at `config_path` (in `config_dir`) points to
fn configured_docs(config_path: &Path, config_dir: &Path, raw_docs: &RawDocs) -> Result<Tree> {
    let which_docs = raw_docs
        .name
        .as_ref()
        .map_or_else(|| "`docs`".to_owned(), |name| format!("docs {name:?}"));
    match (&raw_docs.path, &raw_docs.git, &raw_docs.reference) {
        (Some(local_path), None, None) => {
            local_tree(&config_dir.join(local_path), "docs").context("opening the configured docs checkout")
        }
        (None, Some(git_repo), Some(reference)) => git_tree(config_dir, git_repo, reference),
        (None, Some(_), None) => bail!(
            "{}: {which_docs}: `git` needs a `ref` (branch, tag or commit)",
            config_path.display()
        ),
        _ => bail!(
            "{}: {which_docs} needs either `path`, or `git` and `ref`",
            config_path.display()
        ),
    }
}

/// A local checkout at `root`, of docs or code (`what`)
fn local_tree(root: &Path, what: &str) -> Result<Tree> {
    if !root.is_dir() {
        bail!("{what} checkout not found at {}", root.display());
    }
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving the {what} checkout {}", root.display()))?;
    Ok(Tree::Local(root))
}

/// The git repository `git_repo` (a URL, or a path relative to `config_dir`) at `reference`
fn git_tree(config_dir: &Path, git_repo: &str, reference: &str) -> Result<Tree> {
    let local_repo = config_dir.join(git_repo);
    let repo_url = if local_repo.is_dir() {
        local_repo
            .canonicalize()
            .with_context(|| format!("resolving the repository {git_repo}"))?
            .display()
            .to_string()
    } else {
        git_repo.to_owned()
    };
    let git_tree = GitTree::open(&repo_url, reference).with_context(|| format!("opening {repo_url} at {reference}"))?;
    Ok(Tree::Git(git_tree))
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
