use super::*;

impl DashboardState {
    pub fn build_cache_previewed(
        &mut self,
        generation: u64,
        key: &Value,
        result: Result<Option<mj_core::state::BuildCachePreview>, String>,
        install_mbx_available: bool,
    ) {
        let Some(dialog) = setup_dialog_mut(&mut self.mode) else {
            return;
        };
        if dialog.generation != generation {
            return;
        }
        let machines = dialog
            .build_cache_previews
            .iter()
            .filter(|(machine_id, preview)| {
                &preview.key == key && dialog.build_cache_key(machine_id).as_ref() == Some(key)
            })
            .map(|(machine_id, _)| machine_id.clone())
            .collect::<Vec<_>>();
        if machines.is_empty() {
            return;
        }
        let notice = match &result {
            Ok(Some(preview)) => {
                let host_mbx = match &preview.native_mbx {
                    Some(version) => format!("host mbx {version}"),
                    None => "no mbx on the host".to_owned(),
                };
                match &preview.off_reason {
                    // The reason already names the host's own mbx and what uses it.
                    Some(mj_core::state::BuildCacheOff::UnsupportedPlatform(reason)) => {
                        reason.clone()
                    }
                    Some(reason) => {
                        format!("The mbx build cache is unavailable: {reason} ({host_mbx}).")
                    }
                    None => format!("Sessions here share the build cache ({host_mbx})."),
                }
            }
            Ok(None) => "This target kind cannot share a build cache.".to_owned(),
            Err(error) => format!("Could not resolve the build cache defaults: {error}"),
        };
        let preview = BuildCachePreviewState {
            key: key.clone(),
            result: match result {
                Ok(preview) => BuildCachePreviewResult::Ready(preview.map(Box::new)),
                Err(error) => BuildCachePreviewResult::Failed(error),
            },
            install_mbx_available,
        };
        for machine_id in machines {
            dialog
                .build_cache_previews
                .insert(machine_id, preview.clone());
        }
        dialog.notice = Some(notice);
        dialog.prepare();
    }

    /// Mark the confirmed host mutation busy before the CLI starts its worker.
    pub fn mbx_install_started(&mut self, generation: u64, key: &Value, machine_id: &str) -> bool {
        let Some(dialog) = setup_dialog_mut(&mut self.mode) else {
            return false;
        };
        if dialog.generation != generation || dialog.mbx_install.is_some() {
            return false;
        }
        let Some(offer) = dialog.install_mbx_offer() else {
            return false;
        };
        if offer.machine_id != machine_id || &offer.key != key {
            return false;
        }
        let host = machine_host_label(machine_id, &offer.machine);
        dialog.mbx_install = Some(MbxInstallState {
            key: key.clone(),
            machine_id: machine_id.to_owned(),
            upgrading: offer.upgrading,
        });
        dialog.notice = Some(if offer.upgrading {
            format!("Upgrading mbx on {host}…")
        } else {
            format!("Installing mbx on {host}…")
        });
        dialog.prepare();
        true
    }

    /// Report the install result and apply its fresh cache preview when the
    /// settings draft still describes the machine that was installed.
    pub fn mbx_install_finished(
        &mut self,
        generation: u64,
        key: &Value,
        machine_id: &str,
        result: Result<String, String>,
        preview: Result<Option<mj_core::state::BuildCachePreview>, String>,
        install_mbx_available: bool,
    ) {
        let Some(dialog) = setup_dialog_mut(&mut self.mode) else {
            self.set_notice(format_mbx_install_result(result, None));
            return;
        };
        if dialog.generation != generation {
            dialog.notice = Some(format_mbx_install_result(result, None));
            dialog.prepare();
            return;
        }
        let operation = dialog
            .mbx_install
            .as_ref()
            .filter(|operation| operation.machine_id == machine_id && &operation.key == key)
            .cloned();
        if operation.is_some() {
            dialog.mbx_install = None;
        }
        let mut notice = format_mbx_install_result(result, operation.as_ref());
        if dialog.build_cache_key(machine_id).as_ref() == Some(key) {
            match preview {
                Ok(preview) => {
                    dialog.build_cache_previews.insert(
                        machine_id.to_owned(),
                        BuildCachePreviewState {
                            key: key.clone(),
                            result: BuildCachePreviewResult::Ready(preview.map(Box::new)),
                            install_mbx_available,
                        },
                    );
                }
                Err(error) => {
                    // The host may have changed even if its fresh preview failed.
                    // Never let the pre-install result suppress a new resolution.
                    dialog.build_cache_previews.remove(machine_id);
                    notice.push_str(&format!(
                        " Could not refresh the build cache preview: {error}"
                    ));
                }
            }
        } else if let Err(error) = preview {
            notice.push_str(&format!(
                " Could not refresh the build cache preview: {error}"
            ));
        }
        dialog.notice = Some(notice);
        dialog.prepare();
    }

    /// Take the space an archive window would reclaim, dropping an answer the
    /// user has already typed past.
    pub fn archive_space_previewed(
        &mut self,
        generation: u64,
        older_than_days: Option<u32>,
        result: Result<mj_core::state::ArchiveSpacePreview, String>,
    ) {
        let Some(dialog) = setup_dialog_mut(&mut self.mode) else {
            return;
        };
        if dialog.generation != generation
            || dialog
                .archive_space_preview
                .as_ref()
                .is_none_or(|preview| preview.key != older_than_days)
        {
            return;
        }
        // The estimate itself is drawn beside the value; the notice line only
        // carries a failure to measure.
        if let Err(error) = &result {
            dialog.notice = Some(format!("Could not measure what sessions use: {error}"));
        }
        dialog.archive_space_preview = Some(ArchiveSpacePreviewState {
            key: older_than_days,
            result: match result {
                Ok(preview) => ArchiveSpacePreviewResult::Ready(preview),
                Err(error) => ArchiveSpacePreviewResult::Failed(error),
            },
        });
        dialog.prepare();
    }

    pub fn setup_path_resolved(
        &mut self,
        generation: u64,
        draft: &Value,
        path: &[String],
        value: &str,
        result: Result<std::path::PathBuf, String>,
    ) {
        let Some(dialog) = setup_dialog_mut(&mut self.mode) else {
            return;
        };
        if dialog.generation != generation {
            return;
        }
        let resumes_save = matches!(&dialog.pending_save, PendingSave::Resolving { path: pending_path, value: pending_value, draft: pending_draft }
            if pending_path == path && pending_value == value && pending_draft == draft);
        if &dialog.draft != draft {
            if resumes_save {
                dialog.pending_save = PendingSave::Idle;
            }
            return;
        }
        let Some(field) = dialog.fields.get_mut(path) else {
            return;
        };
        if field.input.value() != value {
            return;
        }
        let applied = match result {
            Ok(resolved) => {
                field
                    .input
                    .set_value(resolved.to_string_lossy().into_owned());
                dialog.apply_pending(path)
            }
            Err(error) => {
                field.error = Some(error.clone());
                Err(error)
            }
        };
        dialog.notice = applied.clone().err();
        if resumes_save {
            dialog.pending_save = if applied.is_ok() {
                PendingSave::Ready
            } else {
                PendingSave::Idle
            };
            if applied.is_err() {
                dialog.open_page(path[..path.len() - 1].to_vec());
                dialog.focus_path(path);
            }
        }
        dialog.form.get_mut().reveal_focused();
    }

    /// Report the result of an interactive sign-in started from the profiles
    /// page. A dialog that closed while the command ran reports on the
    /// dashboard instead.
    pub fn setup_login_finished(
        &mut self,
        generation: u64,
        profile_id: &str,
        result: Result<(), String>,
    ) {
        self.profile_login_refresh = true;
        let message = match result {
            Ok(()) => format!(
                "Signed in to {profile_id}. New and resumed sessions use the updated login."
            ),
            Err(error) => format!("Sign-in failed for {profile_id}: {error}"),
        };
        let current =
            setup_dialog_mut(&mut self.mode).is_some_and(|dialog| dialog.generation == generation);
        if !current {
            self.set_notice(message);
            return;
        }
        if let Some(dialog) = setup_dialog_mut(&mut self.mode) {
            dialog.notice = Some(message);
            dialog.prepare();
        }
    }

    pub fn setup_saved(&mut self, generation: u64, result: Result<Config, String>) {
        let current =
            setup_dialog_mut(&mut self.mode).is_some_and(|dialog| dialog.generation == generation);
        if !current {
            match result {
                Ok(config) => self.set_config(config),
                Err(error) => self.set_failure_notice(format!("Could not save Settings: {error}")),
            }
            return;
        }
        match result {
            Ok(config) => {
                // Workers read the Jev switch when they start, so a session
                // already running keeps its old setting until then.
                let jev_changed = config.jev != self.config.jev;
                self.set_config(config);
                self.cancel_modal();
                self.set_notice(if jev_changed {
                    "Settings saved. New sessions use these defaults and the new Jev setting; running sessions follow it after their next resume or restart. Web listener changes apply on its next start."
                } else {
                    "Settings saved. New sessions use these defaults. Web listener changes apply on its next start."
                });
            }
            Err(error) => {
                if let Some(dialog) = setup_dialog_mut(&mut self.mode) {
                    dialog.saving = false;
                    dialog.notice = Some(format!("Could not save: {error}"));
                    dialog.prepare();
                } else {
                    self.set_failure_notice(error);
                }
            }
        }
    }

    pub fn setup_discovered(&mut self, generation: u64, result: Result<SetupDetection, String>) {
        let Some(dialog) = setup_dialog_mut(&mut self.mode) else {
            return;
        };
        if dialog.generation != generation {
            return;
        }
        dialog.discovering = false;
        match result {
            Ok(detection) => {
                let SetupDetection {
                    scope,
                    config,
                    mut rejected_runtimes,
                    machines,
                } = detection;
                // Reconcile against the current draft, including edits made
                // while discovery was running, rather than dropping collisions.
                let current: Config = match config_from_draft(dialog.draft.clone()) {
                    Ok(current) => current,
                    Err(error) => {
                        dialog.notice = Some(format!(
                            "Review the settings draft before detecting entries: {error}"
                        ));
                        dialog.prepare();
                        return;
                    }
                };
                let additions = current.setup_additions(&config);
                let local_targets = additions.targets.clone();
                let discovered = serde_json::to_value(additions).expect("config serializes");
                // A detection only writes its own section, so a profile run
                // cannot add machines and a runtime run cannot add projects.
                let section = match scope {
                    DetectScope::Profiles => "profiles",
                    DetectScope::Runtimes => "targets",
                };
                let mut added = Vec::new();
                if let Some(entries) = discovered[section].as_object() {
                    for (key, value) in entries {
                        if dialog.draft[section].get(key).is_some() {
                            continue;
                        }
                        dialog.draft[section]
                            .as_object_mut()
                            .unwrap()
                            .insert(key.clone(), value.clone());
                        added.push(key.clone());
                        if section == "profiles" {
                            dialog.invalidate_review_validation_for(Some(key));
                        }
                    }
                }
                // Each machine's runtimes are decided against the draft as it
                // is now, the same way a probe at startup decides against the
                // file: a kind the machine already runs is not added twice,
                // and a machine edited while it was probed gets nothing.
                let mut found_on_machines = false;
                if section == "targets" {
                    let mut current = current;
                    current.targets.extend(local_targets);
                    for probe in &machines {
                        let crate::MachineFinding::Answered { runtimes, rejected } = &probe.finding
                        else {
                            rejected_runtimes.push(RejectedRuntime {
                                label: probe.machine_id.clone(),
                                detail: crate::machine_runtimes::finding_reason(&probe.finding),
                                remediation: None,
                            });
                            continue;
                        };
                        found_on_machines |= !runtimes.is_empty();
                        rejected_runtimes.extend(rejected.iter().map(|runtime| RejectedRuntime {
                            label: format!("{} on {}", runtime.label, probe.machine_id),
                            ..runtime.clone()
                        }));
                        let additions = current.machine_runtime_additions(
                            &probe.machine_id,
                            &probe.machine,
                            runtimes.iter().cloned(),
                        );
                        if additions.is_empty() {
                            continue;
                        }
                        current.targets.extend(additions.clone());
                        // Stored against the draft's own machines, so each
                        // runtime names the machine it was found on.
                        let stored = serde_json::to_value(Config {
                            machines: current.machines.clone(),
                            targets: additions,
                            ..Config::default()
                        })
                        .expect("config serializes");
                        if let Some(entries) = stored["targets"].as_object() {
                            for (key, value) in entries {
                                dialog.draft["targets"]
                                    .as_object_mut()
                                    .unwrap()
                                    .insert(key.clone(), value.clone());
                                added.push(key.clone());
                            }
                        }
                    }
                }
                schema::expand(&mut dialog.draft, &mut Vec::new());
                populate_subagent_profile_choices(&mut dialog.draft);
                let found_any = match scope {
                    DetectScope::Profiles => !config.profiles.is_empty(),
                    DetectScope::Runtimes => !config.targets.is_empty() || found_on_machines,
                };
                dialog.notice = Some(detection_notice(
                    scope,
                    found_any,
                    &added,
                    &rejected_runtimes,
                ));
            }
            Err(error) => dialog.notice = Some(format!("Detection failed: {error}")),
        }
        dialog.prepare();
    }
}
/// How many lines a machine row's missing-runtime reason may take.
pub(super) const MACHINE_NOTE_LINES: usize = 3;

/// Why machine `id` in the Settings draft has no runtime, or `None` when a
/// runtime in the draft runs on it. A machine as the configuration has it
/// reports what its last check found; one the draft adds or changes reports
/// what saving it will do.
pub(super) fn machine_without_runtime_note(
    dashboard: &DashboardState,
    dialog: &SetupDialog,
    id: &str,
) -> Option<String> {
    if id == mj_core::config::LOCAL_MACHINE_ID {
        return None;
    }
    let runs_one = dialog.draft["targets"]
        .as_object()
        .is_some_and(|targets| targets.values().any(|target| target["machine"] == id));
    if runs_one {
        return None;
    }
    let saved =
        serde_json::from_value::<mj_core::config::Machine>(dialog.draft["machines"][id].clone())
            .ok()
            .is_some_and(|machine| dashboard.config.machines.get(id) == Some(&machine));
    if !saved {
        return Some("Save to find what it can run and add those runtimes.".to_owned());
    }
    Some(match dashboard.machine_runtime_reason(id) {
        Some(reason) => format!("No runtime: {reason}"),
        None if dashboard.config.machine_runtimes(id).next().is_some() => {
            "Saving finds what it can run and adds those runtimes again.".to_owned()
        }
        None => "Mjolnir checks what it can run when Settings closes.".to_owned(),
    })
}

/// What the Settings screen says after a detection run: the entries it added
/// by name, and for runtimes the ones it turned down and why. When it added
/// nothing, it says whether it found nothing at all or found only what
/// Settings already lists (launch finding R13-4).
pub(super) fn detection_notice(
    scope: DetectScope,
    found_any: bool,
    added: &[String],
    rejected: &[RejectedRuntime],
) -> String {
    let mut sentences = Vec::new();
    if added.is_empty() {
        sentences.push(match (scope, found_any) {
            (DetectScope::Profiles, false) => format!(
                "No coding agent installation was found on this machine. Add a profile, choose \
                 Sign in, or install {}, sign in to it once, and choose Detect profiles again.",
                mj_core::config::HarnessKind::every_display_name_or()
            ),
            (DetectScope::Profiles, true) => {
                "Every coding agent found on this machine already has a profile.".to_owned()
            }
            (DetectScope::Runtimes, false) => "No usable runtime was found.".to_owned(),
            (DetectScope::Runtimes, true) => {
                "Every usable runtime found is already listed.".to_owned()
            }
        });
    } else {
        let names = added.join(", ");
        sentences.push(match scope {
            DetectScope::Profiles => format!("Added agent profiles: {names}."),
            DetectScope::Runtimes => format!("Added runtimes: {names}."),
        });
    }
    for runtime in rejected {
        let mut reason = format!("Skipped {}: {}", runtime.label, runtime.detail.trim());
        if !reason.ends_with('.') {
            reason.push('.');
        }
        if let Some(remediation) = &runtime.remediation {
            reason.push(' ');
            reason.push_str(remediation.trim());
            if !reason.ends_with('.') {
                reason.push('.');
            }
        }
        sentences.push(reason);
    }
    if !added.is_empty() {
        sentences.push("Review them, then Save.".to_owned());
    }
    sentences.join(" ")
}

pub(super) fn setup_dialog_mut(mode: &mut Mode) -> Option<&mut SetupDialog> {
    match mode {
        Mode::Setup(dialog) => Some(dialog),
        Mode::Help(overlay) => setup_dialog_mut(&mut overlay.return_to),
        _ => None,
    }
}
