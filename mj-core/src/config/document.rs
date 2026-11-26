//! Writing a configuration into the user's existing file.
//!
//! A save edits the file in place instead of replacing it with a fresh
//! serialization, so the comments, blank lines, and order the user gave it
//! survive. Only settings whose value changed are touched.

use toml_edit::{Document, Item, TableLike};

use super::{RETIRED_KEYS, is_removed_harness_profile};

/// The first version whose file shape this build writes in place. Earlier
/// files store fused runtime and host tables that a save has to convert, so
/// they are rewritten whole.
const FIRST_IN_PLACE_VERSION: i64 = 12;

/// The file `existing` with the changes from `loaded` to `updated` applied.
///
/// `loaded` is the configuration as this build read `existing`, serialized;
/// comparing against it rather than against the raw file means a setting the
/// file leaves to its default is not written out just because this build
/// serializes it. `None` means the file cannot be edited in place and should
/// be written whole.
pub(super) fn edit_in_place(existing: &str, loaded: &str, updated: &str) -> Option<String> {
    let mut document = existing.parse::<Document>().ok()?;
    let version = document.get("version").and_then(Item::as_integer)?;
    if version < FIRST_IN_PLACE_VERSION {
        return None;
    }
    let raw = existing.parse::<toml::Table>().ok()?;
    let mut loaded = loaded.parse::<toml::Table>().ok()?;
    // The canonical load already folded this veto into machine settings.
    // Compare machines against the actual file so an otherwise unchanged
    // save writes those opt-outs before removing the obsolete global section.
    if raw
        .get("build_cache")
        .and_then(|cache| cache.get("enabled"))
        == Some(&toml::Value::Boolean(false))
    {
        match raw.get("machines") {
            Some(machines) => {
                loaded.insert("machines".into(), machines.clone());
            }
            None => {
                loaded.remove("machines");
            }
        }
    }
    // The legacy veto was folded during loading. Compare GitHub against
    // the actual file so saving writes the moved switch before removing it.
    if raw
        .get("mailbox")
        .and_then(|mailbox| mailbox.get("enabled"))
        == Some(&toml::Value::Boolean(false))
        && raw
            .get("github")
            .and_then(|github| github.get("watch"))
            .and_then(|watch| watch.get("enabled"))
            .is_none()
    {
        match raw.get("github") {
            Some(github) => {
                loaded.insert("github".into(), github.clone());
            }
            None => {
                loaded.remove("github");
            }
        }
    }
    // Loading removes HTTP userinfo. Compare those bundles against their raw
    // values so saving also removes the credentials from the file.
    if let Some(raw_bundles) = raw.get("bundles").and_then(toml::Value::as_table)
        && let Some(bundles) = loaded
            .get_mut("bundles")
            .and_then(toml::Value::as_table_mut)
    {
        for (id, bundle) in raw_bundles {
            let has_credentials = bundle
                .get("repositories")
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
                .any(|repository| {
                    repository
                        .get("github")
                        .and_then(toml::Value::as_str)
                        .is_some_and(|source| {
                            matches!(
                                crate::remote_git::project_source(
                                    source,
                                    crate::remote_git::Shorthand::Allowed
                                ),
                                Ok(std::borrow::Cow::Owned(_))
                            )
                        })
                });
            if has_credentials {
                bundles.insert(id.clone(), bundle.clone());
            }
        }
    }
    // Compare review against the file so saving replaces the legacy boolean
    // and writes the trigger even when its effective value has not changed.
    if let Some(review) = raw.get("review") {
        loaded.insert("review".into(), review.clone());
    }
    // Loading dropped every profile for a removed harness. Compare those
    // against the file so saving removes them.
    if let Some(raw_profiles) = raw.get("profiles").and_then(toml::Value::as_table)
        && let Some(loaded_profiles) = loaded
            .entry("profiles")
            .or_insert_with(|| toml::Value::Table(Default::default()))
            .as_table_mut()
    {
        for (id, profile) in raw_profiles {
            if is_removed_harness_profile(profile) {
                loaded_profiles.insert(id.clone(), profile.clone());
            }
        }
    }
    let updated_table = updated.parse::<toml::Table>().ok()?;
    let source = updated.parse::<Document>().ok()?;
    let mut next_position = usize::MAX / 2;
    apply(
        document.as_table_mut(),
        Tables {
            raw: Some(&raw),
            before: Some(&loaded),
            after: &updated_table,
        },
        source.as_table(),
        &mut next_position,
    );
    for path in RETIRED_KEYS {
        remove_retired(document.as_table_mut(), path, Some(&updated_table));
    }
    // The version is not a setting and is equal in both serializations, so
    // it is written here: the file now holds what this build writes.
    if let Some(version) = updated_table.get("version")
        && existing_version(&document) != version.as_integer()
        && let Some(item) = document.get_mut("version")
        && let Some(value) = item.as_value_mut()
    {
        let decor = value.decor().clone();
        *value = toml_edit::Value::from(version.as_integer().unwrap_or_default());
        *value.decor_mut() = decor;
    }
    Some(document.to_string())
}

/// Remove the retired key at `path` under `table`, `*` matching every key at
/// its level. A table that held nothing but retired keys is removed with
/// them, unless this build writes it (`written` is what it writes there).
fn remove_retired(table: &mut dyn TableLike, path: &[&str], written: Option<&toml::Table>) {
    let [first, rest @ ..] = path else {
        return;
    };
    let keys = if *first == "*" {
        table.iter().map(|(key, _)| key.to_owned()).collect()
    } else {
        vec![(*first).to_owned()]
    };
    for key in keys {
        if rest.is_empty() {
            table.remove(&key);
            continue;
        }
        let written = written
            .and_then(|written| written.get(&key))
            .and_then(toml::Value::as_table);
        let Some(child) = table.get_mut(&key).and_then(Item::as_table_like_mut) else {
            continue;
        };
        let had_keys = !child.is_empty();
        remove_retired(child, rest, written);
        if had_keys && child.is_empty() && written.is_none() {
            table.remove(&key);
        }
    }
}

fn existing_version(document: &Document) -> Option<i64> {
    document.get("version").and_then(Item::as_integer)
}

/// One table three ways: as the file spells it, as this build read it, and
/// as it is to be saved.
#[derive(Clone, Copy)]
struct Tables<'a> {
    raw: Option<&'a toml::Table>,
    before: Option<&'a toml::Table>,
    after: &'a toml::Table,
}

/// Brings `target` from `before` to `after`, leaving every key whose value
/// did not change exactly as the file wrote it.
///
/// A key the save drops is removed when this build read it (the user
/// cleared it). A default the user wrote out stays, at every level: it is
/// absent from both serializations, as a retired key is too, so retired keys
/// are removed by name instead ([`RETIRED_KEYS`]).
fn apply(
    target: &mut dyn TableLike,
    tables: Tables<'_>,
    source: &dyn TableLike,
    next_position: &mut usize,
) {
    let Tables { raw, before, after } = tables;
    let stale = target
        .iter()
        .map(|(key, _)| key.to_owned())
        .filter(|key| {
            !after.contains_key(key) && before.is_some_and(|before| before.contains_key(key))
        })
        .collect::<Vec<_>>();
    for key in stale {
        target.remove(&key);
    }
    for (key, value) in after {
        let previous = before.and_then(|before| before.get(key));
        let spelled = raw.and_then(|raw| raw.get(key));
        // Unchanged, and either spelled as this build writes it or left to
        // its default. A value the file spells another way, such as a
        // renamed choice, is rewritten in the current spelling.
        if previous == Some(value) && spelled.is_none_or(|spelled| spelled == value) {
            continue;
        }
        let Some(replacement) = source.get(key) else {
            continue;
        };
        match target.get_mut(key) {
            Some(existing) => {
                if let toml::Value::Table(value) = value
                    && let Some(table) = existing.as_table_like_mut()
                    && let Some(source) = replacement.as_table_like()
                {
                    let tables = Tables {
                        raw: spelled.and_then(toml::Value::as_table),
                        before: previous.and_then(toml::Value::as_table),
                        after: value,
                    };
                    apply(table, tables, source, next_position);
                } else if let Some(existing) = existing.as_value_mut()
                    && let Ok(mut value) = replacement.clone().into_value()
                {
                    // Keep the comment beside the value and its spacing.
                    *value.decor_mut() = existing.decor().clone();
                    *existing = value;
                } else {
                    *existing = placed_last(replacement.clone(), next_position);
                }
            }
            None => {
                target.insert(key, placed_last(replacement.clone(), next_position));
            }
        }
    }
}

/// A new table goes after every table the file already has, in the order
/// the serialization gives its parts.
fn placed_last(mut item: Item, next_position: &mut usize) -> Item {
    fn place(item: &mut Item, next_position: &mut usize) {
        match item {
            Item::Table(table) => {
                table.set_position(*next_position);
                *next_position += 1;
                for (_, child) in table.iter_mut() {
                    place(child, next_position);
                }
            }
            Item::ArrayOfTables(array) => {
                for table in array.iter_mut() {
                    table.set_position(*next_position);
                    *next_position += 1;
                    for (_, child) in table.iter_mut() {
                        place(child, next_position);
                    }
                }
            }
            _ => {}
        }
    }
    place(&mut item, next_position);
    item
}
