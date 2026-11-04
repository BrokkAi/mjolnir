use super::*;

#[cfg(test)]
pub(super) use mj_client::settings::page::visible_keys;
pub(super) use mj_client::settings::page::{
    PageRow, RowKind, breadcrumb, is_collection, page_plan, row_label, row_page, visible_paths,
};

/// A size string as the whole number of GB the field is measured in, or the
/// string itself when it is not a size at all.
/// Compiler time the cache avoided, in units somebody reads at a glance. The
/// number is mbx's own estimate, so more precision than this would be false.
/// What one row reports, which on the first page is the state of a whole
/// section rather than the size of it.
pub(super) fn row_summary(
    path: &[String],
    key: &str,
    value: &Value,
    draft: &Value,
    automatic: Option<String>,
) -> String {
    if path.is_empty()
        && let Some(summary) = schema::section_summary(key, draft)
    {
        return summary;
    }
    let mut child_path = path.to_vec();
    child_path.push(key.to_owned());
    machine_build_cache_summary(&child_path, value, automatic.as_deref())
        .unwrap_or_else(|| value_summary(path, key, value, draft, automatic))
}

/// Configured policy only: opening the page resolves the host's actual defaults.
/// A host the preview found unable to share the cache reports `unavailable`,
/// which is the same fact the build cache page shows as an unchecked switch.
pub(super) fn machine_build_cache_summary(
    path: &[String],
    value: &Value,
    unavailable: Option<&str>,
) -> Option<String> {
    let configured = mj_client::settings::page::machine_build_cache_summary(path, value)?;
    Some(format!("{}  ›", unavailable.unwrap_or(&configured)))
}

/// `automatic` replaces the placeholder for an unset value when its resolved
/// default is known.
pub(super) fn value_summary(
    path: &[String],
    key: &str,
    value: &Value,
    draft: &Value,
    automatic: Option<String>,
) -> String {
    let mut child_path = path.to_vec();
    child_path.push(key.to_owned());
    let summary = match value {
        Value::Object(_) | Value::Array(_) => format!(
            "{}  ›",
            mj_client::settings::page::contents_summary(value).expect("a page holds entries")
        ),
        // The machine's own switch is a checkbox, and an unset value means on.
        // A host that cannot support the cache reports an unchecked box
        // through `automatic`, whatever the machine asks for.
        Value::Bool(_) | Value::Null if is_build_cache_field(&child_path, "enabled") => {
            if value.as_bool().unwrap_or(true) {
                automatic.unwrap_or_else(|| Checkbox::marker(true).to_owned())
            } else {
                Checkbox::marker(false).to_owned()
            }
        }
        // Every on/off setting is the same checkbox, whichever section holds it.
        Value::Bool(value) => Checkbox::marker(*value).to_owned(),
        // The cache size is measured in whole GB, whatever unit the file
        // spells it in. Only a hand-edited invalid value keeps its own text.
        Value::String(size) if is_build_cache_budget(&child_path) => automatic
            .filter(|label| label != "Resolving…" && label != "Unknown")
            .unwrap_or_else(|| build_cache_gigabytes_label(size)),
        Value::Null => automatic.unwrap_or_else(|| schema::null_label(&child_path, draft)),
        // The archive window's live estimate carries the value itself, so it
        // replaces the number as well as the "Never" placeholder.
        _ => match automatic.filter(|_| key == "archive_after_days") {
            Some(label) => label,
            None if storage_path(&child_path) == ["theme"] => {
                schema::draft_theme_report(value, draft)
            }
            None => schema::choice_label(&child_path, value, draft),
        },
    };
    if !value.is_object()
        && !value.is_array()
        && !value.is_boolean()
        && !schema::choices(&storage_path(&child_path), draft).is_empty()
    {
        ComboBox::display_value(&summary)
    } else {
        summary
    }
}
