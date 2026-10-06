//! GitHub App JWTs and installation access tokens owned by the controller.

use std::collections::BTreeMap;
use std::fs;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::{Deserialize, Serialize};
use url::Url;

use mj_core::config::{
    GithubAppConfig, GithubPermissionLevel, GithubPermissionSet, ProjectBundle, ProjectRepository,
    valid_github_owner_login,
};
use mj_core::remote_git::{github_owner_repo, resolve_repository};

use super::{Controller, controller_github_token};
use crate::targets::{CommandExecutor, ProcessExecutor};

const TOKEN_REFRESH_THRESHOLD: Duration = Duration::from_secs(10 * 60);
const JWT_BACKDATE_SECS: i64 = 60;
const JWT_LIFETIME_SECS: i64 = 9 * 60;
const GITHUB_API_VERSION: &str = "2022-11-28";

type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// One controller process shares this cache between provisioning, resume,
/// periodic worker reconciliation, and the token API.
pub(crate) struct GithubAppTokenProvider {
    config: GithubAppConfig,
    http: reqwest::Client,
    api_base: Url,
    now: Clock,
    signing_key: tokio::sync::OnceCell<Arc<RsaKeyPair>>,
    installations: Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<Option<u64>>>>>,
    tokens: Mutex<BTreeMap<TokenCacheKey, Arc<tokio::sync::Mutex<Option<TokenEntry>>>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct TokenCacheKey {
    installation_id: u64,
    repositories: Vec<String>,
    permissions: Option<GithubPermissionSet>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InstallationScope {
    pub(super) installation_id: u64,
    pub(super) repositories: Vec<String>,
}

#[derive(Clone)]
struct TokenEntry {
    token: String,
    expires_at: SystemTime,
}

#[derive(Serialize)]
struct JwtHeader {
    alg: &'static str,
    typ: &'static str,
}

#[derive(Serialize)]
struct JwtClaims {
    iss: String,
    iat: i64,
    exp: i64,
}

#[derive(Deserialize)]
struct InstallationResponse {
    id: u64,
    permissions: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
struct AccessTokenResponse {
    token: String,
    expires_at: String,
    permissions: Option<BTreeMap<String, String>>,
}

impl GithubAppTokenProvider {
    fn new(config: GithubAppConfig) -> Result<Self> {
        config.validate()?;
        let http = reqwest::Client::builder()
            .user_agent(concat!("Mjolnir/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .context("build GitHub App HTTP client")?;
        Ok(Self {
            config,
            http,
            api_base: Url::parse("https://api.github.com/").context("parse GitHub API URL")?,
            now: Arc::new(SystemTime::now),
            signing_key: tokio::sync::OnceCell::new(),
            installations: Mutex::new(BTreeMap::new()),
            tokens: Mutex::new(BTreeMap::new()),
        })
    }

    /// Reuse one provider for the current App configuration so token calls
    /// made by separate controller operations still share a cache.
    pub(crate) fn shared(config: &GithubAppConfig) -> Result<Arc<Self>> {
        static PROVIDERS: OnceLock<Mutex<BTreeMap<GithubAppConfig, Arc<GithubAppTokenProvider>>>> =
            OnceLock::new();
        let providers = PROVIDERS.get_or_init(|| Mutex::new(BTreeMap::new()));
        let mut providers = providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(provider) = providers.get(config) {
            return Ok(Arc::clone(provider));
        }
        let provider = Arc::new(Self::new(config.clone())?);
        providers.insert(config.clone(), Arc::clone(&provider));
        Ok(provider)
    }

    #[cfg(test)]
    fn with_test_transport(
        config: GithubAppConfig,
        http: reqwest::Client,
        api_base: Url,
        now: Clock,
    ) -> Self {
        Self {
            config,
            http,
            api_base,
            now,
            signing_key: tokio::sync::OnceCell::new(),
            installations: Mutex::new(BTreeMap::new()),
            tokens: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) async fn token_for_owner(&self, owner: &str) -> Result<String> {
        let installation_id = self.installation_for_owner(owner, None).await?;
        self.token_for_installation(installation_id, &[], self.config.token_permissions.as_ref())
            .await
    }

    pub(crate) async fn token_for_repositories(
        &self,
        repositories: &[(String, String)],
    ) -> Result<String> {
        let scope = self
            .resolve_repository_scope(None, repositories)
            .await
            .map_err(GithubBundleSelectionError::into_anyhow)?
            .ok_or_else(|| anyhow!("at least one repository is required"))?;
        self.token_for_installation(
            scope.installation_id,
            &scope.repositories,
            self.config.token_permissions.as_ref(),
        )
        .await
    }

    pub(crate) async fn installation_for_repo(&self, owner: &str, repository: &str) -> Result<u64> {
        self.installation_for_owner(owner, Some(repository)).await
    }

    pub(super) async fn token_for_owner_repo_pairs(
        &self,
        bundle_id: &str,
        repositories: &[(String, String)],
    ) -> std::result::Result<Option<InstallationScope>, GithubBundleSelectionError> {
        self.resolve_repository_scope(Some(bundle_id), repositories)
            .await
    }

    async fn resolve_repository_scope(
        &self,
        bundle_id: Option<&str>,
        repositories: &[(String, String)],
    ) -> std::result::Result<Option<InstallationScope>, GithubBundleSelectionError> {
        let mut selected = BTreeMap::<u64, Vec<String>>::new();
        for (owner, repository) in repositories {
            let installation_id = self
                .installation_for_repo(owner, repository)
                .await
                .map_err(GithubBundleSelectionError::Provider)?;
            selected
                .entry(installation_id)
                .or_default()
                .push(format!("{owner}/{repository}"));
        }
        if selected.len() > 1 {
            let detail = selected
                .iter()
                .map(|(id, repositories)| format!("installation {id}: {}", repositories.join(", ")))
                .collect::<Vec<_>>()
                .join("; ");
            let message = if let Some(bundle_id) = bundle_id {
                format!(
                    "bundle {bundle_id:?} requires more than one GitHub App installation; v1 supports one installation per session ({detail})"
                )
            } else {
                format!(
                    "selected repositories span more than one GitHub App installation ({detail})"
                )
            };
            return Err(GithubBundleSelectionError::MultipleInstallations(message));
        }
        let Some((installation_id, repositories)) = selected.into_iter().next() else {
            return Ok(None);
        };
        let mut repository_names = repositories
            .into_iter()
            .filter_map(|repository| repository.split_once('/').map(|(_, name)| name.to_owned()))
            .map(|repository| repository.to_ascii_lowercase())
            .collect::<Vec<_>>();
        repository_names.sort();
        repository_names.dedup();
        Ok(Some(InstallationScope {
            installation_id,
            repositories: repository_names,
        }))
    }

    async fn installation_for_owner(&self, owner: &str, repo: Option<&str>) -> Result<u64> {
        ensure!(
            valid_github_owner_login(owner),
            "{owner:?} is not a valid GitHub owner login"
        );
        if let Some(repo) = repo {
            ensure!(
                valid_repository(repo),
                "{repo:?} is not a valid GitHub repository name"
            );
        }
        let owner_key = owner.to_ascii_lowercase();
        if let Some(id) = self
            .config
            .installations
            .iter()
            .find_map(|(configured_owner, id)| {
                configured_owner.eq_ignore_ascii_case(owner).then_some(*id)
            })
        {
            return Ok(id);
        }
        let installation = {
            let mut installations = self
                .installations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(
                installations
                    .entry(owner_key.clone())
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None))),
            )
        };
        let mut installation = installation.lock().await;
        if let Some(id) = *installation {
            return Ok(id);
        }

        let id = if let Some(repo) = repo {
            self.lookup_installation(&["repos", owner, repo, "installation"])
                .await
                .map_err(InstallationLookupError::into_anyhow)?
        } else {
            match self
                .lookup_installation(&["orgs", owner, "installation"])
                .await
            {
                Ok(id) => id,
                Err(InstallationLookupError::NotFound) => self
                    .lookup_installation(&["users", owner, "installation"])
                    .await
                    .map_err(InstallationLookupError::into_anyhow)?,
                Err(error) => return Err(error.into_anyhow()),
            }
        };
        *installation = Some(id);
        Ok(id)
    }

    async fn lookup_installation(
        &self,
        path: &[&str],
    ) -> std::result::Result<u64, InstallationLookupError> {
        let jwt = self
            .app_jwt()
            .await
            .map_err(InstallationLookupError::Other)?;
        let response = self
            .http
            .get(self.api_url(path).map_err(InstallationLookupError::Other)?)
            .bearer_auth(jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
            .send()
            .await
            .map_err(|error| {
                InstallationLookupError::Other(anyhow!(
                    "reach the GitHub App installation API: {error}"
                ))
            })?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(InstallationLookupError::NotFound);
        }
        if !response.status().is_success() {
            return Err(InstallationLookupError::Other(anyhow!(
                "GitHub App installation lookup failed with HTTP {}",
                response.status().as_u16()
            )));
        }
        let installation = response
            .json::<InstallationResponse>()
            .await
            .context("decode GitHub App installation response")
            .map_err(InstallationLookupError::Other)?;
        if installation.id == 0 {
            return Err(InstallationLookupError::Other(anyhow!(
                "GitHub returned an invalid installation ID"
            )));
        }
        Ok(installation.id)
    }

    pub(crate) async fn token_for_installation(
        &self,
        installation_id: u64,
        repositories: &[String],
        permissions: Option<&GithubPermissionSet>,
    ) -> Result<String> {
        ensure!(installation_id != 0, "installation ID must be positive");
        let mut repositories = repositories
            .iter()
            .map(|repository| repository.to_ascii_lowercase())
            .collect::<Vec<_>>();
        repositories.sort();
        repositories.dedup();
        let key = TokenCacheKey {
            installation_id,
            repositories: repositories.clone(),
            permissions: permissions.cloned(),
        };
        let cache = {
            let mut tokens = self
                .tokens
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(
                tokens
                    .entry(key)
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None))),
            )
        };
        let mut cached = cache.lock().await;
        let now = (self.now)();
        if let Some(entry) = cached.as_ref()
            && entry
                .expires_at
                .duration_since(now)
                .is_ok_and(|remaining| remaining >= TOKEN_REFRESH_THRESHOLD)
        {
            return Ok(entry.token.clone());
        }

        match self
            .mint_installation_token(installation_id, &repositories, permissions)
            .await
        {
            Ok(entry) => {
                let token = entry.token.clone();
                *cached = Some(entry);
                Ok(token)
            }
            Err(error) => {
                if error.downcast_ref::<PermissionGrantError>().is_some() {
                    return Err(error);
                }
                if let Some(entry) = cached.as_ref()
                    && entry.expires_at > now
                {
                    tracing::warn!(
                        installation_id,
                        error = %error,
                        "could not refresh GitHub App installation token; using its still-valid cached token"
                    );
                    return Ok(entry.token.clone());
                }
                Err(error)
            }
        }
    }

    async fn mint_installation_token(
        &self,
        installation_id: u64,
        repositories: &[String],
        permissions: Option<&GithubPermissionSet>,
    ) -> Result<TokenEntry> {
        let jwt = self.app_jwt().await?;
        if let Some(permissions) = permissions {
            let installation = self.installation_details(installation_id, &jwt).await?;
            let installed_permissions = installation.permissions.as_ref().ok_or_else(|| {
                permission_grant_error(format!(
                    "GitHub installation {installation_id} response omitted permissions"
                ))
            })?;
            validate_requested_permissions(installation_id, permissions, installed_permissions)?;
        }
        let mut body = serde_json::Map::new();
        if !repositories.is_empty() {
            body.insert("repositories".to_owned(), serde_json::json!(repositories));
        }
        if let Some(permissions) = permissions {
            body.insert("permissions".to_owned(), serde_json::to_value(permissions)?);
        }
        let response = self
            .http
            .post(self.api_url(&[
                "app",
                "installations",
                &installation_id.to_string(),
                "access_tokens",
            ])?)
            .bearer_auth(jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
            .json(&body)
            .send()
            .await
            .context("reach the GitHub App access-token API")?;
        if !response.status().is_success() {
            bail!(
                "GitHub App access-token exchange failed with HTTP {}",
                response.status().as_u16()
            );
        }
        let response = response
            .json::<AccessTokenResponse>()
            .await
            .context("decode GitHub App access-token response")?;
        if let Some(permissions) = permissions {
            let granted_permissions = response.permissions.as_ref().ok_or_else(|| {
                permission_grant_error(
                    "GitHub installation token response omitted permissions".to_owned(),
                )
            })?;
            validate_minted_permissions(installation_id, permissions, granted_permissions)?;
        }
        ensure!(
            !response.token.is_empty(),
            "GitHub returned an empty installation token"
        );
        let expires_at = chrono::DateTime::parse_from_rfc3339(&response.expires_at)
            .context("GitHub returned an invalid installation-token expiry")?
            .with_timezone(&chrono::Utc)
            .timestamp();
        let expires_at = UNIX_EPOCH
            .checked_add(Duration::from_secs(
                expires_at
                    .try_into()
                    .context("GitHub returned an invalid installation-token expiry")?,
            ))
            .context("GitHub returned an invalid installation-token expiry")?;
        ensure!(
            expires_at > (self.now)(),
            "GitHub returned an expired installation token"
        );
        Ok(TokenEntry {
            token: response.token,
            expires_at,
        })
    }

    async fn installation_details(
        &self,
        installation_id: u64,
        jwt: &str,
    ) -> Result<InstallationResponse> {
        let response = self
            .http
            .get(self.api_url(&["app", "installations", &installation_id.to_string()])?)
            .bearer_auth(jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
            .send()
            .await
            .context("reach the GitHub App installation details API")?;
        if !response.status().is_success() {
            bail!(
                "GitHub App installation details lookup failed with HTTP {}",
                response.status().as_u16()
            );
        }
        let installation = response
            .json::<InstallationResponse>()
            .await
            .context("decode GitHub App installation details")?;
        ensure!(
            installation.id == installation_id,
            "GitHub returned installation {} while checking installation {installation_id}",
            installation.id
        );
        Ok(installation)
    }

    async fn app_jwt(&self) -> Result<String> {
        let config = self.config.clone();
        let key = self
            .signing_key
            .get_or_try_init(|| async move {
                let path = config.private_key_path.clone();
                let bytes = tokio::task::spawn_blocking(move || read_private_key_file(&path))
                    .await
                    .context("GitHub App private-key read task failed")??;
                parse_private_key(&bytes).map(Arc::new)
            })
            .await?;
        mint_app_jwt(&self.config, key, (self.now)())
    }

    fn api_url(&self, segments: &[&str]) -> Result<Url> {
        let mut url = self.api_base.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow!("GitHub API base URL cannot accept path segments"))?
            .clear()
            .extend(segments.iter().copied());
        Ok(url)
    }
}

fn validate_requested_permissions(
    installation_id: u64,
    requested: &GithubPermissionSet,
    installed: &BTreeMap<String, String>,
) -> Result<()> {
    for (permission, requested_level) in requested {
        let actual_level = installed.get(permission).map(String::as_str);
        let actual_rank = match actual_level {
            None | Some("none") => 0,
            Some("read") => 1,
            Some("write") => 2,
            Some("admin") => 3,
            Some(other) => {
                return Err(permission_grant_error(format!(
                    "GitHub installation {installation_id} returned unsupported level {other:?} for permission {permission:?}"
                )));
            }
        };
        let requested_rank = match requested_level {
            GithubPermissionLevel::Read => 1,
            GithubPermissionLevel::Write => 2,
        };
        if actual_rank < requested_rank {
            return Err(permission_grant_error(format!(
                "GitHub installation {installation_id} grants {} for permission {permission:?}, but {} was requested",
                actual_level.unwrap_or("none"),
                requested_level.as_str()
            )));
        }
    }
    Ok(())
}

fn validate_minted_permissions(
    installation_id: u64,
    requested: &GithubPermissionSet,
    granted: &BTreeMap<String, String>,
) -> Result<()> {
    validate_requested_permissions(installation_id, requested, granted)?;
    for (permission, actual_level) in granted {
        let Some(requested_level) = requested.get(permission) else {
            return Err(permission_grant_error(format!(
                "GitHub installation token for installation {installation_id} includes unrequested permission {permission:?}"
            )));
        };
        let requested_rank = match requested_level {
            GithubPermissionLevel::Read => 1,
            GithubPermissionLevel::Write => 2,
        };
        let actual_rank = match actual_level.as_str() {
            "none" => 0,
            "read" => 1,
            "write" => 2,
            "admin" => 3,
            other => {
                return Err(permission_grant_error(format!(
                    "GitHub installation token for installation {installation_id} returned unsupported level {other:?} for permission {permission:?}"
                )));
            }
        };
        if actual_rank > requested_rank {
            return Err(permission_grant_error(format!(
                "GitHub installation token for installation {installation_id} has {} for permission {permission:?}, but {} was requested",
                actual_level,
                requested_level.as_str()
            )));
        }
    }
    Ok(())
}

#[derive(Debug)]
struct PermissionGrantError(String);

impl std::fmt::Display for PermissionGrantError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PermissionGrantError {}

fn permission_grant_error(message: String) -> anyhow::Error {
    anyhow::Error::new(PermissionGrantError(message))
}

#[cfg(unix)]
fn read_private_key_file(path: &std::path::Path) -> Result<Vec<u8>> {
    use std::io::Read as _;
    use std::os::unix::fs::MetadataExt as _;

    let mut file = fs::File::open(path)
        .with_context(|| format!("open GitHub App private key {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect GitHub App private key {}", path.display()))?;
    validate_private_key_metadata(
        path,
        metadata.uid(),
        metadata.mode(),
        // SAFETY: geteuid reads the effective UID of the current daemon process.
        unsafe { libc::geteuid() },
    )?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("read GitHub App private key {}", path.display()))?;
    Ok(bytes)
}

#[cfg(not(unix))]
fn read_private_key_file(path: &std::path::Path) -> Result<Vec<u8>> {
    fs::read(path).with_context(|| format!("read GitHub App private key {}", path.display()))
}

#[cfg(unix)]
fn validate_private_key_metadata(
    path: &std::path::Path,
    file_uid: u32,
    mode: u32,
    daemon_uid: u32,
) -> Result<()> {
    ensure!(
        file_uid == daemon_uid && mode & 0o044 == 0,
        "GitHub App private key {} must be owned by daemon UID {daemon_uid} and not readable by group or others; fix its ownership and run `chmod 600 -- {}`",
        path.display(),
        path.display(),
    );
    Ok(())
}

/// Why a bundle's GitHub repositories could not select one App installation.
#[derive(Debug)]
pub enum GithubBundleSelectionError {
    UnknownBundle(String),
    MultipleInstallations(String),
    Provider(anyhow::Error),
}

impl std::fmt::Display for GithubBundleSelectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownBundle(message) => formatter.write_str(message),
            Self::MultipleInstallations(message) => formatter.write_str(message),
            Self::Provider(error) => write!(formatter, "{error:#}"),
        }
    }
}

impl std::error::Error for GithubBundleSelectionError {}

impl GithubBundleSelectionError {
    pub(crate) fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::UnknownBundle(message) => anyhow!("{message}"),
            Self::MultipleInstallations(message) => anyhow!("{message}"),
            Self::Provider(error) => error,
        }
    }
}

impl Controller {
    /// Reject a new bundle before admitting its session when its repositories
    /// resolve to more than one App installation.
    pub(crate) async fn validate_github_bundle_installations(
        &self,
        bundle_id: &str,
    ) -> std::result::Result<(), GithubBundleSelectionError> {
        let Some(app) = self.config.github.app.as_ref() else {
            return Ok(());
        };
        let bundle = self.config.bundles.get(bundle_id).cloned().ok_or_else(|| {
            GithubBundleSelectionError::UnknownBundle(format!("unknown bundle {bundle_id:?}"))
        })?;
        let provider =
            GithubAppTokenProvider::shared(app).map_err(GithubBundleSelectionError::Provider)?;
        let repositories = tokio::task::spawn_blocking(move || {
            github_repositories(&bundle, None, &ProcessExecutor)
        })
        .await
        .map_err(|error| {
            GithubBundleSelectionError::Provider(anyhow!(
                "GitHub repository source task failed: {error}"
            ))
        })?
        .map_err(GithubBundleSelectionError::Provider)?;
        provider
            .token_for_owner_repo_pairs(bundle_id, &repositories)
            .await?;
        Ok(())
    }

    /// Resolve this session's GitHub credential, using the App installation
    /// selected by its accepted repository bundle.
    pub(crate) async fn github_token_for_session(
        &self,
        session_id: &str,
    ) -> Result<Option<String>> {
        if self.config.github.app.is_none() {
            return tokio::task::spawn_blocking(controller_github_token)
                .await
                .context("GitHub token lookup task failed");
        }
        self.github_app_token_for_session(session_id).await
    }

    /// Repository source preflight only needs a token when the accepted bundle
    /// contains a GitHub repository. Preserve the legacy path's lazy lookup so
    /// unrelated resumes do not spawn `gh auth token`.
    pub(crate) async fn github_token_for_repository_preflight(
        &self,
        session_id: &str,
    ) -> Result<Option<String>> {
        let has_github_repository = self
            .state
            .sessions
            .get(session_id)
            .and_then(|session| session.project_bundle(&self.config))
            .is_some_and(|bundle| {
                bundle
                    .repositories
                    .iter()
                    .any(|repository| repository.github.is_some())
            });
        if !has_github_repository {
            return Ok(None);
        }
        self.github_token_for_session(session_id).await
    }

    pub(crate) async fn github_app_token_for_session(
        &self,
        session_id: &str,
    ) -> Result<Option<String>> {
        let Some(app) = self.config.github.app.as_ref() else {
            return Ok(None);
        };
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        let Some(bundle) = session.project_bundle(&self.config).cloned() else {
            return Ok(None);
        };
        let bundle_id = session.bundle_id.clone();
        let network_sources = session
            .project
            .as_ref()
            .map(|project| project.network_sources.clone());
        let repositories = tokio::task::spawn_blocking(move || {
            github_repositories(&bundle, network_sources.as_ref(), &ProcessExecutor)
        })
        .await
        .context("GitHub repository source task failed")??;
        let provider = GithubAppTokenProvider::shared(app)?;
        let Some(scope) = provider
            .token_for_owner_repo_pairs(&bundle_id, &repositories)
            .await
            .map_err(GithubBundleSelectionError::into_anyhow)?
        else {
            return Ok(None);
        };
        provider
            .token_for_installation(
                scope.installation_id,
                &scope.repositories,
                app.session_permissions.as_ref(),
            )
            .await
            .map(Some)
    }
}

pub(crate) async fn github_token_for_session(
    session_id: String,
    github_app_configured: bool,
) -> Result<Option<String>> {
    if !github_app_configured {
        return tokio::task::spawn_blocking(controller_github_token)
            .await
            .context("GitHub token lookup task failed");
    }
    let controller = tokio::task::spawn_blocking(Controller::load)
        .await
        .context("load controller for GitHub credential sync")??;
    if controller.config.github.app.is_none() {
        return tokio::task::spawn_blocking(controller_github_token)
            .await
            .context("GitHub token lookup task failed");
    }
    controller.github_token_for_session(&session_id).await
}

pub(crate) async fn github_app_token_for_session(session_id: String) -> Result<Option<String>> {
    let controller = tokio::task::spawn_blocking(Controller::load)
        .await
        .context("load controller for GitHub App export token")??;
    controller.github_app_token_for_session(&session_id).await
}

pub(super) fn github_repositories(
    bundle: &ProjectBundle,
    network_sources: Option<&BTreeMap<String, mj_core::remote_git::NetworkGitSource>>,
    executor: &impl CommandExecutor,
) -> Result<Vec<(String, String)>> {
    let mut repositories = Vec::new();
    for repository in &bundle.repositories {
        let source = repository_source(repository, network_sources, executor)?;
        if let Some((owner, name)) = source.as_deref().and_then(github_owner_repo) {
            repositories.push((owner, name));
        }
    }
    repositories.sort();
    repositories.dedup();
    Ok(repositories)
}

fn repository_source(
    repository: &ProjectRepository,
    network_sources: Option<&BTreeMap<String, mj_core::remote_git::NetworkGitSource>>,
    executor: &impl CommandExecutor,
) -> Result<Option<String>> {
    if let Some(source) = &repository.github {
        return Ok(Some(source.clone()));
    }
    if let Some(source) = network_sources
        .and_then(|sources| sources.get(&repository.id))
        .map(|source| source.fetch_url.clone())
    {
        return Ok(Some(source));
    }
    if repository.local.is_some() {
        return resolve_repository(repository, executor).map(|source| Some(source.fetch_url));
    }
    Ok(None)
}

enum InstallationLookupError {
    NotFound,
    Other(anyhow::Error),
}

impl InstallationLookupError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::NotFound => {
                anyhow!("no GitHub App installation was found for this owner or repository")
            }
            Self::Other(error) => error,
        }
    }
}

fn valid_repository(repository: &str) -> bool {
    !repository.is_empty()
        && repository.len() <= 100
        && repository != "."
        && repository != ".."
        && repository
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn parse_private_key(pem: &[u8]) -> Result<RsaKeyPair> {
    let pem = std::str::from_utf8(pem).context("GitHub App private key is not UTF-8 PEM")?;
    let (label, body) = [
        (
            "RSA PRIVATE KEY",
            "-----BEGIN RSA PRIVATE KEY-----",
            "-----END RSA PRIVATE KEY-----",
        ),
        (
            "PRIVATE KEY",
            "-----BEGIN PRIVATE KEY-----",
            "-----END PRIVATE KEY-----",
        ),
    ]
    .into_iter()
    .find_map(|(label, begin, end)| {
        let body = pem.strip_prefix(begin)?.split_once(end)?.0;
        Some((label, body))
    })
    .ok_or_else(|| anyhow!("GitHub App key must be an RSA PKCS#1 or PKCS#8 PEM private key"))?;
    let der = STANDARD
        .decode(
            body.bytes()
                .filter(|byte| !byte.is_ascii_whitespace())
                .collect::<Vec<_>>(),
        )
        .context("decode GitHub App PEM private key")?;
    let key = match label {
        "PRIVATE KEY" => RsaKeyPair::from_pkcs8(&der),
        "RSA PRIVATE KEY" => RsaKeyPair::from_der(&der),
        _ => unreachable!("PEM label comes from the fixed list above"),
    };
    key.map_err(|_| anyhow!("GitHub App PEM does not contain a valid RSA private key"))
}

fn mint_app_jwt(config: &GithubAppConfig, key: &RsaKeyPair, now: SystemTime) -> Result<String> {
    let now: i64 = now
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs()
        .try_into()
        .context("system time is outside the supported JWT range")?;
    let header = serde_json::to_vec(&JwtHeader {
        alg: "RS256",
        typ: "JWT",
    })?;
    let claims = serde_json::to_vec(&JwtClaims {
        iss: config.app_id.to_string(),
        iat: now - JWT_BACKDATE_SECS,
        exp: now + JWT_LIFETIME_SECS,
    })?;
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header),
        URL_SAFE_NO_PAD.encode(claims)
    );
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        signing_input.as_bytes(),
        &mut signature,
    )
    .map_err(|_| anyhow!("sign GitHub App JWT"))?;
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature)
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use axum::extract::Path;
    use axum::extract::State;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use ring::signature::{KeyPair, RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
    use serde_json::Value;
    use tokio::net::TcpListener;

    use super::*;

    const TEST_KEY: &[u8] = include_bytes!("testdata/github-app-test.pem");

    #[derive(Clone)]
    struct FakeGithub {
        now: Arc<AtomicU64>,
        lookups: Arc<AtomicU64>,
        exchanges: Arc<AtomicU64>,
        scopes: Arc<Mutex<Vec<Vec<String>>>>,
        bodies: Arc<Mutex<Vec<Value>>>,
        installation_permissions: Arc<Mutex<BTreeMap<String, String>>>,
    }

    async fn installation(State(fake): State<FakeGithub>) -> Json<Value> {
        fake.lookups.fetch_add(1, Ordering::SeqCst);
        Json(serde_json::json!({ "id": 77331 }))
    }

    async fn installation_details(
        State(fake): State<FakeGithub>,
        Path(installation_id): Path<u64>,
    ) -> Json<Value> {
        Json(serde_json::json!({
            "id": installation_id,
            "permissions": fake.installation_permissions.lock().unwrap().clone(),
        }))
    }

    async fn access_token(State(fake): State<FakeGithub>, Json(body): Json<Value>) -> Json<Value> {
        let exchange = fake.exchanges.fetch_add(1, Ordering::SeqCst) + 1;
        fake.bodies.lock().unwrap().push(body.clone());
        let repositories = body
            .get("repositories")
            .and_then(Value::as_array)
            .map(|repositories| {
                repositories
                    .iter()
                    .map(|repository| repository.as_str().unwrap().to_owned())
                    .collect()
            })
            .unwrap_or_default();
        fake.scopes.lock().unwrap().push(repositories);
        let permissions = body.get("permissions").cloned().unwrap_or_else(|| {
            serde_json::to_value(fake.installation_permissions.lock().unwrap().clone()).unwrap()
        });
        Json(serde_json::json!({
            "token": format!("test-installation-token-{exchange}"),
            "permissions": permissions,
            "expires_at": chrono::DateTime::from_timestamp(
                fake.now.load(Ordering::SeqCst) as i64 + 3600,
                0,
            ).unwrap().to_rfc3339(),
        }))
    }

    async fn missing_installation() -> axum::http::StatusCode {
        axum::http::StatusCode::NOT_FOUND
    }

    async fn test_server(fake: FakeGithub) -> (Url, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route("/repos/{owner}/{repo}/installation", get(installation))
            .route("/orgs/{owner}/installation", get(installation))
            .route("/users/{owner}/installation", get(missing_installation))
            .route("/app/installations/{id}", get(installation_details))
            .route("/app/installations/{id}/access_tokens", post(access_token))
            .with_state(fake);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (Url::parse(&format!("http://{address}/")).unwrap(), task)
    }

    fn test_config() -> GithubAppConfig {
        GithubAppConfig {
            app_id: 12_345,
            private_key_path: PathBuf::from("test.pem"),
            installations: BTreeMap::new(),
            session_permissions: None,
            token_permissions: None,
        }
    }

    fn fake_github(now: u64) -> FakeGithub {
        FakeGithub {
            now: Arc::new(AtomicU64::new(now)),
            lookups: Arc::new(AtomicU64::new(0)),
            exchanges: Arc::new(AtomicU64::new(0)),
            scopes: Arc::new(Mutex::new(Vec::new())),
            bodies: Arc::new(Mutex::new(Vec::new())),
            installation_permissions: Arc::new(Mutex::new(BTreeMap::from([
                ("contents".into(), "write".into()),
                ("metadata".into(), "read".into()),
                ("statuses".into(), "read".into()),
            ]))),
        }
    }

    #[test]
    fn jwt_has_expected_claims_and_a_valid_rs256_signature() {
        let key = parse_private_key(TEST_KEY).unwrap();
        let config = test_config();
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let jwt = mint_app_jwt(&config, &key, now).unwrap();
        let mut parts = jwt.split('.');
        let header_part = parts.next().unwrap();
        let claims_part = parts.next().unwrap();
        let signature_part = parts.next().unwrap();
        let header = URL_SAFE_NO_PAD.decode(header_part).unwrap();
        let claims = URL_SAFE_NO_PAD.decode(claims_part).unwrap();
        let signature = URL_SAFE_NO_PAD.decode(signature_part).unwrap();
        assert!(parts.next().is_none());
        assert_eq!(
            serde_json::from_slice::<Value>(&header).unwrap()["alg"],
            "RS256"
        );
        let claims_json: Value = serde_json::from_slice(&claims).unwrap();
        assert_eq!(claims_json["iss"], config.app_id.to_string());
        assert_eq!(claims_json["iat"], 1_800_000_000 - JWT_BACKDATE_SECS);
        assert_eq!(claims_json["exp"], 1_800_000_000 + JWT_LIFETIME_SECS);

        let signing_input = format!("{header_part}.{claims_part}");
        let (n, e) = rsa_public_components(key.public_key().as_ref());
        RsaPublicKeyComponents { n, e }
            .verify(
                &RSA_PKCS1_2048_8192_SHA256,
                signing_input.as_bytes(),
                &signature,
            )
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn private_key_permissions_require_daemon_ownership_and_private_mode() {
        let path = std::path::Path::new("/var/lib/mj/github-app.pem");
        assert!(validate_private_key_metadata(path, 1000, 0o100600, 1000).is_ok());

        for (uid, mode) in [(1000, 0o100640), (1001, 0o100600)] {
            let error = validate_private_key_metadata(path, uid, mode, 1000)
                .unwrap_err()
                .to_string();
            assert!(error.contains(path.to_str().unwrap()));
            assert!(error.contains("chmod 600"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn private_key_file_check_rejects_group_read_permission() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("github-app.pem");
        fs::write(&path, TEST_KEY).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        let error = read_private_key_file(&path).unwrap_err();
        let message = error.to_string();
        assert!(message.contains(path.to_str().unwrap()));
        assert!(message.contains("chmod 600"));
    }

    #[tokio::test]
    async fn installation_lookup_and_token_cache_refresh_below_ten_minutes() {
        let fake = fake_github(1_800_000_000);
        let (api_base, server) = test_server(fake.clone()).await;
        let now_for_clock = Arc::clone(&fake.now);
        let config = test_config();
        let provider = GithubAppTokenProvider::with_test_transport(
            config,
            reqwest::Client::new(),
            api_base,
            Arc::new(move || {
                UNIX_EPOCH + Duration::from_secs(now_for_clock.load(Ordering::SeqCst))
            }),
        );

        // The fake key path is replaced with the test fixture by seeding the
        // cell, so no key file is read from the developer's machine.
        let seeded = provider
            .signing_key
            .set(Arc::new(parse_private_key(TEST_KEY).unwrap()));
        assert!(seeded.is_ok());
        assert_eq!(
            provider
                .token_for_repositories(&[("Acme".into(), "widget".into())])
                .await
                .unwrap(),
            "test-installation-token-1"
        );
        assert_eq!(
            provider
                .token_for_repositories(&[("acme".into(), "widget".into())])
                .await
                .unwrap(),
            "test-installation-token-1"
        );
        assert_eq!(fake.lookups.load(Ordering::SeqCst), 1);
        assert_eq!(fake.exchanges.load(Ordering::SeqCst), 1);
        assert_eq!(*fake.scopes.lock().unwrap(), [vec!["widget".to_owned()]]);
        assert!(fake.bodies.lock().unwrap()[0].get("permissions").is_none());

        fake.now.store(1_800_000_000 + 3600 - 601, Ordering::SeqCst);
        assert_eq!(
            provider
                .token_for_repositories(&[("acme".into(), "widget".into())])
                .await
                .unwrap(),
            "test-installation-token-1"
        );
        assert_eq!(fake.exchanges.load(Ordering::SeqCst), 1);

        fake.now.store(1_800_000_000 + 3600 - 599, Ordering::SeqCst);
        assert_eq!(
            provider
                .token_for_repositories(&[("acme".into(), "widget".into())])
                .await
                .unwrap(),
            "test-installation-token-2"
        );
        assert_eq!(fake.exchanges.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn token_cache_separates_installation_wide_and_sorted_repository_scopes() {
        let fake = fake_github(1_800_000_000);
        let (api_base, server) = test_server(fake.clone()).await;
        let mut config = test_config();
        config.installations.insert("acme".into(), 77331);
        let provider = GithubAppTokenProvider::with_test_transport(
            config,
            reqwest::Client::new(),
            api_base,
            Arc::new(|| UNIX_EPOCH + Duration::from_secs(1_800_000_000)),
        );
        let seeded = provider
            .signing_key
            .set(Arc::new(parse_private_key(TEST_KEY).unwrap()));
        assert!(seeded.is_ok());

        let first_scope = vec!["Zebra".to_owned(), "Alpha".to_owned()];
        let reordered_scope = vec!["alpha".to_owned(), "zebra".to_owned()];
        let write_contents = BTreeMap::from([("contents".into(), GithubPermissionLevel::Write)]);
        let read_contents = BTreeMap::from([("contents".into(), GithubPermissionLevel::Read)]);
        let first = provider
            .token_for_installation(77331, &first_scope, Some(&write_contents))
            .await
            .unwrap();
        let cached = provider
            .token_for_installation(77331, &reordered_scope, Some(&write_contents))
            .await
            .unwrap();
        let different_permissions = provider
            .token_for_installation(77331, &first_scope, Some(&read_contents))
            .await
            .unwrap();
        let narrower = provider
            .token_for_installation(77331, &["alpha".to_owned()], None)
            .await
            .unwrap();
        let installation_wide = provider
            .token_for_installation(77331, &[], None)
            .await
            .unwrap();

        assert_eq!(first, cached);
        assert_ne!(first, narrower);
        assert_ne!(first, different_permissions);
        assert_ne!(narrower, installation_wide);
        assert_eq!(fake.exchanges.load(Ordering::SeqCst), 4);
        assert_eq!(
            *fake.scopes.lock().unwrap(),
            [
                vec!["alpha".to_owned(), "zebra".to_owned()],
                vec!["alpha".to_owned(), "zebra".to_owned()],
                vec!["alpha".to_owned()],
                vec![]
            ]
        );
        assert_eq!(
            fake.bodies.lock().unwrap()[0]["permissions"]["contents"],
            "write"
        );
        assert_eq!(
            fake.bodies.lock().unwrap()[1]["permissions"]["contents"],
            "read"
        );
        server.abort();
    }

    #[tokio::test]
    async fn permission_subset_is_minted_with_repository_scope() {
        let fake = fake_github(1_800_000_000);
        let (api_base, server) = test_server(fake.clone()).await;
        let mut config = test_config();
        config.installations.insert("acme".into(), 77331);
        config.session_permissions = Some(BTreeMap::from([
            ("contents".into(), GithubPermissionLevel::Write),
            ("statuses".into(), GithubPermissionLevel::Read),
        ]));
        let provider = GithubAppTokenProvider::with_test_transport(
            config.clone(),
            reqwest::Client::new(),
            api_base,
            Arc::new(|| UNIX_EPOCH + Duration::from_secs(1_800_000_000)),
        );
        let seeded = provider
            .signing_key
            .set(Arc::new(parse_private_key(TEST_KEY).unwrap()));
        assert!(seeded.is_ok());

        provider
            .token_for_installation(
                77331,
                &["widget".to_owned()],
                config.session_permissions.as_ref(),
            )
            .await
            .unwrap();

        assert_eq!(
            fake.bodies.lock().unwrap()[0],
            serde_json::json!({
                "repositories": ["widget"],
                "permissions": {"contents": "write", "statuses": "read"}
            })
        );
        server.abort();
    }

    #[tokio::test]
    async fn permission_request_above_installation_grant_is_refused_before_minting() {
        let fake = fake_github(1_800_000_000);
        let (api_base, server) = test_server(fake.clone()).await;
        let mut config = test_config();
        config.installations.insert("acme".into(), 77331);
        config.token_permissions = Some(BTreeMap::from([(
            "statuses".into(),
            GithubPermissionLevel::Write,
        )]));
        let provider = GithubAppTokenProvider::with_test_transport(
            config,
            reqwest::Client::new(),
            api_base,
            Arc::new(|| UNIX_EPOCH + Duration::from_secs(1_800_000_000)),
        );
        let seeded = provider
            .signing_key
            .set(Arc::new(parse_private_key(TEST_KEY).unwrap()));
        assert!(seeded.is_ok());

        let error = provider.token_for_owner("acme").await.unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("statuses"), "{message}");
        assert!(
            message.contains("read") && message.contains("write"),
            "{message}"
        );
        assert_eq!(fake.exchanges.load(Ordering::SeqCst), 0);
        assert!(fake.bodies.lock().unwrap().is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn cached_token_does_not_hide_a_newly_insufficient_permission_grant() {
        let fake = fake_github(1_800_000_000);
        let (api_base, server) = test_server(fake.clone()).await;
        let mut config = test_config();
        config.installations.insert("acme".into(), 77331);
        config.token_permissions = Some(BTreeMap::from([(
            "contents".into(),
            GithubPermissionLevel::Write,
        )]));
        let now = Arc::clone(&fake.now);
        let provider = GithubAppTokenProvider::with_test_transport(
            config,
            reqwest::Client::new(),
            api_base,
            Arc::new(move || UNIX_EPOCH + Duration::from_secs(now.load(Ordering::SeqCst))),
        );
        let seeded = provider
            .signing_key
            .set(Arc::new(parse_private_key(TEST_KEY).unwrap()));
        assert!(seeded.is_ok());
        provider.token_for_owner("acme").await.unwrap();

        fake.installation_permissions
            .lock()
            .unwrap()
            .insert("contents".into(), "read".into());
        fake.now.store(1_800_000_000 + 3600 - 599, Ordering::SeqCst);
        let error = provider.token_for_owner("acme").await.unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("contents"), "{message}");
        assert!(
            message.contains("read") && message.contains("write"),
            "{message}"
        );
        assert_eq!(fake.exchanges.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn token_command_uses_its_configured_permission_table() {
        let fake = fake_github(1_800_000_000);
        let (api_base, server) = test_server(fake.clone()).await;
        let mut config = test_config();
        config.installations.insert("acme".into(), 77331);
        config.session_permissions = Some(BTreeMap::from([(
            "contents".into(),
            GithubPermissionLevel::Read,
        )]));
        config.token_permissions = Some(BTreeMap::from([(
            "statuses".into(),
            GithubPermissionLevel::Read,
        )]));
        let provider = GithubAppTokenProvider::with_test_transport(
            config,
            reqwest::Client::new(),
            api_base,
            Arc::new(|| UNIX_EPOCH + Duration::from_secs(1_800_000_000)),
        );
        let seeded = provider
            .signing_key
            .set(Arc::new(parse_private_key(TEST_KEY).unwrap()));
        assert!(seeded.is_ok());

        provider.token_for_owner("acme").await.unwrap();

        assert_eq!(
            fake.bodies.lock().unwrap()[0]["permissions"],
            serde_json::json!({"statuses": "read"})
        );
        server.abort();
    }

    #[tokio::test]
    async fn configured_installation_avoids_network_discovery() {
        let fake = fake_github(1_800_000_000);
        let (api_base, server) = test_server(fake.clone()).await;
        let now = Arc::clone(&fake.now);
        let mut config = test_config();
        config.installations.insert("acme".into(), 77331);
        let provider = GithubAppTokenProvider::with_test_transport(
            config,
            reqwest::Client::new(),
            api_base,
            Arc::new(move || UNIX_EPOCH + Duration::from_secs(now.load(Ordering::SeqCst))),
        );
        let seeded = provider
            .signing_key
            .set(Arc::new(parse_private_key(TEST_KEY).unwrap()));
        assert!(seeded.is_ok());
        assert_eq!(
            provider
                .installation_for_repo("ACME", "repo")
                .await
                .unwrap(),
            77331
        );
        assert_eq!(fake.lookups.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test]
    async fn bundle_selection_rejects_repositories_from_multiple_installations() {
        let mut config = mj_core::config::Config::default();
        config.github.app = Some(GithubAppConfig {
            installations: BTreeMap::from([("acme".into(), 11), ("widgets".into(), 22)]),
            ..test_config()
        });
        config.bundles.insert(
            "multi".into(),
            ProjectBundle {
                primary_repo: "app".into(),
                repositories: vec![
                    ProjectRepository {
                        id: "app".into(),
                        github: Some("acme/app".into()),
                        destination: "app".into(),
                        ..ProjectRepository::default()
                    },
                    ProjectRepository {
                        id: "shared".into(),
                        github: Some("git@github.com:widgets/shared.git".into()),
                        destination: "shared".into(),
                        ..ProjectRepository::default()
                    },
                ],
            },
        );
        let controller = Controller {
            config,
            state: mj_core::state::State::default(),
        };

        let error = controller
            .validate_github_bundle_installations("multi")
            .await
            .unwrap_err();
        let GithubBundleSelectionError::MultipleInstallations(message) = error else {
            panic!("expected the one-installation limit, got {error}");
        };
        assert!(message.contains("one installation per session"));
        assert!(message.contains("acme/app") && message.contains("widgets/shared"));

        let provider =
            GithubAppTokenProvider::shared(controller.config.github.app.as_ref().unwrap()).unwrap();
        let cli_error = provider
            .token_for_repositories(&[
                ("acme".into(), "app".into()),
                ("widgets".into(), "shared".into()),
            ])
            .await
            .unwrap_err();
        assert!(format!("{cli_error:#}").contains("span more than one GitHub App installation"));
    }

    fn rsa_public_components(der: &[u8]) -> (&[u8], &[u8]) {
        let (sequence, sequence_len, mut rest) = der_tlv(der);
        assert_eq!(sequence, 0x30);
        assert_eq!(sequence_len, rest.len());
        let (modulus_tag, _, modulus) = der_tlv(rest);
        assert_eq!(modulus_tag, 0x02);
        rest = &rest[1 + der_length_size(rest[1]) + modulus.len()..];
        let (exponent_tag, _, exponent) = der_tlv(rest);
        assert_eq!(exponent_tag, 0x02);
        (modulus.strip_prefix(&[0]).unwrap_or(modulus), exponent)
    }

    fn der_tlv(bytes: &[u8]) -> (u8, usize, &[u8]) {
        let tag = bytes[0];
        let first_len = bytes[1];
        let (length, length_size) = if first_len & 0x80 == 0 {
            (usize::from(first_len), 1)
        } else {
            let count = usize::from(first_len & 0x7f);
            let mut length = 0usize;
            for byte in &bytes[2..2 + count] {
                length = (length << 8) | usize::from(*byte);
            }
            (length, count + 1)
        };
        let start = 1 + length_size;
        (tag, length, &bytes[start..start + length])
    }

    fn der_length_size(first_len: u8) -> usize {
        if first_len & 0x80 == 0 {
            1
        } else {
            usize::from(first_len & 0x7f) + 1
        }
    }
}
