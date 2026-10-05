use super::*;

pub(super) async fn github_token(
    State(state): State<ServerState>,
    Query(query): Query<GithubTokenQuery>,
) -> Result<Json<GithubTokenResponse>, ApiFailure> {
    let owner = query.owner.trim();
    if !mj_core::config::valid_github_owner_login(owner) {
        return Err(ApiFailure::bad_request(
            "owner must be a valid GitHub owner login",
        ));
    }
    if let Some(repository) = query.repo.as_deref()
        && mj_core::remote_git::github_owner_repo(&format!("{owner}/{repository}"))
            .is_none_or(|(_, parsed_repository)| parsed_repository != repository)
    {
        return Err(ApiFailure::bad_request(
            "repo must be a repository name without an owner or path",
        ));
    }
    let token = backend(&state)?
        .github_token(owner.to_owned(), query.repo)
        .await
        .map_err(|error| {
            ApiFailure::unavailable(format!("could not retrieve a GitHub App token: {error:#}"))
        })?;
    Ok(Json(GithubTokenResponse { token }))
}
