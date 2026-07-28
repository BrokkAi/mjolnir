//! Durable, typed results produced by an agent turn.
//!
//! Pull requests are the first supported artifact. Detection is deliberately
//! conservative: callers must first establish that a successful tool was a PR
//! creation action, or that final prose explicitly says a PR was created.

use serde_json::{Map, Value};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestArtifact {
    pub repository: String,
    pub number: u64,
    pub url: String,
    pub title: Option<String>,
    pub draft: Option<bool>,
    pub state: Option<String>,
}

impl PullRequestArtifact {
    pub fn short_label(&self) -> String {
        format!("{}#{}", self.repository, self.number)
    }

    pub fn status_label(&self) -> Option<&str> {
        match self.draft {
            Some(true) => Some("draft"),
            Some(false) => self.state.as_deref().or(Some("open")),
            None => self.state.as_deref(),
        }
    }

    pub fn summary(&self) -> String {
        let mut summary = format!("Pull request created · {}", self.short_label());
        if let Some(status) = self.status_label() {
            summary.push_str(" · ");
            summary.push_str(status);
        }
        summary
    }

    pub(crate) fn merge_from(&mut self, other: Self) {
        if self.title.is_none() {
            self.title = other.title;
        }
        if self.draft.is_none() {
            self.draft = other.draft;
        }
        if self.state.is_none() {
            self.state = other.state;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnArtifactKind {
    PullRequest(PullRequestArtifact),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnArtifact {
    pub session_id: Option<String>,
    pub prompt_index: Option<usize>,
    pub source_tool_call_id: Option<String>,
    pub kind: TurnArtifactKind,
}

impl TurnArtifact {
    pub fn pull_request(&self) -> &PullRequestArtifact {
        match &self.kind {
            TurnArtifactKind::PullRequest(pull_request) => pull_request,
        }
    }
}

pub fn is_pull_request_creation_tool(
    title: &str,
    raw_input: Option<&Value>,
    meta: Option<&Value>,
) -> bool {
    let mut identity_evidence = vec![title.to_string()];
    if let Some(meta) = meta {
        collect_json_strings(meta, &mut identity_evidence);
    }
    let identity_evidence = identity_evidence.join(" ").to_ascii_lowercase();
    let identity_matches = [
        "create_pull_request",
        "create pull request",
        "create a pull request",
        "create draft pull request",
        "create a draft pull request",
        "gh pr create",
    ]
    .iter()
    .any(|needle| identity_evidence.contains(needle));
    if identity_matches {
        return true;
    }

    let mut input_evidence = Vec::new();
    if let Some(raw_input) = raw_input {
        collect_json_strings(raw_input, &mut input_evidence);
    }
    let input_evidence = input_evidence.join(" ").to_ascii_lowercase();
    input_evidence.contains("gh pr create") || input_evidence.contains("create_pull_request")
}

pub fn pull_requests_from_tool_result(
    raw_output: Option<&Value>,
    meta: Option<&Value>,
    display_text: &str,
) -> Vec<PullRequestArtifact> {
    let mut pull_requests = Vec::new();
    if let Some(raw_output) = raw_output {
        collect_structured_pull_requests(raw_output, &mut pull_requests);
    }
    if let Some(meta) = meta {
        collect_structured_pull_requests(meta, &mut pull_requests);
    }
    pull_requests.extend(pull_requests_from_text(display_text));
    deduplicate_pull_requests(pull_requests)
}

pub fn created_pull_requests_from_prose(text: &str) -> Vec<PullRequestArtifact> {
    let lower = text.to_ascii_lowercase();
    if [
        "no pull request created",
        "pull request was not created",
        "did not create a pull request",
        "could not create a pull request",
        "failed to create a pull request",
        "pull request wasn't created",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        return Vec::new();
    }
    if ![
        "pull request created",
        "created pull request",
        "created a pull request",
        "created the pull request",
        "opened a pull request",
        "opened the pull request",
        "pr created",
        "created a draft pr",
        "created draft pr",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        return Vec::new();
    }

    let draft = lower.contains("draft");
    pull_requests_from_text(text)
        .into_iter()
        .map(|mut pull_request| {
            if draft {
                pull_request.draft = Some(true);
            }
            pull_request
        })
        .collect()
}

fn collect_structured_pull_requests(value: &Value, output: &mut Vec<PullRequestArtifact>) {
    match value {
        Value::Object(object) => {
            if let Some(pull_request) = pull_request_from_object(object) {
                output.push(pull_request);
            }
            for value in object.values() {
                collect_structured_pull_requests(value, output);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_structured_pull_requests(value, output);
            }
        }
        Value::String(text) => output.extend(pull_requests_from_text(text)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn pull_request_from_object(object: &Map<String, Value>) -> Option<PullRequestArtifact> {
    let parsed_url = [
        "html_url",
        "htmlUrl",
        "url",
        "pull_request_url",
        "pullRequestUrl",
    ]
    .iter()
    .filter_map(|key| object.get(*key).and_then(Value::as_str))
    .find_map(parse_structured_pull_url);

    let (repository, number, url) = if let Some(parsed) = parsed_url {
        (parsed.repository, parsed.number, parsed.url)
    } else {
        let repository = repository_from_object(object)?;
        let number = number_from_object(object)?;
        let url = format!("https://github.com/{repository}/pull/{number}");
        (repository, number, url)
    };

    Some(PullRequestArtifact {
        repository,
        number,
        url,
        title: string_field(object, &["title", "pull_request_title", "pullRequestTitle"]),
        draft: bool_field(object, &["draft", "is_draft", "isDraft"]),
        state: string_field(object, &["state", "status"]),
    })
}

fn repository_from_object(object: &Map<String, Value>) -> Option<String> {
    for key in [
        "repository_full_name",
        "repositoryFullName",
        "repo_full_name",
        "repoFullName",
        "nameWithOwner",
    ] {
        if let Some(repository) = object.get(key).and_then(Value::as_str)
            && valid_repository(repository)
        {
            return Some(repository.to_string());
        }
    }

    if let Some(repository) = object.get("repository") {
        match repository {
            Value::String(repository) if valid_repository(repository) => {
                return Some(repository.clone());
            }
            Value::Object(repository) => {
                if let Some(repository) =
                    string_field(repository, &["full_name", "fullName", "nameWithOwner"])
                    && valid_repository(&repository)
                {
                    return Some(repository);
                }
            }
            _ => {}
        }
    }

    object
        .get("base")
        .and_then(Value::as_object)
        .and_then(|base| base.get("repo"))
        .and_then(Value::as_object)
        .and_then(|repo| string_field(repo, &["full_name", "fullName", "nameWithOwner"]))
        .filter(|repository| valid_repository(repository))
}

fn number_from_object(object: &Map<String, Value>) -> Option<u64> {
    for key in [
        "number",
        "pull_number",
        "pullNumber",
        "pr_number",
        "prNumber",
    ] {
        let Some(value) = object.get(key) else {
            continue;
        };
        let number = value
            .as_u64()
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()));
        if number.is_some_and(|number| number > 0) {
            return number;
        }
    }
    None
}

fn string_field(object: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .map(str::to_string)
}

fn bool_field(object: &Map<String, Value>, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|key| {
        let value = object.get(*key)?;
        value
            .as_bool()
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
    })
}

fn collect_json_strings(value: &Value, output: &mut Vec<String>) {
    match value {
        Value::String(text) => output.push(text.clone()),
        Value::Array(values) => {
            for value in values {
                collect_json_strings(value, output);
            }
        }
        Value::Object(object) => {
            for (key, value) in object {
                output.push(key.clone());
                collect_json_strings(value, output);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn pull_requests_from_text(text: &str) -> Vec<PullRequestArtifact> {
    const PREFIX: &str = "https://github.com/";
    let mut pull_requests = Vec::new();
    let mut offset = 0;
    while let Some(relative) = text[offset..].find(PREFIX) {
        let start = offset + relative;
        let candidate = text[start..]
            .split(|character: char| {
                character.is_whitespace()
                    || matches!(
                        character,
                        '"' | '\'' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';'
                    )
            })
            .next()
            .unwrap_or_default()
            .trim_end_matches(['.', ':', '!', '?']);
        if let Some(pull_request) = parse_github_pull_url(candidate) {
            pull_requests.push(pull_request);
        }
        offset = start.saturating_add(PREFIX.len());
    }
    deduplicate_pull_requests(pull_requests)
}

fn parse_structured_pull_url(candidate: &str) -> Option<PullRequestArtifact> {
    parse_github_pull_url(candidate).or_else(|| parse_github_api_pull_url(candidate))
}

fn parse_github_pull_url(candidate: &str) -> Option<PullRequestArtifact> {
    let url = Url::parse(candidate).ok()?;
    if url.scheme() != "https" || !url.host_str()?.eq_ignore_ascii_case("github.com") {
        return None;
    }
    let segments = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.len() != 4 || segments[2] != "pull" {
        return None;
    }
    pull_request_from_segments(segments[0], segments[1], segments[3])
}

fn parse_github_api_pull_url(candidate: &str) -> Option<PullRequestArtifact> {
    let url = Url::parse(candidate).ok()?;
    if url.scheme() != "https" || !url.host_str()?.eq_ignore_ascii_case("api.github.com") {
        return None;
    }
    let segments = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.len() != 5 || segments[0] != "repos" || segments[3] != "pulls" {
        return None;
    }
    pull_request_from_segments(segments[1], segments[2], segments[4])
}

fn pull_request_from_segments(
    owner: &str,
    repository_name: &str,
    number: &str,
) -> Option<PullRequestArtifact> {
    if !valid_repository_part(owner) || !valid_repository_part(repository_name) {
        return None;
    }
    let number = number.parse::<u64>().ok().filter(|number| *number > 0)?;
    let repository = format!("{owner}/{repository_name}");
    Some(PullRequestArtifact {
        url: format!("https://github.com/{repository}/pull/{number}"),
        repository,
        number,
        title: None,
        draft: None,
        state: None,
    })
}

fn valid_repository(repository: &str) -> bool {
    let mut parts = repository.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(owner), Some(name), None)
            if valid_repository_part(owner) && valid_repository_part(name)
    )
}

fn valid_repository_part(part: &str) -> bool {
    !part.is_empty()
        && part
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn deduplicate_pull_requests(pull_requests: Vec<PullRequestArtifact>) -> Vec<PullRequestArtifact> {
    let mut deduplicated: Vec<PullRequestArtifact> = Vec::new();
    for pull_request in pull_requests {
        if let Some(existing) = deduplicated
            .iter_mut()
            .find(|existing| existing.url == pull_request.url)
        {
            existing.merge_from(pull_request);
        } else {
            deduplicated.push(pull_request);
        }
    }
    deduplicated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_fallback_accepts_only_pull_urls() {
        let text = "\
created https://github.com/BrokkAi/mjolnir/pull/487.
ignore https://github.com/BrokkAi/mjolnir/issues/487
ignore https://github.com/BrokkAi/mjolnir/commit/abc
ignore https://github.com/BrokkAi/mjolnir/compare/a...b";
        let pull_requests = pull_requests_from_text(text);

        assert_eq!(pull_requests.len(), 1);
        assert_eq!(pull_requests[0].repository, "BrokkAi/mjolnir");
        assert_eq!(pull_requests[0].number, 487);
        assert_eq!(
            pull_requests[0].url,
            "https://github.com/BrokkAi/mjolnir/pull/487"
        );
    }

    #[test]
    fn structured_result_prefers_metadata_and_converts_api_url() {
        let raw_output = serde_json::json!({
            "pull_request": {
                "url": "https://api.github.com/repos/BrokkAi/mjolnir/pulls/512",
                "number": 512,
                "title": "surface durable artifacts",
                "draft": true,
                "state": "open"
            }
        });
        let pull_requests = pull_requests_from_tool_result(Some(&raw_output), None, "");

        assert_eq!(
            pull_requests,
            vec![PullRequestArtifact {
                repository: "BrokkAi/mjolnir".to_string(),
                number: 512,
                url: "https://github.com/BrokkAi/mjolnir/pull/512".to_string(),
                title: Some("surface durable artifacts".to_string()),
                draft: Some(true),
                state: Some("open".to_string()),
            }]
        );
    }

    #[test]
    fn creation_classifier_rejects_pr_inspection_tools() {
        assert!(is_pull_request_creation_tool(
            "mcp__github__create_pull_request",
            None,
            None
        ));
        assert!(is_pull_request_creation_tool(
            "run command",
            Some(&serde_json::json!({"command": ["gh", "pr", "create"]})),
            None
        ));
        assert!(!is_pull_request_creation_tool(
            "inspect pull request",
            Some(&serde_json::json!({"command": ["gh", "pr", "view", "487"]})),
            None
        ));
        assert!(!is_pull_request_creation_tool(
            "delegate task",
            Some(&serde_json::json!({"prompt": "create a pull request after the work"})),
            None
        ));
    }

    #[test]
    fn prose_fallback_requires_explicit_creation_language() {
        let url = "https://github.com/BrokkAi/mjolnir/pull/487";
        assert!(created_pull_requests_from_prose(&format!("Reviewed {url}")).is_empty());
        assert!(
            created_pull_requests_from_prose(&format!(
                "No pull request created; the existing one is {url}"
            ))
            .is_empty()
        );

        let created =
            created_pull_requests_from_prose(&format!("Draft pull request created: {url}"));
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].draft, Some(true));
    }
}
