//! The host's own mbx configuration, which is only ever read. Each session's
//! launch stages a private snapshot of it; nothing is published into the
//! shared cache directory.

use super::*;

pub fn host_directory(host: &CacheHost, executor: &impl CommandExecutor) -> Result<PathBuf> {
    if matches!(host, CacheHost::Local) {
        return dirs::config_dir()
            .map(|path| path.join("mbx"))
            .context("locate host mbx configuration");
    }
    let command = host.shell_command(
        r#"printf '%s' "${XDG_CONFIG_HOME:-$HOME/.config}/mbx""#,
        LABEL,
        [],
        "locate host mbx configuration",
    );
    let output = checked(executor.execute(&command)?, &command)?;
    let path =
        PathBuf::from(String::from_utf8(output.stdout).context("decode mbx configuration path")?);
    ensure!(
        path.is_absolute(),
        "host mbx configuration directory is not absolute"
    );
    Ok(path)
}

pub fn read_file(
    host: &CacheHost,
    path: &Path,
    executor: &impl CommandExecutor,
) -> Result<Option<String>> {
    let command = host.shell_command(
        READ_CONFIG_SCRIPT,
        LABEL,
        [path.to_string_lossy().into_owned()],
        "read machine mbx configuration",
    );
    let output = executor.execute(&command)?;
    if output.status == 3 {
        return Ok(None);
    }
    let output = checked(output, &command)?;
    Ok(Some(
        String::from_utf8(output.stdout).context("decode machine mbx configuration")?,
    ))
}

pub fn configured_limit(text: Option<&str>, table: &str, field: &str) -> Result<Option<String>> {
    let Some(text) = text else {
        return Ok(None);
    };
    let document: toml::Value = toml::from_str(text).context("parse host mbx configuration")?;
    let Some(value) = document.get(table).and_then(|table| table.get(field)) else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .with_context(|| format!("mbx {table}.{field} must be a size string"))?;
    ensure!(
        value == "none" || mj_core::config::parse_build_cache_size(value).is_some(),
        "mbx {table}.{field} is not a size: {value:?}"
    );
    Ok(Some(value.to_owned()))
}

/// The host configuration as a session's container must see it, so the mbx
/// inside reads and writes only the mounts that session was created with.
///
/// The host's configuration can change after the container was created, or
/// between the inspection that chose its mounts and this read. `cache_dir` and
/// `[target] root` are therefore pinned to what the session recorded; every
/// other setting passes through. A document that already agrees is returned
/// byte for byte. A rewrite drops comments and formatting.
///
/// A record from before the target root was recorded (`None`) mounted only the
/// cache directory, so a root outside it is removed and mbx falls back to
/// `<cache>/targets`; a root inside it is kept.
///
/// A document that is not TOML is refused without quoting it: it can hold
/// credentials, and the error reaches notices and logs.
pub fn reconcile_container_config(text: &str, cache: &SessionBuildCache) -> Result<String> {
    use mj_core::target_path::{is_within, join, normalize, text as path_text};

    let mut document: toml::Table = toml::from_str(text).map_err(|error| {
        let line = error
            .span()
            .map(|span| text[..span.start.min(text.len())].matches('\n').count() + 1);
        match line {
            Some(line) => {
                anyhow::anyhow!("the host mbx configuration is not valid TOML (line {line})")
            }
            None => anyhow::anyhow!("the host mbx configuration is not valid TOML"),
        }
    })?;
    let directory = normalize(&cache.directory);
    let mut changed = false;

    if let Some(value) = document.get("cache_dir") {
        let configured = value
            .as_str()
            .context("the host mbx configuration's cache_dir must be a string")?;
        if normalize(&join(&directory, Path::new(configured))) != directory {
            document.insert(
                "cache_dir".into(),
                toml::Value::String(path_text(&cache.directory)),
            );
            changed = true;
        }
    }

    let configured_root = match document.get("target") {
        None => None,
        Some(target) => target
            .as_table()
            .context("the host mbx configuration's target must be a table")?
            .get("root")
            .map(|root| {
                root.as_str()
                    .map(PathBuf::from)
                    .context("the host mbx configuration's target.root must be a string")
            })
            .transpose()?,
    };
    let current = normalize(&match &configured_root {
        Some(root) => join(&directory, root),
        None => directory.join("targets"),
    });
    match &cache.target_root {
        Some(recorded) if normalize(recorded) != current => {
            let target = document
                .entry("target")
                .or_insert_with(|| toml::Value::Table(toml::Table::new()))
                .as_table_mut()
                .expect("checked to be a table above");
            target.insert("root".into(), toml::Value::String(path_text(recorded)));
            changed = true;
        }
        None if configured_root.is_some() && !is_within(&current, &directory) => {
            if let Some(target) = document
                .get_mut("target")
                .and_then(toml::Value::as_table_mut)
            {
                target.remove("root");
            }
            changed = true;
        }
        _ => {}
    }

    if !changed {
        return Ok(text.to_owned());
    }
    toml::to_string(&document).context("serialize the container's mbx configuration")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recorded(target_root: Option<&str>) -> SessionBuildCache {
        SessionBuildCache {
            host: "local".into(),
            directory: "/cache".into(),
            max_size: None,
            target_root: target_root.map(PathBuf::from),
        }
    }

    fn table(text: &str) -> toml::Table {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn a_configuration_that_agrees_with_the_record_passes_through_byte_for_byte() {
        let text = "# keep me\ncache_dir = \"/cache\"\n[gc]\nmax_total_size = \"1GB\"\n";
        for target_root in [None, Some("/cache/targets")] {
            assert_eq!(
                reconcile_container_config(text, &recorded(target_root)).unwrap(),
                text
            );
        }
    }

    #[test]
    fn a_recorded_target_root_wins_over_a_host_configuration_changed_since() {
        let host = "cache_dir = \"/elsewhere\"\n[target]\nroot = \"/mnt/new\"\n[gc]\nmax_total_size = \"1GB\"\n";
        for (record, root) in [
            (Some("/cache/views"), "/cache/views"),
            (Some("/mnt/slow/targets"), "/mnt/slow/targets"),
            (Some("/cache/targets"), "/cache/targets"),
        ] {
            let out = table(&reconcile_container_config(host, &recorded(record)).unwrap());
            assert_eq!(out["cache_dir"].as_str(), Some("/cache"));
            assert_eq!(out["target"]["root"].as_str(), Some(root));
            assert_eq!(out["gc"]["max_total_size"].as_str(), Some("1GB"));
        }
        // A host that has since gained a root where the session recorded the
        // default is pinned back to what was mounted.
        let out = table(
            &reconcile_container_config(
                "[target]\nroot = \"/mnt/new\"\n",
                &recorded(Some("/cache/targets")),
            )
            .unwrap(),
        );
        assert_eq!(out["target"]["root"].as_str(), Some("/cache/targets"));
    }

    #[test]
    fn a_record_without_a_target_root_drops_an_unmounted_root_but_keeps_one_in_the_cache() {
        for outside in ["/mnt/new", "../outside", "/cache/../outside"] {
            let host = format!("[target]\nroot = \"{outside}\"\nmax_age = \"30d\"\n");
            let out = table(&reconcile_container_config(&host, &recorded(None)).unwrap());
            assert!(
                out["target"].as_table().unwrap().get("root").is_none(),
                "{outside}"
            );
            assert_eq!(out["target"]["max_age"].as_str(), Some("30d"));
        }
        let inside = "[target]\nroot = \"views\"\n";
        assert_eq!(
            reconcile_container_config(inside, &recorded(None)).unwrap(),
            inside
        );
    }

    #[test]
    fn a_malformed_configuration_is_refused_without_quoting_it() {
        let secret = "token = \"s3cr3t-value\"\n[";
        let error = reconcile_container_config(secret, &recorded(None)).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("not valid TOML (line "), "{message}");
        assert!(!message.contains("s3cr3t"), "{message}");
    }
}
