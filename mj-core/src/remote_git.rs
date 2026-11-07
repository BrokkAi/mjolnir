//! Resolve repository sources that can be used by a network-backed checkout.
//!
//! A source contains the URLs to put in the target repository's `origin`.
//! Resolution is deliberately separate from discovering the default branch:
//! reading a host checkout's configuration is local work, while asking a
//! server what `HEAD` means is an explicit, bounded operation.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::config::ProjectRepository;
use crate::targets::{CommandExecutor, CommandOutput, CommandSpec};

pub use crate::local_git::resolve_local_repository;

/// An immutable starting selection for one repository in a bundle. The API
/// names it `at` and `branch`; the daemon picks the bundle's primary
/// repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExactCheckout {
    pub repository_id: String,
    pub commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The workspace holds `commit`, the diff base, and their ancestors, and
    /// nothing later: it has no `origin` remote, remote-tracking refs, or
    /// tags, and does not borrow objects from the clone cache. The API and
    /// `mj new` name it `no_later_history`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_later_history: bool,
}

impl ExactCheckout {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.repository_id.is_empty(),
            "checkout repository_id is required"
        );
        anyhow::ensure!(
            is_object_id(&self.commit),
            "`at` must be a full nonzero commit object ID"
        );
        if let Some(branch) = &self.branch {
            validate_branch_name(branch).context("invalid `branch`")?;
            anyhow::ensure!(branch != "HEAD", "`branch` cannot be HEAD");
        }
        Ok(())
    }
}

/// The network endpoints a target checkout should use for Git fetches and
/// pushes. The first push URL is the primary destination; all configured push
/// URLs are retained because Git intentionally supports pushing to several
/// destinations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkGitSource {
    pub fetch_url: String,
    pub push_urls: Vec<String>,
}

/// Resolve a configured repository without contacting its server.
///
/// GitHub's `owner/repository` shorthand is expanded to the HTTPS clone URL.
/// The default branch is intentionally not inferred here; callers that need it
/// must make the separate [`default_branch`] request.
pub fn resolve_repository(
    repository: &ProjectRepository,
    executor: &impl CommandExecutor,
) -> Result<NetworkGitSource> {
    if repository.git_ref.is_some() {
        bail!(
            "repository {:?} sets obsolete `git_ref`; remove it so Hel can resolve the remote default branch",
            repository.id
        );
    }

    match (&repository.github, &repository.local) {
        (Some(source), None) => {
            let fetch_url = expand_github_source(source)?;
            Ok(NetworkGitSource {
                push_urls: vec![fetch_url.clone()],
                fetch_url,
            })
        }
        (None, Some(path)) => {
            {
                // A missing remote is the person's to fix, so the reason is
                // marked as a refusal and travels to whoever asked for the
                // session instead of stopping at the daemon log.
                crate::local_git::resolve_local_repository(path, executor).with_context(|| {
                    crate::refusal::Refusal::precondition(format!(
                        "repository {:?} needs valid default network Git remotes for an isolated session; configure a remote or use a raw local session",
                        repository.id
                    ))
                })
            }
        }
        (None, None) => bail!(
            "repository {:?} has no configured Git source",
            repository.id
        ),
        (Some(_), Some(_)) => bail!(
            "repository {:?} must configure exactly one of `github` or `local`",
            repository.id
        ),
    }
}

/// Ask the configured server for the branch its symbolic `HEAD` names.
///
/// This is the only network operation in this module. The supplied executor
/// owns its timeout, cancellation, and subprocess supervision; this function
/// only makes the command noninteractive and parses its bounded result.
pub fn default_branch(
    source: &NetworkGitSource,
    executor: &impl CommandExecutor,
) -> Result<String> {
    validate_network_url(&source.fetch_url)
        .with_context(|| format!("invalid Git fetch URL {}", display_url(&source.fetch_url)))?;
    if executor.cancellation_requested() {
        bail!("operation cancelled while resolving the remote default branch");
    }

    // Keep the source as an argument, including any configured credentials.
    // `display_url` is used only for diagnostics; stripping credentials here
    // would change how Git authenticates.
    let mut command = CommandSpec::new(
        "git",
        [
            "ls-remote".to_owned(),
            "--symref".to_owned(),
            "--".to_owned(),
            source.fetch_url.clone(),
            "HEAD".to_owned(),
        ],
    )
    .purpose("resolve remote default branch");
    // Git does not read GH_TOKEN itself. Scope the normal GitHub token/CLI
    // credential flow to this command, without changing the host Git config.
    if Url::parse(&source.fetch_url)
        .is_ok_and(|url| url.scheme() == "https" && url.host_str() == Some("github.com"))
    {
        command.args.splice(0..0, [
            "-c".to_owned(),
            r#"credential.https://github.com.helper=!f() { if [ "$1" = get ] && [ -n "${GH_TOKEN:-${GITHUB_TOKEN:-}}" ]; then printf '%s\n' username=x-access-token "password=${GH_TOKEN:-$GITHUB_TOKEN}"; else gh auth git-credential "$@"; fi; }; f"#.to_owned(),
        ]);
    }
    crate::local_git::unattended_git_environment(&mut command);

    let output = match executor.execute(&command) {
        Ok(output) => output,
        Err(error) => {
            let detail = sanitize_diagnostic(&error.to_string(), &source.fetch_url);
            bail!(
                "could not resolve the default branch from {}: {detail}",
                display_url(&source.fetch_url)
            );
        }
    };
    if executor.cancellation_requested() {
        bail!("operation cancelled while resolving the remote default branch");
    }
    if output.status != 0 {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = sanitize_diagnostic(detail.trim(), &source.fetch_url);
        if detail.is_empty() {
            bail!(
                "could not resolve the default branch from {} (Git exited with status {})",
                display_url(&source.fetch_url),
                output.status
            );
        }
        bail!(
            "could not resolve the default branch from {}: {detail}",
            display_url(&source.fetch_url)
        );
    }
    parse_default_branch(&output).with_context(|| {
        format!(
            "parse default branch advertised by {}",
            display_url(&source.fetch_url)
        )
    })
}

/// Validate that a Git endpoint uses a network transport.
///
/// Git accepts local paths and several helper transports anywhere a remote URL
/// is expected. They would make a target checkout depend on the controller's
/// filesystem or execute an unbounded helper, so they are rejected here.
pub fn validate_network_url(url: &str) -> Result<()> {
    let value = url.trim();
    if value.is_empty() {
        bail!("Git URL is empty");
    }
    if value != url || value.starts_with('-') || value.chars().any(char::is_whitespace) {
        bail!(
            "Git URL {} contains invalid whitespace or option syntax",
            display_url(value)
        );
    }
    if crate::path_completion::looks_like_path(value) {
        bail!("Git URL {} is a local repository path", display_url(value));
    }
    if value.starts_with("ext::")
        || value.starts_with("helper::")
        || value.starts_with("local::")
        || value.starts_with("fd::")
    {
        bail!(
            "Git URL {} uses a local or helper transport",
            display_url(value)
        );
    }

    if !value.contains("://")
        && let Some((scheme, _)) = value.split_once(':')
        && matches!(
            scheme.to_ascii_lowercase().as_str(),
            "file" | "ext" | "helper" | "local" | "fd" | "http" | "https" | "ssh" | "git"
        )
    {
        bail!(
            "Git URL {} uses an unsupported non-network or malformed URL transport",
            display_url(value)
        );
    }

    if value.contains("://") {
        let parsed =
            Url::parse(value).with_context(|| format!("parse Git URL {}", display_url(value)))?;
        let scheme = parsed.scheme();
        if !matches!(scheme, "http" | "https" | "ssh" | "git") || parsed.host_str().is_none() {
            bail!(
                "Git URL {} does not use a supported network transport",
                display_url(value)
            );
        }
        return Ok(());
    }

    // The remaining network spelling Git accepts is scp-like SSH, for
    // example `git@github.com:owner/repository.git`. A path without a colon is
    // a local path and is rejected.
    let Some((host, remote_path)) = value.split_once(':') else {
        bail!("Git URL {} is a local repository path", display_url(value));
    };
    if host.is_empty()
        || remote_path.is_empty()
        || host.starts_with('.')
        || host.contains('/')
        || host.contains('\\')
        || host.contains('@') && host.ends_with('@')
    {
        bail!(
            "Git URL {} is not a valid scp-style SSH endpoint",
            display_url(value)
        );
    }
    if let Some((user, hostname)) = host.rsplit_once('@')
        && (user.is_empty() || hostname.is_empty())
    {
        bail!(
            "Git URL {} is not a valid scp-style SSH endpoint",
            display_url(value)
        );
    }
    if remote_path.starts_with(':') {
        bail!(
            "Git URL {} uses an unsupported helper transport",
            display_url(value)
        );
    }
    Ok(())
}

/// Return a URL suitable for diagnostics or persistence in user-visible state.
/// Userinfo is removed from both URL and scp-style SSH spellings.
pub fn display_url(url: &str) -> String {
    if let Ok(mut parsed) = Url::parse(url) {
        if !parsed.username().is_empty() || parsed.password().is_some() {
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
        }
        return parsed.to_string();
    }

    if let Some(scheme_end) = url.find("://") {
        let authority_start = scheme_end + 3;
        let authority_end = url[authority_start..]
            .find(['/', '?', '#'])
            .map_or(url.len(), |offset| authority_start + offset);
        if let Some(at) = url[authority_start..authority_end].rfind('@') {
            let at = authority_start + at;
            return format!("{}{}", &url[..authority_start], &url[at + 1..]);
        }
    } else if let Some(colon) = url.find(':') {
        let authority = &url[..colon];
        if let Some(at) = authority.rfind('@') {
            return format!("{}{}", &url[at + 1..colon], &url[colon..]);
        }
    }
    url.to_owned()
}

fn expand_github_source(source: &str) -> Result<String> {
    let source = source.trim();
    let shorthand = !source.contains("://")
        && !source.contains('@')
        && !source.contains(':')
        && source.contains('/');
    let expanded = if shorthand {
        let (owner, repository) = github_owner_repo(source)
            .with_context(|| format!("invalid GitHub owner/repository {source:?}"))?;
        format!("https://github.com/{owner}/{repository}.git")
    } else {
        source.to_owned()
    };
    validate_network_url(&expanded).with_context(|| {
        format!(
            "invalid configured GitHub source {}",
            display_url(&expanded)
        )
    })?;
    Ok(expanded)
}

/// Whether [`parse_github_remote`] accepts a bare `owner/repository`.
///
/// Text typed into a `github =` setting, the CLI or the API may be shorthand.
/// Text read from `git config remote.origin.url` may not: there `mirror/repo`
/// is a relative filesystem path, not GitHub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shorthand {
    Allowed,
    Rejected,
}

/// How a GitHub remote was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubSpelling {
    /// A bare `owner/repository`.
    Shorthand,
    Https,
    Http,
    /// `[git@]github.com:owner/repository`.
    Scp,
    /// `ssh://[git@]host[:port]/owner/repository`.
    Ssh,
}

/// Which GitHub host a remote names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubHost {
    /// `github.com`, and shorthand, which means `github.com`.
    GithubCom,
    /// `ssh.github.com`, GitHub's SSH endpoint on port 443.
    SshGithubCom,
}

/// A remote that names a repository on GitHub. Owner and repository keep the
/// case they were written in; callers that compare case-insensitively lower
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubRemote {
    pub owner: String,
    pub repository: String,
    pub spelling: GithubSpelling,
    pub host: GithubHost,
    /// A port written in the remote that is not the scheme's default. The
    /// default ports of `http` and `https` are never reported.
    pub port: Option<u16>,
}

impl GithubRemote {
    /// Whether the remote names GitHub's main endpoint without an explicit
    /// port. `ssh://git@github.com:22/...` and `ssh.github.com` are the same
    /// repository over another route, and identities keep them apart.
    pub fn is_default_endpoint(&self) -> bool {
        self.host == GithubHost::GithubCom && self.port.is_none()
    }
}

/// The two SSH spellings Git can rewrite to HTTPS with
/// `url.https://github.com/.insteadOf`.
pub const GITHUB_SSH_REWRITE_PREFIXES: [&str; 2] = ["git@github.com:", "ssh://git@github.com/"];

/// Whether `remote` begins with one of [`GITHUB_SSH_REWRITE_PREFIXES`], which
/// is exactly when such a rewrite applies to it.
pub fn uses_github_ssh_rewrite_prefix(remote: &str) -> bool {
    GITHUB_SSH_REWRITE_PREFIXES
        .iter()
        .any(|prefix| remote.starts_with(prefix))
}

/// Parse a GitHub remote: `owner/repository` (with [`Shorthand::Allowed`]),
/// `https://` and `http://` URLs for `github.com` (userinfo ignored),
/// `[git@]github.com:owner/repository`, `ssh://[git@]github.com[:22]/...`, and
/// `ssh://[git@]ssh.github.com:443/...`. The host is case-insensitive. A
/// trailing `/` and one `.git` are removed. Other hosts, ports, schemes, SSH
/// users, extra path segments, queries and fragments return `None`, as do
/// owner or repository names GitHub cannot have.
pub fn parse_github_remote(source: &str, shorthand: Shorthand) -> Option<GithubRemote> {
    let source = source.trim();
    let (spelling, host, port, path) = if source.contains("://") {
        let url = Url::parse(source).ok()?;
        if url.query().is_some() || url.fragment().is_some() {
            return None;
        }
        let host_name = url.host_str()?;
        let (spelling, host, port) = match url.scheme() {
            "https" | "http" => {
                let spelling = if url.scheme() == "https" {
                    GithubSpelling::Https
                } else {
                    GithubSpelling::Http
                };
                if !is_github_web_host(&url) {
                    return None;
                }
                (spelling, GithubHost::GithubCom, None)
            }
            "ssh" => {
                if (!url.username().is_empty() && url.username() != "git")
                    || url.password().is_some()
                {
                    return None;
                }
                let (host, port_ok) = if host_name.eq_ignore_ascii_case("github.com") {
                    (GithubHost::GithubCom, matches!(url.port(), None | Some(22)))
                } else if host_name.eq_ignore_ascii_case("ssh.github.com") {
                    (GithubHost::SshGithubCom, url.port() == Some(443))
                } else {
                    return None;
                };
                if !port_ok {
                    return None;
                }
                (GithubSpelling::Ssh, host, url.port())
            }
            _ => return None,
        };
        (
            spelling,
            host,
            port,
            url.path().strip_prefix('/')?.to_owned(),
        )
    } else if let Some((authority, path)) = source.split_once(':') {
        let host = match authority.rsplit_once('@') {
            Some(("git", host)) => host,
            Some(_) => return None,
            None => authority,
        };
        // `ssh.github.com` is GitHub's SSH-over-443 endpoint; the person's SSH
        // configuration selects the port for this spelling.
        let host = if host.eq_ignore_ascii_case("github.com") {
            GithubHost::GithubCom
        } else if host.eq_ignore_ascii_case("ssh.github.com") {
            GithubHost::SshGithubCom
        } else {
            return None;
        };
        (GithubSpelling::Scp, host, None, path.to_owned())
    } else if shorthand == Shorthand::Allowed && !source.contains('@') {
        (
            GithubSpelling::Shorthand,
            GithubHost::GithubCom,
            None,
            source.to_owned(),
        )
    } else {
        return None;
    };

    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repository = parts.next()?;
    if parts.next().is_some()
        || !crate::config::valid_github_owner_login(owner)
        || !valid_github_repository_name(repository)
    {
        return None;
    }
    Some(GithubRemote {
        owner: owner.to_owned(),
        repository: repository.to_owned(),
        spelling,
        host,
        port,
    })
}

/// Whether `url` names GitHub's web host on its default port.
fn is_github_web_host(url: &Url) -> bool {
    url.port().is_none()
        && url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("github.com"))
}

/// Whether `source` is an https URL on GitHub's web host, whatever its path.
/// Authentication follows the server, so a caller deciding whether to send a
/// GitHub token must not depend on the path being a repository name
/// [`parse_github_remote`] accepts, such as one written with percent escapes.
pub fn is_github_https_url(source: &str) -> bool {
    Url::parse(source.trim()).is_ok_and(|url| url.scheme() == "https" && is_github_web_host(&url))
}

/// The owner and repository of a GitHub source, in the case written. Accepts
/// the spellings [`resolve_repository`] does, including `owner/repository`.
pub fn github_owner_repo(source: &str) -> Option<(String, String)> {
    let remote = parse_github_remote(source, Shorthand::Allowed)?;
    Some((remote.owner, remote.repository))
}

fn valid_github_repository_name(repository: &str) -> bool {
    !repository.is_empty()
        && repository.len() <= 100
        && !matches!(repository, "." | "..")
        && repository
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn network_git_output(output: &CommandOutput) -> Result<(Option<String>, Option<String>)> {
    let text = String::from_utf8(output.stdout.clone()).context("decode `git ls-remote` output")?;
    let mut branch = None;
    let mut commit = None;
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some(line) = line.strip_prefix("ref:") {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.len() == 2 && fields[1] == "HEAD" {
                let reference = fields[0];
                let Some(candidate) = reference.strip_prefix("refs/heads/") else {
                    continue;
                };
                if branch.replace(candidate.to_owned()).is_some() {
                    bail!("remote HEAD advertises multiple default branches");
                }
            }
            continue;
        }
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() == 2
            && fields[1] == "HEAD"
            && commit.replace(fields[0].to_owned()).is_some()
        {
            bail!("remote HEAD advertises multiple commit ids");
        }
    }
    Ok((branch, commit))
}

fn parse_default_branch(output: &CommandOutput) -> Result<String> {
    let (branch, commit) = network_git_output(output)?;
    let branch =
        branch.ok_or_else(|| anyhow::anyhow!("remote did not advertise a symbolic HEAD branch"))?;
    validate_branch_name(&branch)?;
    let commit =
        commit.ok_or_else(|| anyhow::anyhow!("remote did not advertise a commit for HEAD"))?;
    if !is_object_id(&commit) {
        bail!("remote advertised an invalid commit id for HEAD");
    }
    Ok(branch)
}

fn validate_branch_name(branch: &str) -> Result<()> {
    if branch.is_empty()
        || branch.starts_with('-')
        || branch.starts_with('/')
        || branch.ends_with('/')
        || branch.starts_with('.')
        || branch.ends_with('.')
        || branch.contains("..")
        || branch.contains("//")
        || branch.contains("@{")
        || branch == "@"
        || branch.chars().any(|character| {
            character.is_control() || character.is_whitespace() || "~^:?*[\\".contains(character)
        })
        || branch
            .split('/')
            .any(|part| part == "." || part == ".." || part.ends_with(".lock"))
    {
        bail!("remote advertised an invalid default branch name");
    }
    Ok(())
}

/// A full SHA-1 or SHA-256 Git object id other than the null id.
pub fn is_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        && value.bytes().any(|byte| byte != b'0')
}

pub(crate) fn is_windows_absolute_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

fn sanitize_diagnostic(text: &str, source: &str) -> String {
    let displayed = display_url(source);
    text.replace(source, &displayed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_shorthand_expands_to_https() {
        let repository = ProjectRepository {
            id: "repo".into(),
            github: Some("BrokkAi/hel".into()),
            local: None,
            destination: "repo".into(),
            git_ref: None,
        };
        let source = resolve_repository(&repository, &crate::targets::ProcessExecutor).unwrap();
        assert_eq!(source.fetch_url, "https://github.com/BrokkAi/hel.git");
        assert_eq!(source.push_urls, ["https://github.com/BrokkAi/hel.git"]);
    }

    // Hard-won: a2e8bd8e9: a GitHub token was withheld from a URL whose path the parser rejects.
    #[test]
    fn github_https_hosts_are_recognized_whatever_the_path() {
        for url in [
            "https://github.com/o/r",
            "https://github.com/acme/%72epo.git",
            "https://GITHUB.com/o/r",
            "https://github.com:443/o/r",
        ] {
            assert!(is_github_https_url(url), "{url}");
        }
        // The token goes only to GitHub's web host, over https, on its port.
        for url in [
            "http://github.com/o/r",
            "https://github.com:444/o/r",
            "https://gist.github.com/o/r",
            "https://github.example.com/o/r",
            "https://notgithub.com/o/r",
            "https://github.com.example.com/o/r",
            "git@github.com:o/r.git",
            "ssh://git@github.com/o/r.git",
            "o/r",
        ] {
            assert!(!is_github_https_url(url), "{url}");
        }
    }

    #[test]
    fn github_remote_parser_separates_real_remotes_from_relative_paths_and_other_hosts() {
        // (input, result with shorthand allowed, whether only shorthand makes
        // it GitHub). Results read `owner/repo spelling host port`.
        let table: &[(&str, Option<&str>, bool)] = &[
            (
                "owner/repo",
                Some("owner/repo Shorthand GithubCom None"),
                true,
            ),
            (
                "owner/repo.git",
                Some("owner/repo Shorthand GithubCom None"),
                true,
            ),
            (
                "mirror/repo.git",
                Some("mirror/repo Shorthand GithubCom None"),
                true,
            ),
            (
                "https://github.com/o/r",
                Some("o/r Https GithubCom None"),
                false,
            ),
            (
                "https://github.com/o/r.git",
                Some("o/r Https GithubCom None"),
                false,
            ),
            (
                "https://github.com/o/r/",
                Some("o/r Https GithubCom None"),
                false,
            ),
            ("https://github.com/o/r/extra", None, false),
            (
                "http://github.com/o/r",
                Some("o/r Http GithubCom None"),
                false,
            ),
            (
                "https://user:pass@github.com/o/r.git",
                Some("o/r Https GithubCom None"),
                false,
            ),
            (
                "https://github.com:443/o/r",
                Some("o/r Https GithubCom None"),
                false,
            ),
            ("https://github.com:444/o/r", None, false),
            ("http://github.com:443/o/r", None, false),
            (
                "git@github.com:o/r.git",
                Some("o/r Scp GithubCom None"),
                false,
            ),
            ("github.com:o/r.git", Some("o/r Scp GithubCom None"), false),
            (
                "ssh://git@github.com/o/r.git",
                Some("o/r Ssh GithubCom None"),
                false,
            ),
            (
                "ssh://github.com/o/r.git",
                Some("o/r Ssh GithubCom None"),
                false,
            ),
            (
                "ssh://git@github.com:22/o/r.git",
                Some("o/r Ssh GithubCom Some(22)"),
                false,
            ),
            ("ssh://git@github.com:443/o/r.git", None, false),
            (
                "ssh://git@ssh.github.com:443/o/r.git",
                Some("o/r Ssh SshGithubCom Some(443)"),
                false,
            ),
            (
                "ssh://ssh.github.com:443/o/r.git",
                Some("o/r Ssh SshGithubCom Some(443)"),
                false,
            ),
            ("ssh://git@ssh.github.com/o/r", None, false),
            (
                "git@GitHub.com:O/R.git",
                Some("O/R Scp GithubCom None"),
                false,
            ),
            (
                "https://GITHUB.com/O/R",
                Some("O/R Https GithubCom None"),
                false,
            ),
            (
                "  git@github.com:o/r.git\n",
                Some("o/r Scp GithubCom None"),
                false,
            ),
            ("./o/r", None, false),
            ("/o/r", None, false),
            ("https://github.com/o/r?x=1", None, false),
            ("https://github.com/o/r#readme", None, false),
            ("https://gist.github.com/o/r", None, false),
            ("https://github.example.com/o/r", None, false),
            ("https://notgithub.com/o/r", None, false),
            ("git@gitlab.com:o/r", None, false),
            (
                "git@ssh.github.com:o/r",
                Some("o/r Scp SshGithubCom None"),
                false,
            ),
            ("alice@github.com:o/r", None, false),
            ("ssh://alice@github.com/o/r", None, false),
            ("git://github.com/o/r", None, false),
            ("acme/widget/child", None, false),
            ("-o/r", None, false),
            ("o/r$", None, false),
        ];
        let describe = |remote: Option<GithubRemote>| {
            remote.map(|remote| {
                format!(
                    "{}/{} {:?} {:?} {:?}",
                    remote.owner, remote.repository, remote.spelling, remote.host, remote.port
                )
            })
        };
        for (input, allowed, shorthand_only) in table {
            assert_eq!(
                describe(parse_github_remote(input, Shorthand::Allowed)).as_deref(),
                *allowed,
                "{input:?} with shorthand allowed"
            );
            assert_eq!(
                describe(parse_github_remote(input, Shorthand::Rejected)).as_deref(),
                if *shorthand_only { None } else { *allowed },
                "{input:?} with shorthand rejected"
            );
        }
    }

    #[test]
    fn default_branch_requires_symbolic_head_and_commit() {
        let output = CommandOutput {
            status: 0,
            stdout: b"ref: refs/heads/main\tHEAD\n0123456789012345678901234567890123456789\tHEAD\n"
                .to_vec(),
            stderr: vec![],
        };
        assert_eq!(parse_default_branch(&output).unwrap(), "main");

        let missing_commit = CommandOutput {
            status: 0,
            stdout: b"ref: refs/heads/main\tHEAD\n".to_vec(),
            stderr: vec![],
        };
        assert!(parse_default_branch(&missing_commit).is_err());
    }

    #[test]
    fn network_url_validation_rejects_local_and_helper_endpoints() {
        for url in [
            ".",
            "./repo",
            "/tmp/repo",
            "file:/tmp/repo",
            "file:///tmp/repo",
            "ssh:/example/repo.git",
            "ext::foo",
            "helper::foo",
        ] {
            assert!(validate_network_url(url).is_err(), "accepted {url}");
        }
        for url in [
            "https://github.com/BrokkAi/hel.git",
            "ssh://git@example.com/repo.git",
            "git://example.com/repo.git",
            "git@example.com:org/repo.git",
        ] {
            assert!(validate_network_url(url).is_ok(), "rejected {url}");
        }
    }

    #[test]
    fn display_url_removes_userinfo() {
        assert_eq!(
            display_url("https://user:secret@example.com/repo.git"),
            "https://example.com/repo.git"
        );
        assert_eq!(
            display_url("git@github.com:org/repo.git"),
            "github.com:org/repo.git"
        );
    }
}
