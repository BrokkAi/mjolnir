//! Sync episodes outlive individual debounced queries.
use crate::{
    DashboardState,
    resume::{resume_dialog, resume_dialog_mut},
};
use mj_client::daemon::{WikiSyncOutcome, WikiSyncStatus};
use ratatui::text::{Line, Span};

const SYNC_FAILED_PREFIX: &str = "Index sync failed: ";

fn filled_blocks(done: usize, total: usize, width: usize) -> usize {
    done.saturating_mul(width)
        .checked_div(total)
        .unwrap_or(width)
        .min(width)
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SyncEpisode {
    pub id: u64,
    pub status: Option<Result<WikiSyncStatus, String>>,
}

impl SyncEpisode {
    fn waiting(&self) -> bool {
        self.status.as_ref().is_some_and(|result| {
            result
                .as_ref()
                .is_ok_and(|status| status.target.is_some_and(|target| status.started < target))
        })
    }

    pub fn label(&self) -> Option<String> {
        let status = match self.status.as_ref() {
            None => return Some("Syncing session index…".into()),
            Some(Err(error)) => return Some(format!("{SYNC_FAILED_PREFIX}{error}")),
            Some(Ok(status)) => status,
        };
        if let Some(target) = status.target
            && status.finished >= target
        {
            if status.successful >= target {
                return None;
            }
            return match &status.outcome {
                Some(WikiSyncOutcome::Failed(error)) => {
                    Some(format!("{SYNC_FAILED_PREFIX}{error}"))
                }
                _ => None,
            };
        }
        if self.waiting() {
            return Some("Waiting for session index sync…".into());
        }
        let Some(progress) = &status.running else {
            return Some("Syncing session index…".into());
        };
        if progress.tool.is_empty() {
            return Some("Syncing session index…".into());
        }
        let Some(total) = progress.total else {
            return Some(format!("Scanning {}", progress.tool));
        };
        let blocks = filled_blocks(progress.done, total, 10);
        Some(format!(
            "Syncing {} {}/{} ▕{}{}▏ {}/{}",
            progress.tool,
            progress.adapter,
            progress.adapters,
            "█".repeat(blocks),
            "░".repeat(10 - blocks),
            progress.done,
            total
        ))
    }

    pub fn scanning(&self) -> bool {
        if self.waiting() {
            return true;
        }
        match &self.status {
            None => true,
            Some(Ok(status))
                if !status
                    .target
                    .is_some_and(|target| status.finished >= target) =>
            {
                status
                    .running
                    .as_ref()
                    .is_none_or(|progress| progress.total.is_none())
            }
            _ => false,
        }
    }

    pub fn line_fitting(
        &self,
        spinner: mj_core::config::SpinnerStyle,
        width: u16,
    ) -> Option<Line<'static>> {
        let full = self.line(spinner)?;
        let width = usize::from(width);
        if full.width() <= width {
            return Some(full);
        }
        let label = self.label()?;
        let short = if let Some(error) = label.strip_prefix(SYNC_FAILED_PREFIX) {
            // Keep the actionable cause, rather than spending a narrow row on its prefix.
            error.to_owned()
        } else if !self.waiting()
            && let Some(Ok(status)) = &self.status
            && let Some(progress) = &status.running
            && let Some(total) = progress.total
        {
            let counts = format!("{}/{}", progress.done, total);
            let blocks = width.saturating_sub(counts.len() + 3).min(10);
            if blocks == 0 {
                counts
            } else {
                let filled = filled_blocks(progress.done, total, blocks);
                format!(
                    "▕{}{}▏ {counts}",
                    "█".repeat(filled),
                    "░".repeat(blocks - filled)
                )
            }
        } else {
            return Some(mj_chat::chat::truncate_line_to_width(full, width));
        };
        Some(mj_chat::chat::truncate_line_to_width(
            Line::from(short),
            width,
        ))
    }

    pub fn line(&self, spinner: mj_core::config::SpinnerStyle) -> Option<Line<'static>> {
        let label = self.label()?;
        let mut spans = Vec::new();
        if self.scanning() {
            spans.push(mj_chat::spinner::compact_span(
                spinner,
                mj_chat::spinner::elapsed_ms(),
            ));
            spans.push(Span::raw(" "));
        }
        spans.push(Span::raw(label));
        Some(Line::from(spans))
    }
}

impl DashboardState {
    pub fn wiki_sync_episode(&self, dialog: bool) -> Option<u64> {
        if dialog {
            resume_dialog(&self.mode).map(|dialog| dialog.sync.id)
        } else {
            self.sessions_text.sync.as_ref().map(|sync| sync.id)
        }
    }

    /// Returns true exactly when the caller should rerun the current query.
    pub fn apply_wiki_sync(
        &mut self,
        dialog: bool,
        episode: u64,
        result: Result<WikiSyncStatus, String>,
    ) -> bool {
        let sync = if dialog {
            resume_dialog_mut(&mut self.mode).map(|dialog| &mut dialog.sync)
        } else {
            self.sessions_text.sync.as_mut()
        };
        let Some(sync) = sync.filter(|sync| sync.id == episode) else {
            return false;
        };
        let finished = result.as_ref().is_ok_and(|status| {
            status
                .target
                .is_some_and(|target| status.finished >= target && status.successful >= target)
        });
        sync.status = Some(result);
        finished
    }

    pub fn refresh_sessions_text_search(&mut self) -> Option<(u64, String)> {
        if self.sessions_text.asked.is_empty() {
            return None;
        }
        self.sessions_text.request_id = self.sessions_text.request_id.wrapping_add(1);
        self.sessions_text.pending =
            Some(crate::dashboard_sessions::SessionsTextQuery::SyncRefresh);
        Some((
            self.sessions_text.request_id,
            self.sessions_text.asked.clone(),
        ))
    }

    pub(crate) fn sessions_sync_line(&self, width: u16) -> Option<Line<'static>> {
        let sync = self.sessions_text.sync.as_ref()?;
        sync.line_fitting(self.config.spinner, width).or_else(|| {
            self.sessions_text
                .is_pending()
                .then(|| Line::from("Searching…"))
        })
    }

    pub(crate) fn sessions_sync_label(&self) -> Option<String> {
        self.sessions_sync_line(u16::MAX)
            .map(|line| line.to_string())
    }
}
