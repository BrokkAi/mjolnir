use super::*;
use agent_client_protocol::schema::v1::Diff;

fn diff(old_text: Option<&str>, new_text: &str) -> Diff {
    let mut diff = Diff::new("src/main.rs", new_text);
    diff.old_text = old_text.map(ToOwned::to_owned);
    diff
}

/// The point of the change: what gets stored is the size of the edit, not the
/// size of the file.
#[test]
fn compacting_an_edit_costs_the_edit_rather_than_the_file() {
    let old_text = (0..2_000)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    let new_text = old_text.replace("line 900\n", "line 900 changed\n");
    let mut compacted = diff(Some(&old_text), &new_text);
    let original = serde_json::to_vec(&compacted).unwrap().len();

    assert!(compact_diff(&mut compacted));

    let stored = serde_json::to_vec(&compacted).unwrap().len();
    assert!(
        stored * 20 < original,
        "storing a one-line edit to a {original}-byte diff still cost {stored} bytes"
    );
    assert_eq!(compacted.old_text, None);
    assert_eq!(compacted.new_text, "");
    let patch = patch_of(&compacted);
    assert_eq!((patch.insertions, patch.deletions), (1, 1));
    assert!(patch.text.contains("-line 900\n"));
    assert!(patch.text.contains("+line 900 changed\n"));
    assert!(!patch.text.contains("line 100\n"), "kept distant context");
    assert!(
        patch
            .text
            .starts_with("--- a/src/main.rs\n+++ b/src/main.rs\n")
    );
    assert!(!patch.created);
    assert!(!patch.truncated);
}

/// Counting changed lines must not depend on which form the record is in, or
/// the diffstat shown for an old session would disagree with a new one.
#[test]
fn the_stat_is_the_same_before_and_after_compaction() {
    let mut compacted = diff(Some("alpha\nbeta\ngamma\n"), "alpha\ndelta\ngamma\nomega\n");
    let before = patch_of(&compacted);

    compact_diff(&mut compacted);

    assert_eq!(patch_of(&compacted), before);
    assert_eq!((before.insertions, before.deletions), (2, 1));
}

/// A new file has no previous copy to diff against, and the reader has to be
/// able to say so.
#[test]
fn a_created_file_records_that_it_was_created() {
    let mut created = diff(None, "alpha\nbeta\n");

    assert!(compact_diff(&mut created));

    let patch = patch_of(&created);
    assert!(patch.created);
    assert_eq!((patch.insertions, patch.deletions), (2, 0));
    assert!(patch.text.starts_with("--- /dev/null\n+++ b/src/main.rs\n"));
    assert!(patch.text.contains("+alpha\n"));
}

/// A patch is proportional to the edit, but an edit can still be enormous. The
/// text is capped on hunk boundaries; the counts keep describing the whole
/// edit, so the stat stays honest even when the text is short.
#[test]
fn an_enormous_edit_keeps_whole_hunks_and_says_it_dropped_the_rest() {
    let old_text = (0..20_000)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    let new_text = old_text.replace("line ", "changed ");
    let mut huge = diff(Some(&old_text), &new_text);

    assert!(compact_diff(&mut huge));

    let patch = patch_of(&huge);
    assert!(patch.truncated);
    assert!(patch.text.len() <= DIFF_PATCH_BYTES + 64);
    assert!(patch.text.ends_with("[mj dropped the remaining hunks]\n"));
    assert_eq!(patch.insertions, 20_000);
    assert_eq!(patch.deletions, 20_000);
    // Whole hunks only: every line is a patch line, never a severed one.
    for line in patch
        .text
        .lines()
        .skip(2)
        .take_while(|line| !line.starts_with('['))
    {
        assert!(
            line.starts_with(['@', '+', '-', ' ']),
            "not a patch line: {line:?}"
        );
    }
}

/// Compaction runs on every recorded update, so it has to be idempotent and it
/// must not touch what the agent put in `_meta`.
#[test]
fn compaction_is_idempotent_and_leaves_other_metadata_alone() {
    let mut compacted = diff(Some("alpha\n"), "beta\n");
    compacted.meta = Some(
        [("agent.note".to_owned(), serde_json::json!("keep me"))]
            .into_iter()
            .collect(),
    );

    assert!(compact_diff(&mut compacted));
    let once = compacted.clone();
    assert!(!compact_diff(&mut compacted));

    assert_eq!(compacted, once);
    assert_eq!(
        compacted.meta.as_ref().unwrap().get("agent.note"),
        Some(&serde_json::json!("keep me"))
    );
}
