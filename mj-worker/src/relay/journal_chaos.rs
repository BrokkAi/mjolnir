//! Property-based chaos tests for the relay journal's recoverability.
//!
//! Each case builds a real on-disk journal, injects one randomized fault, and
//! asserts the recovery contract: every record that is not actively corrupted
//! is recovered, exactly the corrupt records are reported as gaps, no corrupt
//! record is ever served as valid, and ordinals stay monotonic. Faults and
//! their reproduction (seed + shrunk minimal case) are handled by proptest; the
//! `proptest-regressions/` file pins any discovered minimal case as a permanent
//! regression.
//!
//! Fully hermetic: every journal lives in a `tempfile::tempdir()`; nothing
//! touches the user's real data or config directories.

use std::ops::ControlFlow;

use proptest::prelude::*;
use tempfile::TempDir;

use super::{
    JournalReadMode, RELAY_ACTIVE_SEGMENT, RELAY_JOURNAL_DIR, sealed_relay_journal_metadata,
    set_seal_byte_limit_override, visit_relay_journal_file,
};
use crate::relay::test_support::{acknowledge_relay, ready_checkpoint, submit_relay};
use crate::relay::{
    DurableRelay, RelayCommand, RelayEvent, RelayObservation, validate_relay_event_self,
};

const SESSION: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";

/// An ordinal and its digest — the ground truth for a recovered record.
type Record = (u64, String);

fn chaos_config(default_cases: u32) -> ProptestConfig {
    let mut config = ProptestConfig::default();
    if std::env::var_os("PROPTEST_CASES").is_none() {
        config.cases = default_cases;
    }
    config
}

struct SealOverride;

impl SealOverride {
    fn install(limit: u64) -> Self {
        set_seal_byte_limit_override(Some(limit));
        Self
    }
}

impl Drop for SealOverride {
    fn drop(&mut self) {
        set_seal_byte_limit_override(None);
    }
}

/// Build a single-segment (active-only) journal of `sizes.len()` v2 records and
/// return the tempdir plus the truth set in order. Sealing is disabled so every
/// record lands in `active.jsonl` with a predictable layout.
fn build_active_journal(sizes: &[usize]) -> (TempDir, Vec<Record>) {
    let _seal = SealOverride::install(u64::MAX);
    let temp = tempfile::tempdir().unwrap();
    let mut truth = Vec::new();
    {
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        for &size in sizes {
            let ordinal = relay
                .record_observation(RelayObservation::Warning {
                    message: "x".repeat(size),
                })
                .unwrap();
            truth.push((ordinal, relay.latest_digest().to_owned()));
        }
    }
    (temp, truth)
}

fn capture_new_records(relay: &DurableRelay, truth: &mut Vec<Record>) {
    let (after, digest) = truth
        .last()
        .cloned()
        .unwrap_or((0, crate::relay::RELAY_EVENT_GENESIS_DIGEST.to_owned()));
    truth.extend(
        relay
            .events_after(after, &digest)
            .unwrap()
            .into_iter()
            .map(|event| (event.ordinal, event.digest)),
    );
}

fn append_warning(relay: &mut DurableRelay, truth: &mut Vec<Record>, size: usize) {
    let ordinal = relay
        .record_observation(RelayObservation::Warning {
            message: "m".repeat(size),
        })
        .unwrap();
    truth.push((ordinal, relay.latest_digest().to_owned()));
}

fn active_events(path: &std::path::Path) -> Vec<(RelayEvent, (usize, usize))> {
    let bytes = std::fs::read(path).unwrap();
    line_ranges(&bytes)
        .into_iter()
        .filter_map(|range @ (start, len)| {
            serde_json::from_slice::<RelayEvent>(&bytes[start..start + len - 1])
                .ok()
                .map(|event| (event, range))
        })
        .collect()
}

fn journal_files(temp: &TempDir) -> Vec<std::path::PathBuf> {
    let mut files = std::fs::read_dir(temp.path().join(RELAY_JOURNAL_DIR))
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn active_path(temp: &TempDir) -> std::path::PathBuf {
    temp.path()
        .join(RELAY_JOURNAL_DIR)
        .join(RELAY_ACTIVE_SEGMENT)
}

/// Byte ranges `(start, len_including_newline)` of each newline-terminated line.
fn line_ranges(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            out.push((start, i - start + 1));
            start = i + 1;
        }
    }
    out
}

/// Read every record `Recover` yields, and the gaps it reports.
fn recover(path: &std::path::Path) -> Result<(Vec<Record>, usize), String> {
    let mut recovered = Vec::new();
    let gaps = visit_relay_journal_file(path, JournalReadMode::Recover, |event, _| {
        recovered.push((event.ordinal, event.digest.clone()));
        Ok(ControlFlow::Continue(()))
    })
    .map_err(|error| format!("recover read errored: {error:#}"))?;
    Ok((recovered, gaps.len()))
}

proptest! {
    #![proptest_config(chaos_config(128))]

    /// Whatever single fault hits the active segment, the Recover reader returns
    /// exactly the records the fault could not have destroyed, reports the
    /// corrupt ones as gaps, and never yields a record that is not in the truth
    /// set or that fails its own digest.
    #[test]
    fn recover_reader_isolates_a_single_active_fault(
        message_sizes in prop::collection::vec(0usize..48, 1..12),
        fault_kind in 0u8..4,
        idx_seed in any::<u64>(),
        byte_seed in any::<u64>(),
    ) {
        let (temp, truth) = build_active_journal(&message_sizes);
        let active = active_path(&temp);
        let bytes = std::fs::read(&active).unwrap();
        let lines = line_ranges(&bytes);
        prop_assert_eq!(lines.len(), truth.len(), "one line per recorded event");
        let count = truth.len();
        let idx = (idx_seed as usize) % count;

        // Apply the fault and compute the exact oracle (survivors + gap count).
        let (expected_survivors, expected_gaps): (Vec<Record>, usize) = match fault_kind {
            0 => {
                // Flip a content byte of record `idx` (never its trailing
                // newline). Any flip either breaks the JSON or changes a digested
                // field, so record `idx` becomes corrupt and nothing else does.
                let (start, len) = lines[idx];
                let content_end = start + len - 1; // exclude '\n'
                let pos = start + (byte_seed as usize) % (content_end - start).max(1);
                let mut faulted = bytes.clone();
                faulted[pos] ^= 0x40;
                std::fs::write(&active, &faulted).unwrap();
                let survivors = truth
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != idx)
                    .map(|(_, record)| record.clone())
                    .collect();
                (survivors, 1)
            }
            1 => {
                // Insert a terminated garbage line after record `idx`: one extra
                // corrupt record, every real record survives.
                let insert_at = lines[idx].0 + lines[idx].1;
                let mut faulted = bytes[..insert_at].to_vec();
                faulted.extend_from_slice(br#"{"ordinal":0,"observation": BROKEN"#);
                faulted.push(b'\n');
                faulted.extend_from_slice(&bytes[insert_at..]);
                std::fs::write(&active, &faulted).unwrap();
                (truth.clone(), 1)
            }
            2 => {
                // Torn append: a partial record with no newline. Not a committed
                // record, so it is not a gap and every real record survives.
                let mut faulted = bytes.clone();
                faulted.extend_from_slice(br#"{"ordinal":999,"partial"#);
                std::fs::write(&active, &faulted).unwrap();
                (truth.clone(), 0)
            }
            _ => {
                // Truncate in the middle of record `idx`: records fully before it
                // survive; `idx` and everything after are lost to a torn tail
                // (no gap — an unterminated tail is never a committed record).
                let (start, len) = lines[idx];
                let cut = start + (len / 2).max(1);
                let faulted = bytes[..cut].to_vec();
                std::fs::write(&active, &faulted).unwrap();
                (truth[..idx].to_vec(), 0)
            }
        };

        let (recovered, gaps) = recover(&active).map_err(TestCaseError::fail)?;

        // No fabricated data: every recovered record is a real one.
        for record in &recovered {
            prop_assert!(
                truth.contains(record),
                "recovered a record not in the truth set: {record:?}"
            );
        }
        // Ordinals are strictly increasing.
        prop_assert!(
            recovered.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "recovered ordinals must be strictly increasing: {recovered:?}"
        );
        // Exactly the survivors, exactly the expected gaps.
        prop_assert_eq!(&recovered, &expected_survivors);
        prop_assert_eq!(gaps, expected_gaps);
    }

    /// Corruption is always detected, never fabricated: no record that passes
    /// self-validation is outside the truth set, and the corrupt record never
    /// passes as its original self. (The raw reader is a parser; digest
    /// validation is the caller's job, so we apply it here as real serve paths
    /// like `read_events_after` do.)
    #[test]
    fn corruption_is_detected_and_never_fabricated(
        message_sizes in prop::collection::vec(0usize..48, 2..10),
        idx_seed in any::<u64>(),
        byte_seed in any::<u64>(),
    ) {
        let (temp, truth) = build_active_journal(&message_sizes);
        let active = active_path(&temp);
        let bytes = std::fs::read(&active).unwrap();
        let lines = line_ranges(&bytes);
        let idx = (idx_seed as usize) % truth.len();
        let (start, len) = lines[idx];
        let content_end = start + len - 1;
        let pos = start + (byte_seed as usize) % (content_end - start).max(1);
        let mut faulted = bytes.clone();
        faulted[pos] ^= 0x40;
        std::fs::write(&active, &faulted).unwrap();

        let mut served_valid = Vec::new();
        // Strict aborts on a parse error; keep whatever it parsed before that.
        let _ = visit_relay_journal_file(&active, JournalReadMode::Strict, |event, _| {
            if validate_relay_event_self(&event).is_ok() {
                served_valid.push((event.ordinal, event.digest.clone()));
            }
            Ok(ControlFlow::Continue(()))
        });

        for record in &served_valid {
            prop_assert!(
                truth.contains(record),
                "a record that passed self-validation must be real: {record:?}"
            );
        }
        prop_assert!(
            !served_valid.contains(&truth[idx]),
            "the corrupt record must not validate as its original: {:?}",
            truth[idx]
        );
    }

    /// Generated multi-segment histories exercise append, seal, acknowledge,
    /// checkpoint-prune, reopen, corrupt, truncate, delete, duplicate, and
    /// recover transitions. A fault may discard only unacknowledged tail data:
    /// a successful reopen must preserve the exact acknowledged cursor and may
    /// serve only genuine, strictly ordered records from the reference model.
    #[test]
    fn acknowledged_frontier_survives_generated_multisegment_faults(
        prefix_sizes in prop::collection::vec(8usize..96, 8..24),
        tail_sizes in prop::collection::vec(8usize..96, 4..16),
        checkpoint_prune in any::<bool>(),
        ack_seed in any::<u64>(),
        fault_kind in 0u8..6,
    ) {
        let _seal = SealOverride::install(384);
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let mut truth = Vec::new();
        for size in prefix_sizes {
            append_warning(&mut relay, &mut truth, size);
        }

        let acknowledged = if checkpoint_prune {
            let ready = ready_checkpoint(&mut relay, "model-checkpoint");
            capture_new_records(&relay, &mut truth);
            acknowledge_relay(&mut relay, "model-ack", ready.ordinal);
            submit_relay(
                &mut relay,
                "model-checkpoint-complete",
                RelayCommand::CompleteCheckpoint {
                    barrier_command_id: "model-checkpoint".into(),
                },
            );
            capture_new_records(&relay, &mut truth);
            (ready.ordinal, ready.digest)
        } else {
            let index = (ack_seed as usize) % (truth.len() + 1);
            if index == 0 {
                (0, crate::relay::RELAY_EVENT_GENESIS_DIGEST.to_owned())
            } else {
                let acknowledged = truth[index - 1].clone();
                acknowledge_relay(&mut relay, "model-ack", acknowledged.0);
                acknowledged
            }
        };

        for size in tail_sizes {
            append_warning(&mut relay, &mut truth, size);
        }
        prop_assert!(
            journal_files(&temp)
                .iter()
                .filter(|path| path.extension().is_some_and(|extension| extension == "gz"))
                .count()
                >= 2,
            "generated history must cross multiple sealed segments"
        );
        drop(relay);

        let active = active_path(&temp);
        match fault_kind {
            0 => {
                // Corrupt the last unacknowledged record without changing the
                // newline boundary, so recovery either rejects the journal or
                // drops exactly damaged tail data.
                let bytes = std::fs::read(&active).unwrap();
                if let Some((event, (start, len))) = active_events(&active)
                    .into_iter()
                    .rev()
                    .find(|(event, _)| event.ordinal > acknowledged.0)
                {
                    let mut faulted = bytes;
                    let content_end = start + len - 1;
                    let position = start + (content_end - start).saturating_sub(1) / 2;
                    faulted[position] ^= 0x40;
                    std::fs::write(&active, faulted).unwrap();
                    prop_assert!(event.ordinal > acknowledged.0);
                }
            }
            1 => {
                // Truncate only the unacknowledged tail record.
                let bytes = std::fs::read(&active).unwrap();
                if let Some((_event, (start, len))) = active_events(&active)
                    .into_iter()
                    .rev()
                    .find(|(event, _)| event.ordinal > acknowledged.0)
                {
                    std::fs::write(&active, &bytes[..start + (len / 2).max(1)]).unwrap();
                }
            }
            2 => {
                // Delete an unacknowledged sealed segment. A gap must fail
                // closed; if another copy covers it, recovery may proceed.
                if let Some(path) = journal_files(&temp).into_iter().find(|path| {
                    path.extension().is_some_and(|extension| extension == "gz")
                        && sealed_relay_journal_metadata(path)
                            .is_ok_and(|span| span.file_first_ordinal > acknowledged.0)
                }) {
                    std::fs::remove_file(path).unwrap();
                }
            }
            3 => {
                // A duplicated committed line models a stale active copy left
                // beside a sealed generation.
                let bytes = std::fs::read(&active).unwrap();
                if let Some((_, (start, len))) = active_events(&active).last() {
                    let mut duplicated = bytes.clone();
                    duplicated.extend_from_slice(&bytes[*start..*start + *len]);
                    std::fs::write(&active, duplicated).unwrap();
                }
            }
            4 => {
                // A torn append was never committed and is not a gap.
                let mut bytes = std::fs::read(&active).unwrap();
                bytes.extend_from_slice(br#"{"ordinal":999,"partial"#);
                std::fs::write(&active, bytes).unwrap();
            }
            _ => {
                // A terminated corrupt record is recoverable as one gap and
                // cannot fabricate a valid event.
                let mut bytes = std::fs::read(&active).unwrap();
                bytes.extend_from_slice(br#"{"ordinal":999,"observation":BROKEN}"#);
                bytes.push(b'\n');
                std::fs::write(&active, bytes).unwrap();
            }
        }

        let reopened = DurableRelay::open(temp.path(), SESSION, "1.0.0");
        if let Ok(mut reopened) = reopened {
            prop_assert_eq!(reopened.acknowledged_through(), acknowledged.0);
            prop_assert_eq!(reopened.acknowledged_digest(), acknowledged.1.as_str());
            let retained = reopened
                .events_after(acknowledged.0, &acknowledged.1)
                .map_err(|error| TestCaseError::fail(format!("serve recovered suffix: {error:#}")))?;
            let recovered = retained
                .iter()
                .map(|event| (event.ordinal, event.digest.clone()))
                .collect::<Vec<_>>();
            prop_assert!(
                recovered.windows(2).all(|pair| pair[0].0 < pair[1].0),
                "recovered records must stay strictly ordered: {recovered:?}"
            );
            for record in &recovered {
                prop_assert!(truth.contains(record), "recovery fabricated {record:?}");
            }
            let next = reopened
                .record_observation(RelayObservation::Warning { message: "after-recovery".into() })
                .map_err(|error| TestCaseError::fail(format!("append after recovery: {error:#}")))?;
            prop_assert!(next > acknowledged.0);
        }
    }
}
