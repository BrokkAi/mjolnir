//! GitHub references shared by transcript readers.
use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubReference {
    pub owner: Option<String>,
    pub repo: Option<String>,
    pub number: u64,
    pub text: String,
    /// Byte offset of `text` in the parsed string.
    pub start: usize,
}

pub fn github_references(text: &str) -> Vec<GithubReference> {
    static REFERENCES: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)https://github\.com/([a-z0-9_-]+)/([a-z0-9_.-]+)/(?:issues|pull)/([0-9]+)|(?:([a-z0-9_-]+)/([a-z0-9_.-]+))?#([0-9]+)").unwrap()
    });
    REFERENCES
        .captures_iter(text)
        .filter_map(|capture| {
            let span = capture.get(0)?;
            let boundary = |c: char| c.is_alphanumeric() || matches!(c, '_' | '/' | '#' | '-');
            if text[..span.start()]
                .chars()
                .next_back()
                .is_some_and(boundary)
                || text[span.end()..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
            {
                return None;
            }
            let (owner, repo, number) = if capture.get(3).is_some() {
                (1, 2, 3)
            } else {
                (4, 5, 6)
            };
            Some(GithubReference {
                owner: capture.get(owner).map(|m| m.as_str().to_owned()),
                repo: capture.get(repo).map(|m| m.as_str().to_owned()),
                number: capture.get(number)?.as_str().parse().ok()?,
                text: span.as_str().to_owned(),
                start: span.start(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_boundaries_keep_numbers_and_repositories_distinct() {
        let references = github_references(
            "#12 #123 x#12 owner/repo#12 https://github.com/OWNER/repo/pull/12/files). #12x",
        );
        assert_eq!(
            references.iter().map(|r| r.number).collect::<Vec<_>>(),
            vec![12, 123, 12, 12]
        );
        assert_eq!(references[2].owner.as_deref(), Some("owner"));
        assert_eq!(references[3].repo.as_deref(), Some("repo"));
    }
}
