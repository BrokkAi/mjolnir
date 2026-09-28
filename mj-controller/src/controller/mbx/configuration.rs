//! Machine policy. Inspection never writes; application atomically publishes
//! the same document all containers mount. Native mbx configuration is read-only.

use super::*;
use mj_core::state::BuildCacheApplication;

const DEFAULT_MARKER: &str = "# mj automatic total: ";

/// Already reachable through every session's cache mount, including sessions
/// provisioned before machine-level policy. No container recreation is needed.
pub(super) fn shared_directory(cache: &Path) -> PathBuf {
    mj_core::config::build_cache_configuration_directory(cache)
}

pub(super) fn host_directory(host: &CacheHost, executor: &impl CommandExecutor) -> Result<PathBuf> {
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

pub(super) fn read_file(
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

pub(super) fn automatic_total(text: Option<&str>) -> Option<String> {
    text?
        .lines()
        .find_map(|line| line.strip_prefix(DEFAULT_MARKER))
        .filter(|size| mj_core::config::parse_build_cache_size(size).is_some())
        .map(str::to_owned)
}

pub(super) fn managed_document(settings: &TargetBuildCache, automatic: &str) -> Result<String> {
    #[derive(serde::Serialize)]
    struct Document<'a> {
        gc: Gc<'a>,
        target: Target<'a>,
    }
    #[derive(serde::Serialize)]
    struct Gc<'a> {
        max_total_size: &'a str,
    }
    #[derive(serde::Serialize)]
    struct Target<'a> {
        #[serde(skip_serializing_if = "Option::is_none")]
        max_size: Option<&'a str>,
    }
    let document = Document {
        gc: Gc {
            max_total_size: settings.max_size.as_deref().unwrap_or(automatic),
        },
        target: Target {
            max_size: settings.target_max_size.as_deref(),
        },
    };
    Ok(format!(
        "# Managed by Mjolnir. Change the machine's Build cache settings.\n{DEFAULT_MARKER}{automatic}\n{}",
        toml::to_string(&document)?
    ))
}

pub(super) fn configured_limit(
    text: Option<&str>,
    table: &str,
    field: &str,
) -> Result<Option<String>> {
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

/// Match the pinned mbx 1.16.0 defaults, not whatever a newer host binary uses.
pub(super) fn scaled_budget(
    total: Option<u64>,
    percent: u64,
    floor: u64,
    ceiling: u64,
    fallback: u64,
) -> String {
    const GIB: u64 = 1 << 30;
    let bytes = match total.filter(|total| *total > 0) {
        Some(total) => ((total / 100).saturating_mul(percent) / (5 * GIB) * (5 * GIB))
            .clamp(floor * GIB, ceiling * GIB),
        None => fallback * GIB,
    };
    format!("{bytes}B")
}

pub(super) fn disk_total(
    host: &CacheHost,
    directory: &Path,
    executor: &impl CommandExecutor,
) -> Result<u64> {
    let volume = nearest_existing_ancestor(host, directory, executor)?;
    let command = host.command(
        vec![
            "df".into(),
            "-B1".into(),
            "-P".into(),
            "--".into(),
            volume.to_string_lossy().into_owned(),
        ],
        "measure build cache capacity",
    );
    let output = checked(executor.execute(&command)?, &command)?;
    let text = String::from_utf8_lossy(&output.stdout);
    let fields = text
        .lines()
        .nth(1)
        .context("missing cache capacity")?
        .split_whitespace()
        .collect::<Vec<_>>();
    fields
        .get(
            fields
                .len()
                .checked_sub(5)
                .context("missing cache capacity column")?,
        )
        .context("missing cache capacity column")?
        .parse()
        .context("invalid cache capacity")
}

// A lock belongs to the host, not the container or daemon. Compare-and-replace
// prevents an inspection made before another application from overwriting it.
// Atomic rename also leaves the last complete policy intact on cancellation.
const APPLY_SCRIPT: &str = r#"set -eu
directory=$1
expected=$2
mkdir -p -- "$directory"
exec 9>"$directory/.mj-apply.lock"
flock -w 5 9
file="$directory/config.toml"
actual=missing
if [ -f "$file" ]; then actual=$(sha256sum -- "$file"); actual=${actual%% *}; fi
[ "$actual" = "$expected" ] || exit 75
temporary=$(mktemp "$directory/.config.XXXXXX")
trap 'rm -f -- "$temporary"' EXIT HUP INT TERM
cat > "$temporary"
chmod 600 "$temporary"
if [ -f "$file" ] && cmp -s -- "$temporary" "$file"; then exit 0; fi
mv -f -- "$temporary" "$file"
"#;

pub(super) fn apply(
    host: &CacheHost,
    cache: &ResolvedBuildCache,
    executor: &impl CommandExecutor,
) -> Result<bool> {
    if cache.previous_config == cache.config_file {
        return Ok(true);
    }
    let expected = cache
        .previous_config
        .as_deref()
        .map(|text| mj_core::hex::lower_hex(Sha256::digest(text.as_bytes())))
        .unwrap_or_else(|| "missing".into());
    let command = host
        .shell_command(
            APPLY_SCRIPT,
            LABEL,
            [
                cache.config_directory.to_string_lossy().into_owned(),
                expected,
            ],
            "apply machine build cache budgets",
        )
        .with_sensitive_stdin(
            cache
                .config_file
                .as_deref()
                .context("missing managed mbx configuration")?
                .as_bytes()
                .to_vec(),
        );
    let output = executor.execute(&command)?;
    if output.status == 75 {
        return Ok(false);
    }
    checked(output, &command)?;
    Ok(true)
}

pub(super) fn application(previous: Option<&str>, desired: Option<&str>) -> BuildCacheApplication {
    if previous == desired {
        BuildCacheApplication::Applied
    } else {
        BuildCacheApplication::Pending
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, symlink};

    fn cache(directory: &Path, previous: Option<String>, text: String) -> ResolvedBuildCache {
        ResolvedBuildCache {
            directory: directory.to_owned(),
            target_root: None,
            config_directory: shared_directory(directory),
            config_file: Some(text),
            previous_config: previous,
        }
    }

    #[test]
    fn both_consumers_see_atomic_budget_updates_without_relinking() {
        let temporary = tempfile::tempdir().unwrap();
        let executor = targets::ProcessExecutor;
        let first = managed_document(
            &TargetBuildCache {
                max_size: Some("500GiB".into()),
                target_max_size: Some("200GiB".into()),
                ..Default::default()
            },
            "100GB",
        )
        .unwrap();
        let initial = cache(temporary.path(), None, first.clone());
        assert!(apply(&CacheHost::Local, &initial, &executor).unwrap());
        let file = initial.config_directory.join("config.toml");
        let paths = [
            temporary.path().join("harness.toml"),
            temporary.path().join("reviewer.toml"),
        ];
        for path in &paths {
            symlink(&file, path).unwrap();
        }
        let second = managed_document(
            &TargetBuildCache {
                target_max_size: Some("300GiB".into()),
                ..Default::default()
            },
            "100GB",
        )
        .unwrap();
        // Exercise the shared subprocess stdin path beyond a pipe buffer.
        let second = format!("{second}# {}\n", "x".repeat(256 * 1024));
        let updated = cache(temporary.path(), Some(first), second.clone());
        assert!(apply(&CacheHost::Local, &updated, &executor).unwrap());
        for path in &paths {
            let text = std::fs::read_to_string(path).unwrap();
            assert_eq!(text, second);
            assert_eq!(
                configured_limit(Some(&text), "target", "max_size")
                    .unwrap()
                    .as_deref(),
                Some("300GiB")
            );
            assert_eq!(
                configured_limit(Some(&text), "gc", "max_total_size")
                    .unwrap()
                    .as_deref(),
                Some("100GB")
            );
        }
        let inode = std::fs::metadata(&file).unwrap().ino();
        assert!(
            apply(
                &CacheHost::Local,
                &cache(temporary.path(), Some(second.clone()), second),
                &executor
            )
            .unwrap()
        );
        assert_eq!(
            std::fs::metadata(&file).unwrap().ino(),
            inode,
            "unchanged policy must not be rewritten"
        );
    }

    #[test]
    fn existing_cache_mounts_receive_machine_changes_without_changing_the_source() {
        let temporary = tempfile::tempdir().unwrap();
        let host = CacheHost::Local;
        let executor = targets::ProcessExecutor;
        let source = temporary.path().join("native.toml");
        let current = temporary.path().join("current-cache");
        let existing = temporary.path().join("existing-container-cache");
        for budget in ["150GiB", "250GiB"] {
            let text = format!("[target]\nmax_size = '{budget}'\n");
            std::fs::write(&source, &text).unwrap();
            let policy = cache(&current, None, text.clone());
            super::super::publish_at(&host, &policy, &existing, &executor).unwrap();
            assert_eq!(std::fs::read_to_string(&source).unwrap(), text);
            assert_eq!(
                std::fs::read_to_string(shared_directory(&existing).join("config.toml")).unwrap(),
                text,
            );
        }
    }

    #[test]
    fn stale_application_cannot_overwrite_a_newer_machine_policy() {
        let temporary = tempfile::tempdir().unwrap();
        let executor = targets::ProcessExecutor;
        let first = cache(
            temporary.path(),
            None,
            "[gc]\nmax_total_size = '500GiB'\n".into(),
        );
        let stale = cache(
            temporary.path(),
            None,
            "[gc]\nmax_total_size = '1GiB'\n".into(),
        );
        assert!(apply(&CacheHost::Local, &first, &executor).unwrap());
        assert!(!apply(&CacheHost::Local, &stale, &executor).unwrap());
        assert_eq!(
            std::fs::read_to_string(first.config_directory.join("config.toml")).unwrap(),
            first.config_file.unwrap()
        );
    }

    #[test]
    fn managed_defaults_survive_explicit_budgets_and_clearing_them() {
        let initial = managed_document(&TargetBuildCache::default(), "17GB").unwrap();
        let automatic = automatic_total(Some(&initial)).unwrap();
        let explicit = managed_document(
            &TargetBuildCache {
                max_size: Some("500GiB".into()),
                target_max_size: Some("250GiB".into()),
                ..Default::default()
            },
            &automatic,
        )
        .unwrap();
        let restored = managed_document(
            &TargetBuildCache::default(),
            &automatic_total(Some(&explicit)).unwrap(),
        )
        .unwrap();
        assert_eq!(restored, initial);
        assert_eq!(
            configured_limit(Some(&restored), "target", "max_size").unwrap(),
            None
        );
    }

    #[test]
    fn action_store_budget_is_never_a_combined_budget() {
        assert_eq!(
            configured_limit(Some("[gc]\nmax_size = '500GiB'"), "gc", "max_total_size").unwrap(),
            None
        );
        assert_eq!(
            configured_limit(Some("[target]\nmax_size = 'none'"), "target", "max_size")
                .unwrap()
                .as_deref(),
            Some("none")
        );
        assert!(
            configured_limit(Some("[target]\nmax_size = 'lots'"), "target", "max_size").is_err()
        );
    }

    #[test]
    fn pinned_target_default_scales_and_caps_independently_of_the_total() {
        let gib = 1 << 30;
        assert_eq!(
            scaled_budget(Some(40 * gib), 10, 10, 100, 30),
            format!("{}B", 10 * gib)
        );
        assert_eq!(
            scaled_budget(Some(777 * gib), 10, 10, 100, 30),
            format!("{}B", 75 * gib)
        );
        assert_eq!(
            scaled_budget(Some(4000 * gib), 10, 10, 100, 30),
            format!("{}B", 100 * gib)
        );
        assert_eq!(
            scaled_budget(None, 10, 10, 100, 30),
            format!("{}B", 30 * gib)
        );
    }
}
