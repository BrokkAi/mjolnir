//! Writing a configuration into the user's existing file.
//!
//! A save edits the file in place instead of replacing it with a fresh
//! serialization, so the comments, blank lines, and order the user gave it
//! survive. Only settings whose value changed are touched.

use toml_edit::{Document, Item, TableLike};

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
    let loaded = loaded.parse::<toml::Table>().ok()?;
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
        true,
        &mut next_position,
    );
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
/// cleared it), and at the top level whatever this build no longer writes,
/// such as an obsolete section. A default the user wrote out stays.
fn apply(
    target: &mut dyn TableLike,
    tables: Tables<'_>,
    source: &dyn TableLike,
    top_level: bool,
    next_position: &mut usize,
) {
    let Tables { raw, before, after } = tables;
    let stale = target
        .iter()
        .map(|(key, _)| key.to_owned())
        .filter(|key| {
            !after.contains_key(key)
                && (top_level || before.is_some_and(|before| before.contains_key(key)))
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
                    apply(table, tables, source, false, next_position);
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
