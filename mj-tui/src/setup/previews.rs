use super::*;

pub(super) fn machine_host_label(machine_id: &str, machine: &mj_core::config::Machine) -> String {
    match machine {
        mj_core::config::Machine::Local { .. } => "the local machine".to_owned(),
        mj_core::config::Machine::Ssh { ssh, .. } => ssh
            .user
            .as_ref()
            .map_or_else(|| ssh.host.clone(), |user| format!("{user}@{}", ssh.host)),
        mj_core::config::Machine::AwsEc2 { .. } => machine_id.to_owned(),
    }
}

pub(super) fn format_mbx_install_result(
    result: Result<String, String>,
    operation: Option<&MbxInstallState>,
) -> String {
    match result {
        Ok(message) => message,
        Err(error) => {
            let action = operation.map_or("install or upgrade", |operation| {
                if operation.upgrading {
                    "upgrade"
                } else {
                    "install"
                }
            });
            format!("Could not {action} mbx: {error}")
        }
    }
}

pub(super) fn format_compiler_time(nanoseconds: u64) -> String {
    let seconds = nanoseconds / 1_000_000_000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60),
        _ => format!("{}d {}h", seconds / 86_400, (seconds % 86_400) / 3600),
    }
}

pub(super) fn build_cache_gigabytes_label(size: &str) -> String {
    mj_core::config::build_cache_size_gigabytes(size)
        .map_or_else(|| size.to_owned(), |gigabytes| gigabytes.to_string())
}

/// Whether a full path names the given field of a machine's build cache.
pub(super) fn is_build_cache_budget(path: &[String]) -> bool {
    is_build_cache_field(path, "max_total_size")
}

pub(super) fn is_build_cache_field(path: &[String], field: &str) -> bool {
    match path {
        [section, _, page, key] => section == "machines" && page == "build_cache" && key == field,
        _ => false,
    }
}
impl SetupDialog {
    /// The machine whose build cache page is showing, with the settings its
    /// preview depends on.
    pub(super) fn build_cache_page(&self) -> Option<(String, Value)> {
        let [section, machine_id, page] = self.path.as_slice() else {
            return None;
        };
        if section != "machines" || page != "build_cache" {
            return None;
        }
        let key = serde_json::json!({
            "machine": self.draft["machines"][machine_id],
        });
        Some((machine_id.clone(), key))
    }

    /// The machine page that holds the build cache row, whose summary depends
    /// on what the machine's host can do.
    pub(super) fn build_cache_machine_page(&self) -> Option<(String, Value)> {
        let [section, machine_id] = self.path.as_slice() else {
            return None;
        };
        if section != "machines" || self.draft["machines"][machine_id]["build_cache"].is_null() {
            return None;
        }
        let key = serde_json::json!({
            "machine": self.draft["machines"][machine_id],
        });
        Some((machine_id.clone(), key))
    }

    /// Install eligibility and the exact machine draft a confirmation will use.
    pub(super) fn install_mbx_offer(&self) -> Option<MbxInstallOffer> {
        let (machine_id, key) = self.build_cache_machine_page()?;
        let preview = self.build_cache_preview.as_ref()?;
        if preview.key != key || !preview.install_mbx_available {
            return None;
        }
        let BuildCachePreviewResult::Ready(Some(preview)) = &preview.result else {
            return None;
        };
        let profile_file = preview.mbx_profile_file.clone()?;
        let profile_warning = preview.mbx_profile_warning.clone();
        let manual_path_line = preview.mbx_manual_path_line.clone();
        let machine = self.machine(&machine_id)?;
        Some(MbxInstallOffer {
            machine_id,
            key,
            upgrading: preview.native_mbx.is_some(),
            profile_file,
            profile_warning,
            manual_path_line,
            machine,
        })
    }

    pub(super) fn install_mbx_page_action(&self) -> Option<(&'static str, bool)> {
        let [section, machine_id] = self.path.as_slice() else {
            return None;
        };
        if section != "machines" {
            return None;
        }
        if let Some(install) = self
            .mbx_install
            .as_ref()
            .filter(|install| install.machine_id == *machine_id)
        {
            return Some((
                if install.upgrading {
                    "Upgrading mbx…"
                } else {
                    "Installing mbx…"
                },
                false,
            ));
        }
        let upgrading = self.install_mbx_offer()?.upgrading;
        Some((
            if upgrading {
                "Upgrade mbx"
            } else {
                "Install mbx"
            },
            true,
        ))
    }

    /// What the machine page's build cache row says when the resolved preview
    /// shows the host cannot share the cache.
    pub(super) fn build_cache_row_unavailable(&self) -> Option<String> {
        use mj_core::state::BuildCacheOff;
        let (_, key) = self.build_cache_machine_page()?;
        let preview = self.build_cache_preview.as_ref()?;
        if preview.key != key {
            return None;
        }
        let BuildCachePreviewResult::Ready(Some(preview)) = &preview.result else {
            return None;
        };
        match (&preview.off_reason, &preview.native_mbx) {
            // Only a Mac reports its own mbx while its containers cannot use it.
            (Some(BuildCacheOff::UnsupportedPlatform(_)), Some(version)) => {
                Some(format!("mbx {version}, bare sessions only"))
            }
            (Some(BuildCacheOff::Unavailable(_) | BuildCacheOff::UnsupportedPlatform(_)), _) => {
                Some("mbx unavailable".to_owned())
            }
            (Some(BuildCacheOff::TurnedOff) | None, _) => None,
        }
    }

    /// Start resolving the build cache page's automatic values on the target's
    /// host, unless the current settings are already resolved or in flight.
    pub(super) fn preview_build_cache_action(&mut self) -> DashboardAction {
        let Some((_, key)) = self
            .build_cache_page()
            .or_else(|| self.build_cache_machine_page())
        else {
            return DashboardAction::None;
        };
        if self
            .build_cache_preview
            .as_ref()
            .is_some_and(|preview| preview.key == key)
        {
            return DashboardAction::None;
        }
        let machine: mj_core::config::Machine = match serde_json::from_value(key["machine"].clone())
        {
            Ok(machine) => machine,
            // A draft that does not parse yet has nothing to resolve.
            Err(_) => return DashboardAction::None,
        };
        self.build_cache_preview = Some(BuildCachePreviewState {
            key: key.clone(),
            result: BuildCachePreviewResult::Resolving,
            install_mbx_available: false,
        });
        self.notice = Some("Resolving the build cache defaults on the machine…".into());
        DashboardAction::PreviewBuildCache {
            generation: self.generation,
            key,
            machine: Box::new(machine),
        }
    }

    /// What the build cache page shows for a blank field: the value its host
    /// resolves, shown bare in place of the "automatic" placeholder.
    pub(super) fn build_cache_automatic_label(&self, field: &str) -> Option<String> {
        use mj_core::state::BuildCacheLimit;
        let (_, key) = self.build_cache_page()?;
        if field == "tools_directory" {
            return None;
        }
        let preview = self.build_cache_preview.as_ref()?;
        if preview.key != key {
            return None;
        }
        let label = match &preview.result {
            BuildCachePreviewResult::Ready(Some(preview)) => match field {
                "enabled"
                    if matches!(
                        preview.off_reason,
                        Some(mj_core::state::BuildCacheOff::TurnedOff)
                    ) =>
                {
                    Checkbox::marker(false).to_owned()
                }
                "enabled" => Checkbox::marker(true).to_owned(),
                "directory" => preview
                    .directory
                    .as_ref()
                    .map(|directory| directory.display().to_string())
                    .unwrap_or_else(|| "Unknown".to_owned()),
                // Resolved sizes are shown in the same whole GB the field is
                // edited in.
                "max_total_size" => {
                    let limit = &preview.max_total_size;
                    match limit {
                        Some(BuildCacheLimit::Size(size)) => {
                            format!("{} GB, mj setting", build_cache_gigabytes_label(size))
                        }
                        Some(BuildCacheLimit::MjDefault(size)) => {
                            format!("{} GB, mj default", build_cache_gigabytes_label(size))
                        }
                        Some(BuildCacheLimit::HostConfiguration(Some(size))) if size == "none" => {
                            "Unlimited, host mbx".into()
                        }
                        Some(BuildCacheLimit::HostConfiguration(Some(size))) => {
                            format!("{} GB, host mbx config", build_cache_gigabytes_label(size))
                        }
                        Some(BuildCacheLimit::HostConfiguration(None)) => {
                            "Unknown, host mbx config".into()
                        }
                        Some(BuildCacheLimit::MbxDefault(Some(size))) => {
                            format!("{} GB, mbx default", build_cache_gigabytes_label(size))
                        }
                        Some(BuildCacheLimit::MbxDefault(None)) => "Unlimited, mbx default".into(),
                        None => "Unknown".into(),
                    }
                }
                _ => return None,
            },
            // The checkbox keeps showing the machine's own setting while the
            // other fields report how far the lookup got.
            _ if field == "enabled" => return None,
            BuildCachePreviewResult::Resolving => "Resolving…".to_owned(),
            BuildCachePreviewResult::Failed(_) => "Unknown".to_owned(),
            BuildCachePreviewResult::Ready(None) => "Not available for this machine".to_owned(),
        };
        Some(label)
    }

    pub(super) fn current_build_cache_preview(&self) -> Option<&mj_core::state::BuildCachePreview> {
        let (_, key) = self.build_cache_page()?;
        let preview = self.build_cache_preview.as_ref()?;
        if preview.key != key {
            return None;
        }
        match &preview.result {
            BuildCachePreviewResult::Ready(Some(preview)) => Some(preview.as_ref()),
            _ => None,
        }
    }

    pub(super) fn build_cache_user_managed(&self) -> bool {
        self.current_build_cache_preview()
            .is_some_and(|preview| preview.user_managed)
    }

    pub(super) fn build_cache_policy_lines(&self) -> Vec<String> {
        use mj_core::state::BuildCacheApplication;
        let Some(preview) = self.current_build_cache_preview() else {
            return Vec::new();
        };
        // An unsupported host has no policy to apply.
        if preview.directory.is_none() {
            return Vec::new();
        }
        let owner = if preview.user_managed {
            "User-managed mbx; mj budgets are inactive"
        } else {
            "Mj-managed mbx"
        };
        let status = match &preview.application {
            BuildCacheApplication::Pending => "Pending application".into(),
            BuildCacheApplication::Applied => "Applied to the machine".into(),
            BuildCacheApplication::Failed(error) => format!("Application failed: {error}"),
        };
        let mut lines = vec![format!("{owner} · {status}")];
        if let Some(note) = &preview.budget_note {
            lines.push(note.clone());
        }
        lines
    }

    /// What this machine's cache has actually done, for a line under the
    /// fields that configure it. The page otherwise only predicts.
    pub(super) fn build_cache_stats_line(&self) -> Option<String> {
        let (_, key) = self.build_cache_page()?;
        let preview = self.build_cache_preview.as_ref()?;
        if preview.key != key {
            return None;
        }
        let BuildCachePreviewResult::Ready(Some(preview)) = &preview.result else {
            return None;
        };
        let stats = preview.stats.as_ref()?;
        if stats.builds == 0 {
            return Some("No build has used this cache yet.".to_owned());
        }
        Some(format!(
            "{} builds, {} compilations from cache, {} of compiler time saved, {} cloned",
            stats.builds,
            stats.cached_compilations,
            format_compiler_time(stats.avoided_compiler_ns),
            crate::widgets::format_resource_bytes(stats.reflinked_bytes),
        ))
    }

    /// mbx availability does not govern native tool caches or their switch.
    pub(super) fn mbx_unavailable(&self) -> Option<&str> {
        use mj_core::state::BuildCacheOff;
        let (_, key) = self.build_cache_page()?;
        let preview = self.build_cache_preview.as_ref()?;
        if preview.key != key {
            return None;
        }
        match &preview.result {
            BuildCachePreviewResult::Ready(Some(preview)) => match &preview.off_reason {
                Some(
                    BuildCacheOff::Unavailable(reason) | BuildCacheOff::UnsupportedPlatform(reason),
                ) => Some(reason.as_str()),
                Some(BuildCacheOff::TurnedOff) | None => None,
            },
            _ => None,
        }
    }

    /// The `archive_after_days` value the SessionWiki page is showing right
    /// now: the text being typed when that editor is open, otherwise the
    /// saved value. The outer `None` means no SessionWiki page is showing, so
    /// there is nothing to estimate.
    pub(super) fn archive_after_days_page(&self) -> Option<Option<u32>> {
        if !self.path.iter().map(String::as_str).eq(["sessionwiki"]) {
            return None;
        }
        if let Some(editor) = self.editor.as_ref().filter(|editor| {
            editor
                .path
                .last()
                .is_some_and(|key| key == "archive_after_days")
        }) {
            // A half-typed or cleared number means "Never" until it parses.
            return Some(editor.input.to_string().trim().parse::<u32>().ok());
        }
        // Edits store a number; a hand-written string is still read the same.
        Some(match &self.draft["sessionwiki"]["archive_after_days"] {
            Value::String(text) => text.trim().parse::<u32>().ok(),
            value => value.as_u64().and_then(|days| u32::try_from(days).ok()),
        })
    }

    /// Start measuring what the SessionWiki page's current archive window
    /// would reclaim, unless that value is already measured or in flight.
    pub(super) fn preview_archive_space_action(&mut self) -> DashboardAction {
        let Some(older_than_days) = self.archive_after_days_page() else {
            return DashboardAction::None;
        };
        if self
            .archive_space_preview
            .as_ref()
            .is_some_and(|preview| preview.key == older_than_days)
        {
            return DashboardAction::None;
        }
        self.archive_space_preview = Some(ArchiveSpacePreviewState {
            key: older_than_days,
            result: ArchiveSpacePreviewResult::Resolving,
        });
        DashboardAction::PreviewArchiveSpace {
            generation: self.generation,
            older_than_days,
        }
    }

    /// What the SessionWiki page shows in the value column of "Archive after
    /// (days)": the value, then the space sessions use today and what that
    /// value would reclaim. Replaces the plain number, not just the placeholder.
    pub(super) fn archive_space_automatic_label(&self, field: &str) -> Option<String> {
        if field != "archive_after_days" {
            return None;
        }
        let key = self.archive_after_days_page()?;
        let estimate = self.archive_space_estimate()?;
        Some(match key {
            None => estimate,
            Some(days) => format!("{days} · {estimate}"),
        })
    }

    /// The estimate for the archive window the SessionWiki page is showing,
    /// without the value itself: under the open editor the value is the text
    /// being typed, so repeating it would only add noise.
    pub(super) fn archive_space_estimate(&self) -> Option<String> {
        let key = self.archive_after_days_page()?;
        let preview = self.archive_space_preview.as_ref()?;
        if preview.key != key {
            return None;
        }
        Some(match &preview.result {
            ArchiveSpacePreviewResult::Resolving => "Resolving…".to_owned(),
            ArchiveSpacePreviewResult::Failed(_) => "Unknown".to_owned(),
            ArchiveSpacePreviewResult::Ready(preview) => match key {
                None => format!(
                    "Never · sessions use {}",
                    crate::widgets::format_resource_bytes(preview.bytes)
                ),
                Some(_) => format!(
                    "would reclaim {} of {} ({} of {} sessions)",
                    crate::widgets::format_resource_bytes(preview.reclaimable_bytes),
                    crate::widgets::format_resource_bytes(preview.bytes),
                    preview.reclaimable_sessions,
                    preview.sessions
                ),
            },
        })
    }
}
