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
                crate::local_git::resolve_local_repository(path, executor)
                    .with_context(|| format!("repository {:?} needs valid default network Git remotes for an isolated session; configure a remote or use a raw local session", repository.id))
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
    command
        .env
        .insert("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned());
    command
        .env
        .insert("GIT_NO_LAZY_FETCH".to_owned(), "1".to_owned());
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        command.env.insert(
            "GIT_SSH_COMMAND".to_owned(),
            "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=15"
                .to_owned(),
        );
    }

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
    if value == "." || value == ".." || value.starts_with("./") || value.starts_with("../") {
        bail!("Git URL {} is a local repository path", display_url(value));
    }
    if is_windows_absolute_path(value) || value.starts_with('/') || value.starts_with('~') {
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
        && source.split('/').count() == 2
        && source.split('/').all(|part| !part.is_empty());
    let expanded = if shorthand {
        let path = source.strip_suffix(".git").unwrap_or(source);
        format!("https://github.com/{path}.git")
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

fn is_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        && value.bytes().any(|byte| byte != b'0')
}

fn is_windows_absolute_path(value: &str) -> bool {
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
        let source = resolve_repository(&repository, &NoopExecutor).unwrap();
        assert_eq!(source.fetch_url, "https://github.com/BrokkAi/hel.git");
        assert_eq!(source.push_urls, ["https://github.com/BrokkAi/hel.git"]);
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

    struct NoopExecutor;

    impl CommandExecutor for NoopExecutor {
        fn execute(&self, _command: &CommandSpec) -> Result<CommandOutput> {
            bail!("unexpected command")
        }
    }
}
