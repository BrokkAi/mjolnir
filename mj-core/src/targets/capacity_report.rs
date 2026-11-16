//! The per-host capacity readings the daemon serves and `mj hosts` prints.
//!
//! One row stands for one deployment host, or for one EC2 fleet. A host row
//! reports how much CPU and RAM is in use; a fleet has no percentage of its
//! own and reports the cores and memory it is running. Free space comes from
//! the storage owner's verdicts, the same ones that refuse writes.

use serde::{Deserialize, Serialize};

use super::storage::{StorageCondition, TargetStorageView, capacity_storage_hosts, views_for};
use super::{DeploymentCapacityKind, DeploymentCapacityTarget, DeploymentCapacityUsage};
use crate::text::{counted, format_resource_bytes, refresh_age};

/// A reading older than this stopped tracking the host: the poller samples
/// every 30 seconds, so three missed rounds mean the number shown is no
/// longer what the host is doing.
pub const CAPACITY_SAMPLE_STALE_AFTER_SECONDS: u64 = 90;

/// Whether a reading sampled at `sampled_at` has stopped tracking its host.
/// Every surface that marks capacity stale asks this one rule.
pub fn capacity_sample_is_stale(sampled_at: u64, now_epoch_seconds: u64) -> bool {
    now_epoch_seconds.saturating_sub(sampled_at) > CAPACITY_SAMPLE_STALE_AFTER_SECONDS
}

/// Why a reading cannot be trusted, if it cannot: a probe that failed, or a
/// sample that stopped refreshing. `None` means the reading is current.
pub fn capacity_staleness(
    has_reading: bool,
    probe_error: Option<&str>,
    sampled_at_epoch_seconds: Option<u64>,
    now_epoch_seconds: u64,
) -> Option<String> {
    if let Some(error) = probe_error {
        // A probe that never produced a reading already says "unavailable";
        // only say more when stale numbers are still shown and could be
        // mistaken for current.
        return has_reading.then(|| format!("stale: {error}"));
    }
    let sampled_at = sampled_at_epoch_seconds?;
    capacity_sample_is_stale(sampled_at, now_epoch_seconds).then(|| {
        format!(
            "stale: sampled {}",
            refresh_age(now_epoch_seconds, sampled_at)
        )
    })
}

/// Everything the capacity service knows about one probe target. One value
/// owns the last good reading, when it was taken, the latest failure and
/// whether the fleet answered empty, so no reader can pair a fresh sample time
/// with an older reading. Writers change it only through [`Self::apply`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TargetCapacityState {
    /// The last good reading. A failed probe leaves it in place.
    pub usage: Option<DeploymentCapacityUsage>,
    /// When `usage` (or the empty-fleet answer) was taken.
    pub sampled_at_epoch_seconds: Option<u64>,
    /// The latest probe failed.
    pub probe_error: Option<String>,
    /// The fleet answered and has no instances running. Cleared by a failed
    /// probe: after a failure the fleet's size is unknown, not zero.
    pub on_demand: bool,
    /// The newest refresh request that a probe began after. A result whose
    /// probe started after request `n` answers every request up to `n`; one
    /// that started earlier, or a scheduled round, does not answer a later
    /// request. A surface that nudges the poller waits for this to reach the
    /// generation it was given.
    pub answered_refresh: u64,
}

impl TargetCapacityState {
    /// Whether any probe has answered, successfully or not. Until then the
    /// target is loading, not idle.
    pub fn has_answered(&self) -> bool {
        self.sampled_at_epoch_seconds.is_some() || self.probe_error.is_some()
    }

    /// Fold one probe result into the state.
    pub fn apply(
        &mut self,
        result: &Result<Option<DeploymentCapacityUsage>, String>,
        sampled_at_epoch_seconds: u64,
        refresh_generation: u64,
    ) {
        self.answered_refresh = self.answered_refresh.max(refresh_generation);
        match result {
            Ok(usage) => {
                self.on_demand = usage.is_none();
                self.usage.clone_from(usage);
                self.sampled_at_epoch_seconds = Some(sampled_at_epoch_seconds);
                self.probe_error = None;
            }
            Err(error) => {
                self.on_demand = false;
                self.probe_error = Some(error.clone());
            }
        }
    }
}

/// One filesystem Mjolnir writes to on a host, with the storage verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostFilesystemRow {
    /// The machine measured: the host, or one instance of a fleet.
    pub host: String,
    pub mount: String,
    pub available_bytes: u64,
    pub total_bytes: u64,
    pub condition: StorageCondition,
}

/// One row of the Targets capacity table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCapacityRow {
    pub id: String,
    pub host: String,
    /// The configured targets that deploy to this host or fleet.
    pub target_ids: Vec<String>,
    pub kind: DeploymentCapacityKind,
    /// Machines probed: one per live instance for a fleet.
    pub machines: usize,
    /// No reading has arrived, or only an empty fleet answered.
    pub has_reading: bool,
    pub cpu_percent: Option<u8>,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub logical_cores: u64,
    pub filesystems: Vec<HostFilesystemRow>,
    pub sampled_at_epoch_seconds: Option<u64>,
    pub probe_error: Option<String>,
    pub on_demand: bool,
}

impl HostCapacityRow {
    pub fn new(
        target: &DeploymentCapacityTarget,
        state: &TargetCapacityState,
        storage: &[TargetStorageView],
    ) -> Self {
        let usage = state.usage.as_ref();
        let hosts = capacity_storage_hosts(target, usage);
        let filesystems = views_for(storage, &hosts)
            .into_iter()
            .flat_map(|view| {
                view.filesystems.iter().map(|filesystem| HostFilesystemRow {
                    host: view.host.clone(),
                    mount: filesystem.space.mount.clone(),
                    available_bytes: filesystem.space.available_bytes,
                    total_bytes: filesystem.space.total_bytes,
                    condition: filesystem.condition,
                })
            })
            .collect();
        Self {
            id: target.id.clone(),
            host: target.host.clone(),
            target_ids: target.target_ids.clone(),
            kind: target.kind,
            machines: target.probes.len(),
            has_reading: usage.is_some(),
            cpu_percent: usage.and_then(|usage| usage.cpu_percent),
            memory_used_bytes: usage.map_or(0, |usage| usage.memory_used_bytes),
            memory_total_bytes: usage.map_or(0, |usage| usage.memory_total_bytes),
            logical_cores: usage.map_or(0, |usage| usage.logical_cores),
            filesystems,
            sampled_at_epoch_seconds: state.sampled_at_epoch_seconds,
            probe_error: state.probe_error.clone(),
            on_demand: state.on_demand,
        }
    }

    /// Whether `name` selects this row: its host, or a target it contains.
    pub fn matches(&self, name: &str) -> bool {
        self.host == name || self.target_ids.iter().any(|id| id == name)
    }

    pub fn staleness(&self, now_epoch_seconds: u64) -> Option<String> {
        capacity_staleness(
            self.has_reading,
            self.probe_error.as_deref(),
            self.sampled_at_epoch_seconds,
            now_epoch_seconds,
        )
    }

    /// Used memory as a whole percent, or `None` when the total is unknown.
    pub fn memory_percent(&self) -> Option<u8> {
        if self.memory_total_bytes == 0 {
            return None;
        }
        let percent = (u128::from(self.memory_used_bytes) * 100
            / u128::from(self.memory_total_bytes))
        .min(100);
        Some(u8::try_from(percent).unwrap_or(100))
    }

    pub fn fleet_machines_label(&self) -> String {
        counted(self.machines, "VM", "VMs")
    }

    /// The CPU and RAM cells.
    pub fn cpu_and_memory(&self) -> (String, String) {
        let unavailable = || "unavailable".to_owned();
        match (self.kind, self.has_reading) {
            (DeploymentCapacityKind::Host, true) => (
                self.cpu_percent
                    .map_or_else(unavailable, |percent| format!("{percent}%")),
                self.memory_percent().map_or_else(unavailable, |percent| {
                    format!(
                        "{percent}% ({}/{})",
                        format_resource_bytes(self.memory_used_bytes),
                        format_resource_bytes(self.memory_total_bytes)
                    )
                }),
            ),
            (DeploymentCapacityKind::AwsFleet, true) => (
                format!(
                    "{} cores on {}",
                    self.logical_cores,
                    self.fleet_machines_label()
                ),
                format_resource_bytes(self.memory_total_bytes),
            ),
            // A fleet with nothing running has no capacity figures, and the
            // count is the whole answer.
            (DeploymentCapacityKind::AwsFleet, false) if self.on_demand => {
                (self.fleet_machines_label(), String::new())
            }
            _ => (unavailable(), String::new()),
        }
    }

    /// Where a filesystem is: its mount, qualified by machine for a fleet.
    pub fn filesystem_place(&self, filesystem: &HostFilesystemRow) -> String {
        if self.kind == DeploymentCapacityKind::AwsFleet {
            format!("{} {}", filesystem.host, filesystem.mount)
        } else {
            filesystem.mount.clone()
        }
    }
}

/// The condition suffix after a filesystem's free space.
pub fn storage_condition_flag(condition: StorageCondition) -> &'static str {
    match condition {
        StorageCondition::Full => " full",
        StorageCondition::Low => " low",
        StorageCondition::Ok => "",
    }
}

/// `mj hosts` text: a table of hosts with CPU and RAM, then each host's
/// filesystems and why its reading is stale, if it is.
pub fn render_host_capacity(rows: &[HostCapacityRow], now_epoch_seconds: u64) -> String {
    if rows.is_empty() {
        return "no hosts".to_owned();
    }
    let cells: Vec<[String; 4]> = rows
        .iter()
        .map(|row| {
            let (cpu, memory) = row.cpu_and_memory();
            [row.host.clone(), row.target_ids.join(", "), cpu, memory]
        })
        .collect();
    let headers = ["HOST / FLEET", "TARGETS", "CPU", "RAM"];
    let widths: [usize; 4] = std::array::from_fn(|column| {
        cells
            .iter()
            .map(|cell| cell[column].chars().count())
            .chain(std::iter::once(headers[column].chars().count()))
            .max()
            .unwrap_or(0)
    });
    let line = |cell: &[String; 4]| {
        let mut text = String::new();
        for (column, value) in cell.iter().enumerate() {
            if column + 1 == cell.len() {
                text.push_str(value);
            } else {
                text.push_str(&format!("{value:<width$}  ", width = widths[column]));
            }
        }
        text.trim_end().to_owned()
    };
    let mut lines = vec![line(&headers.map(str::to_owned))];
    for (row, cell) in rows.iter().zip(&cells) {
        lines.push(line(cell));
        for filesystem in &row.filesystems {
            lines.push(format!(
                "  {} {} free of {}{}",
                row.filesystem_place(filesystem),
                format_resource_bytes(filesystem.available_bytes),
                format_resource_bytes(filesystem.total_bytes),
                storage_condition_flag(filesystem.condition)
            ));
        }
        if let Some(stale) = row.staleness(now_epoch_seconds) {
            lines.push(format!("  {stale}"));
        }
    }
    lines.join("\n")
}
