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
