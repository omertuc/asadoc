//! Where the docs come from: a local checkout, or a git repository at a ref.
//!
//! For git, asadoc keeps a bare, blobless clone in its cache directory, fetches
//! just the ref (depth 1), and reads files straight from git objects, fetching
//! only the blobs it needs. That keeps huge docs repos cheap.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

pub enum Docs {
    Local(PathBuf),
    Git(GitDocs),
}

pub struct GitDocs {
    pub url: String,
    pub reference: String,
    pub commit: String,
    dir: PathBuf,
    /// Files read so far (the commit never changes, so neither do they)
    files: Mutex<HashMap<String, Option<String>>>,
}

impl Docs {
    /// A file's text, by path relative to the docs root
    pub fn read(&self, path: &str) -> Option<String> {
        match self {
            Docs::Local(root) => std::fs::read_to_string(root.join(path)).ok(),
            Docs::Git(g) => g.read(path),
        }
    }

    /// Makes the given paths (files, or directories for everything under them)
    /// cheap to read: one fetch for all the blobs instead of one per file
    pub fn prefetch(&self, paths: &[String]) {
        if let Docs::Git(g) = self {
            g.prefetch(paths);
        }
    }

    /// For people: where the docs are
    pub fn describe(&self) -> String {
        match self {
            Docs::Local(root) => root.display().to_string(),
            Docs::Git(g) => format!("{} at {} ({})", g.url, g.reference, &g.commit[..12.min(g.commit.len())]),
        }
    }

    /// The local directory holding the modules, when there is one to watch
    pub fn local_modules_dir(&self) -> Option<PathBuf> {
        match self {
            Docs::Local(root) => Some(root.join("modules")),
            Docs::Git(_) => None,
        }
    }

    /// Base URL for links to docs files, for GitHub repositories
    pub fn default_link_base(&self) -> Option<String> {
        let Docs::Git(g) = self else { return None };
        let repo = g.url.strip_suffix(".git").unwrap_or(&g.url);
        repo.starts_with("https://github.com/").then(|| format!("{repo}/blob/{}/", g.commit))
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().context("running git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("asadoc")
}

impl GitDocs {
    /// Fetches `reference` (a branch, tag or commit) of `url` into the cache
    pub fn open(url: &str, reference: &str) -> Result<GitDocs> {
        let name: String = url.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
        let dir = cache_dir().join("docs").join(name);
        if !dir.join("HEAD").exists() {
            std::fs::create_dir_all(&dir)?;
            git(&dir, &["init", "--quiet", "--bare"])?;
            git(&dir, &["remote", "add", "origin", url])?;
            git(&dir, &["config", "remote.origin.promisor", "true"])?;
            git(&dir, &["config", "remote.origin.partialclonefilter", "blob:none"])?;
        }
        eprintln!("asadoc: fetching {url} at {reference}...");
        git(&dir, &["fetch", "--quiet", "--no-tags", "--depth=1", "--filter=blob:none", "origin", reference])
            .with_context(|| format!("fetching {reference} of {url}"))?;
        let commit = git(&dir, &["rev-parse", "FETCH_HEAD"])?.trim().to_string();
        Ok(GitDocs { url: url.to_string(), reference: reference.to_string(), commit, dir, files: Mutex::new(HashMap::new()) })
    }

    fn read(&self, path: &str) -> Option<String> {
        if let Some(cached) = self.files.lock().unwrap().get(path) {
            return cached.clone();
        }
        let text = git(&self.dir, &["cat-file", "blob", &format!("{}:{path}", self.commit)]).ok();
        self.files.lock().unwrap().insert(path.to_string(), text.clone());
        text
    }

    fn prefetch(&self, paths: &[String]) {
        let wanted: Vec<&String> = {
            let files = self.files.lock().unwrap();
            paths.iter().filter(|p| !files.contains_key(p.as_str())).collect()
        };
        if wanted.is_empty() {
            return;
        }
        let mut args = vec!["ls-tree", "-r", self.commit.as_str(), "--"];
        args.extend(wanted.iter().map(|p| p.as_str()));
        let Ok(listing) = git(&self.dir, &args) else { return };
        // "<mode> blob <oid>\t<path>"
        let oids: Vec<&str> = listing.lines().filter_map(|l| l.split_whitespace().nth(2)).collect();
        let missing = self.missing(&oids);
        if missing.is_empty() {
            return;
        }
        let mut args = vec!["-c", "fetch.negotiationAlgorithm=noop", "fetch", "--quiet", "--no-tags", "--no-write-fetch-head", "--filter=blob:none", "origin"];
        args.extend(missing);
        let _ = git(&self.dir, &args);
    }

    /// The objects not in the cache yet (checked without fetching them)
    fn missing<'a>(&self, oids: &[&'a str]) -> Vec<&'a str> {
        use std::io::Write;
        let child = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args(["cat-file", "--batch-check"])
            .env("GIT_NO_LAZY_FETCH", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn();
        let Ok(mut child) = child else { return oids.to_vec() };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(oids.join("\n").as_bytes());
        }
        let Ok(out) = child.wait_with_output() else { return oids.to_vec() };
        // "<oid> missing" for each one that isn't there
        let out = String::from_utf8_lossy(&out.stdout);
        let absent: std::collections::HashSet<&str> = out.lines().filter_map(|l| l.strip_suffix(" missing")).collect();
        oids.iter().copied().filter(|o| absent.contains(o)).collect()
    }
}
