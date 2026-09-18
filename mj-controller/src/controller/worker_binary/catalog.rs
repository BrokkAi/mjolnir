use super::*;

/// Fetches a provider's model catalog. A function parameter so tests can supply
/// a body without reaching the network. Takes the catalog URL and the API key;
/// returns the raw response body.
pub(in crate::controller) type CatalogFetch<'a> = &'a dyn Fn(&str, &str) -> Result<Vec<u8>>;

/// The catalog file Mjolnir writes into a staged Codex home, and the key it
/// points `config.toml` at. Relative to `CODEX_HOME`, so the staged copy works
/// unchanged on any target.
pub(super) const STAGED_CATALOG_FILE: &str = "models.json";

/// The `profile_config_cache` row one provider's fetched catalog is kept in.
///
/// That table is keyed by profile and model, and a discovered `ProfileConfig`
/// for a profile's default model already occupies the empty model key, so a
/// catalog body stored under it would evict the discovered configuration and
/// be evicted by it in turn. A provider's base URL is not a model slug, so
/// naming the row after it keeps the two apart and gives every provider a row
/// of its own.
pub(in crate::controller) fn catalog_cache_key(base_url: &str) -> String {
    format!("catalog:{base_url}")
}

/// Where a fetched catalog is remembered so a provider outage cannot block a
/// launch. The live store is one implementation; a test can supply another.
/// `key` is [`catalog_cache_key`] of the provider the body came from.
pub(in crate::controller) trait CatalogCache {
    fn load(&self, profile_id: &str, key: &str) -> Option<String>;
    fn store(&self, profile_id: &str, key: &str, body: &str);
}

/// The catalog cache backed by Mjolnir's own `profile_config_cache` table.
pub(in crate::controller) struct SharedCatalogCache;

impl CatalogCache for SharedCatalogCache {
    fn load(&self, profile_id: &str, key: &str) -> Option<String> {
        crate::database::load_profile_config_cache(profile_id, key, key)
            .ok()
            .flatten()
    }

    fn store(&self, profile_id: &str, key: &str, body: &str) {
        if let Err(error) = crate::database::save_profile_config_cache(
            profile_id.to_owned(),
            key.to_owned(),
            key.to_owned(),
            body.to_owned(),
        ) {
            tracing::warn!(profile_id, "could not cache the model catalog: {error:#}");
        }
    }
}

/// Give a staged Codex home the model catalog its provider advertises.
///
/// Codex fetches its model list from its own service only for ChatGPT logins.
/// Without a catalog file a profile pointed at another provider would offer
/// OpenAI's built-in model names and send them to that provider, so Mjolnir
/// fetches the provider's own catalog, stamps the Guardian reviewer on every
/// entry, writes it beside the staged `config.toml`, and points the staged
/// configuration at it.
///
/// Profiles with no custom provider are left alone. A failed fetch falls back to
/// the last catalog stored for this profile, so a provider outage does not block
/// a launch; with neither, the launch fails naming the profile and the URL.
pub(in crate::controller) fn stage_codex_catalog(
    profile_id: &str,
    profile: &mj_core::config::HarnessProfile,
    destination: &Path,
    fetch: CatalogFetch<'_>,
    cache: &dyn CatalogCache,
) -> Result<()> {
    let Some(provider) = profile.codex_provider()? else {
        return Ok(());
    };
    let Some(env_key) = provider.env_key.as_deref() else {
        // An inline `experimental_bearer_token` provider carries its key in the
        // staged file itself; Mjolnir has no key of its own to authorize a
        // catalog fetch with.
        return Ok(());
    };
    let api_key = profile.environment.get(env_key).with_context(|| {
        format!("profile {profile_id:?} has no {env_key} entry to read its model catalog with")
    })?;
    let url = format!("{}/models", provider.base_url.trim_end_matches('/'));
    let key = catalog_cache_key(&provider.base_url);
    let body = match fetch(&url, api_key) {
        Ok(body) => {
            if let Ok(text) = std::str::from_utf8(&body) {
                cache.store(profile_id, &key, text);
            }
            body
        }
        Err(error) => match cache.load(profile_id, &key) {
            Some(body) => {
                tracing::warn!(
                    profile_id,
                    provider = %provider.id,
                    "could not fetch the model catalog from {url}, using the last cached copy: {error:#}"
                );
                body.into_bytes()
            }
            None => bail!(
                "profile {profile_id:?}: could not fetch the model catalog from {url} and no cached copy is available: {error:#}"
            ),
        },
    };
    let mut catalog = mj_core::codex_catalog::parse(&body)
        .with_context(|| format!("profile {profile_id:?}: model catalog from {url}"))?;
    apply_catalog_overrides(profile_id, &profile.home, &mut catalog)?;
    stamp_guardian_reviewer(profile_id, profile, &mut catalog)?;
    std::fs::create_dir_all(destination)?;
    std::fs::write(destination.join(STAGED_CATALOG_FILE), catalog.to_json())?;
    point_config_at_catalog(&destination.join("config.toml"))
}

/// Refine the fetched catalog with the user's own `models.json`, when the
/// profile home has one.
///
/// A provider that serves OpenAI's plain model list gives Mjolnir only model
/// ids, so the translated entries carry conservative defaults. The override
/// file is how a user states what that provider actually supports: each entry
/// is matched by `slug` and its fields are copied over the fetched entry, and a
/// slug the provider did not list is added. Mjolnir writes the merged result
/// over the staged `models.json`, so the user's own file never reaches Codex
/// unmerged.
pub(super) fn apply_catalog_overrides(
    profile_id: &str,
    home: &Path,
    catalog: &mut mj_core::codex_catalog::CodexCatalog,
) -> Result<()> {
    let path = home.join(STAGED_CATALOG_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", path.display()));
        }
    };
    let overrides = mj_core::codex_catalog::parse_codex_shape(&bytes).with_context(|| {
        format!(
            "profile {profile_id:?}: model catalog overrides in {}",
            path.display()
        )
    })?;
    mj_core::codex_catalog::merge_overrides(catalog, &overrides);
    Ok(())
}

/// Record the Guardian reviewer choice on every catalog entry.
///
/// Codex reads the reviewer from the session model's own catalog entry, so the
/// override is stamped on all of them. The profile setting decides which model
/// that is: the newest flash model by default, the session model itself when
/// the setting is `session` (nothing is stamped, which is Codex's own
/// fallback), or a named slug. A named slug the catalog does not list fails the
/// launch, because stamping it would leave Codex silently reviewing with
/// something else.
pub(super) fn stamp_guardian_reviewer(
    profile_id: &str,
    profile: &mj_core::config::HarnessProfile,
    catalog: &mut mj_core::codex_catalog::CodexCatalog,
) -> Result<()> {
    let setting = profile
        .guardian_review_model
        .as_deref()
        .unwrap_or(mj_core::config::GUARDIAN_REVIEW_NEWEST_FLASH);
    if setting == mj_core::config::GUARDIAN_REVIEW_SESSION {
        tracing::info!(
            profile_id,
            "guardian_review_model is \"session\"; Guardian reviews run on the session model"
        );
        return Ok(());
    }
    if setting == mj_core::config::GUARDIAN_REVIEW_NEWEST_FLASH {
        match mj_core::codex_catalog::guardian_review_model(catalog.slugs()) {
            Some(reviewer) => mj_core::codex_catalog::stamp_reviewer(catalog, &reviewer),
            // With no small model to review with, Codex falls back to reviewing
            // with the session model, which still runs Guardian.
            None => tracing::info!(
                profile_id,
                "the model catalog lists no flash model; Guardian reviews run on the session model"
            ),
        }
        return Ok(());
    }
    let slugs = catalog.slugs();
    if !slugs.iter().any(|slug| slug == setting) {
        bail!(
            "profile {profile_id:?}: guardian_review_model {setting:?} is not in the provider's model catalog, which lists {}",
            slugs.join(", ")
        );
    }
    mj_core::codex_catalog::stamp_reviewer(catalog, setting);
    Ok(())
}

/// Prepend `model_catalog_json` to a staged Codex `config.toml`.
///
/// The key is top-level in Codex's configuration, and TOML puts every top-level
/// key before the first table header, so the line goes at the front. Appending
/// would make it a key of whichever table happens to come last, which Codex
/// ignores. Profile validation guarantees the user wrote no such key.
pub(super) fn point_config_at_catalog(path: &Path) -> Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    std::fs::write(
        path,
        format!("model_catalog_json = \"{STAGED_CATALOG_FILE}\"\n{existing}"),
    )
    .with_context(|| format!("point {} at the staged model catalog", path.display()))
}

/// Fetch a provider's catalog over HTTPS. Mirrors the bounded client the Coding
/// Plan quota reader uses: a short timeout and no redirects.
pub(in crate::controller) fn fetch_catalog_over_https(url: &str, api_key: &str) -> Result<Vec<u8>> {
    on_dedicated_thread(|| {
        let response = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .redirect(reqwest::redirect::Policy::none())
            .build()?
            .get(url)
            .bearer_auth(api_key)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()?
            .error_for_status()?;
        Ok(response.bytes()?.to_vec())
    })
}
