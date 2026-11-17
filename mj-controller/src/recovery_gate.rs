//! Per-session coordination between foreground operations and background recovery.
use mj_core::state::RecoveryObservation;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, mpsc, watch};
/// Validate a queued placement after admission. A lifecycle may have finished
/// between the observation and this attempt, so the old target cannot be used.
pub(crate) fn current_background_session(
    observed: &mj_core::state::SessionRecord,
) -> anyhow::Result<Option<mj_core::state::SessionRecord>> {
    if crate::database::container_upgrade_owns_target(&observed.id)? {
        return Ok(None);
    }
    let current = crate::database::read_durable_session_record(&observed.id)?;
    Ok(current.filter(|current| {
        current.state == mj_core::state::SessionState::Running
            && current.target == observed.target
            && current.native_session_id == observed.native_session_id
            && current.harness_kind == observed.harness_kind
            && current.last_profile == observed.last_profile
    }))
}

/// Resolves once an admission's cancellation flag is set. Admitted work that
/// cannot be cancelled by being dropped polls this instead.
pub(crate) async fn wait_until_cancelled(flag: &AtomicBool) {
    while !flag.load(Ordering::Acquire) {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Reports session activity to the recovery coordinator.
///
/// Reporting retains only the newest observation per session. Completed-turn
/// frontiers are merged monotonically, so coalescing does not lose the boundary
/// that makes a recovery copy due.
///
/// A caller that must know no copy can start uses [`RecoveryObserver::reserve`]
/// rather than the queue: the reservation blocks a copy from starting whether
/// or not queued observations have been read yet.
#[derive(Clone)]
pub struct RecoveryObserver {
    pub(crate) observations: ObservationSender<PendingRecoveryObservation>,
    pub gate: Arc<RecoveryGate>,
}

#[derive(Clone)]
pub(crate) struct PendingRecoveryObservation {
    pub observation: RecoveryObservation,
    // A busy-to-idle edge releases a deferral even when both observations are
    // folded into one pending entry before the policy consumes them.
    pub observed_wait: bool,
}

/// A per-session reservation held by a foreground lifecycle operation. The
/// coordinator cannot start another recovery copy until this value is dropped.
pub struct RecoveryReservation {
    _admission: ForegroundAdmission,
}

#[derive(Clone)]
pub struct ForegroundAdmission(Arc<ForegroundHolder>);

struct ForegroundHolder {
    label: &'static str,
    identity: Arc<()>,
    session_id: String,
    gate: Arc<RecoveryGate>,
}

impl Drop for ForegroundHolder {
    fn drop(&mut self) {
        self.gate.release(&self.session_id, &self.identity);
    }
}

/// What a background admission is for. The gate admits a kind only beside
/// holders it can share the session with (see [`BackgroundWork::runs_beside`]),
/// and a foreground lifecycle operation preempts every holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundWork {
    /// Stops and replaces the worker, so it shares the session with nothing.
    WorkerReplacement,
    /// Copies the session's state from the running worker. A large remote
    /// copy takes minutes and the next one is due soon after (#1564).
    RecoveryCopyCapturing,
    RecoveryCopyTransferring,
    RecoveryCopyAbandoning,
    DelegationDelivery,
    /// Reconciles the session's logins (harness, AWS, GitHub, skills) with a
    /// few relay requests.
    CredentialSync,
}

impl BackgroundWork {
    /// Whether this work may start while `other` holds the session.
    ///
    /// A credential sync runs beside a recovery copy: the worker serves its
    /// requests on the live relay connection whatever ACP dispatch or a
    /// checkpoint barrier is doing, and archives leave login files out. If
    /// it had to wait, back-to-back copies of a large session would keep a
    /// renewed login, which revokes the old one, from ever reaching it.
    pub fn label(self) -> &'static str {
        match self {
            Self::WorkerReplacement => "worker upgrade",
            Self::RecoveryCopyCapturing => "recovery copy",
            Self::RecoveryCopyTransferring => "recovery copy transfer",
            Self::RecoveryCopyAbandoning => "recovery copy cleanup",
            Self::DelegationDelivery => "delegation delivery",
            Self::CredentialSync => "credential sync",
        }
    }

    pub fn needs_lease(self) -> bool {
        !matches!(self, Self::CredentialSync | Self::RecoveryCopyTransferring)
    }

    fn runs_beside(self, other: Self) -> bool {
        use BackgroundWork::*;
        matches!(
            (self, other),
            (
                CredentialSync,
                RecoveryCopyCapturing
                    | RecoveryCopyTransferring
                    | RecoveryCopyAbandoning
                    | DelegationDelivery
            ) | (
                RecoveryCopyCapturing
                    | RecoveryCopyTransferring
                    | RecoveryCopyAbandoning
                    | DelegationDelivery,
                CredentialSync
            ) | (RecoveryCopyTransferring, DelegationDelivery)
                | (DelegationDelivery, RecoveryCopyTransferring)
        )
    }
}

/// Admission for the background work that acts on a session's live worker.
///
/// It is shared rather than per-coordinator so that one place decides which
/// kinds of work may run together on a session, and a foreground lifecycle
/// operation preempts all of them.
#[derive(Debug)]
pub struct Refusal {
    pub holders: Vec<&'static str>,
}

pub struct RecoveryGate {
    state: Mutex<RecoveryGateState>,
    closed: Notify,
    /// Which sessions are busy, for waiters. Published from inside the gate so
    /// every holder updates it, whatever started the work.
    busy: watch::Sender<BTreeSet<String>>,
    credential_wakes: watch::Sender<BTreeSet<String>>,
}

impl Default for RecoveryGate {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            closed: Notify::new(),
            busy: watch::channel(BTreeSet::new()).0,
            credential_wakes: watch::channel(BTreeSet::new()).0,
        }
    }
}

#[derive(Default)]
struct RecoveryGateState {
    closed: bool,
    /// In-flight work per session, each holder with the cancel flag its
    /// executor watches, so a foreground lifecycle operation can preempt it
    /// instead of waiting. A session's entry is never empty.
    busy: BTreeMap<String, Vec<BackgroundHolder>>,
    reservations: BTreeMap<String, Vec<Reservation>>,
    leases: BTreeMap<String, &'static str>,
    deferred_credentials: BTreeSet<String>,
}

/// Reviews exclude automatic work for their whole lifetime, but cannot hold
/// up a person's lifecycle or daemon handoff. Their worker work survives it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReservationKind {
    Foreground,
    ForegroundDelegationDrain,
    Review,
}
impl ReservationKind {
    fn runs_beside(self, other: Self) -> bool {
        self == Self::Review || other == Self::Review
    }
}
struct Reservation {
    identity: Arc<()>,
    label: &'static str,
    kind: ReservationKind,
}

struct BackgroundHolder {
    work: BackgroundWork,
    cancelled: Arc<AtomicBool>,
}

impl RecoveryGate {
    /// Run disposable background work under the session's background
    /// admission. Lifecycle reservations preempt it before they touch the
    /// worker. Dropping the future also releases admission.
    pub async fn run_background<T>(
        self: &Arc<Self>,
        session_id: &str,
        kind: BackgroundWork,
        work: impl std::future::Future<Output = T>,
    ) -> Option<T> {
        self.run_background_owned(session_id, kind, |_| work).await
    }

    /// `run_background` for work that outlives the future awaiting it, such as
    /// a job queued on a session actor: `work` receives the admission and may
    /// hand it to that job, which then holds the slot until it has finished or
    /// been dropped, whether or not the awaiting future is still there.
    pub async fn run_background_owned<T, F: std::future::Future<Output = T>>(
        self: &Arc<Self>,
        session_id: &str,
        kind: BackgroundWork,
        work: impl FnOnce(RecoveryAttempt) -> F,
    ) -> Option<T> {
        let admission = match self.try_start(session_id, kind) {
            Ok(admission) => admission,
            Err(refusal) => {
                tracing::debug!(session_id, holders = ?refusal.holders, "background work deferred");
                return None;
            }
        };
        let cancelled = admission.cancellation();
        // Held here as well, so work that ignores the admission still runs
        // under it for as long as this future lives.
        let _admission = admission.clone();
        let work = work(admission);
        tokio::select! {
            biased;
            () = wait_until_cancelled(&cancelled) => None,
            result = work => Some(result),
        }
    }

    pub fn reserve(self: &Arc<Self>, session_id: &str) -> RecoveryReservation {
        RecoveryReservation {
            _admission: self.reserve_labelled(session_id, "foreground operation"),
        }
    }

    fn reserve_labelled(
        self: &Arc<Self>,
        session_id: &str,
        label: &'static str,
    ) -> ForegroundAdmission {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.reserve_locked(&mut state, session_id, label, ReservationKind::Foreground)
    }

    fn reserve_locked(
        self: &Arc<Self>,
        state: &mut RecoveryGateState,
        session_id: &str,
        label: &'static str,
        kind: ReservationKind,
    ) -> ForegroundAdmission {
        let identity = Arc::new(());
        state
            .reservations
            .entry(session_id.to_owned())
            .or_default()
            .push(Reservation {
                identity: identity.clone(),
                label,
                kind,
            });
        ForegroundAdmission(Arc::new(ForegroundHolder {
            label,
            identity,
            session_id: session_id.to_owned(),
            gate: self.clone(),
        }))
    }

    pub async fn enter_foreground(
        self: &Arc<Self>,
        session_id: &str,
        label: &'static str,
        cancelled: &AtomicBool,
        notices: &(impl mj_core::targets::CommandExecutor + Sync),
    ) -> anyhow::Result<ForegroundAdmission> {
        self.enter(
            session_id,
            label,
            cancelled,
            notices,
            ReservationKind::Foreground,
        )
        .await
    }

    /// Reviews exclude background work without preempting it. They share
    /// the session with foreground lifecycles, whose worker actions already
    /// cancel or refuse overlapping review capture/selection requests.
    pub async fn reserve_when_idle(self: &Arc<Self>, session_id: &str) -> RecoveryReservation {
        let admission = self
            .enter(
                session_id,
                "turn review",
                &AtomicBool::new(false),
                &mj_core::targets::ProcessExecutor,
                ReservationKind::Review,
            )
            .await
            .expect("an uncancelled review admission retains its publication");
        RecoveryReservation {
            _admission: admission,
        }
    }

    async fn enter(
        self: &Arc<Self>,
        session_id: &str,
        label: &'static str,
        cancelled: &AtomicBool,
        notices: &(impl mj_core::targets::CommandExecutor + Sync),
        kind: ReservationKind,
    ) -> anyhow::Result<ForegroundAdmission> {
        let mut busy = self.subscribe();
        let mut reservation = None;
        let mut previous = Vec::new();
        loop {
            anyhow::ensure!(
                !cancelled.load(Ordering::Acquire),
                "operation cancelled while waiting for session admission"
            );
            let holders = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if reservation.is_none()
                    // A waiting review does not preempt background work, so
                    // compatible work (notably credential sync beside a copy)
                    // must remain admissible until the review can start.
                    && (kind == ReservationKind::Foreground || !state.busy.contains_key(session_id))
                    && !state
                        .reservations
                        .get(session_id)
                        .into_iter()
                        .flatten()
                        .any(|holder| !kind.runs_beside(holder.kind))
                {
                    reservation = Some(self.reserve_locked(&mut state, session_id, label, kind));
                    if kind == ReservationKind::Foreground
                        && let Some(holders) = state.busy.get(session_id)
                    {
                        for holder in holders {
                            holder.cancelled.store(true, Ordering::Release);
                        }
                    }
                }
                let mut holders = state
                    .busy
                    .get(session_id)
                    .into_iter()
                    .flatten()
                    .map(|holder| holder.work.label())
                    .collect::<Vec<_>>();
                if reservation.is_none() {
                    holders.extend(
                        state
                            .reservations
                            .get(session_id)
                            .into_iter()
                            .flatten()
                            .filter(|holder| !kind.runs_beside(holder.kind))
                            .map(|holder| holder.label),
                    );
                }
                holders
            };
            if holders.is_empty()
                && let Some(reservation) = reservation.take()
            {
                return Ok(reservation);
            }
            if holders != previous {
                notices.notify_notice(&format!("Waiting for {} to stop", holders.join(" and ")));
                previous = holders;
            }
            tokio::select! {
                result = busy.changed() => { result?; }
                () = wait_until_cancelled(cancelled) => {}
            }
        }
    }

    fn release(&self, session_id: &str, identity: &Arc<()>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(holders) = state.reservations.get_mut(session_id) {
            holders.retain(|holder| !Arc::ptr_eq(&holder.identity, identity));
            if holders.is_empty() {
                state.reservations.remove(session_id);
            }
            self.publish_busy(&state);
        }
    }

    /// Claims the session for `kind` with an identity-bearing guard whose
    /// cancellation flag the executor watches, or a labelled refusal when work that `kind`
    /// cannot run beside or a reservation already holds it, or a daemon
    /// upgrade is waiting.
    ///
    /// The work does not hold up a daemon upgrade: it is cancelled when the
    /// daemon exits, and the next daemon starts it again. Starting it while a
    /// handoff waits would only waste it.
    pub fn try_start(
        self: &Arc<Self>,
        session_id: &str,
        kind: BackgroundWork,
    ) -> Result<RecoveryAttempt, Refusal> {
        if crate::upgrade::is_draining() {
            return Err(Refusal {
                holders: vec!["daemon handoff"],
            });
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A review reservation is compatible with the foreground operation.
        // It cannot block the completion work that foreground explicitly yields
        // to during its drain; other background exclusions remain unchanged.
        let delegation_drain = kind == BackgroundWork::DelegationDelivery
            && state
                .reservations
                .get(session_id)
                .into_iter()
                .flatten()
                .any(|holder| holder.kind == ReservationKind::ForegroundDelegationDrain);
        if state.closed
            || (kind.needs_lease() && state.leases.contains_key(session_id))
            || state
                .reservations
                .get(session_id)
                .into_iter()
                .flatten()
                .any(|holder| !delegation_drain || holder.kind == ReservationKind::Foreground)
            || state
                .busy
                .get(session_id)
                .is_some_and(|holders| holders.iter().any(|holder| !kind.runs_beside(holder.work)))
        {
            let mut holders = state
                .busy
                .get(session_id)
                .into_iter()
                .flatten()
                .map(|holder| holder.work.label())
                .collect::<Vec<_>>();
            holders.extend(
                state
                    .reservations
                    .get(session_id)
                    .into_iter()
                    .flatten()
                    .map(|holder| holder.label),
            );
            if state.closed {
                holders.push("daemon shutdown");
            }
            return Err(Refusal { holders });
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        state
            .busy
            .entry(session_id.to_owned())
            .or_default()
            .push(BackgroundHolder {
                work: kind,
                cancelled: cancelled.clone(),
            });
        self.publish_busy(&state);
        Ok(RecoveryAttempt(Arc::new(RecoveryAdmission {
            gate: self.clone(),
            session_id: session_id.to_owned(),
            cancelled,
        })))
    }

    fn finish(&self, session_id: &str, identity: &Arc<AtomicBool>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(holders) = state.busy.get_mut(session_id) else {
            return;
        };
        let before = holders.len();
        holders.retain(|holder| !Arc::ptr_eq(&holder.cancelled, identity));
        if holders.len() == before {
            return;
        }
        if holders.is_empty() {
            state.busy.remove(session_id);
        }
        self.publish_busy(&state);
    }

    fn publish_busy(&self, state: &RecoveryGateState) {
        self.busy.send_replace(state.busy.keys().cloned().collect());
    }

    pub async fn closed(&self) {
        loop {
            let notified = self.closed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .closed
            {
                return;
            }
            notified.await;
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<BTreeSet<String>> {
        self.busy.subscribe()
    }

    pub fn is_busy(&self, session_id: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .busy
            .contains_key(session_id)
    }

    /// Asks all in-flight work for this session, if any, to stop.
    pub fn cancel_busy(&self, session_id: &str) {
        if let Some(holders) = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .busy
            .get(session_id)
        {
            for holder in holders {
                holder.cancelled.store(true, Ordering::Release);
            }
        }
    }

    /// Close background admission and cancel every admitted attempt in one transition.
    /// Existing guards retain ownership until their executors have settled.
    pub fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        for holder in state.busy.values().flatten() {
            holder.cancelled.store(true, Ordering::Release);
        }
        self.publish_busy(&state);
        self.closed.notify_waiters();
    }

    pub fn busy_sessions(&self) -> BTreeSet<String> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .busy
            .keys()
            .cloned()
            .collect()
    }
}

/// The cancel flag also identifies this particular admission. Only its owner
/// can release the slot, including when an executor or coordinator unwinds.
#[derive(Clone)]
pub struct RecoveryAttempt(Arc<RecoveryAdmission>);

struct RecoveryAdmission {
    gate: Arc<RecoveryGate>,
    session_id: String,
    cancelled: Arc<AtomicBool>,
}

impl RecoveryAttempt {
    pub fn cancellation(&self) -> Arc<AtomicBool> {
        self.0.cancelled.clone()
    }

    /// Both the waiter and blocking executor own admission. Aborting the waiter
    /// cannot release a running executor; a panicking executor cannot release
    /// its slot before the supervisor applies the failure outcome.
    pub(crate) async fn run_blocking<T: Send + 'static>(
        self,
        work: impl FnOnce(RecoveryAttempt) -> T + Send + 'static,
    ) -> (Result<T, tokio::task::JoinError>, Self) {
        let executing = self.clone();
        let result = tokio::task::spawn_blocking(move || work(executing)).await;
        (result, self)
    }
}

impl std::ops::Deref for RecoveryAttempt {
    type Target = AtomicBool;
    fn deref(&self) -> &AtomicBool {
        &self.0.cancelled
    }
}

impl Drop for RecoveryAdmission {
    fn drop(&mut self) {
        self.gate.finish(&self.session_id, &self.cancelled);
    }
}

/// An admitted operation, explicitly carried across executor boundaries.
#[derive(Clone)]
pub enum SessionAdmission {
    Foreground(ForegroundAdmission),
    Background(RecoveryAttempt),
}

pub trait HoldsSession: Send + Sync {
    fn admission(&self) -> SessionAdmission;
}
impl HoldsSession for ForegroundAdmission {
    fn admission(&self) -> SessionAdmission {
        SessionAdmission::Foreground(self.clone())
    }
}
impl HoldsSession for RecoveryAttempt {
    fn admission(&self) -> SessionAdmission {
        SessionAdmission::Background(self.clone())
    }
}
impl HoldsSession for SessionAdmission {
    fn admission(&self) -> SessionAdmission {
        self.clone()
    }
}
impl SessionAdmission {
    fn gate_and_session(&self) -> (&Arc<RecoveryGate>, &str) {
        match self {
            Self::Foreground(a) => (&a.0.gate, &a.0.session_id),
            Self::Background(a) => (&a.0.gate, &a.0.session_id),
        }
    }
    pub(crate) fn drain_delegation(&self) -> anyhow::Result<ForegroundDelegationDrain> {
        let Self::Foreground(admission) = self else {
            anyhow::bail!("delegation drain requires foreground ownership");
        };
        let mut state = admission
            .0
            .gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let holder = state
            .reservations
            .get_mut(&admission.0.session_id)
            .into_iter()
            .flatten()
            .find(|holder| Arc::ptr_eq(&holder.identity, &admission.0.identity))
            .ok_or_else(|| anyhow::anyhow!("foreground admission has ended"))?;
        anyhow::ensure!(
            holder.kind == ReservationKind::Foreground,
            "foreground is already draining delegation"
        );
        holder.kind = ReservationKind::ForegroundDelegationDrain;
        admission.0.gate.publish_busy(&state);
        Ok(ForegroundDelegationDrain(admission.clone()))
    }

    pub fn narrow_copy(&self) -> anyhow::Result<()> {
        if let Self::Background(a) = self {
            a.narrow(BackgroundWork::RecoveryCopyTransferring)?;
        }
        Ok(())
    }
    pub async fn abandon_copy(&self) -> anyhow::Result<()> {
        if let Self::Background(a) = self {
            // A failure before the latch still owns its capture capability.
            let capturing = {
                let state =
                    a.0.gate
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                state
                    .busy
                    .get(&a.0.session_id)
                    .into_iter()
                    .flatten()
                    .any(|holder| {
                        Arc::ptr_eq(&holder.cancelled, &a.0.cancelled)
                            && holder.work == BackgroundWork::RecoveryCopyCapturing
                    })
            };
            if !capturing {
                a.abandon().await?;
            }
        }
        Ok(())
    }
    pub(crate) async fn lease(&self, session_id: &str) -> anyhow::Result<RelayLeaseAdmission> {
        let (gate, admitted_session) = self.gate_and_session();
        anyhow::ensure!(
            session_id == admitted_session,
            "relay lease admission belongs to another session"
        );
        let mut changed = gate.subscribe();
        loop {
            {
                let mut state = gate
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let label = match self {
                    Self::Foreground(a) => {
                        let holder = state
                            .reservations
                            .get(session_id)
                            .into_iter()
                            .flatten()
                            .find(|holder| Arc::ptr_eq(&holder.identity, &a.0.identity))
                            .ok_or_else(|| anyhow::anyhow!("foreground admission has ended"))?;
                        anyhow::ensure!(
                            holder.kind == ReservationKind::Foreground,
                            "foreground relay ownership is yielded to delegation drain"
                        );
                        a.0.label
                    }
                    Self::Background(a) => {
                        let holder = state
                            .busy
                            .get(session_id)
                            .into_iter()
                            .flatten()
                            .find(|h| Arc::ptr_eq(&h.cancelled, &a.0.cancelled))
                            .ok_or_else(|| anyhow::anyhow!("relay lease admission has ended"))?;
                        anyhow::ensure!(
                            holder.work == BackgroundWork::RecoveryCopyAbandoning
                                || !holder.cancelled.load(Ordering::Acquire),
                            "{} was cancelled before taking its relay lease",
                            holder.work.label()
                        );
                        anyhow::ensure!(
                            holder.work.needs_lease(),
                            "{} cannot take a relay lease",
                            holder.work.label()
                        );
                        holder.work.label()
                    }
                };
                if !state.leases.contains_key(session_id)
                    && (!matches!(self, Self::Foreground(_))
                        || !state.busy.contains_key(session_id))
                {
                    state.leases.insert(session_id.to_owned(), label);
                    return Ok(RelayLeaseAdmission {
                        admission: self.clone(),
                        label,
                    });
                }
            }
            changed.changed().await?;
        }
    }
}

/// A temporary phase of the same foreground identity. Closing it refuses new
/// delivery and preempts admitted delivery; source reacquisition waits for its
/// holders and leases to return. Drop also closes it on cancellation or unwind.
pub(crate) struct ForegroundDelegationDrain(ForegroundAdmission);
impl Drop for ForegroundDelegationDrain {
    fn drop(&mut self) {
        let admission = &self.0.0;
        let mut state = admission
            .gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(holder) = state
            .reservations
            .get_mut(&admission.session_id)
            .into_iter()
            .flatten()
            .find(|holder| Arc::ptr_eq(&holder.identity, &admission.identity))
        {
            holder.kind = ReservationKind::Foreground;
        }
        for holder in state.busy.get(&admission.session_id).into_iter().flatten() {
            holder.cancelled.store(true, Ordering::Release);
        }
        admission.gate.publish_busy(&state);
    }
}

/// Retained by the actor until it has applied the lease return. Admission
/// cannot end or wake deferred credential work before the relay is available.
pub struct RelayLeaseAdmission {
    admission: SessionAdmission,
    pub(crate) label: &'static str,
}
impl RelayLeaseAdmission {
    pub(crate) fn defer_credentials(&self) {
        let (gate, session) = self.admission.gate_and_session();
        gate.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .deferred_credentials
            .insert(session.to_owned());
    }
}
impl Drop for RelayLeaseAdmission {
    fn drop(&mut self) {
        let (gate, session) = self.admission.gate_and_session();
        let mut state = gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.leases.remove(session);
        if state.deferred_credentials.remove(session) {
            gate.credential_wakes.send_modify(|sessions| {
                sessions.insert(session.to_owned());
            });
        }
        gate.publish_busy(&state);
    }
}
impl RecoveryGate {
    #[cfg(test)]
    pub(crate) fn credentials_deferred_for_test(&self, session_id: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .deferred_credentials
            .contains(session_id)
    }

    pub(crate) fn credential_wakes(&self) -> watch::Receiver<BTreeSet<String>> {
        let mut wakes = self.credential_wakes.subscribe();
        wakes.mark_changed();
        wakes
    }
    pub(crate) fn take_credential_wakes(&self) -> BTreeSet<String> {
        let mut ready = BTreeSet::new();
        self.credential_wakes.send_if_modified(|sessions| {
            ready = std::mem::take(sessions);
            false
        });
        ready
    }
}
impl RecoveryAttempt {
    pub fn narrow(&self, kind: BackgroundWork) -> anyhow::Result<()> {
        let mut state = self
            .0
            .gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let holder = state
            .busy
            .get_mut(&self.0.session_id)
            .into_iter()
            .flatten()
            .find(|h| Arc::ptr_eq(&h.cancelled, &self.0.cancelled))
            .ok_or_else(|| anyhow::anyhow!("copy admission has ended"))?;
        anyhow::ensure!(
            holder.work == BackgroundWork::RecoveryCopyCapturing
                && kind == BackgroundWork::RecoveryCopyTransferring,
            "invalid recovery copy transition"
        );
        holder.work = kind;
        self.0.gate.publish_busy(&state);
        Ok(())
    }
    pub async fn abandon(&self) -> anyhow::Result<()> {
        let mut changed = self.0.gate.subscribe();
        loop {
            {
                let mut state = self
                    .0
                    .gate
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let holders = state
                    .busy
                    .get_mut(&self.0.session_id)
                    .ok_or_else(|| anyhow::anyhow!("copy admission has ended"))?;
                let own = holders
                    .iter()
                    .position(|h| Arc::ptr_eq(&h.cancelled, &self.0.cancelled))
                    .ok_or_else(|| anyhow::anyhow!("copy admission has ended"))?;
                anyhow::ensure!(
                    matches!(
                        holders[own].work,
                        BackgroundWork::RecoveryCopyTransferring
                            | BackgroundWork::RecoveryCopyAbandoning
                    ),
                    "only a transferring copy can abandon"
                );
                // Publish the stronger kind before waiting, so another delivery
                // cannot overtake cleanup while the existing delivery stops.
                holders[own].work = BackgroundWork::RecoveryCopyAbandoning;
                let mut delivery = false;
                for h in holders.iter() {
                    if h.work == BackgroundWork::DelegationDelivery {
                        h.cancelled.store(true, Ordering::Release);
                        delivery = true;
                    }
                }
                if !delivery && !state.leases.contains_key(&self.0.session_id) {
                    self.0.gate.publish_busy(&state);
                    return Ok(());
                }
            }
            changed.changed().await?;
        }
    }
}

/// Per-session latest-state mailbox shared by the two background policies.
/// A capacity-one wake channel never queues an observation history.
#[derive(Clone)]
pub(crate) struct ObservationSender<T> {
    pending: Arc<Mutex<PendingObservations<T>>>,
    wake: mpsc::Sender<()>,
}

pub(crate) struct ObservationReceiver<T> {
    pending: Arc<Mutex<PendingObservations<T>>>,
    wake: mpsc::Receiver<()>,
}

struct PendingObservations<T> {
    values: BTreeMap<String, T>,
    ready: VecDeque<String>,
}

pub(crate) fn observation_channel<T>() -> (ObservationSender<T>, ObservationReceiver<T>) {
    let pending = Arc::new(Mutex::new(PendingObservations {
        values: BTreeMap::new(),
        ready: VecDeque::new(),
    }));
    let (tx, rx) = mpsc::channel(1);
    (
        ObservationSender {
            pending: pending.clone(),
            wake: tx,
        },
        ObservationReceiver { pending, wake: rx },
    )
}

impl<T> ObservationSender<T> {
    pub(crate) fn send(
        &self,
        session_id: String,
        mut observation: T,
        merge: impl FnOnce(&T, &mut T),
    ) {
        if self.wake.is_closed() {
            return;
        }
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(previous) = pending.values.get(&session_id) {
            merge(previous, &mut observation);
        } else {
            pending.ready.push_back(session_id.clone());
        }
        pending.values.insert(session_id, observation);
        // Full means a wake is already pending. Closure discards disposable observations.
        let _ = self.wake.try_send(());
    }
}

impl<T> ObservationReceiver<T> {
    pub(crate) fn try_recv(&mut self) -> Option<T> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let session = pending.ready.pop_front()?;
        pending.values.remove(&session)
    }

    pub(crate) async fn recv(&mut self) -> Option<T> {
        tokio::task::yield_now().await;
        loop {
            if let Some(observation) = self.try_recv() {
                return Some(observation);
            }
            self.wake.recv().await?;
        }
    }
}

impl RecoveryObserver {
    /// Queues one observation for the coordinator. Returns as soon as the
    /// observation is queued; a stopped coordinator makes this a no-op.
    pub fn observe(&self, observation: RecoveryObservation) {
        let pending = PendingRecoveryObservation {
            observed_wait: observation.checkpoint_wait.is_some(),
            observation,
        };
        self.observations.send(
            pending.observation.session.id.clone(),
            pending,
            |previous, next| {
                next.observed_wait |= previous.observed_wait;
                next.observation.latest_completed_turn_ordinal = next
                    .observation
                    .latest_completed_turn_ordinal
                    .max(previous.observation.latest_completed_turn_ordinal);
            },
        );
    }

    pub fn is_busy(&self, session_id: &str) -> bool {
        self.gate.is_busy(session_id)
    }

    /// Holds off any recovery copy for this session until the returned
    /// reservation is dropped. This, not the observation queue, is what a
    /// lifecycle operation relies on: queued observations may still be
    /// unread, and the coordinator refuses to start a copy for a reserved
    /// session whenever it reads them.
    pub fn reserve(&self, session_id: &str) -> RecoveryReservation {
        self.gate.reserve(session_id)
    }

    /// Asks an in-flight recovery copy for this session to stop. A foreground
    /// lifecycle operation calls this after reserving so it preempts the copy
    /// instead of waiting behind it.
    pub fn cancel_busy(&self, session_id: &str) {
        self.gate.cancel_busy(session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lifecycle_reservation_preempts_background_work_before_releasing_admission() {
        let gate = Arc::new(RecoveryGate::default());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn({
            let gate = gate.clone();
            async move {
                gate.run_background("child", BackgroundWork::CredentialSync, async {
                    started_tx.send(()).unwrap();
                    std::future::pending::<()>().await;
                })
                .await
            }
        });
        started_rx.await.unwrap();
        let reservation = gate.reserve("child");
        assert!(gate.is_busy("child"));
        gate.cancel_busy("child");
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
        assert!(!gate.is_busy("child"));
        assert!(
            gate.run_background("child", BackgroundWork::CredentialSync, async {
                panic!("reserved worker accessed")
            })
            .await
            .is_none()
        );
        assert_eq!(
            gate.run_background("other", BackgroundWork::CredentialSync, async { 7 })
                .await,
            Some(7)
        );
        drop(reservation);
        assert_eq!(
            gate.run_background("child", BackgroundWork::CredentialSync, async { 9 })
                .await,
            Some(9)
        );
    }

    /// With a copy and a sync holding one session, a lifecycle operation must
    /// stop both and still find the session busy until the last one is gone.
    #[test]
    fn lifecycle_reservation_waits_for_every_holder_it_preempts() {
        let gate = Arc::new(RecoveryGate::default());
        let copy = gate
            .try_start("session", BackgroundWork::RecoveryCopyCapturing)
            .unwrap();
        let sync = gate
            .try_start("session", BackgroundWork::CredentialSync)
            .unwrap();
        let reservation = gate.reserve("session");
        gate.cancel_busy("session");
        assert!(copy.load(Ordering::Acquire) && sync.load(Ordering::Acquire));
        drop(copy);
        assert!(gate.is_busy("session"), "the running sync was forgotten");
        assert!(
            gate.try_start("session", BackgroundWork::CredentialSync)
                .is_err()
        );
        drop(sync);
        assert!(gate.busy_sessions().is_empty());
        drop(reservation);
    }

    #[tokio::test]
    async fn aborting_background_task_releases_worker_admission() {
        let gate = Arc::new(RecoveryGate::default());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn({
            let gate = gate.clone();
            async move {
                gate.run_background("child", BackgroundWork::CredentialSync, async {
                    started_tx.send(()).unwrap();
                    std::future::pending::<()>().await;
                })
                .await
            }
        });
        started_rx.await.unwrap();
        assert!(
            gate.run_background("child", BackgroundWork::CredentialSync, async { 1 })
                .await
                .is_none()
        );
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(
            gate.run_background("child", BackgroundWork::CredentialSync, async { 2 })
                .await,
            Some(2)
        );
    }
    #[test]
    fn closing_gate_cancels_admitted_work_and_refuses_late_admission() {
        for _ in 0..100 {
            let gate = Arc::new(RecoveryGate::default());
            let worker_gate = gate.clone();
            let worker = std::thread::spawn(move || {
                worker_gate.try_start("session", BackgroundWork::RecoveryCopyCapturing)
            });
            gate.close();
            if let Ok(attempt) = worker.join().unwrap() {
                assert!(attempt.load(Ordering::Acquire));
                assert!(gate.is_busy("session"));
                drop(attempt);
            }
            assert!(
                gate.try_start("late", BackgroundWork::RecoveryCopyCapturing)
                    .is_err()
            );
            assert!(gate.busy_sessions().is_empty());
        }
    }

    #[test]
    fn old_attempt_identity_cannot_release_a_replacement() {
        let gate = Arc::new(RecoveryGate::default());
        let old = gate
            .try_start("session", BackgroundWork::RecoveryCopyCapturing)
            .unwrap();
        let identity = old.cancellation();
        drop(old);
        let replacement = gate
            .try_start("session", BackgroundWork::RecoveryCopyCapturing)
            .unwrap();
        gate.finish("session", &identity);
        assert!(gate.is_busy("session"));
        drop(replacement);
        assert!(!gate.is_busy("session"));
    }

    #[test]
    fn busy_publication_tracks_concurrent_session_transitions() {
        let gate = Arc::new(RecoveryGate::default());
        let view = gate.subscribe();
        std::thread::scope(|scope| {
            for session in 0..8 {
                let gate = gate.clone();
                scope.spawn(move || {
                    for _ in 0..1000 {
                        drop(
                            gate.try_start(
                                &session.to_string(),
                                BackgroundWork::RecoveryCopyCapturing,
                            )
                            .unwrap(),
                        );
                    }
                });
            }
        });
        assert_eq!(*view.borrow(), gate.busy_sessions());
        assert!(view.borrow().is_empty());
    }

    #[tokio::test]
    async fn aborting_a_blocking_waiter_keeps_admission_until_executor_settles() {
        let gate = Arc::new(RecoveryGate::default());
        let attempt = gate
            .try_start("session", BackgroundWork::RecoveryCopyCapturing)
            .unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let waiter = tokio::spawn(attempt.run_blocking(move |_| {
            started_tx.send(()).unwrap();
            finish_rx.recv().unwrap();
        }));
        started_rx.await.unwrap();
        waiter.abort();
        assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
        assert!(gate.is_busy("session"));
        gate.close();
        finish_tx.send(()).unwrap();
        let mut busy = gate.subscribe();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            busy.wait_for(|sessions| sessions.is_empty()),
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[tokio::test]
    async fn panicking_executor_retains_admission_until_failure_is_settled() {
        let gate = Arc::new(RecoveryGate::default());
        let attempt = gate
            .try_start("session", BackgroundWork::RecoveryCopyCapturing)
            .unwrap();
        let (result, settlement) = attempt
            .run_blocking::<()>(|_| panic!("executor failed"))
            .await;
        assert!(result.unwrap_err().is_panic());
        assert!(
            gate.try_start("session", BackgroundWork::RecoveryCopyCapturing)
                .is_err()
        );
        drop(settlement);
        assert!(
            gate.try_start("session", BackgroundWork::RecoveryCopyCapturing)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn coalesced_observations_keep_independent_sessions_fair() {
        let (sender, mut receiver) = observation_channel();
        for value in 0..100_000 {
            sender.send("a".into(), value, |_, _| {});
        }
        sender.send("b".into(), 7, |_, _| {});
        assert_eq!(receiver.recv().await, Some(99_999));
        sender.send("a".into(), 100_000, |_, _| {});
        assert_eq!(receiver.recv().await, Some(7));
        assert_eq!(receiver.recv().await, Some(100_000));
        assert!(receiver.try_recv().is_none());
        drop(sender);
        assert_eq!(receiver.recv().await, None);
    }

    /// A review waits for a recovery copy already running, then holds the
    /// session so no copy or worker upgrade can start until it is done.
    // Hard-won: 352a87453: a review hold blocked foreground work and daemon handoff.
    // Hard-won: 373784672: a waiting review blocked credential sync beside a recovery copy.
    #[tokio::test]
    async fn reserving_when_idle_waits_for_running_work_then_blocks_new_work() {
        let gate = Arc::new(RecoveryGate::default());
        let copy = gate
            .try_start("session-1", BackgroundWork::RecoveryCopyCapturing)
            .expect("the slot starts free");
        let mut waiter = Box::pin(gate.reserve_when_idle("session-1"));
        assert!(futures::poll!(&mut waiter).is_pending());
        let sync = gate
            .try_start("session-1", BackgroundWork::CredentialSync)
            .expect("a waiting review must allow credentials through beside the running copy");
        assert!(!copy.cancellation().load(Ordering::Acquire));
        drop(copy);
        assert!(futures::poll!(&mut waiter).is_pending());
        assert!(!sync.cancellation().load(Ordering::Acquire));
        drop(sync);
        let reservation = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("the reservation follows all background holders");
        assert!(
            gate.try_start("session-1", BackgroundWork::CredentialSync)
                .is_err(),
            "an admitted review excludes new background work"
        );
        assert!(
            gate.try_start("session-1", BackgroundWork::RecoveryCopyCapturing)
                .is_err(),
            "no copy or upgrade starts while the review holds the session"
        );
        assert!(
            gate.try_start("session-2", BackgroundWork::RecoveryCopyCapturing)
                .is_ok(),
            "other sessions are free"
        );
        let foreground = gate
            .enter_foreground(
                "session-1",
                "Move",
                &AtomicBool::new(false),
                &mj_core::targets::ProcessExecutor,
            )
            .await
            .unwrap();
        drop(reservation);
        let refusal = gate
            .try_start("session-1", BackgroundWork::RecoveryCopyCapturing)
            .err()
            .unwrap();
        assert_eq!(
            refusal.holders,
            vec!["Move"],
            "dropping the review must retain the foreground holder's identity"
        );
        drop(foreground);
        assert!(
            gate.try_start("session-1", BackgroundWork::RecoveryCopyCapturing)
                .is_ok()
        );
    }

    #[derive(Default)]
    struct Notices(Mutex<Vec<String>>);
    impl mj_core::targets::CommandExecutor for Notices {
        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }
        fn execute_with_stdin_without_deadline(
            &self,
            _: &mj_core::targets::CommandSpec,
            _: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            panic!("admission must not execute a process");
        }
        fn cancellation_requested_without_deadline(&self) -> bool {
            false
        }
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }
        fn execute(
            &self,
            _: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            panic!("admission must not execute a process");
        }
        fn notify_notice(&self, notice: &str) {
            self.0.lock().unwrap().push(notice.to_owned());
        }
    }

    #[tokio::test]
    async fn foreground_admission_names_and_waits_for_all_holders_and_releases_cancelled_intents() {
        let gate = Arc::new(RecoveryGate::default());
        let notices = Notices::default();
        let copy = gate
            .try_start("session", BackgroundWork::RecoveryCopyCapturing)
            .unwrap();
        let sync = gate
            .try_start("session", BackgroundWork::CredentialSync)
            .unwrap();
        let cancelled = AtomicBool::new(true);
        assert!(
            gate.enter_foreground("session", "Move", &cancelled, &notices)
                .await
                .is_err()
        );
        assert!(!copy.load(Ordering::Acquire));
        assert!(!sync.load(Ordering::Acquire));
        cancelled.store(false, Ordering::Release);
        let mut waiting = Box::pin(gate.enter_foreground("session", "Move", &cancelled, &notices));
        assert!(futures::poll!(&mut waiting).is_pending());
        let refusal = gate
            .try_start("session", BackgroundWork::WorkerReplacement)
            .err()
            .unwrap();
        assert_eq!(
            notices.0.lock().unwrap().as_slice(),
            &["Waiting for recovery copy and credential sync to stop"]
        );
        assert_eq!(
            refusal.holders,
            vec!["recovery copy", "credential sync", "Move"]
        );
        assert!(copy.load(Ordering::Acquire));
        assert!(sync.load(Ordering::Acquire));
        drop(copy);
        assert!(futures::poll!(&mut waiting).is_pending());
        drop(sync);
        let admission = waiting.await.unwrap();
        let mut second = Box::pin(gate.enter_foreground("session", "close", &cancelled, &notices));
        assert!(futures::poll!(&mut second).is_pending());
        drop(admission);
        let second = second.await.unwrap();
        assert!(
            gate.try_start("session", BackgroundWork::WorkerReplacement)
                .is_err()
        );
        drop(second);
        let copy = gate
            .try_start("session", BackgroundWork::RecoveryCopyCapturing)
            .unwrap();
        let mut waiting = Box::pin(gate.enter_foreground("session", "Move", &cancelled, &notices));
        assert!(futures::poll!(&mut waiting).is_pending());
        cancelled.store(true, Ordering::Release);
        assert!(waiting.await.is_err());
        drop(copy);
        assert!(
            gate.try_start("session", BackgroundWork::WorkerReplacement)
                .is_ok()
        );
    }
}

#[cfg(test)]
mod copy_transition_tests {
    use super::*;

    #[tokio::test]
    async fn a_transferring_copy_admits_delegation_and_abandonment_preempts_it() {
        let gate = Arc::new(RecoveryGate::default());
        let copy = gate
            .try_start("session", BackgroundWork::RecoveryCopyCapturing)
            .unwrap();
        assert!(
            gate.try_start("session", BackgroundWork::DelegationDelivery)
                .is_err()
        );
        let sync = gate
            .try_start("session", BackgroundWork::CredentialSync)
            .unwrap();
        drop(sync);
        copy.narrow(BackgroundWork::RecoveryCopyTransferring)
            .unwrap();
        assert!(copy.narrow(BackgroundWork::RecoveryCopyCapturing).is_err());
        let sync = gate
            .try_start("session", BackgroundWork::CredentialSync)
            .unwrap();
        drop(sync);
        let delivery = gate
            .try_start("session", BackgroundWork::DelegationDelivery)
            .unwrap();
        let delivery_lease = delivery.admission().lease("session").await.unwrap();
        let mut abandoning = Box::pin(copy.abandon());
        assert!(futures::poll!(&mut abandoning).is_pending());
        assert!(delivery.load(Ordering::Acquire));
        let cancelled = AtomicBool::new(false);
        let mut foreground = Box::pin(gate.enter_foreground(
            "session",
            "Move",
            &cancelled,
            &mj_core::targets::ProcessExecutor,
        ));
        assert!(futures::poll!(&mut foreground).is_pending());
        drop(delivery);
        assert!(futures::poll!(&mut abandoning).is_pending());
        drop(delivery_lease);
        tokio::time::timeout(std::time::Duration::from_secs(1), abandoning)
            .await
            .unwrap()
            .unwrap();
        let cleanup = copy.admission().lease("session").await.unwrap();
        assert!(futures::poll!(&mut foreground).is_pending());
        drop(cleanup);
        drop(copy);
        let foreground = foreground.await.unwrap();
        drop(foreground);
        // Without a pending foreground, cleanup still permits credentials.
        let copy = gate
            .try_start("session", BackgroundWork::RecoveryCopyCapturing)
            .unwrap();
        copy.narrow(BackgroundWork::RecoveryCopyTransferring)
            .unwrap();
        copy.abandon().await.unwrap();
        assert!(
            gate.try_start("session", BackgroundWork::CredentialSync)
                .is_ok()
        );
        assert!(
            gate.try_start("session", BackgroundWork::DelegationDelivery)
                .is_err()
        );
    }
}
