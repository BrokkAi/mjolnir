use super::*;

pub(super) async fn github_token(
    State(state): State<ServerState>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
) -> Result<Json<GithubTokenResponse>, ApiFailure> {
    let query = parse_github_token_query(raw_query.as_deref())?;
    let (owner, repositories) = if let Some(owner) = query.owner {
        if !query.repo.is_empty() {
            return Err(ApiFailure::bad_request(
                "choose --owner or one or more --repo values, not both",
            ));
        }
        let owner = owner.trim();
        if !mj_core::config::valid_github_owner_login(owner) {
            return Err(ApiFailure::bad_request(
                "owner must be a valid GitHub owner login",
            ));
        }
        (Some(owner.to_owned()), Vec::new())
    } else {
        if query.repo.is_empty() {
            return Err(ApiFailure::bad_request(
                "supply --owner or at least one --repo OWNER/NAME",
            ));
        }
        let mut repositories = Vec::with_capacity(query.repo.len());
        for repository in &query.repo {
            let Some((owner, name)) = mj_core::remote_git::github_owner_repo(repository)
                .filter(|(owner, name)| format!("{owner}/{name}") == repository.as_str())
            else {
                return Err(ApiFailure::bad_request(
                    "repo values must use OWNER/NAME form",
                ));
            };
            repositories.push((owner, name));
        }
        (None, repositories)
    };
    let token = backend(&state)?
        .github_token(owner, repositories)
        .await
        .map_err(|error| {
            ApiFailure::unavailable(format!("could not retrieve a GitHub App token: {error:#}"))
        })?;
    Ok(Json(GithubTokenResponse { token }))
}

fn parse_github_token_query(raw_query: Option<&str>) -> Result<GithubTokenQuery, ApiFailure> {
    let mut query = GithubTokenQuery::default();
    for (key, value) in url::form_urlencoded::parse(raw_query.unwrap_or_default().as_bytes()) {
        match key.as_ref() {
            "owner" if query.owner.is_none() => query.owner = Some(value.into_owned()),
            "owner" => return Err(ApiFailure::bad_request("owner may be supplied only once")),
            "repo" => query.repo.push(value.into_owned()),
            _ => {
                return Err(ApiFailure::bad_request(
                    "unsupported GitHub token query field",
                ));
            }
        }
    }
    Ok(query)
}
