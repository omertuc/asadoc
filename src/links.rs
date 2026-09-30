//! Links to files on the web, for the review UI: to the doc a block is in,
//! and to marked code. Each docs and code source has its own
//! `external-link-format`:
//!
//! - omitted: detected from the repo's remote, for public hosts (github.com,
//!   gitlab.com, codeberg.org, bitbucket.org); no links elsewhere
//! - a kind of host, `github`, `gitlab`, `gitea` (also Forgejo) or `bitbucket`:
//!   for self-hosted and enterprise instances, at the remote's host
//! - a template with `{path}`, and optionally `{repo}` (the remote's web
//!   address), `{commit}`, `{first-line}` and `{last-line}`. Links to whole
//!   files drop everything from its `#`
//! - `none`: no links
//!
//! Links point at a commit: the one fetched for a git source, HEAD for a
//! local checkout.

#![expect(
    clippy::literal_string_with_formatting_args,
    reason = "link templates have `{name}` placeholders"
)]

use anyhow::{Result, bail};

/// A kind of host, by how it lays out links
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostKind {
    GitHub,
    GitLab,
    Gitea,
    Bitbucket,
}

/// Public hosts, detected from the remote
const PUBLIC_HOSTS: &[(&str, HostKind)] = &[
    ("github.com", HostKind::GitHub),
    ("gitlab.com", HostKind::GitLab),
    ("codeberg.org", HostKind::Gitea),
    ("bitbucket.org", HostKind::Bitbucket),
];

impl HostKind {
    fn named(name: &str) -> Option<Self> {
        match name {
            "github" => Some(Self::GitHub),
            "gitlab" => Some(Self::GitLab),
            "gitea" | "forgejo" => Some(Self::Gitea),
            "bitbucket" => Some(Self::Bitbucket),
            _ => None,
        }
    }

    fn file_url(self, repo: &str, commit: &str, path: &str) -> String {
        match self {
            Self::GitHub => format!("{repo}/blob/{commit}/{path}"),
            Self::GitLab => format!("{repo}/-/blob/{commit}/{path}"),
            Self::Gitea => format!("{repo}/src/commit/{commit}/{path}"),
            Self::Bitbucket => format!("{repo}/src/{commit}/{path}"),
        }
    }

    fn lines_anchor(self, first_line: usize, last_line: usize) -> String {
        match (self, first_line == last_line) {
            (Self::GitHub | Self::Gitea | Self::GitLab, true) => format!("#L{first_line}"),
            (Self::GitHub | Self::Gitea, false) => format!("#L{first_line}-L{last_line}"),
            (Self::GitLab, false) => format!("#L{first_line}-{last_line}"),
            (Self::Bitbucket, _) => format!("#lines-{first_line}:{last_line}"),
        }
    }

    /// Asks for the file's source rather than its rendering (which has no
    /// line anchors), for docs
    const fn source_view_query(self) -> &'static str {
        match self {
            Self::GitHub | Self::GitLab => "?plain=1",
            Self::Gitea => "?display=source",
            Self::Bitbucket => "",
        }
    }
}

enum LinkFormat {
    Host { kind: HostKind, repo: String },
    Template(String),
}

/// How to link to a source's files
pub(crate) struct ExternalLinks {
    format: LinkFormat,
    commit: String,
}

impl ExternalLinks {
    /// How to link to the files of a repo with the given remote (URL, or
    /// None when it has none) at `commit`, per its `external-link-format`
    /// setting; None for no links
    pub(crate) fn new(setting: Option<&str>, remote: Option<&str>, commit: Option<&str>) -> Result<Option<Self>> {
        let repo = remote.and_then(web_address);
        let format = match setting {
            Some("none") => return Ok(None),
            Some(template) if template.contains("{path}") => {
                if template.contains("{repo}") && repo.is_none() {
                    bail!("`{{repo}}` in {template:?}, but the repo has no remote with a web address");
                }
                LinkFormat::Template(template.replace("{repo}", repo.as_deref().unwrap_or_default()))
            }
            Some(kind_name) => {
                let Some(kind) = HostKind::named(kind_name) else {
                    bail!(
                        "bad external-link-format {kind_name:?}: use github, gitlab, gitea, bitbucket, none, \
                         or a template with {{path}}"
                    );
                };
                let Some(repo) = repo else {
                    bail!("external-link-format {kind_name:?}, but the repo has no remote with a web address");
                };
                LinkFormat::Host { kind, repo }
            }
            None => {
                let Some((kind, repo)) = repo.and_then(|repo| Some((public_host_kind(&repo)?, repo))) else {
                    return Ok(None);
                };
                LinkFormat::Host { kind, repo }
            }
        };
        let Some(commit) = commit else {
            if matches!(&format, LinkFormat::Template(template) if !template.contains("{commit}")) {
                return Ok(Some(Self {
                    format,
                    commit: String::new(),
                }));
            }
            // A repo with no commit yet has nothing on the web to link to
            return Ok(None);
        };
        Ok(Some(Self {
            format,
            commit: commit.to_owned(),
        }))
    }

    /// A link to a file, at its lines when given (first, last; 1-based)
    pub(crate) fn file(&self, path: &str, lines: Option<(usize, usize)>) -> String {
        self.link(path, lines, false)
    }

    /// A link to a file's source (not its rendering) at a line: for docs
    pub(crate) fn source_line(&self, path: &str, line: usize) -> String {
        self.link(path, Some((line, line)), true)
    }

    fn link(&self, path: &str, lines: Option<(usize, usize)>, source_view: bool) -> String {
        match &self.format {
            LinkFormat::Host { kind, repo } => {
                let query = if source_view { kind.source_view_query() } else { "" };
                let anchor = lines.map_or_else(String::new, |(first_line, last_line)| {
                    kind.lines_anchor(first_line, last_line)
                });
                format!("{}{query}{anchor}", kind.file_url(repo, &self.commit, path))
            }
            LinkFormat::Template(template) => {
                let template = match lines {
                    Some(_) => template.as_str(),
                    None => template.split('#').next().unwrap_or_default(),
                };
                let (first_line, last_line) = lines.unwrap_or_default();
                template
                    .replace("{commit}", &self.commit)
                    .replace("{path}", path)
                    .replace("{first-line}", &first_line.to_string())
                    .replace("{last-line}", &last_line.to_string())
            }
        }
    }
}

fn public_host_kind(repo: &str) -> Option<HostKind> {
    let host = repo.split_once("://")?.1.split('/').next()?;
    PUBLIC_HOSTS
        .iter()
        .find(|(public_host, _)| *public_host == host)
        .map(|(_, kind)| *kind)
}

/// A git remote's web address, `https://<host>/<path>`: from an HTTP(S) URL,
/// an `ssh://` URL or `[user@]host:path`. None for local paths.
fn web_address(remote: &str) -> Option<String> {
    let (scheme, host, path) = if let Some((scheme, rest)) = remote.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        let host_and_port = authority.rsplit('@').next()?;
        match scheme {
            "http" | "https" => (scheme, host_and_port, path),
            // The SSH port isn't the web one
            "ssh" | "git" => ("https", host_and_port.split(':').next()?, path),
            _ => return None,
        }
    } else {
        // `[user@]host:path`; a colon after a slash makes it a local path
        let (authority, path) = remote.split_once(':')?;
        if authority.contains('/') || path.starts_with("//") {
            return None;
        }
        ("https", authority.rsplit('@').next()?, path)
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    (!host.is_empty() && !path.is_empty()).then(|| format!("{scheme}://{host}/{path}"))
}

#[cfg(test)]
#[allow(clippy::panic_in_result_fn, reason = "assertions are how tests fail")]
mod tests {
    use super::*;

    #[test]
    fn finds_web_addresses() {
        let cases = [
            ("https://github.com/org/repo.git", Some("https://github.com/org/repo")),
            ("https://user@git.corp:8443/a/b/", Some("https://git.corp:8443/a/b")),
            (
                "git@gitlab.com:group/sub/repo.git",
                Some("https://gitlab.com/group/sub/repo"),
            ),
            (
                "ssh://git@git.corp:2222/org/repo.git",
                Some("https://git.corp/org/repo"),
            ),
            ("/home/me/repo", None),
            ("../repo", None),
            ("file:///home/me/repo", None),
        ];
        for (remote, expected) in cases {
            assert_eq!(web_address(remote).as_deref(), expected, "{remote}");
        }
    }

    #[test]
    fn links_by_host() -> Result<()> {
        let links = |setting, remote| ExternalLinks::new(setting, Some(remote), Some("abc"));
        let github = links(None, "git@github.com:org/repo.git")?;
        assert_eq!(
            github.map(|links| links.source_line("modules/m.adoc", 7)).as_deref(),
            Some("https://github.com/org/repo/blob/abc/modules/m.adoc?plain=1#L7")
        );
        let self_hosted_gitlab = links(Some("gitlab"), "https://gitlab.corp/g/repo.git")?;
        assert_eq!(
            self_hosted_gitlab
                .map(|links| links.file("a.yaml", Some((3, 5))))
                .as_deref(),
            Some("https://gitlab.corp/g/repo/-/blob/abc/a.yaml#L3-5")
        );
        assert!(links(None, "https://gitlab.corp/g/repo.git")?.is_none());
        assert!(links(Some("none"), "https://github.com/org/repo")?.is_none());
        assert!(links(Some("gitlub"), "https://github.com/org/repo").is_err());
        let custom = links(
            Some("{repo}/view/{commit}/{path}#from-{first-line}-to-{last-line}"),
            "git@host:r",
        )?;
        let custom = custom.as_ref();
        assert_eq!(
            custom.map(|links| links.file("a.yaml", Some((3, 5)))).as_deref(),
            Some("https://host/r/view/abc/a.yaml#from-3-to-5")
        );
        assert_eq!(
            custom.map(|links| links.file("a.yaml", None)).as_deref(),
            Some("https://host/r/view/abc/a.yaml")
        );
        Ok(())
    }
}
