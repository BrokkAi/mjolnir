use super::*;

/// Inputs that can change a session's source without changing its ID.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct ProjectSourceKey {
    pub(super) directory: Option<PathBuf>,
    pub(super) worktree: Option<mj_core::state::ManagedWorktree>,
    pub(super) target: Option<mj_core::config::TargetTemplate>,
    pub(super) fallback: ProjectSourceIdentity,
}

impl ProjectSourceKey {
    pub(super) fn of(session: &SessionRecord, config: &Config) -> Self {
        Self {
            directory: session.project_directory.clone(),
            worktree: session.managed_worktree.clone(),
            target: config.targets.get(&session.target_template_id).cloned(),
            fallback: session.project_source(config),
        }
    }
}

pub(super) struct ProjectSourceEntry {
    pub(super) key: ProjectSourceKey,
    pub(super) source: Option<ProjectSourceIdentity>,
    pub(super) retry_at: Option<Instant>,
    /// The error the last attempt failed with, so a retry that fails the same
    /// way is not reported again.
    pub(super) last_error: Option<String>,
    pub(super) cancelled: Arc<AtomicBool>,
}

pub(super) struct ProjectSourceResolved {
    pub(super) cancelled: Arc<AtomicBool>,
    pub(super) session_id: String,
    pub(super) key: ProjectSourceKey,
    pub(super) result: Result<ProjectSourceIdentity, String>,
}

/// Git/SSH probes run independently of snapshot publication and are bounded
/// and cancelled when their inputs disappear or the server shuts down.
///
/// Entries are kept per project identity session
/// ([`State::project_identity_session`]): a Mjolnir sub-agent works in its
/// parent's checkout and shares the parent's entry. Probing the child's own
/// directory failed once the parent's suspend removed that checkout (R11-2).
#[derive(Default)]
pub(super) struct PhoneProjectSources {
    pub(super) entries: std::collections::BTreeMap<String, ProjectSourceEntry>,
    pub(super) jobs: tokio::task::JoinSet<ProjectSourceResolved>,
}

impl PhoneProjectSources {
    pub(super) fn synchronize(&mut self, controller: &Controller) {
        let state = &controller.state;
        let own_identity =
            |session: &SessionRecord| state.project_identity_session(session).id == session.id;
        self.entries.retain(|id, entry| {
            let keep = state.sessions.get(id).is_some_and(|session| {
                own_identity(session)
                    && session.project_directory.is_some()
                    && entry.key == ProjectSourceKey::of(session, &controller.config)
            });
            if !keep {
                entry.cancelled.store(true, Ordering::Release);
            }
            keep
        });
        for session in state.sessions.values() {
            if self.jobs.len() >= 8 {
                break;
            }
            if !own_identity(session)
                || session.project_directory.is_none()
                || self.entries.get(&session.id).is_some_and(|entry| {
                    entry
                        .retry_at
                        .is_none_or(|deadline| Instant::now() < deadline)
                })
            {
                continue;
            }
            let key = ProjectSourceKey::of(session, &controller.config);
            let cancelled = Arc::new(AtomicBool::new(false));
            let last_error = self
                .entries
                .remove(&session.id)
                .and_then(|entry| entry.last_error);
            self.entries.insert(
                session.id.clone(),
                ProjectSourceEntry {
                    key: key.clone(),
                    source: None,
                    retry_at: None,
                    last_error,
                    cancelled: cancelled.clone(),
                },
            );
            let source_controller = Controller {
                config: controller.config.clone(),
                state: State {
                    sessions: [(session.id.clone(), session.clone())]
                        .into_iter()
                        .collect(),
                    ..State::default()
                },
            };
            let session_id = session.id.clone();
            self.jobs.spawn_blocking(move || {
                let executor = CancellableProcessExecutor::new(cancelled.clone())
                    .with_deadline(Duration::from_secs(8));
                let result = source_controller
                    .resolve_session_project_source(&session_id, &executor)
                    .map_err(|error| format!("{error:#}"));
                ProjectSourceResolved {
                    cancelled,
                    session_id,
                    key,
                    result,
                }
            });
        }
    }

    pub(super) fn complete(&mut self, resolved: ProjectSourceResolved) {
        let Some(entry) = self.entries.get_mut(&resolved.session_id) else {
            return;
        };
        if entry.key != resolved.key || !Arc::ptr_eq(&entry.cancelled, &resolved.cancelled) {
            return;
        }
        match resolved.result {
            Ok(source) => {
                entry.source = Some(source);
                entry.last_error = None;
            }
            Err(error) => {
                if entry.last_error.as_ref() == Some(&error) {
                    tracing::debug!(session_id = %resolved.session_id, %error, "could not resolve web project source");
                } else {
                    tracing::warn!(session_id = %resolved.session_id, %error, "could not resolve web project source");
                }
                entry.last_error = Some(error);
                entry.retry_at = Some(Instant::now() + Duration::from_secs(30));
            }
        }
    }

    /// The resolved source of `session`, read from its project identity
    /// session as the entries are kept.
    pub(super) fn source(
        &self,
        session: &SessionRecord,
        controller: &Controller,
    ) -> Option<&ProjectSourceIdentity> {
        let session = controller.state.project_identity_session(session);
        self.entries
            .get(&session.id)
            .filter(|entry| entry.key == ProjectSourceKey::of(session, &controller.config))
            .and_then(|entry| entry.source.as_ref())
    }
}

impl Drop for PhoneProjectSources {
    fn drop(&mut self) {
        for entry in self.entries.values() {
            entry.cancelled.store(true, Ordering::Release);
        }
    }
}
