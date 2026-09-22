//! Private, versioned terminal handoff between installed builds. Keep this
//! format backward compatible: the receiving executable is a newer release.
use super::*;
use std::path::PathBuf;

pub(crate) const RESUME_ENV: &str = "MJ_UPGRADE_RESUME_FILE";

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct UpgradeResume {
    version: u32,
    pub workspace_id: String,
    pub client_id: String,
    drafts: ComposerDraftCache,
    questions: BTreeMap<String, Vec<CachedQuestionDraft>>,
    positions: BTreeMap<String, mj_chat::chat::TranscriptPosition>,
    layouts: BTreeMap<String, ConversationLayout>,
    read_positions: BTreeMap<String, u64>,
}

impl UpgradeResume {
    pub(crate) async fn load() -> Result<Option<Self>> {
        let Some(path) = std::env::var_os(RESUME_ENV) else {
            return Ok(None);
        };
        tokio::task::spawn_blocking(move || {
            let path = PathBuf::from(path);
            let resume: Self = serde_json::from_slice(&std::fs::read(&path)?)
                .context("read terminal upgrade handoff")?;
            anyhow::ensure!(
                resume.version == 1,
                "unsupported terminal upgrade handoff version {}",
                resume.version
            );
            Ok(Some(resume))
        })
        .await
        .context("load terminal upgrade handoff task failed")?
    }

    /// Called only after the old terminal and runtime have been released.
    pub(crate) fn restart(self, target: &crate::daemon::UpgradeTarget) -> Result<()> {
        let path = mj_core::config::data_dir()
            .join("terminal-upgrades")
            .join(format!("{}.json", std::process::id()));
        mj_core::config::atomic_write(&path, &serde_json::to_vec(&self)?)
            .context("preserve terminal state for upgrade")?;
        eprintln!("Mjolnir upgraded; reconnecting this terminal.");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let error = std::process::Command::new(&target.executable)
                .args(std::env::args_os().skip(1))
                .env(RESUME_ENV, &path)
                .env("MJ_UPGRADE_DAEMON", &target.generation)
                .env("MJOLNIR_NO_UPDATE_CHECK", "1")
                .exec();
            Err(error).with_context(|| {
                format!(
                    "restart upgraded terminal; state preserved at {}",
                    path.display()
                )
            })
        }
        #[cfg(not(unix))]
        {
            let status = mj_core::subprocess::run_interactive(
                std::process::Command::new(&target.executable)
                    .args(std::env::args_os().skip(1))
                    .env(RESUME_ENV, &path)
                    .env("MJ_UPGRADE_DAEMON", &target.generation)
                    .env("MJOLNIR_NO_UPDATE_CHECK", "1"),
            )?;
            anyhow::ensure!(status.success(), "upgraded terminal exited with {status}");
            Ok(())
        }
    }
}

impl DashboardContext {
    pub(super) fn capture_upgrade(&mut self) -> UpgradeResume {
        for session in self
            .controller
            .state
            .sessions
            .keys()
            .cloned()
            .collect::<Vec<_>>()
        {
            self.capture_composer_draft(&session);
            self.save_question_draft(&session);
            if let Some(text) = self.dashboard.take_standby_prompt_draft(&session) {
                self.composer_drafts.capture(
                    &session,
                    text,
                    &self.controller.state.sessions[&session].draft_input,
                );
            }
        }
        UpgradeResume {
            version: 1,
            workspace_id: self.workspace_id.clone(),
            client_id: self.client_id.clone(),
            drafts: std::mem::take(&mut self.composer_drafts),
            questions: std::mem::take(&mut self.question_drafts),
            positions: std::mem::take(&mut self.transcript_positions),
            layouts: self.workspace_layouts.clone(),
            read_positions: self
                .controller
                .state
                .sessions
                .iter()
                .map(|(id, session)| (id.clone(), session.viewed_through_event_ordinal))
                .collect(),
        }
    }

    pub(super) async fn restore_upgrade(&mut self, resume: UpgradeResume) {
        self.composer_drafts = resume.drafts;
        self.question_drafts = resume.questions;
        self.transcript_positions = resume.positions;
        for (id, layout) in resume.layouts {
            if self.known_workspace_layouts.contains(&id) {
                self.dashboard.cache_workspace_layout(&id, layout.clone());
                self.set_workspace_layout(&id, layout);
            }
        }
        for (id, through) in resume.read_positions {
            if let Some(session) = self.controller.state.sessions.get_mut(&id) {
                session.viewed_through_event_ordinal =
                    session.viewed_through_event_ordinal.max(through);
                if let Some(through) = self.read_receipts.acknowledge(&id, through) {
                    self.spawn_read_receipt(id, through);
                }
            }
        }
        // Only discard the handoff after its contents have reached the new
        // context. A startup error before this point leaves it recoverable.
        if let Some(path) = std::env::var_os(RESUME_ENV) {
            let result = tokio::task::spawn_blocking(move || std::fs::remove_file(path)).await;
            if !matches!(&result, Ok(Ok(()))) {
                tracing::warn!(
                    ?result,
                    "could not remove restored terminal upgrade handoff"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_one_handoffs_remain_readable_by_future_builds() {
        // A fixed previous-build document, independent of today's serializer.
        // New fields must be optional so a skipped release can still resume.
        let resume: UpgradeResume = serde_json::from_str(r#"{
            "version": 1, "workspace_id": "work", "client_id": "terminal-1",
            "drafts": {"drafts": {"session-1": {"text": "unfinished prompt", "inherited_input": "old"}}},
            "questions": {}, "positions": {"session-1": "Bottom"},
            "layouts": {}, "read_positions": {"session-1": 42}
        }"#).unwrap();
        assert_eq!(
            resume.drafts.get("session-1").unwrap().text,
            "unfinished prompt"
        );
        assert_eq!(resume.read_positions["session-1"], 42);
        assert_eq!(resume.workspace_id, "work");
    }

    #[test]
    fn a_failed_exec_retains_large_drafts_and_cleared_inputs() {
        if crate::test_support::rerun_in_isolated_child(
            "MJ_TEST_UPGRADE_HANDOFF",
            "dashboard::upgrade::tests::a_failed_exec_retains_large_drafts_and_cleared_inputs",
        ) {
            return;
        }
        let mut drafts = ComposerDraftCache::default();
        let text = "unsent 🦀\n".repeat(16_000);
        drafts.capture("edited", text.clone(), "shared baseline");
        drafts.capture("cleared", String::new(), "do not resurrect");
        let resume = UpgradeResume {
            version: 1,
            workspace_id: "workspace-original".into(),
            client_id: "terminal-original".into(),
            drafts,
            questions: BTreeMap::new(),
            positions: BTreeMap::new(),
            layouts: BTreeMap::new(),
            read_positions: BTreeMap::from([("edited".into(), 123)]),
        };
        let error = resume
            .restart(&crate::daemon::UpgradeTarget {
                executable: mj_core::config::data_dir().join("missing-executable"),
                generation: "test-daemon".into(),
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("restart upgraded terminal"));
        let path = mj_core::config::data_dir()
            .join("terminal-upgrades")
            .join(format!("{}.json", std::process::id()));
        let saved = std::fs::read(&path).unwrap();
        assert!(saved.len() > 64 * 1024);
        let resume: UpgradeResume = serde_json::from_slice(&saved).unwrap();
        assert_eq!(resume.workspace_id, "workspace-original");
        assert_eq!(resume.client_id, "terminal-original");
        assert_eq!(resume.drafts.get("edited").unwrap().text, text);
        assert_eq!(resume.drafts.get("cleared").unwrap().text, "");
        assert_eq!(
            resume
                .drafts
                .get("cleared")
                .unwrap()
                .inherited_input
                .as_deref(),
            Some("do not resurrect")
        );
        assert_eq!(resume.read_positions["edited"], 123);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
