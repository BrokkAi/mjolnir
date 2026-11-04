//! Requests to change what the person's dashboard shows, made on their behalf.
//!
//! An agent asked to "maximize the Profiles pane" or "turn the bell off" sends
//! one of these to the daemon, which checks it and publishes it in the runtime
//! feed. Each attached dashboard applies it as input, through the same paths
//! its own keys use, so a request can do only what a keypress could. Settings
//! are staged in an unsaved Settings draft; the person saves them.

use mj_core::workspace::PaneSize;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The dashboard's support panes, by the names the person sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiPane {
    Sessions,
    Targets,
    Profiles,
}

/// Where a shown session goes: the main conversation, or a new pane beside
/// or below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPlacement {
    Main,
    Right,
    Below,
}

/// One setting to stage, by its Settings page path (`["notify", "bell"]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingEdit {
    pub path: Vec<String>,
    pub value: Value,
}

impl SettingEdit {
    /// The path as people and the configuration reference write it.
    pub fn dotted(&self) -> String {
        self.path.join(".")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiAction {
    PaneSize {
        pane: UiPane,
        size: PaneSize,
    },
    ShowSession {
        session_id: String,
        placement: SessionPlacement,
    },
    /// Open Settings with these edits applied to an unsaved draft.
    StageSettings {
        edits: Vec<SettingEdit>,
    },
}

/// What a caller sends to `POST /api/v1/ui/requests`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiRequestBody {
    /// The session whose agent asked, named in the dashboard's notice.
    #[serde(default)]
    pub from_session_id: Option<String>,
    pub action: UiAction,
}

/// A checked request as the runtime feed publishes it.
///
/// `id` increases within one daemon `incarnation`; a dashboard that outlives
/// a daemon restart sees a new incarnation and knows the numbering began again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiRequest {
    pub incarnation: String,
    pub id: u64,
    #[serde(default)]
    pub from_session_id: Option<String>,
    pub action: UiAction,
}

/// The daemon's answer: the request's id and how many dashboards were
/// attached to receive it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiRequestReceipt {
    pub id: u64,
    pub dashboards: usize,
}

/// Why a request was not published.
#[derive(Debug)]
pub enum UiRequestRefusal {
    /// The request itself cannot be carried out.
    Invalid(String),
    /// Nobody is looking: no dashboard is attached to apply it.
    NoDashboard,
}

impl std::fmt::Display for UiRequestRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(reason) => f.write_str(reason),
            Self::NoDashboard => f.write_str(
                "no dashboard is open to show this; ask the person to start `mj`, then send it again",
            ),
        }
    }
}

/// The newest request a dashboard has applied, or the newest present when
/// it attached, which it must not replay.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UiRequestCursor(Option<(String, u64)>);

impl UiRequestCursor {
    /// Start after every request already published.
    pub fn after(requests: &[UiRequest]) -> Self {
        Self(
            requests
                .iter()
                .max_by_key(|request| request.id)
                .map(|request| (request.incarnation.clone(), request.id)),
        )
    }

    /// The requests not yet applied, in order, advancing past them.
    ///
    /// A request from another daemon incarnation is new whatever its id: the
    /// daemon that published it started counting from one again.
    pub fn take_new<'a>(&mut self, requests: &'a [UiRequest]) -> Vec<&'a UiRequest> {
        let mut fresh = requests
            .iter()
            .filter(|request| match &self.0 {
                Some((incarnation, id)) => *incarnation != request.incarnation || request.id > *id,
                None => true,
            })
            .collect::<Vec<_>>();
        fresh.sort_by_key(|request| request.id);
        if let Some(last) = fresh.last() {
            self.0 = Some((last.incarnation.clone(), last.id));
        }
        fresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(incarnation: &str, id: u64) -> UiRequest {
        UiRequest {
            incarnation: incarnation.into(),
            id,
            from_session_id: None,
            action: UiAction::PaneSize {
                pane: UiPane::Sessions,
                size: PaneSize::Minimized,
            },
        }
    }

    // A dashboard survives daemon restarts; the new daemon numbers its
    // requests from one again and they must still be applied.
    #[test]
    fn requests_from_a_restarted_daemon_apply_although_their_ids_restart() {
        let mut cursor = UiRequestCursor::after(&[request("old", 1), request("old", 7)]);
        assert!(
            cursor
                .take_new(&[request("old", 1), request("old", 7)])
                .is_empty()
        );
        let after_restart = [request("new", 1), request("new", 2)];
        let ids = cursor
            .take_new(&after_restart)
            .into_iter()
            .map(|request| request.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, [1, 2]);
        assert!(cursor.take_new(&after_restart).is_empty());
    }
}
