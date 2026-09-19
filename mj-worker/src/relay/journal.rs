//! Durable storage for the relay: the on-disk journal of sealed and active
//! segments, snapshot persistence, restart recovery, and the parts of
//! `DurableRelay` that make an event or a snapshot durable before it is
//! acknowledged to a caller.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

use mj_core::clock::epoch_millis;

use super::snapshot::{
    RELAY_EVENT_FORMAT_V2, RelayCommand, RelayCommandOutcome, RelayDispatchState, RelayEvent,
    RelayObservation, RelaySnapshot, apply_relay_event, clamp_observation, ensure_byte_budget,
    ensure_serialized_budget, observation_changes_state, relay_event_digest, validate_relay_event,
    validate_relay_event_self,
};
use super::{
    DurableRelay, RELAY_ACTIVE_SEGMENT, RELAY_EVENT_BYTE_BUDGET, RELAY_EVENT_ENVELOPE_RESERVE,
    RELAY_HOT_EVENT_CAPACITY, RELAY_JOURNAL_DIR, RELAY_SEGMENT_BYTE_LIMIT,
    RELAY_SNAPSHOT_BYTE_BUDGET, RELAY_SNAPSHOT_LAG_BYTE_LIMIT, RELAY_STATE_BYTE_BUDGET,
    RELAY_STATE_FILE, RestoredRelaySeed, restored_relay_seed_path,
};

#[derive(Debug, Clone)]
pub(crate) struct RelayJournalSpan {
    pub(crate) path: PathBuf,
    /// Physical first ordinal in `path`; it may precede `after_ordinal` when a
    /// crash left an overlapping active/sealed copy.
    pub(crate) file_first_ordinal: u64,
    /// Present when this process has read the segment. Sealed segment names
    /// carry their ordinal range, so startup does not need to decompress old
    /// transcript history merely to discover its layout.
    pub(crate) file_first_previous_digest: Option<String>,
    pub(crate) file_last_ordinal: u64,
    pub(crate) file_last_digest: Option<String>,
    /// This canonical span contributes only ordinals greater than this value.
    pub(crate) after_ordinal: u64,
}

/// The digest before a span, as cached by its first record. A v1 record
/// carries it in `previous_digest`; a v2 record carries none, so the boundary
/// digest must be read from the event itself when it is needed.
pub(crate) fn span_previous_digest(first: &RelayEvent) -> Option<String> {
    (!first.previous_digest.is_empty()).then(|| first.previous_digest.clone())
}

/// Persist the snapshot without ever recreating the worker root. Session
/// teardown deletes that directory while this daemon may still be alive, and a
/// recreated root holding only a snapshot cannot be reopened: its frontier
/// would run ahead of a journal that no longer exists.
pub(crate) fn persist_relay_snapshot(root: &Path, snapshot: &RelaySnapshot) -> Result<()> {
    let body = serde_json::to_vec_pretty(snapshot)?;
    ensure_byte_budget(body.len(), RELAY_SNAPSHOT_BYTE_BUDGET, "relay snapshot")?;
    mj_core::config::atomic_write_existing(&root.join(RELAY_STATE_FILE), &body)
}

pub(crate) fn open_relay_journal(
    journal: &Path,
    retained_through: u64,
    retained_digest: &str,
    snapshot_ordinal: u64,
    snapshot: &mut RelaySnapshot,
) -> Result<(Vec<RelayJournalSpan>, VecDeque<RelayEvent>)> {
    let mut paths = Vec::new();
    if journal.exists() {
        for entry in fs::read_dir(journal)? {
            let path = entry?.path();
            if path
                .file_name()
                .is_some_and(|name| name == RELAY_ACTIVE_SEGMENT)
                || path.extension().is_some_and(|extension| extension == "gz")
            {
                paths.push(path);
            }
        }
    }
    paths.sort();
    let active = journal.join(RELAY_ACTIVE_SEGMENT);
    let mut files = Vec::new();
    for path in paths {
        let metadata = if path == active {
            inspect_relay_journal_file(&path, true)?
        } else {
            Some(sealed_relay_journal_metadata(&path)?)
        };
        if let Some(metadata) = metadata {
            files.push(metadata);
        }
    }

    let original_frontiers = [
        (
            "snapshot",
            snapshot.latest_ordinal,
            snapshot.latest_digest.clone(),
        ),
        (
            "acknowledgement",
            snapshot.acknowledged_through,
            snapshot.acknowledged_digest.clone(),
        ),
        (
            "recovery floor",
            snapshot.recovery_floor_ordinal,
            snapshot.recovery_floor_digest.clone(),
        ),
    ];
    for (name, ordinal, digest) in &original_frontiers {
        if *ordinal == retained_through && digest != retained_digest {
            bail!("relay {name} digest conflicts with retained frontier");
        }
    }

    let journal_latest = files
        .iter()
        .map(|file| file.file_last_ordinal)
        .max()
        .unwrap_or(retained_through);
    let mut previous_ordinal = retained_through;
    let mut spans = Vec::new();

    while previous_ordinal < journal_latest {
        let next_ordinal = previous_ordinal
            .checked_add(1)
            .ok_or_else(|| anyhow!("relay event ordinal exhausted"))?;
        let Some(candidate) = files
            .iter()
            .filter(|file| {
                file.file_first_ordinal <= next_ordinal && file.file_last_ordinal > previous_ordinal
            })
            .max_by_key(|file| (file.file_last_ordinal, usize::from(file.path == active)))
            .cloned()
        else {
            bail!("relay journal has a gap after event {previous_ordinal}");
        };
        let contribution_after = previous_ordinal;
        previous_ordinal = candidate.file_last_ordinal;
        spans.push(RelayJournalSpan {
            after_ordinal: contribution_after,
            ..candidate
        });
    }

    if journal_latest < snapshot_ordinal {
        bail!(
            "relay snapshot frontier {} is ahead of retained journal {journal_latest}",
            snapshot_ordinal
        );
    }
    for (name, ordinal, _) in &original_frontiers {
        if *ordinal > retained_through && *ordinal > journal_latest {
            bail!("relay {name} event {ordinal} is not retained");
        }
    }

    // The durable snapshot already contains the current operational state.
    // Only a crash tail newer than that snapshot must be decompressed and
    // applied during startup; retained history is validated when a controller
    // actually requests it. This keeps worker readiness proportional to the
    // bounded snapshot lag instead of the lifetime transcript size.
    let mut hot_events = VecDeque::new();
    let mut recovered_ordinal = snapshot_ordinal;
    let mut recovered_digest = snapshot.latest_digest.clone();
    for span in &spans {
        if span.file_last_ordinal <= recovered_ordinal {
            continue;
        }
        let boundary = recovered_ordinal;
        let mut boundary_verified = span.file_first_ordinal == boundary.saturating_add(1);
        visit_relay_journal_file(&span.path, JournalReadMode::Strict, |event, _| {
            if event.ordinal < boundary {
                return Ok(ControlFlow::Continue(()));
            }
            if event.ordinal == boundary {
                if event.digest != recovered_digest {
                    bail!(
                        "overlapping relay journal {} conflicts at event {}",
                        span.path.display(),
                        event.ordinal
                    );
                }
                boundary_verified = true;
                return Ok(ControlFlow::Continue(()));
            }
            if !boundary_verified {
                bail!(
                    "overlapping relay journal {} does not contain boundary event {}",
                    span.path.display(),
                    boundary
                );
            }
            validate_relay_event(recovered_ordinal, &recovered_digest, &event)
                .context("validate relay journal recovery tail")?;
            // Replay lacks the process-local activity levels that accompanied
            // a state change. Preserve known idle history across warnings, but
            // do not manufacture a transition time after an interrupted save.
            if observation_changes_state(&event.observation)
                || matches!(event.observation, RelayObservation::SessionUpdate { .. })
            {
                snapshot.idle_since_ms = None;
                snapshot.activity_was_idle = None;
            }
            apply_relay_event(snapshot, &event)?;
            recovered_ordinal = event.ordinal;
            recovered_digest = event.digest.clone();
            if hot_events.len() == RELAY_HOT_EVENT_CAPACITY {
                hot_events.pop_front();
            }
            hot_events.push_back(event);
            Ok(ControlFlow::Continue(()))
        })?;
        if recovered_ordinal != span.file_last_ordinal {
            bail!(
                "relay journal {} ended at event {recovered_ordinal}, expected {}",
                span.path.display(),
                span.file_last_ordinal
            );
        }
    }
    if hot_events.len() < RELAY_HOT_EVENT_CAPACITY {
        let mut recent = VecDeque::new();
        for span in spans.iter().rev() {
            let mut segment_events = Vec::new();
            let mut previous: Option<RelayEvent> = None;
            visit_relay_journal_file(&span.path, JournalReadMode::Strict, |event, _| {
                // Validate each record by its own digest; between consecutive
                // records the v1 chain link is also enforced. The first record
                // read cold has no trusted predecessor, so it is validated on
                // its own digest alone (a v2 record carries no back-reference).
                if let Some(previous) = &previous {
                    validate_relay_event(previous.ordinal, &previous.digest, &event).with_context(
                        || format!("validate relay journal {}", span.path.display()),
                    )?;
                } else {
                    validate_relay_event_self(&event).with_context(|| {
                        format!("validate relay journal {}", span.path.display())
                    })?;
                }
                if event.ordinal > span.after_ordinal && event.ordinal <= snapshot.latest_ordinal {
                    segment_events.push(event.clone());
                }
                previous = Some(event);
                Ok(ControlFlow::Continue(()))
            })?;
            for event in segment_events.into_iter().rev() {
                recent.push_front(event);
                if recent.len() == RELAY_HOT_EVENT_CAPACITY {
                    break;
                }
            }
            if recent.len() == RELAY_HOT_EVENT_CAPACITY {
                break;
            }
        }
        hot_events = recent;
    }
    if snapshot.latest_ordinal > retained_through {
        let Some(latest) = hot_events.back() else {
            bail!(
                "relay journal is missing snapshot frontier event {}",
                snapshot.latest_ordinal
            );
        };
        if latest.ordinal != snapshot.latest_ordinal || latest.digest != snapshot.latest_digest {
            bail!(
                "relay snapshot digest conflicts with journal event {}",
                snapshot.latest_ordinal
            );
        }
    }

    // An active copy left behind by a crash may be fully covered by a sealed
    // span. Keep it as a zero-width canonical span when it reaches the current
    // frontier so future appends remain contiguous. Preserve a stale shorter
    // copy under an ignored name before replacing it: sealed filenames are
    // enough to assemble history lazily, but are not proof that deleting the
    // overlapping active data would be safe.
    if let Some(active_file) = files.iter().find(|file| file.path == active)
        && !spans.iter().any(|span| span.path == active)
    {
        if active_file.file_last_ordinal == previous_ordinal {
            spans.push(RelayJournalSpan {
                after_ordinal: previous_ordinal,
                ..active_file.clone()
            });
        } else if active_file.file_last_ordinal < previous_ordinal {
            archive_stale_active_relay_journal(journal, &active, active_file)?;
        }
    }
    Ok((spans, hot_events))
}

fn seal_active_relay_segment(journal: &Path, metadata: &mut RelayJournalSpan) -> Result<()> {
    let active = journal.join(RELAY_ACTIVE_SEGMENT);
    let sealed_name = format!(
        "segment-{:020}-{:020}.jsonl.gz",
        metadata.file_first_ordinal, metadata.file_last_ordinal
    );
    let temporary = journal.join(format!("{sealed_name}.new"));
    let destination = journal.join(sealed_name);
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    let mut encoder = GzEncoder::new(file, Compression::default());
    visit_relay_journal_file(&active, JournalReadMode::Strict, |event, _| {
        serde_json::to_writer(&mut encoder, &event)?;
        encoder.write_all(b"\n")?;
        Ok(ControlFlow::Continue(()))
    })?;
    let file = encoder.finish()?;
    file.sync_all()?;
    fs::rename(&temporary, &destination)?;
    // The sealed copy must be durable before the active segment is replaced.
    sync_directory(journal)?;

    let replacement = journal.join("active.jsonl.new");
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&replacement)?;
    file.sync_all()?;
    fs::rename(&replacement, active)?;
    // From this point the live active path is empty. Publish the sealed path
    // in memory before the final directory sync so even a reported sync error
    // cannot make a subsequent retry append against stale file metadata.
    metadata.path = destination;
    sync_directory(journal)
}

fn inspect_relay_journal_file(
    path: &Path,
    repair_partial_tail: bool,
) -> Result<Option<RelayJournalSpan>> {
    let mut first: Option<RelayEvent> = None;
    let mut previous: Option<RelayEvent> = None;
    let mode = if repair_partial_tail {
        JournalReadMode::RepairTail
    } else {
        JournalReadMode::Strict
    };
    visit_relay_journal_file(path, mode, |event, encoded_len| {
        ensure_byte_budget(encoded_len, RELAY_EVENT_BYTE_BUDGET, "relay event")?;
        if let Some(previous) = &previous {
            validate_relay_event(previous.ordinal, &previous.digest, &event)
                .with_context(|| format!("validate relay journal {}", path.display()))?;
        } else {
            validate_relay_event_self(&event)
                .with_context(|| format!("validate relay journal {}", path.display()))?;
            first = Some(event.clone());
        }
        previous = Some(event);
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(first.zip(previous).map(|(first, last)| RelayJournalSpan {
        path: path.to_owned(),
        file_first_ordinal: first.ordinal,
        file_first_previous_digest: span_previous_digest(&first),
        file_last_ordinal: last.ordinal,
        file_last_digest: Some(last.digest),
        after_ordinal: 0,
    }))
}

fn sealed_relay_journal_metadata(path: &Path) -> Result<RelayJournalSpan> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        bail!("relay segment has a non-UTF-8 name: {}", path.display());
    };
    let Some(range) = name
        .strip_prefix("segment-")
        .and_then(|name| name.strip_suffix(".jsonl.gz"))
    else {
        bail!("invalid sealed relay segment name {}", path.display());
    };
    let Some((first, last)) = range.split_once('-') else {
        bail!("invalid sealed relay segment range {}", path.display());
    };
    let first = first
        .parse::<u64>()
        .with_context(|| format!("parse first ordinal from {}", path.display()))?;
    let last = last
        .parse::<u64>()
        .with_context(|| format!("parse last ordinal from {}", path.display()))?;
    if first == 0 || first > last {
        bail!("invalid sealed relay segment range {first}-{last}");
    }
    Ok(RelayJournalSpan {
        path: path.to_owned(),
        file_first_ordinal: first,
        file_first_previous_digest: None,
        file_last_ordinal: last,
        file_last_digest: None,
        after_ordinal: 0,
    })
}

#[cfg(test)]
thread_local! {
    /// Per-thread override of the seal threshold so tests build multi-segment
    /// journals from a handful of small records instead of megabytes.
    static SEAL_BYTE_LIMIT_OVERRIDE: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
}

/// Set (or clear) the active-segment seal threshold for the current test thread.
#[cfg(test)]
pub(crate) fn set_seal_byte_limit_override(limit: Option<u64>) {
    SEAL_BYTE_LIMIT_OVERRIDE.with(|cell| cell.set(limit));
}

fn relay_segment_byte_limit() -> u64 {
    #[cfg(test)]
    if let Some(limit) = SEAL_BYTE_LIMIT_OVERRIDE.with(std::cell::Cell::get) {
        return limit;
    }
    RELAY_SEGMENT_BYTE_LIMIT
}

pub(crate) fn visit_relay_journal_file(
    path: &Path,
    mode: JournalReadMode,
    mut visitor: impl FnMut(RelayEvent, usize) -> Result<ControlFlow<()>>,
) -> Result<Vec<RelayJournalGap>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let compressed = path.extension().is_some_and(|extension| extension == "gz");
    if compressed {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let decoder = GzDecoder::new(file);
        let mut reader = std::io::BufReader::new(decoder);
        let scan = visit_relay_journal_reader(
            path,
            &mut reader,
            mode.without_tail_repair(),
            &mut visitor,
        )?;
        return Ok(scan.gaps);
    }

    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let scan = visit_relay_journal_reader(path, &mut reader, mode, &mut visitor)?;
    drop(reader);
    if let Some(valid_len) = scan.truncate_to {
        let file = OpenOptions::new()
            .write(true)
            .open(path)
            .with_context(|| format!("open torn relay journal {}", path.display()))?;
        file.set_len(valid_len)?;
        file.sync_data()?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
    }
    Ok(scan.gaps)
}

/// How a journal file is read when a record does not parse.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum JournalReadMode {
    /// Read-only and strict: an unparseable terminated record aborts the scan.
    /// A torn tail (no trailing newline) stops cleanly without touching the file.
    Strict,
    /// Startup repair of the live active segment: like `Strict`, but a torn tail
    /// is truncated to the last complete record.
    RepairTail,
    /// Recovery: an unparseable terminated record is skipped and recorded as a
    /// byte gap; the scan recovers every intact record either side of it. A torn
    /// tail stops cleanly (never truncates — recovery is read-only).
    Recover,
}

impl JournalReadMode {
    /// Tail repair is meaningless for a compressed sealed segment (its bytes are
    /// gzip-framed and immutable), so drop it there while keeping recovery.
    fn without_tail_repair(self) -> Self {
        match self {
            JournalReadMode::RepairTail => JournalReadMode::Strict,
            other => other,
        }
    }
}

/// A run of unrecoverable bytes skipped during `Recover` — one or more corrupt
/// records. The surrounding good records are recovered; the caller correlates
/// the byte range with ordinals from the events it did receive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RelayJournalGap {
    pub(crate) byte_offset: u64,
    pub(crate) byte_len: usize,
}

struct RelayJournalScan {
    truncate_to: Option<u64>,
    gaps: Vec<RelayJournalGap>,
}

fn visit_relay_journal_reader(
    path: &Path,
    reader: &mut impl BufRead,
    mode: JournalReadMode,
    visitor: &mut impl FnMut(RelayEvent, usize) -> Result<ControlFlow<()>>,
) -> Result<RelayJournalScan> {
    let mut line = Vec::new();
    let mut complete_bytes = 0_u64;
    let mut gaps = Vec::new();
    loop {
        let (consumed, terminated) = read_bounded_line(reader, &mut line, RELAY_EVENT_BYTE_BUDGET)
            .with_context(|| format!("read relay journal {}", path.display()))?;
        if consumed == 0 {
            return Ok(RelayJournalScan {
                truncate_to: None,
                gaps,
            });
        }
        if !terminated {
            // A line with no trailing newline is a partial or in-flight append,
            // never a committed event: `append_relay_event` writes each record
            // and its newline in a single call. Stop at the last complete
            // record instead of parsing the torn bytes (which fails with an
            // "EOF while parsing" error). Repair mode truncates the torn tail;
            // every other mode leaves the file untouched for the writer to
            // finish.
            return Ok(RelayJournalScan {
                truncate_to: (mode == JournalReadMode::RepairTail).then_some(complete_bytes),
                gaps,
            });
        }
        let record_offset = complete_bytes;
        complete_bytes = complete_bytes
            .checked_add(u64::try_from(consumed).context("relay journal length overflow")?)
            .ok_or_else(|| anyhow!("relay journal length overflow"))?;
        if line.is_empty() {
            continue;
        }
        let event = match serde_json::from_slice(&line) {
            Ok(event) => event,
            Err(error) => {
                // A terminated record that will not parse is unrecoverable. In
                // recovery, skip it so its corruption cannot poison the intact
                // records around it; otherwise fail as before.
                if mode == JournalReadMode::Recover {
                    tracing::warn!(
                        journal = %path.display(),
                        byte_offset = record_offset,
                        bytes = line.len(),
                        %error,
                        "skipping unparseable relay journal record during recovery",
                    );
                    gaps.push(RelayJournalGap {
                        byte_offset: record_offset,
                        byte_len: line.len(),
                    });
                    continue;
                }
                return Err(anyhow::Error::new(error))
                    .with_context(|| format!("parse relay journal {}", path.display()));
            }
        };
        // A record that parses but whose bytes were altered fails to recompute
        // its own digest. In recovery that is corruption too — skip it so a
        // flipped byte cannot be served as a valid but wrong record, and so it
        // cannot poison its neighbours. Strict/RepairTail leave this to the
        // caller's own validation, as before.
        if mode == JournalReadMode::Recover
            && let Err(error) = validate_relay_event_self(&event)
        {
            tracing::warn!(
                journal = %path.display(),
                byte_offset = record_offset,
                bytes = line.len(),
                %error,
                "skipping relay journal record that failed self-validation during recovery",
            );
            gaps.push(RelayJournalGap {
                byte_offset: record_offset,
                byte_len: line.len(),
            });
            continue;
        }
        if visitor(event, line.len())?.is_break() {
            return Ok(RelayJournalScan {
                truncate_to: None,
                gaps,
            });
        }
    }
}

use mj_core::relay::protocol::read_bounded_line;

fn archive_stale_active_relay_journal(
    journal: &Path,
    active: &Path,
    metadata: &RelayJournalSpan,
) -> Result<()> {
    let archived = journal.join(format!(
        "stale-active-{:020}-{:020}.jsonl",
        metadata.file_first_ordinal, metadata.file_last_ordinal
    ));
    if archived.exists() {
        bail!(
            "cannot preserve stale relay journal {} because {} already exists",
            active.display(),
            archived.display()
        );
    }
    fs::rename(active, &archived).with_context(|| {
        format!(
            "preserve stale relay journal {} as {}",
            active.display(),
            archived.display()
        )
    })?;
    sync_directory(journal)?;

    let replacement = journal.join("active.jsonl.new");
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&replacement)?;
    file.sync_all()?;
    fs::rename(&replacement, active)?;
    sync_directory(journal)
}

pub(crate) fn read_restored_relay_seed(root: &Path) -> Result<Option<RestoredRelaySeed>> {
    let path = restored_relay_seed_path(root);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let seed: RestoredRelaySeed =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    seed.validate()
        .with_context(|| format!("validate {}", path.display()))?;
    Ok(Some(seed))
}

use mj_core::config::sync_directory;

impl DurableRelay {
    #[cfg(test)]
    fn retained_event_body_count(&self) -> usize {
        self.hot_events.len()
    }

    pub(crate) fn append_relay_event(
        &mut self,
        command_id: Option<&str>,
        observation: RelayObservation,
    ) -> Result<u64> {
        let ordinal = self
            .snapshot
            .latest_ordinal
            .checked_add(1)
            .ok_or_else(|| anyhow!("relay event ordinal exhausted"))?;
        // Clamp before digesting so the recorded digest covers what was
        // actually written, and so recording an observation cannot fail on
        // size alone.
        let observation = clamp_observation(
            observation,
            RELAY_EVENT_BYTE_BUDGET - RELAY_EVENT_ENVELOPE_RESERVE,
        )?;
        // v2: self-describing records carry no chain link, so a corrupt record
        // can never invalidate the events after it. The frontier is still
        // tracked by `latest_digest` for cursor validation; it just is not
        // folded into the next record's digest.
        let event = RelayEvent {
            format: RELAY_EVENT_FORMAT_V2,
            ordinal,
            previous_digest: String::new(),
            digest: String::new(),
            recorded_at_ms: epoch_millis(),
            command_id: command_id.map(str::to_owned),
            observation,
        };
        let event = RelayEvent {
            digest: relay_event_digest(&event)?,
            ..event
        };
        // The same bytes are measured against the event budget and written to
        // the journal, so the encoding is done once.
        let mut encoded = serde_json::to_vec(&event).context("serialize relay event")?;
        ensure_byte_budget(encoded.len(), RELAY_EVENT_BYTE_BUDGET, "relay event")?;
        encoded.push(b'\n');
        // A transcript observation leaves everything but the frontier alone, so
        // only a state-moving event pays for the staged snapshot copy and the
        // two budget serializations that validate it.
        let stage_snapshot = self.stages_snapshot(&event.observation);
        let staged = if stage_snapshot {
            let mut next_snapshot = self.snapshot.clone();
            apply_relay_event(&mut next_snapshot, &event)?;
            ensure_serialized_budget(&next_snapshot, RELAY_SNAPSHOT_BYTE_BUDGET, "relay snapshot")?;
            ensure_serialized_budget(
                &next_snapshot.operational_state(),
                RELAY_STATE_BYTE_BUDGET,
                "relay operational state",
            )?;
            Some(next_snapshot)
        } else {
            None
        };
        self.seal_active_segment_if_needed()?;
        let journal = self.root.join(RELAY_JOURNAL_DIR);
        let path = journal.join(RELAY_ACTIVE_SEGMENT);
        let created_active_segment = !path.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        file.write_all(&encoded)?;
        file.sync_data()?;
        if created_active_segment {
            sync_directory(&journal)?;
        }
        mj_core::test_hooks::reach_test_hook("journal_append_before_snapshot_publication")?;

        match staged {
            Some(next_snapshot) => self.snapshot = next_snapshot,
            // Applying is still what moves the frontier, so a misclassified
            // observation cannot silently lose its state change. It cannot
            // fail here either: this event was digested from this exact
            // frontier, which `relay_event_digest` already validated.
            None => apply_relay_event(&mut self.snapshot, &event)?,
        }
        if let RelayObservation::CommandStarted { command_id, .. } = &event.observation
            && let Some(dispatch) = self.snapshot.dispatches.get(command_id)
            && let RelayCommand::Prompt { prompt } = &dispatch.command
        {
            let prompt_text = prompt
                .iter()
                .filter_map(|block| match block {
                    agent_client_protocol::schema::v1::ContentBlock::Text(text) => {
                        Some(text.text.as_str())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            self.turn_context.reset(&prompt_text);
            self.replied_verdict_pending = false;
        }
        if matches!(
            event.observation,
            RelayObservation::HarnessTurnStarted { .. }
        ) || matches!(
            self.snapshot.execution,
            mj_core::relay::RelayExecutionState::Closing
                | mj_core::relay::RelayExecutionState::Closed
        ) {
            self.turn_context.invalidate();
            self.replied_verdict_pending = false;
        }
        let idle_changed = self.refresh_idle_clock(event.recorded_at_ms);
        self.record_journal_append(&path, &event);
        self.push_hot_event(event);
        self.unpersisted_journal_bytes =
            self.unpersisted_journal_bytes.saturating_add(encoded.len());
        // Recovery replays journal events past the snapshot frontier, so a
        // streamed observation does not need its own snapshot write. Persisting
        // on every state move and once per bounded run of transcript bytes
        // keeps that replay short without paying two fsyncs per chunk.
        if stage_snapshot
            || idle_changed
            || self.unpersisted_journal_bytes >= RELAY_SNAPSHOT_LAG_BYTE_LIMIT
        {
            self.persist_snapshot()?;
        }
        Ok(ordinal)
    }

    /// Whether appending this observation has to stage and persist a snapshot.
    fn stages_snapshot(&self, observation: &RelayObservation) -> bool {
        #[cfg(test)]
        if self.stage_snapshot_every_append {
            return true;
        }
        observation_changes_state(observation)
    }

    pub(crate) fn persist_snapshot(&mut self) -> Result<()> {
        persist_relay_snapshot(&self.root, &self.snapshot)?;
        self.unpersisted_journal_bytes = 0;
        Ok(())
    }

    /// Adopt a staged snapshot only once it is durable. Every write of
    /// `relay-state.json` goes through here or [`Self::persist_snapshot`], so
    /// `unpersisted_journal_bytes == 0` means the file matches memory.
    pub(crate) fn commit_snapshot(&mut self, next_snapshot: RelaySnapshot) -> Result<()> {
        persist_relay_snapshot(&self.root, &next_snapshot)?;
        self.snapshot = next_snapshot;
        self.unpersisted_journal_bytes = 0;
        Ok(())
    }

    fn seal_active_segment_if_needed(&mut self) -> Result<()> {
        let journal = self.root.join(RELAY_JOURNAL_DIR);
        let active = journal.join(RELAY_ACTIVE_SEGMENT);
        if !active.exists() || active.metadata()?.len() < relay_segment_byte_limit() {
            return Ok(());
        }
        let Some(index) = self
            .journal_spans
            .iter()
            .position(|span| span.path == active)
        else {
            bail!("active relay segment has data but no journal metadata");
        };
        // Sealing moves the active segment's events into a new file, so any
        // replay plan already captured stops describing the journal here.
        self.invalidate_replay_plans();
        seal_active_relay_segment(&journal, &mut self.journal_spans[index])
    }

    fn record_journal_append(&mut self, active: &Path, event: &RelayEvent) {
        if let Some(span) = self
            .journal_spans
            .last_mut()
            .filter(|span| span.path == active)
        {
            debug_assert_eq!(span.file_last_ordinal + 1, event.ordinal);
            span.file_last_ordinal = event.ordinal;
            span.file_last_digest = Some(event.digest.clone());
            return;
        }
        self.journal_spans.push(RelayJournalSpan {
            path: active.to_owned(),
            file_first_ordinal: event.ordinal,
            file_first_previous_digest: span_previous_digest(event),
            file_last_ordinal: event.ordinal,
            file_last_digest: Some(event.digest.clone()),
            after_ordinal: event.ordinal - 1,
        });
    }

    fn push_hot_event(&mut self, event: RelayEvent) {
        if self.hot_events.len() == RELAY_HOT_EVENT_CAPACITY {
            self.hot_events.pop_front();
        }
        self.hot_events.push_back(event);
    }

    /// Announce that the journal's files no longer match any replay plan a
    /// reader captured earlier. Callers reading a page off the relay lock
    /// compare the generation to tell a stale plan from a real desync.
    fn invalidate_replay_plans(&mut self) {
        self.journal_generation = self.journal_generation.wrapping_add(1);
    }

    fn rewrite_relay_journal(&mut self, retain_after: u64) -> Result<()> {
        // Rewriting replaces the active segment and deletes every sealed one.
        self.invalidate_replay_plans();
        let journal = self.root.join(RELAY_JOURNAL_DIR);
        fs::create_dir_all(&journal)?;
        let replacement = journal.join("active.jsonl.new");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&replacement)?;
        let mut first: Option<RelayEvent> = None;
        let mut last: Option<RelayEvent> = None;
        let mut written_through = retain_after;
        for span in &self.journal_spans {
            visit_relay_journal_file(&span.path, JournalReadMode::Strict, |event, _| {
                if event.ordinal <= span.after_ordinal || event.ordinal <= written_through {
                    return Ok(ControlFlow::Continue(()));
                }
                if event.ordinal <= retain_after {
                    written_through = event.ordinal;
                    return Ok(ControlFlow::Continue(()));
                }
                let expected = written_through
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("relay event ordinal exhausted"))?;
                if event.ordinal != expected {
                    bail!(
                        "relay journal rewrite has a gap: expected {expected}, found {}",
                        event.ordinal
                    );
                }
                serde_json::to_writer(&mut file, &event)?;
                file.write_all(b"\n")?;
                if first.is_none() {
                    first = Some(event.clone());
                }
                written_through = event.ordinal;
                last = Some(event);
                Ok(ControlFlow::Continue(()))
            })?;
        }
        file.sync_all()?;
        let active = journal.join(RELAY_ACTIVE_SEGMENT);
        let next_spans = match (first, last) {
            (Some(first), Some(last)) => vec![RelayJournalSpan {
                path: active.clone(),
                file_first_ordinal: first.ordinal,
                file_first_previous_digest: span_previous_digest(&first),
                file_last_ordinal: last.ordinal,
                file_last_digest: Some(last.digest),
                after_ordinal: retain_after,
            }],
            (None, None) => Vec::new(),
            _ => unreachable!("relay journal rewrite recorded only one boundary"),
        };
        fs::rename(&replacement, &active)?;
        // Publish the new canonical path immediately after the atomic rename.
        // If directory sync or redundant-copy cleanup fails, the live relay
        // must not retain paths that no longer contain its canonical events.
        self.journal_spans = next_spans;
        // Make the replacement durable before removing any segment that may
        // contain the same unacknowledged observations.
        sync_directory(&journal)?;
        for entry in fs::read_dir(&journal)? {
            let path = entry?.path();
            if path.extension().is_some_and(|extension| extension == "gz") {
                fs::remove_file(path)?;
            }
        }
        sync_directory(&journal)?;
        Ok(())
    }

    pub(crate) fn garbage_collect_relay_history(&mut self) -> Result<()> {
        let through = self.snapshot.retained_through();
        let journal_floor = self
            .journal_spans
            .first()
            .map_or(self.snapshot.latest_ordinal, |span| span.after_ordinal);
        let rewrites_journal = through > journal_floor;

        // The retained frontier this collection acts on has to be durable
        // before the journal loses the events below it: recovery reads that
        // frontier from the snapshot and replays forward from there.
        if rewrites_journal && self.unpersisted_journal_bytes > 0 {
            self.persist_snapshot()?;
        }
        // With the ACK and recovery frontiers durable, either the old or the
        // rewritten journal is valid after a crash.
        if rewrites_journal {
            self.rewrite_relay_journal(through)?;
        }
        // The pruned ledger reaches memory only after its own durable write
        // succeeds, so a transient failure cannot forget command IDs while the
        // daemon keeps serving retries. A catch-up ACK usually prunes nothing
        // and rewrites nothing, and its own persist already made this snapshot
        // durable; writing it again would cost two more fsyncs for a file that
        // would not change.
        let prunable = Self::prunable_command_ids(&self.snapshot, through);
        if !prunable.is_empty() || self.unpersisted_journal_bytes > 0 {
            let mut next_snapshot = self.snapshot.clone();
            for command_id in prunable {
                next_snapshot.handled_commands.remove(&command_id);
                next_snapshot.dispatches.remove(&command_id);
            }
            self.commit_snapshot(next_snapshot)?;
        }
        self.hot_events.retain(|event| event.ordinal > through);
        Ok(())
    }

    /// Ledger entries whose command is terminal at or below `through`. Their
    /// events are no longer retained, so the IDs no longer have to be
    /// remembered for idempotency.
    fn prunable_command_ids(snapshot: &RelaySnapshot, through: u64) -> Vec<String> {
        snapshot
            .handled_commands
            .iter()
            .filter(|(_, handled)| {
                handled
                    .terminal_ordinal
                    .is_some_and(|terminal| terminal <= through)
            })
            .map(|(command_id, _)| command_id.clone())
            .collect()
    }

    pub(crate) fn recover_nonterminal_commands(&mut self) -> Result<()> {
        let mut relay_local: Vec<(u64, String)> = self
            .snapshot
            .dispatches
            .iter()
            .filter(|(_, dispatch)| {
                dispatch.command.is_relay_local()
                    && !matches!(
                        dispatch.state,
                        RelayDispatchState::Completed
                            | RelayDispatchState::Rejected
                            | RelayDispatchState::Interrupted
                    )
            })
            .filter_map(|(command_id, _)| {
                self.snapshot
                    .handled_commands
                    .get(command_id)
                    .map(|handled| (handled.accepted_ordinal, command_id.clone()))
            })
            .collect();
        relay_local.sort();
        for (_, command_id) in relay_local {
            self.finish_relay_local_command(&command_id)?;
        }

        // Checkpoint barriers are controller-owned coordination commands. A
        // restarted relay has no owner that can complete them, regardless of
        // whether they were merely accepted, started, or already ready.
        let mut ownerless_barriers: Vec<(u64, String)> = self
            .snapshot
            .dispatches
            .iter()
            .filter(|(_, dispatch)| {
                matches!(dispatch.command, RelayCommand::BeginCheckpoint { .. })
                    && !matches!(
                        dispatch.state,
                        RelayDispatchState::Completed
                            | RelayDispatchState::Rejected
                            | RelayDispatchState::Interrupted
                    )
            })
            .filter_map(|(command_id, _)| {
                self.snapshot
                    .handled_commands
                    .get(command_id)
                    .map(|handled| (handled.accepted_ordinal, command_id.clone()))
            })
            .collect();
        ownerless_barriers.sort();
        for (_, command_id) in ownerless_barriers {
            self.record_command_interrupted(
                &command_id,
                "relay restarted without the controller that owned the checkpoint barrier",
            )?;
        }

        let mut in_flight: Vec<(u64, String)> = self
            .snapshot
            .dispatches
            .iter()
            .filter(|(_, dispatch)| dispatch.state == RelayDispatchState::InFlight)
            .filter_map(|(command_id, _)| {
                self.snapshot
                    .handled_commands
                    .get(command_id)
                    .map(|handled| (handled.accepted_ordinal, command_id.clone()))
            })
            .collect();
        in_flight.sort();
        let mut restored_close = false;
        for (_, command_id) in in_flight {
            if matches!(
                self.snapshot.dispatches[&command_id].command,
                RelayCommand::Close { .. }
            ) {
                // Closing an already-closed session is idempotent. Preserve
                // the durable close intent across a relay process restart.
                self.snapshot
                    .dispatches
                    .get_mut(&command_id)
                    .expect("in-flight close disappeared")
                    .state = RelayDispatchState::Pending;
                restored_close = true;
                continue;
            }
            if let RelayCommand::RunUserShell { command } =
                self.snapshot.dispatches[&command_id].command.clone()
            {
                self.record_command_completed(
                    &command_id,
                    RelayCommandOutcome::UserShell {
                        result: crate::relay::UserShellResult {
                            command,
                            stdout: String::new(),
                            stderr: String::new(),
                            stdout_truncated: false,
                            stderr_truncated: false,
                            exit_code: None,
                            signal: None,
                            duration_ms: 0,
                            status: crate::relay::UserShellStatus::Interrupted,
                            error: Some(
                                "worker restarted while the shell command was running; it was not replayed"
                                    .to_owned(),
                            ),
                        },
                    },
                )?;
                continue;
            }
            self.record_command_interrupted(
                &command_id,
                "relay restarted while the ACP command was in flight; it was not replayed",
            )?;
        }
        if restored_close {
            self.persist_snapshot()?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "journal_chaos.rs"]
mod chaos;

#[cfg(test)]
mod tests;
