//! A flat index over every settings page and value.
//!
//! Settings are stored as a tree and browsed one page at a time, so reaching a
//! value means already knowing which section holds it. The index makes every
//! row addressable by what it is called, what it does, and what it is set to,
//! so the dialog can jump straight to one.

use super::{pointer, row_label, row_summary, schema, visible_keys};
use crate::widgets::counted;
use mj_chat::theme;
use ratatui::text::{Line, Span};
use serde_json::Value;

/// One row the dialog can navigate to, addressed by the same display path that
/// browsing to it would have produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SearchEntry {
    /// The display path, which keeps the synthetic `interface` section that
    /// [`pointer`] strips before it touches the draft.
    pub(super) path: Vec<String>,
    pub(super) label: String,
    /// The pages above this row, such as `Runtimes › podman`.
    pub(super) trail: String,
    /// What the row shows on its own page, so a search often answers the
    /// question without opening anything.
    pub(super) value: String,
    /// Everything matched against, lowercased once at build time.
    haystack: String,
}

/// Every page and value in `draft`, in the order the pages list them.
pub(super) fn index(draft: &Value) -> Vec<SearchEntry> {
    let mut entries = Vec::new();
    walk(draft, &mut Vec::new(), "", &mut entries);
    entries
}

fn walk(draft: &Value, path: &mut Vec<String>, trail: &str, entries: &mut Vec<SearchEntry>) {
    let Some(current) = draft.pointer(&pointer(path)) else {
        return;
    };
    for key in visible_keys(path, current) {
        let parent = path.clone();
        path.push(key.clone());
        let Some(value) = draft.pointer(&pointer(path)) else {
            path.pop();
            continue;
        };
        let label = row_label(&parent, current, &key, Some(value));
        let summary = page_summary(path, value, draft)
            .unwrap_or_else(|| row_summary(&parent, &key, value, draft, None));
        entries.push(SearchEntry {
            haystack: format!("{label} {trail} {key} {summary} {}", schema::help(path))
                .to_lowercase(),
            path: path.clone(),
            label: label.clone(),
            trail: trail.to_owned(),
            value: summary,
        });
        // Code Review is edited by its own dialog, so its values are not
        // reachable as a page of the tree and must not be indexed as one.
        if (value.is_object() || value.is_array()) && path.as_slice() != ["review"] {
            let nested = if trail.is_empty() {
                label
            } else {
                format!("{trail} › {label}")
            };
            walk(draft, path, &nested, entries);
        }
        path.pop();
    }
}

/// What a page row holds, counted the way the page itself lists it: `interface`
/// gathers three settings stored at the root, and hidden keys are left out. The
/// stored key count answers neither.
fn page_summary(path: &[String], value: &Value, draft: &Value) -> Option<String> {
    if !value.is_object() && !value.is_array() {
        return None;
    }
    // A first-page section says what it is set to, exactly as the page does.
    if let [key] = path
        && let Some(summary) = schema::section_summary(key, draft)
    {
        return Some(summary);
    }
    let count = visible_keys(path, value).len();
    Some(if value.is_array() {
        format!("{}  \u{203a}", counted(count, "entry", "entries"))
    } else {
        format!("{}  \u{203a}", counted(count, "setting", "settings"))
    })
}

/// The entries matching `query`, as indices into `entries`.
///
/// An empty query matches everything, which makes opening the search a flat
/// view of every setting. The ranking is the command palette's rule: a label
/// the query starts, else anything the query appears in.
pub(super) fn matches(entries: &[SearchEntry], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return (0..entries.len()).collect();
    }
    let prefix = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.label.to_lowercase().starts_with(&query))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if !prefix.is_empty() {
        return prefix;
    }
    entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.haystack.contains(&query))
        .map(|(index, _)| index)
        .collect()
}

/// One result row: the name, the pages it lives under, and its current value.
pub(super) fn row(entry: &SearchEntry, width: u16) -> Line<'static> {
    let width = usize::from(width).max(24);
    let label_width = (width / 3).clamp(12, 34);
    let trail_width = (width / 3).clamp(10, 32);
    let value_width = width.saturating_sub(label_width + trail_width + 4);
    Line::from(vec![
        Span::raw(pad(&entry.label, label_width)),
        Span::raw("  "),
        Span::styled(pad(&entry.trail, trail_width), theme::muted()),
        Span::raw("  "),
        Span::styled(clip(&entry.value, value_width), theme::muted()),
    ])
}

fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    text.chars()
        .take(width.saturating_sub(1))
        .chain(['…'])
        .collect()
}

fn pad(text: &str, width: usize) -> String {
    let text = clip(text, width);
    let padding = width.saturating_sub(text.chars().count());
    format!("{text}{}", " ".repeat(padding))
}
