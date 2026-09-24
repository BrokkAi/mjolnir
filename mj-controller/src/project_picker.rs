//! Repository discovery shared by the terminal and authenticated web picker.
//!
//! Callers run this blocking service on a supervised task and supply a
//! cancellable executor with a deadline. No credentials or subprocess output
//! are attached to errors, so the same actionable messages suit either UI.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::Result;
use mj_core::targets::{CommandExecutor, CommandSpec};
use serde::Deserialize;

const MAX_ENTRIES: usize = 100;
const MAX_INPUT_BYTES: usize = 4096;

pub use mj_core::project_picker::{
    ProjectDiscovery, ProjectDiscoveryRequest, ProjectEntry, ProjectEntryKind,
};

#[derive(Debug)]
struct DiscoveryError(&'static str);

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for DiscoveryError {}

/// Only intentionally public sentences cross the HTTP boundary, even if a
/// future implementation adds an error from another source.
pub(crate) fn error_message(error: &anyhow::Error) -> &'static str {
    error
        .downcast_ref::<DiscoveryError>()
        .map_or("Project discovery failed. Retry the request.", |error| {
            error.0
        })
}

pub(crate) fn validate_request(request: &ProjectDiscoveryRequest) -> Result<()> {
    let inputs = match request {
        ProjectDiscoveryRequest::Github { query } => [query.as_str(), ""],
        ProjectDiscoveryRequest::Directory { path, filter } => [path.as_str(), filter.as_str()],
    };
    if inputs.iter().any(|input| input.len() > MAX_INPUT_BYTES) {
        return Err(DiscoveryError(
            "The discovery input is too long; use a shorter query or path.",
        )
        .into());
    }
    if inputs.iter().any(|input| input.contains('\0')) {
        return Err(DiscoveryError(
            "The discovery input contains a NUL character; remove it and retry.",
        )
        .into());
    }
    Ok(())
}

/// Browse immediate subdirectories or the first page of authenticated GitHub
/// results. Directory sources are absolute paths; GitHub sources are owner/repo.
pub fn discover(
    request: &ProjectDiscoveryRequest,
    executor: &impl CommandExecutor,
) -> Result<ProjectDiscovery> {
    validate_request(request)?;
    check_cancelled(executor)?;
    match request {
        ProjectDiscoveryRequest::Github { query } => discover_github(query.trim(), executor),
        ProjectDiscoveryRequest::Directory { path, filter } => {
            discover_directory(path, filter, executor)
        }
    }
}

fn check_cancelled(executor: &impl CommandExecutor) -> Result<()> {
    if executor.cancellation_requested() {
        return Err(DiscoveryError(
            "Project discovery was cancelled or timed out; retry the request.",
        )
        .into());
    }
    Ok(())
}

fn directory_error(error: std::io::Error) -> DiscoveryError {
    DiscoveryError(match error.kind() {
        ErrorKind::NotFound => "That directory no longer exists. Choose another directory.",
        ErrorKind::NotADirectory => "That path is not a directory. Choose a folder to browse.",
        ErrorKind::PermissionDenied => {
            "The directory cannot be read. Check its permissions or choose another directory."
        }
        _ => "The directory could not be read. Check the path and filesystem, then retry.",
    })
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        DiscoveryError("This directory contains a path that is not valid Unicode and cannot be shown in the picker.").into()
    })
}

fn is_repository(path: &Path) -> Result<bool> {
    match fs::metadata(path.join(".git")) {
        Ok(metadata) => Ok(metadata.is_dir() || metadata.is_file()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(directory_error(error).into()),
    }
}

fn discover_directory(
    path: &str,
    filter: &str,
    executor: &impl CommandExecutor,
) -> Result<ProjectDiscovery> {
    let path = mj_core::path_input::expand_local(Path::new(if path.is_empty() { "~" } else { path }))
        .map_err(|_| DiscoveryError("The home directory could not be resolved. Enter an explicit directory path instead of ~ or ~user."))?;
    let directory = fs::canonicalize(path).map_err(directory_error)?;
    let source = path_text(&directory)?;
    let children = fs::read_dir(&directory).map_err(directory_error)?;
    let mut entries = Vec::new();
    if is_repository(&directory)? {
        let basename = directory
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&source);
        entries.push(ProjectEntry {
            name: format!("Use {basename}"),
            source: source.clone(),
            description: "Use this repository".into(),
            kind: ProjectEntryKind::Repository,
        });
    }
    let remaining = MAX_ENTRIES - entries.len();
    let mut selected = BTreeMap::new();
    let mut truncated = false;
    let filter = filter.to_lowercase();
    // Keep memory bounded while sorting independently of read_dir order. The
    // scan is nonrecursive and checks the caller's deadline throughout. Apply
    // the name filter before limiting so omitted folders remain reachable.
    for child in children {
        check_cancelled(executor)?;
        let child = child.map_err(directory_error)?;
        let name = child.file_name();
        if name == ".git"
            || name
                .to_str()
                .is_some_and(|name| !name.to_lowercase().contains(&filter))
        {
            continue;
        }
        let file_type = child.file_type().map_err(directory_error)?;
        let is_dir = if file_type.is_symlink() {
            match fs::metadata(child.path()) {
                Ok(metadata) => metadata.is_dir(),
                Err(error) if error.kind() == ErrorKind::NotFound => false,
                Err(error) => return Err(directory_error(error).into()),
            }
        } else {
            file_type.is_dir()
        };
        if !is_dir {
            continue;
        }
        let name = path_text(Path::new(&name))?;
        selected.insert((name.to_lowercase(), name), child.path());
        if selected.len() > remaining {
            selected.pop_last();
            truncated = true;
        }
    }
    for ((_, name), path) in selected {
        check_cancelled(executor)?;
        let repository = is_repository(&path)?;
        entries.push(ProjectEntry {
            name,
            source: path_text(&path)?,
            description: if repository {
                "Git repository"
            } else {
                "Directory"
            }
            .into(),
            kind: if repository {
                ProjectEntryKind::Repository
            } else {
                ProjectEntryKind::Directory
            },
        });
    }
    check_cancelled(executor)?;
    Ok(ProjectDiscovery {
        entries,
        directory: Some(source),
        parent: directory.parent().map(path_text).transpose()?,
        truncated,
    })
}

#[derive(Deserialize)]
struct GithubRepository {
    full_name: String,
    description: Option<String>,
}

#[derive(Deserialize)]
struct GithubSearch {
    items: Vec<GithubRepository>,
    total_count: usize,
    incomplete_results: bool,
}

fn discover_github(query: &str, executor: &impl CommandExecutor) -> Result<ProjectDiscovery> {
    let endpoint = if query.is_empty() {
        "user/repos"
    } else {
        "search/repositories"
    };
    let mut command = CommandSpec::new(
        "gh",
        [
            "api",
            endpoint,
            "--hostname",
            "github.com",
            "--method",
            "GET",
            "--include",
            "--raw-field",
            "per_page=100",
            "--raw-field",
            "sort=updated",
        ],
    )
    .purpose("discover GitHub repositories");
    if query.is_empty() {
        command.args.extend([
            "--raw-field".into(),
            "direction=desc".into(),
            "--raw-field".into(),
            "visibility=all".into(),
            "--raw-field".into(),
            "affiliation=owner,collaborator,organization_member".into(),
        ]);
    } else {
        // --raw-field treats @, braces, spaces, and qualifiers as literal query
        // data; --field would expand filenames and repository placeholders.
        command.args.extend([
            "--raw-field".into(),
            format!("q={query}"),
            "--raw-field".into(),
            "order=desc".into(),
        ]);
    }
    command.env.insert("GH_PROMPT_DISABLED".into(), "1".into());
    command.env.insert("GH_DEBUG".into(), String::new());
    command.env.insert("NO_COLOR".into(), "1".into());
    let output = executor.execute(&command).map_err(|error| {
        if executor.cancellation_requested() {
            DiscoveryError("Project discovery was cancelled or timed out; retry the request.")
        } else if error.downcast_ref::<std::io::Error>().is_some_and(|error| error.kind() == ErrorKind::NotFound) {
            DiscoveryError("GitHub CLI (gh) is not installed or is not on PATH. Install it on the machine running discovery and retry.")
        } else {
            DiscoveryError("GitHub CLI (gh) could not run. Check its installation and the network connection, then retry.")
        }
    })?;
    check_cancelled(executor)?;
    if output.status != 0 {
        return Err(github_error(output.status, &output.stderr).into());
    }
    let invalid = || {
        DiscoveryError(
            "GitHub returned an unreadable repository list. Update GitHub CLI (gh) and retry.",
        )
    };
    let text = std::str::from_utf8(&output.stdout).map_err(|_| invalid())?;
    let (headers, body) = text
        .split_once("\r\n\r\n")
        .or_else(|| text.split_once("\n\n"))
        .ok_or_else(invalid)?;
    let mut truncated = headers.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("link")
                && value.split(',').any(|link| link.contains("rel=\"next\""))
        })
    });
    let repositories: Vec<GithubRepository> = if query.is_empty() {
        serde_json::from_str(body).map_err(|_| invalid())?
    } else {
        let result: GithubSearch = serde_json::from_str(body).map_err(|_| invalid())?;
        truncated |= result.incomplete_results || result.total_count > result.items.len();
        result.items
    };
    truncated |= repositories.len() > MAX_ENTRIES;
    let entries = repositories
        .into_iter()
        .take(MAX_ENTRIES)
        .map(|repo| ProjectEntry {
            name: repo.full_name.clone(),
            source: repo.full_name,
            description: repo.description.unwrap_or_default(),
            kind: ProjectEntryKind::Repository,
        })
        .collect();
    Ok(ProjectDiscovery {
        entries,
        directory: None,
        parent: None,
        truncated,
    })
}

fn github_error(status: i32, stderr: &[u8]) -> DiscoveryError {
    // Inspect only to select a fixed sentence. Never retain, log, or return
    // raw stderr: gh can include credentials, URLs, or account details there.
    let error = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    DiscoveryError(
        if error.contains("rate limit") || error.contains("http 429") {
            "GitHub's API rate limit was reached. Wait a few minutes and retry."
        } else if status == 4
            || error.contains("gh auth login")
            || error.contains("http 401")
            || error.contains("bad credentials")
            || error.contains("not logged")
        {
            "GitHub login is unavailable or expired. Run gh auth login on the machine running discovery, then retry."
        } else if error.contains("http 403") {
            "GitHub denied access. Check the existing gh login's repository permissions and organization authorization, then retry."
        } else if error.contains("http 422") {
            "GitHub rejected that search. Check the query and its qualifiers, then retry."
        } else {
            "GitHub could not be reached. Check the network connection and GitHub availability, then retry."
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::targets::CommandOutput;
    use std::cell::{Cell, RefCell};

    struct FakeGh {
        output: RefCell<Option<Result<CommandOutput>>>,
        command: RefCell<Option<CommandSpec>>,
    }

    impl FakeGh {
        fn new(output: Result<CommandOutput>) -> Self {
            Self {
                output: RefCell::new(Some(output)),
                command: RefCell::new(None),
            }
        }

        fn json(body: serde_json::Value, next: bool) -> Self {
            let headers = if next {
                "Link: <https://api.github.com/user/repos?page=2>; rel=\"next\"\r\n"
            } else {
                ""
            };
            Self::new(Ok(CommandOutput {
                status: 0,
                stdout: format!("HTTP/2.0 200 OK\r\n{headers}\r\n{body}").into_bytes(),
                stderr: Vec::new(),
            }))
        }
    }

    impl CommandExecutor for FakeGh {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.command.replace(Some(command.clone()));
            self.output.borrow_mut().take().expect("one GitHub request")
        }
    }

    struct NoCommands;
    impl CommandExecutor for NoCommands {
        fn execute(&self, _: &CommandSpec) -> Result<CommandOutput> {
            panic!("directory discovery must not spawn a command")
        }
    }

    fn browse(path: &Path) -> ProjectDiscovery {
        discover(
            &ProjectDiscoveryRequest::Directory {
                path: path_text(path).unwrap(),
                filter: String::new(),
            },
            &NoCommands,
        )
        .unwrap()
    }

    #[test]
    fn browsing_lists_immediate_folders_and_identifies_git_directories_and_files() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("projects with spaces");
        fs::create_dir_all(root.join("zeta checkout/.git")).unwrap();
        fs::create_dir_all(root.join("Alpha folder/nested repository/.git")).unwrap();
        fs::create_dir_all(root.join("beta worktree")).unwrap();
        fs::write(root.join("beta worktree/.git"), "gitdir: elsewhere").unwrap();
        fs::write(root.join("regular file"), "not a folder").unwrap();
        let answer = browse(&root);
        assert_eq!(
            answer.directory,
            Some(path_text(&root.canonicalize().unwrap()).unwrap())
        );
        assert_eq!(
            answer.parent,
            Some(path_text(&temporary.path().canonicalize().unwrap()).unwrap())
        );
        assert!(!answer.truncated);
        assert_eq!(
            answer
                .entries
                .iter()
                .map(|entry| (entry.name.as_str(), &entry.kind))
                .collect::<Vec<_>>(),
            vec![
                ("Alpha folder", &ProjectEntryKind::Directory),
                ("beta worktree", &ProjectEntryKind::Repository),
                ("zeta checkout", &ProjectEntryKind::Repository),
            ]
        );
        assert_eq!(
            answer.entries[1].source,
            path_text(&root.canonicalize().unwrap().join("beta worktree")).unwrap()
        );
        assert_eq!(
            browse(Path::new(&answer.entries[0].source)).entries[0].name,
            "nested repository"
        );
    }

    #[test]
    fn browsing_a_repository_offers_it_and_keeps_children_browsable() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join(".git"),
            "gitdir: ../main/.git/worktrees/other",
        )
        .unwrap();
        fs::create_dir(root.path().join("src")).unwrap();
        let answer = browse(root.path());
        assert_eq!(answer.entries.len(), 2);
        assert!(answer.entries[0].name.starts_with("Use "));
        assert_eq!(answer.entries[0].kind, ProjectEntryKind::Repository);
        assert_eq!(answer.entries[0].source, answer.directory.unwrap());
        assert_eq!(answer.entries[1].kind, ProjectEntryKind::Directory);
    }

    #[test]
    fn browsing_sorts_before_truncation_and_reserves_the_current_repository() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        for index in (0..=MAX_ENTRIES).rev() {
            fs::create_dir(root.path().join(format!("folder-{index:03}"))).unwrap();
        }
        let answer = browse(root.path());
        assert!(answer.truncated);
        assert_eq!(answer.entries.len(), MAX_ENTRIES);
        assert_eq!(answer.entries[1].name, "folder-000");
        assert_eq!(answer.entries.last().unwrap().name, "folder-098");
    }

    #[test]
    fn browsing_reports_missing_directories_and_file_paths() {
        let root = tempfile::tempdir().unwrap();
        for (name, expected) in [("missing", "no longer exists"), ("file", "not a directory")] {
            if name == "file" {
                fs::write(root.path().join(name), "file").unwrap();
            }
            let error = discover(
                &ProjectDiscoveryRequest::Directory {
                    path: path_text(&root.path().join(name)).unwrap(),
                    filter: String::new(),
                },
                &NoCommands,
            )
            .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
            assert!(!format!("{error:#}").contains(&path_text(root.path()).unwrap()));
        }
    }

    #[test]
    fn filtering_reaches_omitted_folders_and_keeps_the_current_repository() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        for index in 0..MAX_ENTRIES {
            fs::create_dir(root.path().join(format!("folder-{index}"))).unwrap();
        }
        fs::create_dir(root.path().join("ZZ matching project")).unwrap();
        assert!(browse(root.path()).truncated);
        let request = ProjectDiscoveryRequest::Directory {
            path: path_text(root.path()).unwrap(),
            filter: "MATCHING".into(),
        };
        let answer = discover(&request, &NoCommands).unwrap();
        assert!(!answer.truncated);
        assert_eq!(answer.entries.len(), 2);
        assert!(answer.entries[0].name.starts_with("Use "));
        assert_eq!(answer.entries[1].name, "ZZ matching project");
        let legacy: ProjectDiscoveryRequest = serde_json::from_value(serde_json::json!({
            "kind": "directory", "path": "/work"
        }))
        .unwrap();
        assert_eq!(
            legacy,
            ProjectDiscoveryRequest::Directory {
                path: "/work".into(),
                filter: String::new()
            }
        );
    }

    #[test]
    fn directory_scan_observes_the_callers_cancellation() {
        struct CancelDuringScan(Cell<usize>);
        impl CommandExecutor for CancelDuringScan {
            fn execute(&self, _: &CommandSpec) -> Result<CommandOutput> {
                unreachable!()
            }
            fn cancellation_requested(&self) -> bool {
                let checks = self.0.get();
                self.0.set(checks + 1);
                checks > 2
            }
        }
        let root = tempfile::tempdir().unwrap();
        for index in 0..10 {
            fs::create_dir(root.path().join(index.to_string())).unwrap();
        }
        let error = discover(
            &ProjectDiscoveryRequest::Directory {
                path: path_text(root.path()).unwrap(),
                filter: String::new(),
            },
            &CancelDuringScan(Cell::new(0)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("cancelled or timed out"));
    }

    #[test]
    fn github_lists_private_accessible_repositories_in_updated_order() {
        let executor = FakeGh::json(
            serde_json::json!([
                {"full_name": "team/recent-private", "description": "Private project", "private": true},
                {"full_name": "person/older", "description": null}
            ]),
            false,
        );
        let answer = discover(
            &ProjectDiscoveryRequest::Github {
                query: String::new(),
            },
            &executor,
        )
        .unwrap();
        assert_eq!(
            answer
                .entries
                .iter()
                .map(|entry| entry.source.as_str())
                .collect::<Vec<_>>(),
            ["team/recent-private", "person/older"]
        );
        assert_eq!(answer.entries[0].description, "Private project");
        assert_eq!(answer.entries[1].description, "");
        assert_eq!(answer.directory, None);
        assert_eq!(answer.parent, None);
        assert!(!answer.truncated);
        let command = executor.command.borrow();
        let command = command.as_ref().unwrap();
        assert_eq!(command.program, "gh");
        for arg in [
            "user/repos",
            "sort=updated",
            "direction=desc",
            "per_page=100",
            "visibility=all",
            "affiliation=owner,collaborator,organization_member",
        ] {
            assert!(
                command.args.iter().any(|value| value == arg),
                "missing {arg}"
            );
        }
        assert!(!command.args.iter().any(|value| value == "--paginate"));
    }

    #[test]
    fn github_search_passes_queries_literally_and_reports_more_or_incomplete_results() {
        for (total_count, incomplete_results, truncated) in
            [(1, false, false), (101, false, true), (1, true, true)]
        {
            let executor = FakeGh::json(
                serde_json::json!({
                    "total_count": total_count, "incomplete_results": incomplete_results,
                    "items": [{"full_name": "team/private", "description": null, "private": true}]
                }),
                false,
            );
            let query = "@private {owner} in:name org:team --jq secret";
            let answer = discover(
                &ProjectDiscoveryRequest::Github {
                    query: query.into(),
                },
                &executor,
            )
            .unwrap();
            assert_eq!(answer.entries[0].source, "team/private");
            assert_eq!(answer.truncated, truncated);
            let command = executor.command.borrow();
            let command = command.as_ref().unwrap();
            assert!(command.args.iter().any(|arg| arg == "search/repositories"));
            assert!(
                command
                    .args
                    .windows(2)
                    .any(|args| args == ["--raw-field", &format!("q={query}")])
            );
        }
    }

    #[test]
    fn github_uses_pagination_headers_and_caps_the_returned_entries() {
        for (count, next, truncated) in [(100, false, false), (100, true, true), (101, false, true)]
        {
            let repos = (0..count).map(|index| serde_json::json!({"full_name": format!("owner/repo-{index}"), "description": null})).collect::<Vec<_>>();
            let executor = FakeGh::json(serde_json::json!(repos), next);
            let answer = discover(
                &ProjectDiscoveryRequest::Github {
                    query: String::new(),
                },
                &executor,
            )
            .unwrap();
            assert_eq!(answer.entries.len(), count.min(MAX_ENTRIES));
            assert_eq!(answer.truncated, truncated);
        }
    }

    #[test]
    fn github_failures_are_actionable_and_never_expose_subprocess_output() {
        let request = ProjectDiscoveryRequest::Github {
            query: String::new(),
        };
        for (status, stderr, expected) in [
            (4, "gh auth login secret-token", "gh auth login"),
            (
                1,
                "Bad credentials (HTTP 401) secret-token",
                "gh auth login",
            ),
            (
                1,
                "failed connection https://secret-token@example.invalid",
                "network connection",
            ),
            (1, "API rate limit exceeded secret-token", "rate limit"),
            (1, "HTTP 403 secret-token", "permissions"),
            (1, "HTTP 422 secret-token", "qualifiers"),
        ] {
            let executor = FakeGh::new(Ok(CommandOutput {
                status,
                stdout: b"secret-token".to_vec(),
                stderr: stderr.as_bytes().to_vec(),
            }));
            let error = discover(&request, &executor).unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
            assert!(!format!("{error:#?}").contains("secret-token"));
            assert!(!error_message(&error).contains("secret-token"));
        }
        let executor = FakeGh::new(Err(std::io::Error::new(
            ErrorKind::NotFound,
            "secret-token",
        )
        .into()));
        let error = discover(&request, &executor).unwrap_err();
        assert!(error.to_string().contains("Install it"));
        assert!(!format!("{error:#?}").contains("secret-token"));
        let executor = FakeGh::new(Ok(CommandOutput {
            status: 0,
            stdout: b"secret-token".to_vec(),
            stderr: Vec::new(),
        }));
        assert!(
            discover(&request, &executor)
                .unwrap_err()
                .to_string()
                .contains("unreadable")
        );
        assert_eq!(
            error_message(&anyhow::anyhow!("secret-token")),
            "Project discovery failed. Retry the request."
        );
    }

    #[test]
    fn wire_contract_round_trips_and_rejects_unbounded_input_before_work() {
        let request: ProjectDiscoveryRequest =
            serde_json::from_str(r#"{"kind":"github","query":"private org:team"}"#).unwrap();
        assert_eq!(
            request,
            ProjectDiscoveryRequest::Github {
                query: "private org:team".into()
            }
        );
        assert_eq!(
            serde_json::to_value(ProjectEntryKind::Repository).unwrap(),
            "repository"
        );
        assert_eq!(
            serde_json::to_value(ProjectEntryKind::Directory).unwrap(),
            "directory"
        );
        for input in ["x".repeat(MAX_INPUT_BYTES + 1), "a\0b".into()] {
            assert!(
                discover(
                    &ProjectDiscoveryRequest::Github {
                        query: input.clone()
                    },
                    &NoCommands
                )
                .is_err()
            );
            assert!(
                discover(
                    &ProjectDiscoveryRequest::Directory {
                        path: input,
                        filter: String::new()
                    },
                    &NoCommands
                )
                .is_err()
            );
        }
    }
}
