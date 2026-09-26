//! Where the docs come from: a local checkout, or a git repository at a ref.
//!
//! For git, asadoc keeps a bare, blobless clone in its cache directory, fetches
//! just the ref (depth 1), and reads files straight from git objects, fetching
//! only the blobs it needs. That keeps huge docs repos cheap.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};

pub(crate) enum Docs {
    Local(PathBuf),
    Git(GitDocs),
}

pub(crate) struct GitDocs {
    pub url: String,
    pub reference: String,
    pub commit: String,
    dir: PathBuf,
    /// Files read so far (the commit never changes, so neither do they)
    files: Mutex<HashMap<String, Option<String>>>,
}

impl Docs {
    /// A file's text, by path relative to the docs root; None when there's no such file
    pub(crate) fn read(&self, path: &str) -> Result<Option<String>> {
        match self {
            Self::Local(root) => {
                let full = root.join(path);
                match fs::read_to_string(&full) {
                    Ok(text) => Ok(Some(text)),
                    Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(error).with_context(|| format!("reading {}", full.display())),
                }
            }
            Self::Git(git_docs) => git_docs
                .read(path)
                .with_context(|| format!("reading {path} from {}", git_docs.url)),
        }
    }

    /// Makes the given paths (files, or directories for everything under them)
    /// cheap to read: one fetch for all the blobs instead of one per file
    pub(crate) fn prefetch(&self, paths: &[String]) -> Result<()> {
        match self {
            Self::Local(_) => Ok(()),
            Self::Git(git_docs) => git_docs
                .prefetch(paths)
                .with_context(|| format!("prefetching docs files from {}", git_docs.url)),
        }
    }

    /// For people: where the docs are
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Local(root) => root.display().to_string(),
            Self::Git(git_docs) => {
                let short: String = git_docs.commit.chars().take(12).collect();
                format!("{} at {} ({short})", git_docs.url, git_docs.reference)
            }
        }
    }

    /// The local directory holding the modules, when there is one to watch
    pub(crate) fn local_modules_dir(&self) -> Option<PathBuf> {
        match self {
            Self::Local(root) => Some(root.join("modules")),
            Self::Git(_) => None,
        }
    }

    /// Base URL for links to docs files, for GitHub repositories
    pub(crate) fn default_link_base(&self) -> Option<String> {
        let Self::Git(git_docs) = self else { return None };
        let repo = git_docs.url.strip_suffix(".git").unwrap_or(&git_docs.url);
        repo.starts_with("https://github.com/")
            .then(|| format!("{repo}/blob/{}/", git_docs.commit))
    }
}

fn run_git(dir: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))
}

/// Runs git in `dir`: its output, or None when it exits unsuccessfully
fn try_git(dir: &Path, args: &[&str]) -> Result<Option<String>> {
    let output = run_git(dir, args)?;
    Ok(output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned()))
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = run_git(dir, args)?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn cache_dir() -> PathBuf {
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(env::temp_dir)
        .join("asadoc")
}

/// Where the clone of `url` lives in the cache
fn clone_dir(url: &str) -> PathBuf {
    let name: String = url
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect();
    cache_dir().join("docs").join(name)
}

/// Makes `dir` a bare, blobless clone of `url` with nothing fetched yet
fn init_clone(dir: &Path, url: &str) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    [
        ["init", "--quiet", "--bare"].as_slice(),
        &["remote", "add", "origin", url],
        &["config", "remote.origin.promisor", "true"],
        &["config", "remote.origin.partialclonefilter", "blob:none"],
    ]
    .into_iter()
    .try_for_each(|args| git(dir, args).map(drop))
}

/// Whether `reference` is a full commit hash already in the clone. Such a
/// commit can't have changed, so there's no need to ask the remote
fn has_pinned_commit(dir: &Path, reference: &str) -> bool {
    let pinned = reference.len() == 40 && reference.chars().all(|character| character.is_ascii_hexdigit());
    pinned
        && Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["cat-file", "-e", &format!("{reference}^{{commit}}")])
            .env("GIT_NO_LAZY_FETCH", "1")
            .output()
            .is_ok_and(|output| output.status.success())
}

/// Fetches just `reference` (depth 1, no blobs): the commit it points to
fn fetch_commit(dir: &Path, url: &str, reference: &str) -> Result<String> {
    git(
        dir,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            "--depth=1",
            "--filter=blob:none",
            "origin",
            reference,
        ],
    )
    .with_context(|| format!("fetching {reference} of {url}"))?;
    Ok(git(dir, &["rev-parse", "FETCH_HEAD"])
        .context("resolving the fetched commit")?
        .trim()
        .to_owned())
}

impl GitDocs {
    /// Fetches `reference` (a branch, tag or commit) of `url` into the cache
    pub(crate) fn open(url: &str, reference: &str) -> Result<Self> {
        let dir = clone_dir(url);
        if !dir.join("HEAD").exists() {
            init_clone(&dir, url).with_context(|| format!("setting up the docs cache in {}", dir.display()))?;
            eprintln!("asadoc: fetching {url} at {reference} into {}", dir.display());
        }
        let commit = if has_pinned_commit(&dir, reference) {
            reference.to_owned()
        } else {
            fetch_commit(&dir, url, reference)?
        };
        Ok(Self {
            url: url.to_owned(),
            reference: reference.to_owned(),
            commit,
            dir,
            files: Mutex::new(HashMap::new()),
        })
    }

    fn files(&self) -> Result<MutexGuard<'_, HashMap<String, Option<String>>>> {
        self.files
            .lock()
            .map_err(|error| anyhow!("{error}"))
            .context("locking the docs file cache")
    }

    fn read(&self, path: &str) -> Result<Option<String>> {
        if let Some(cached) = self.files()?.get(path) {
            return Ok(cached.clone());
        }
        // Resolving the path only needs trees, which the blobless clone has, so
        // a failure here means there's no such file
        let spec = format!("{}:{path}", self.commit);
        let text = try_git(&self.dir, &["rev-parse", "--verify", "--quiet", &spec])
            .context("looking the file up")?
            .map(|oid| git(&self.dir, &["cat-file", "blob", oid.trim()]).context("reading the file's blob"))
            .transpose()?;
        self.files()?.insert(path.to_owned(), text.clone());
        Ok(text)
    }

    fn prefetch(&self, paths: &[String]) -> Result<()> {
        let wanted: Vec<&str> = {
            let files = self.files()?;
            paths
                .iter()
                .map(String::as_str)
                .filter(|path| !files.contains_key(*path))
                .collect()
        };
        if wanted.is_empty() {
            return Ok(());
        }
        let ls_tree_args: Vec<&str> = ["ls-tree", "-r", self.commit.as_str(), "--"]
            .into_iter()
            .chain(wanted)
            .collect();
        let listing = git(&self.dir, &ls_tree_args).context("listing the files to fetch")?;
        // "<mode> blob <oid>\t<path>"
        let oids: Vec<&str> = listing
            .lines()
            .filter_map(|entry| entry.split_whitespace().nth(2))
            .collect();
        let missing = self.missing(&oids).context("finding the blobs not fetched yet")?;
        if missing.is_empty() {
            return Ok(());
        }
        let fetch_args: Vec<&str> = [
            "-c",
            "fetch.negotiationAlgorithm=noop",
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--filter=blob:none",
            "origin",
        ]
        .into_iter()
        .chain(missing)
        .collect();
        git(&self.dir, &fetch_args).context("fetching the blobs")?;
        Ok(())
    }

    /// The objects not in the cache yet (checked without fetching them)
    fn missing<'a>(&self, oids: &[&'a str]) -> Result<Vec<&'a str>> {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args(["cat-file", "--batch-check"])
            .env("GIT_NO_LAZY_FETCH", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("running git cat-file --batch-check")?;
        // Dropped at the end of the block, closing git's stdin
        {
            let mut stdin = child.stdin.take().context("git cat-file has no stdin")?;
            stdin
                .write_all(oids.join("\n").as_bytes())
                .context("writing the object IDs to git cat-file")?;
        }
        let output = child.wait_with_output().context("waiting for git cat-file")?;
        if !output.status.success() {
            bail!("git cat-file --batch-check failed");
        }
        // "<oid> missing" for each one that isn't there
        let report = String::from_utf8_lossy(&output.stdout);
        let absent: HashSet<&str> = report
            .lines()
            .filter_map(|line| line.strip_suffix(" missing"))
            .collect();
        Ok(oids.iter().copied().filter(|oid| absent.contains(oid)).collect())
    }
}
