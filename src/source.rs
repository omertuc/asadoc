//! Where docs and code come from: a local checkout, or a git repository at a ref.
//!
//! For git, asadoc keeps a bare, blobless clone in its cache directory, fetches
//! just the ref (depth 1), and reads files straight from git objects, fetching
//! only the blobs it needs. That keeps huge repos cheap.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::thread;

pub(crate) enum Tree {
    Local(PathBuf),
    Git(GitTree),
}

pub(crate) struct GitTree {
    pub url: String,
    pub reference: String,
    pub commit: String,
    clone_dir: PathBuf,
    /// Files read so far (the commit never changes, so neither do they)
    file_cache: Mutex<HashMap<String, Option<String>>>,
}

impl Tree {
    /// A file's text, by path relative to the root; None when there's no such
    /// file (or, on disk, when it isn't text)
    pub(crate) fn read(&self, path: &str) -> Result<Option<String>> {
        match self {
            Self::Local(root) => {
                let full_path = root.join(path);
                match fs::read_to_string(&full_path) {
                    Ok(text) => Ok(Some(text)),
                    Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::InvalidData) => Ok(None),
                    Err(error) => Err(error).with_context(|| format!("reading {}", full_path.display())),
                }
            }
            Self::Git(git_tree) => git_tree
                .read(path)
                .with_context(|| format!("reading {path} from {}", git_tree.url)),
        }
    }

    /// Makes the given paths (files, or directories for everything under them)
    /// cheap to read: one fetch for all the blobs instead of one per file
    pub(crate) fn prefetch(&self, paths: &[String]) -> Result<()> {
        match self {
            Self::Local(_) => Ok(()),
            Self::Git(git_tree) => git_tree
                .prefetch(paths)
                .with_context(|| format!("prefetching files from {}", git_tree.url)),
        }
    }

    /// Several files' text, as `read` gives each: in one go when they come from git
    pub(crate) fn read_all(&self, paths: &[String]) -> Result<Vec<Option<String>>> {
        match self {
            Self::Local(_) => paths.iter().map(|path| self.read(path)).collect(),
            Self::Git(git_tree) => git_tree
                .read_all(paths)
                .with_context(|| format!("reading files from {}", git_tree.url)),
        }
    }

    /// Every file, with its size: (path, bytes). For a local checkout, the
    /// files git tracks, or would (untracked but not ignored)
    pub(crate) fn list_files(&self) -> Result<Vec<(String, u64)>> {
        match self {
            Self::Local(root) => {
                let listing = git_stdout(root, &["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
                    .context("listing the files git knows")?;
                listing
                    .split('\0')
                    .filter(|path| !path.is_empty())
                    .map(|path| {
                        let full_path = root.join(path);
                        match fs::metadata(&full_path) {
                            Ok(metadata) => Ok(metadata.is_file().then(|| (path.to_owned(), metadata.len()))),
                            // Deleted, but still in the index
                            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
                            Err(error) => Err(error).with_context(|| format!("inspecting {}", full_path.display())),
                        }
                    })
                    .filter_map(Result::transpose)
                    .collect()
            }
            Self::Git(git_tree) => git_tree.list_files(""),
        }
    }

    /// The directory the files are in, when they're on disk (and can be changed)
    pub(crate) fn local_root(&self) -> Option<&Path> {
        match self {
            Self::Local(root) => Some(root),
            Self::Git(_) => None,
        }
    }

    /// For people: where the files are
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Local(root) => root.display().to_string(),
            Self::Git(git_tree) => {
                let short_commit: String = git_tree.commit.chars().take(12).collect();
                format!("{} at {} ({short_commit})", git_tree.url, git_tree.reference)
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

    /// Where the repo is published: the git URL, or a checkout's `origin`
    pub(crate) fn remote(&self) -> Result<Option<String>> {
        match self {
            Self::Local(root) => {
                Ok(try_git_stdout(root, &["remote", "get-url", "origin"])?.map(|remote| remote.trim().to_owned()))
            }
            Self::Git(git_tree) => Ok(Some(git_tree.url.clone())),
        }
    }

    /// The commit the files are at: the fetched one, or a checkout's HEAD
    /// (None when it has no commit yet)
    pub(crate) fn commit(&self) -> Result<Option<String>> {
        match self {
            Self::Local(root) => Ok(try_git_stdout(root, &["rev-parse", "--verify", "--quiet", "HEAD"])?
                .map(|commit| commit.trim().to_owned())),
            Self::Git(git_tree) => Ok(Some(git_tree.commit.clone())),
        }
    }
}

fn run_git(repo_dir: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))
}

/// Runs git in `dir`: its output, or None when it exits unsuccessfully
fn try_git_stdout(repo_dir: &Path, args: &[&str]) -> Result<Option<String>> {
    let output = run_git(repo_dir, args)?;
    Ok(output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned()))
}

fn git_stdout(repo_dir: &Path, args: &[&str]) -> Result<String> {
    let output = run_git(repo_dir, args)?;
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
fn cached_clone_dir(url: &str) -> PathBuf {
    let dir_name: String = url
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect();
    cache_dir().join("docs").join(dir_name)
}

/// The first object in `git cat-file --batch` output: its text (None when
/// it's missing or isn't text), and the output after it
fn next_batch_object(batch_output: &[u8]) -> Result<(Option<String>, &[u8])> {
    let header_end = batch_output
        .iter()
        .position(|byte| *byte == b'\n')
        .context("git cat-file's output ended early")?;
    let (header, after_header) = batch_output.split_at(header_end);
    let after_header = after_header.get(1..).unwrap_or_default();
    let header = String::from_utf8_lossy(header);
    // "<name> missing", or "<oid> <type> <size>" then the content and a newline
    if header.ends_with(" missing") {
        return Ok((None, after_header));
    }
    let size: usize = header
        .rsplit(' ')
        .next()
        .and_then(|size| size.parse().ok())
        .with_context(|| format!("unexpected git cat-file output {header:?}"))?;
    let content = after_header.get(..size).context("git cat-file's output ended early")?;
    let rest = after_header.get(size + 1..).unwrap_or_default();
    Ok((String::from_utf8(content.to_vec()).ok(), rest))
}

/// Makes `dir` a bare, blobless clone of `url` with nothing fetched yet
fn init_clone(clone_dir: &Path, url: &str) -> Result<()> {
    fs::create_dir_all(clone_dir).with_context(|| format!("creating {}", clone_dir.display()))?;
    [
        ["init", "--quiet", "--bare"].as_slice(),
        &["remote", "add", "origin", url],
        &["config", "remote.origin.promisor", "true"],
        &["config", "remote.origin.partialclonefilter", "blob:none"],
    ]
    .into_iter()
    .try_for_each(|args| git_stdout(clone_dir, args).map(drop))
}

/// Whether `reference` is a full commit hash already in the clone. Such a
/// commit can't have changed, so there's no need to ask the remote
fn has_pinned_commit(clone_dir: &Path, reference: &str) -> bool {
    let is_full_hash = reference.len() == 40 && reference.chars().all(|character| character.is_ascii_hexdigit());
    is_full_hash
        && Command::new("git")
            .arg("-C")
            .arg(clone_dir)
            .args(["cat-file", "-e", &format!("{reference}^{{commit}}")])
            .env("GIT_NO_LAZY_FETCH", "1")
            .output()
            .is_ok_and(|output| output.status.success())
}

/// Fetches just `reference` (depth 1, no blobs): the commit it points to
fn fetch_commit(clone_dir: &Path, url: &str, reference: &str) -> Result<String> {
    git_stdout(
        clone_dir,
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
    Ok(git_stdout(clone_dir, &["rev-parse", "FETCH_HEAD"])
        .context("resolving the fetched commit")?
        .trim()
        .to_owned())
}

impl GitTree {
    /// Fetches `reference` (a branch, tag or commit) of `url` into the cache
    pub(crate) fn open(url: &str, reference: &str) -> Result<Self> {
        let clone_dir = cached_clone_dir(url);
        if !clone_dir.join("HEAD").exists() {
            init_clone(&clone_dir, url)
                .with_context(|| format!("setting up the docs cache in {}", clone_dir.display()))?;
        }
        let commit = if has_pinned_commit(&clone_dir, reference) {
            reference.to_owned()
        } else {
            eprintln!("asadoc: fetching {url} at {reference} into {}", clone_dir.display());
            fetch_commit(&clone_dir, url, reference)?
        };
        Ok(Self {
            url: url.to_owned(),
            reference: reference.to_owned(),
            commit,
            clone_dir,
            file_cache: Mutex::new(HashMap::new()),
        })
    }

    fn lock_file_cache(&self) -> Result<MutexGuard<'_, HashMap<String, Option<String>>>> {
        self.file_cache
            .lock()
            .map_err(|error| anyhow!("{error}"))
            .context("locking the file cache")
    }

    fn read(&self, path: &str) -> Result<Option<String>> {
        if let Some(cached) = self.lock_file_cache()?.get(path) {
            return Ok(cached.clone());
        }
        // Resolving the path only needs trees, which the blobless clone has, so
        // a failure here means there's no such file
        let object_spec = format!("{}:{path}", self.commit);
        let text = try_git_stdout(&self.clone_dir, &["rev-parse", "--verify", "--quiet", &object_spec])
            .context("looking the file up")?
            .map(|blob_oid| {
                git_stdout(&self.clone_dir, &["cat-file", "blob", blob_oid.trim()]).context("reading the file's blob")
            })
            .transpose()?;
        self.lock_file_cache()?.insert(path.to_owned(), text.clone());
        Ok(text)
    }

    fn read_all(&self, paths: &[String]) -> Result<Vec<Option<String>>> {
        self.prefetch(paths)?;
        let uncached_paths: Vec<&str> = {
            let file_cache = self.lock_file_cache()?;
            paths
                .iter()
                .map(String::as_str)
                .filter(|path| !file_cache.contains_key(*path))
                .collect()
        };
        let object_specs = uncached_paths.iter().fold(String::new(), |mut object_specs, path| {
            object_specs.push_str(&self.commit);
            object_specs.push(':');
            object_specs.push_str(path);
            object_specs.push('\n');
            object_specs
        });
        let batch_output = self.cat_file_batch(&object_specs)?;
        let mut remaining_output = batch_output.as_slice();
        for path in uncached_paths {
            let (text, rest) = next_batch_object(remaining_output).with_context(|| format!("reading {path}"))?;
            self.lock_file_cache()?.insert(path.to_owned(), text);
            remaining_output = rest;
        }
        paths.iter().map(|path| self.read(path)).collect()
    }

    /// `git cat-file --batch`'s output for the given object names (one per line)
    fn cat_file_batch(&self, object_specs: &str) -> Result<Vec<u8>> {
        let mut cat_file = Command::new("git")
            .arg("-C")
            .arg(&self.clone_dir)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("running git cat-file --batch")?;
        let mut stdin = cat_file.stdin.take().context("git cat-file has no stdin")?;
        let object_specs = object_specs.to_owned();
        // Written from another thread, as git's output would fill its pipe otherwise
        let writer = thread::spawn(move || stdin.write_all(object_specs.as_bytes()));
        let output = cat_file.wait_with_output().context("waiting for git cat-file")?;
        writer
            .join()
            .map_err(|_panic| anyhow!("writing to git cat-file panicked"))?
            .context("writing the object names to git cat-file")?;
        if !output.status.success() {
            bail!("git cat-file --batch failed");
        }
        Ok(output.stdout)
    }

    /// Every file under `dir` ("" for all), with its size: (path, bytes)
    pub(crate) fn list_files(&self, dir: &str) -> Result<Vec<(String, u64)>> {
        let ls_tree_args: Vec<&str> = ["ls-tree", "-r", "-l", "-z", self.commit.as_str(), "--"]
            .into_iter()
            .chain((!dir.is_empty()).then_some(dir))
            .collect();
        let tree_listing =
            git_stdout(&self.clone_dir, &ls_tree_args).with_context(|| format!("listing the files of {}", self.url))?;
        // "<mode> <type> <oid> <size>\t<path>", NUL-terminated
        Ok(tree_listing
            .split('\0')
            .filter_map(|tree_entry| {
                let (entry_info, path) = tree_entry.split_once('\t')?;
                let mut info_fields = entry_info.split_whitespace();
                // Symbolic links (120000) are blobs too, holding the link's target
                let is_file = info_fields.next()? != "120000" && info_fields.next() == Some("blob");
                let size = info_fields.nth(1)?.parse().ok()?;
                is_file.then(|| (path.to_owned(), size))
            })
            .collect())
    }

    fn prefetch(&self, paths: &[String]) -> Result<()> {
        let uncached_paths: Vec<&str> = {
            let file_cache = self.lock_file_cache()?;
            paths
                .iter()
                .map(String::as_str)
                .filter(|path| !file_cache.contains_key(*path))
                .collect()
        };
        if uncached_paths.is_empty() {
            return Ok(());
        }
        let ls_tree_args: Vec<&str> = ["ls-tree", "-r", self.commit.as_str(), "--"]
            .into_iter()
            .chain(uncached_paths)
            .collect();
        let tree_listing = git_stdout(&self.clone_dir, &ls_tree_args).context("listing the files to fetch")?;
        // "<mode> blob <oid>\t<path>"
        let blob_oids: Vec<&str> = tree_listing
            .lines()
            .filter_map(|tree_entry| tree_entry.split_whitespace().nth(2))
            .collect();
        let missing_oids = self
            .missing_objects(&blob_oids)
            .context("finding the blobs not fetched yet")?;
        if missing_oids.is_empty() {
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
        .chain(missing_oids.iter().copied())
        .collect();
        eprintln!(
            "asadoc: fetching {} files from {} into the cache",
            missing_oids.len(),
            self.url
        );
        git_stdout(&self.clone_dir, &fetch_args).context("fetching the blobs")?;
        Ok(())
    }

    /// The objects not in the cache yet (checked without fetching them)
    fn missing_objects<'a>(&self, oids: &[&'a str]) -> Result<Vec<&'a str>> {
        let mut cat_file = Command::new("git")
            .arg("-C")
            .arg(&self.clone_dir)
            .args(["cat-file", "--batch-check"])
            .env("GIT_NO_LAZY_FETCH", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("running git cat-file --batch-check")?;
        // Dropped at the end of the block, closing git's stdin
        {
            let mut stdin = cat_file.stdin.take().context("git cat-file has no stdin")?;
            stdin
                .write_all(oids.join("\n").as_bytes())
                .context("writing the object IDs to git cat-file")?;
        }
        let output = cat_file.wait_with_output().context("waiting for git cat-file")?;
        if !output.status.success() {
            bail!("git cat-file --batch-check failed");
        }
        // "<oid> missing" for each one that isn't there
        let batch_check_output = String::from_utf8_lossy(&output.stdout);
        let absent_oids: HashSet<&str> = batch_check_output
            .lines()
            .filter_map(|line| line.strip_suffix(" missing"))
            .collect();
        Ok(oids.iter().copied().filter(|oid| absent_oids.contains(oid)).collect())
    }
}
