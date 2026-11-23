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

/// Finalize a managed checkout using its recorded immutable launch base.
/// The caller supplies the command boundary (local Git or a target executor).
/// Only legacy restores whose base object is already absent skip retention;
/// their recorded markers retain the same meaning as before base refs existed.
pub fn finalize_managed_checkout(
    base: &str,
    retain_base: bool,
    mut run: impl FnMut(&[&str]) -> Result<()>,
) -> Result<()> {
    anyhow::ensure!(
        !base.is_empty(),
        "managed checkout has no immutable base commit"
    );
    // Git config is not a GC root when the launch commit becomes orphaned.
    if retain_base {
        run(&["update-ref", crate::targets::HistoryCutoff::BASE_REF, base])?;
    }
    for (key, value) in [
        ("push.default", "current"),
        ("push.autoSetupRemote", "true"),
        ("remote.pushDefault", "origin"),
        ("remote.origin.mirror", "false"),
        ("mj.baseCommit", base),
        ("mj.remoteWorkspace", "true"),
    ] {
        run(&["config", "--local", "--replace-all", key, value])?;
    }
    Ok(())
}

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

impl NetworkGitSource {
    pub fn normalize(&mut self) {
        self.fetch_url = normalize_network_url(&self.fetch_url, Shorthand::Rejected);
        let mut seen = std::collections::HashSet::new();
        self.push_urls = self
            .push_urls
            .iter()
            .map(|url| normalize_network_url(url, Shorthand::Rejected))
            .filter(|url| seen.insert(url.clone()))
            .collect();
    }
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
    network_url(url, false).map(drop)
}

/// Parse a project source and remove HTTP userinfo before it can be saved.
/// Return the accepted spelling verbatim except for its HTTP credentials.
pub fn project_source(source: &str, shorthand: Shorthand) -> Result<std::borrow::Cow<'_, str>> {
    let source = source.trim();
    if parse_github_remote(source, shorthand)
        .is_some_and(|remote| remote.spelling == GithubSpelling::Shorthand)
    {
        return Ok(source.into());
    }
    network_url(source, true)
}

/// Keep loading all sources accepted by the previous config validator.
pub(crate) fn validate_configured_project_source(source: &str) -> Result<()> {
    match project_source(source, Shorthand::Allowed) {
        Ok(_) => Ok(()),
        Err(_) if was_configured_github_source(source) => Ok(()),
        Err(error) => Err(error),
    }
}

// Validation at checkout still rejects unusable sources.
fn was_configured_github_source(source: &str) -> bool {
    let source = source.trim();
    if source.is_empty() || source.starts_with('-') || source.chars().any(char::is_whitespace) {
        return false;
    }
    if parse_github_remote(source, Shorthand::Allowed).is_some() {
        return true;
    }
    let repository_path = source
        .strip_prefix("https://github.com/")
        .or_else(|| source.strip_prefix("git@github.com:"))
        .or_else(|| source.strip_prefix("ssh://git@github.com/"))
        .unwrap_or(source);
    let mut parts = repository_path.trim_end_matches(".git").split('/');
    matches!((parts.next(), parts.next(), parts.next()), (Some(owner), Some(repository), None) if !owner.is_empty() && !repository.is_empty())
}

/// The SSH login in an accepted Git URL. An empty login uses SSH configuration.
pub fn ssh_login(source: &str) -> Option<String> {
    if source.contains("://") {
        let url = Url::parse(source).ok()?;
        return (url.scheme() == "ssh").then(|| url.username().to_owned());
    }
    let (authority, _) = source.split_once(':')?;
    Some(
        authority
            .rsplit_once('@')
            .map_or("", |(user, _)| user)
            .to_owned(),
    )
}

/// Label derived from the repository path, independent of HTTP query data.
pub fn project_source_name(source: &str) -> Option<String> {
    let remote = parse_github_remote(source, Shorthand::Allowed);
    if let Some(remote) = remote {
        return Some(remote.repository);
    }
    let parsed = if source.contains("://") {
        Url::parse(source).ok()
    } else {
        None
    };
    let path = parsed.as_ref().map_or(source, Url::path);
    path.trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit(['/', ':'])
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

pub const HTTP_USERINFO_REMOVED: &str =
    "Removed credentials from the HTTP Git URL. Configure Git credentials on the target.";

fn network_url(url: &str, strip_http_userinfo: bool) -> Result<std::borrow::Cow<'_, str>> {
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
        if strip_http_userinfo && matches!(scheme, "http" | "https") {
            // Keep the path, query, fragment and escape spelling exactly as
            // entered. The parsed URL establishes its transport and host.
            let (_, endpoint) = value.split_once("://").expect("validated URL scheme");
            let authority = endpoint.trim_start_matches(['/', '\\']);
            let start = value.len() - authority.len();
            let authority = authority
                .split(['/', '?', '#', '\\'])
                .next()
                .unwrap_or_default();
            if let Some(at) = authority.rfind('@') {
                return Ok(format!("{}{}", &value[..start], &value[start + at + 1..]).into());
            }
        }
        return Ok(value.into());
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
    Ok(value.into())
}

/// Whether a Git source names the host's filesystem or loopback network.
/// Bundle validation rejects local paths and file URLs before checkout.
pub fn is_host_only_git_source(source: &str) -> bool {
    let source = source.trim();
    if crate::path_completion::looks_like_path(source) {
        return true;
    }
    if source.contains("://")
        || source
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:"))
    {
        return Url::parse(source).is_ok_and(|url| {
            url.scheme() == "file" || url.host_str().is_some_and(is_loopback_git_host)
        });
    }
    let Some((authority, _)) = source.split_once(':') else {
        return true; // Git interprets a bare name as a relative filesystem path.
    };
    if authority.contains(['/', '\\']) {
        return true;
    }
    let host = authority.rsplit('@').next().unwrap_or(authority);
    if host.starts_with('[') {
        // Git recognizes the colon after a bracketed IPv6 host as the path separator.
        let endpoint = &source[authority.len() - host.len()..];
        return endpoint
            .split_once("]:")
            .is_some_and(|(host, _)| is_loopback_git_host(host));
    }
    is_loopback_git_host(host)
}

fn is_loopback_git_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.trim_end_matches('.').eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
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

pub fn expand_github_source(source: &str) -> Result<String> {
    let canonical = canonical_github_https_url(source, Shorthand::Allowed);
    let source = source.trim();
    let shorthand = !source.contains("://")
        && !source.contains('@')
        && !source.contains(':')
        && source.contains('/');
    let expanded = if let Some(canonical) = canonical {
        canonical
    } else if shorthand {
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

pub fn canonical_github_https_url(source: &str, shorthand: Shorthand) -> Option<String> {
    let remote = parse_github_remote(source, shorthand)?;
    remote.is_default_endpoint().then(|| {
        format!(
            "https://github.com/{}/{}.git",
            remote.owner, remote.repository
        )
    })
}

pub fn normalize_network_url(source: &str, shorthand: Shorthand) -> String {
    canonical_github_https_url(source, shorthand).unwrap_or_else(|| source.trim().to_owned())
}

pub fn normalize_project_source(source: &str, shorthand: Shorthand) -> String {
    match parse_github_remote(source, shorthand).filter(|remote| remote.is_default_endpoint()) {
        Some(remote) if remote.spelling == GithubSpelling::Shorthand => {
            format!("{}/{}", remote.owner, remote.repository)
        }
        Some(remote) => format!(
            "https://github.com/{}/{}.git",
            remote.owner, remote.repository
        ),
        None => source.trim().to_owned(),
    }
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
    /// `git://github.com/owner/repository`. GitHub stopped serving the
    /// protocol in 2022, but an older remote may still be written this way.
    Git,
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
    /// repository over another route, and identities keep them apart, as they
    /// do for `git://`, which was never a GitHub identity.
    pub fn is_default_endpoint(&self) -> bool {
        self.host == GithubHost::GithubCom
            && self.port.is_none()
            && self.spelling != GithubSpelling::Git
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
            "git" => {
                if !host_name.eq_ignore_ascii_case("github.com")
                    || url.port().is_some()
                    || !url.username().is_empty()
                    || url.password().is_some()
                {
                    return None;
                }
                (GithubSpelling::Git, GithubHost::GithubCom, None)
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

/// `git -c` arguments that answer credential requests for GitHub's web hosts
/// with the token in the child's `GH_TOKEN`. Git matches a scoped helper
/// against the server it actually contacts, so a host whose Git
/// configuration rewrites github.com (`url.*.insteadOf`) to a mirror never
/// sees the token, and the host's own helpers still answer for the mirror.
/// The empty value first clears any host helper scoped to the same URL.
pub fn github_token_credential_config() -> Vec<String> {
    ["github.com", "gist.github.com"]
        .into_iter()
        .flat_map(|host| {
            [
                "-c".to_owned(),
                format!("credential.https://{host}.helper="),
                "-c".to_owned(),
                format!(
                    "credential.https://{host}.helper=!f() {{ if [ \"$1\" = get ]; then printf '%s\\n' username=x-access-token \"password=$GH_TOKEN\"; fi; }}; f"
                ),
            ]
        })
        .collect()
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
    fn github_sources_normalize_without_collapsing_alternate_routes() {
        assert_eq!(
            expand_github_source("git@github.com:BrokkAi/mjolnir.git").unwrap(),
            "https://github.com/BrokkAi/mjolnir.git"
        );
        assert_eq!(
            normalize_project_source("https://github.com/BrokkAi/mjolnir", Shorthand::Allowed),
            "https://github.com/BrokkAi/mjolnir.git"
        );
        assert_eq!(
            normalize_project_source("BrokkAi/mjolnir.git", Shorthand::Allowed),
            "BrokkAi/mjolnir"
        );
        let mut source = NetworkGitSource {
            fetch_url: "git@github.com:BrokkAi/mjolnir.git".into(),
            push_urls: vec![
                "https://github.com/BrokkAi/mjolnir".into(),
                "git@github.com:BrokkAi/mjolnir.git".into(),
                "ssh://git@ssh.github.com:443/BrokkAi/mjolnir.git".into(),
                "ssh://git@github.com:22/BrokkAi/mjolnir.git".into(),
                "https://git.example/repo".into(),
            ],
        };
        source.normalize();
        assert_eq!(source.fetch_url, "https://github.com/BrokkAi/mjolnir.git");
        assert_eq!(
            source.push_urls,
            vec![
                "https://github.com/BrokkAi/mjolnir.git",
                "ssh://git@ssh.github.com:443/BrokkAi/mjolnir.git",
                "ssh://git@github.com:22/BrokkAi/mjolnir.git",
                "https://git.example/repo"
            ]
        );
    }

    #[test]
    fn github_shorthand_expands_to_https() {
        let repository = ProjectRepository {
            id: "repo".into(),
            github: Some("BrokkAi/hel".into()),
            local: None,
            destination: "repo".into(),
            git_ref: None,
            mbx_namespace: None,
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
            (
                "git://github.com/o/r",
                Some("o/r Git GithubCom None"),
                false,
            ),
            ("git://github.com:9418/o/r", None, false),
            ("git://gitlab.com/o/r", None, false),
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
            "git@example.com:path@[::1]:repo.git",
        ] {
            assert!(validate_network_url(url).is_ok(), "rejected {url}");
            assert!(
                !is_host_only_git_source(url),
                "classified {url} as host-only"
            );
        }
        for url in [
            "git://127.192.1.9/repo.git",
            "ssh://git@[::1]/repo.git",
            "git@LOCALHOST:repo.git",
        ] {
            assert!(validate_network_url(url).is_ok(), "rejected {url}");
            assert!(
                is_host_only_git_source(url),
                "missed host-only source {url}"
            );
        }
        for url in ["file:///tmp/repo", "relative/repo", r"C:\git\repo"] {
            assert!(validate_network_url(url).is_err(), "accepted {url}");
            assert!(
                is_host_only_git_source(url),
                "missed host-only source {url}"
            );
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
