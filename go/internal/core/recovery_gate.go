// Port of mj-core/src/recovery_gate.rs.

// Per-session coordination between foreground operations and background
// recovery.
//
// Rust's Arc-and-Drop ownership becomes explicit handles: every admission,
// reservation, drain and lease is released with Release (or Close), and
// Clone hands out another handle on the same admission, which ends when its
// last handle is released. Dropping a Rust future becomes cancelling the
// context.Context the Go call was given.

package core

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

// recoveryCancellationPoll is how often a wait checks an admission's
// cancellation flag; the flag is a plain atomic its setters do not signal.
// The same interval as Rust's.
const recoveryCancellationPoll = 25 * time.Millisecond

// WaitUntilCancelled returns once an admission's cancellation flag is set,
// or with ctx's error. Admitted work that cannot be cancelled by being
// abandoned polls this instead.
func WaitUntilCancelled(ctx context.Context, flag *atomic.Bool) error {
	ticker := time.NewTicker(recoveryCancellationPoll)
	defer ticker.Stop()
	for !flag.Load() {
		select {
		case <-ticker.C:
		case <-ctx.Done():
			return ctx.Err()
		}
	}
	return nil
}

// RecoveryReservation is a per-session reservation held by a foreground
// lifecycle operation. The gate cannot admit conflicting background work
// until it is released.
type RecoveryReservation struct {
	admission *ForegroundAdmission
}

// Release ends the reservation.
func (r *RecoveryReservation) Release() {
	if r != nil {
		r.admission.Release()
	}
}

type LifecycleOrigin int

const (
	LifecycleOriginPerson LifecycleOrigin = iota
	LifecycleOriginAutomatic
)

// ForegroundAdmission is a foreground lifecycle's hold on a session. Clones
// share it; it ends when the last handle is released.
type ForegroundAdmission struct {
	holder   *foregroundHolder
	released atomic.Bool
}

// foregroundHolder is the shared admission; its address is the reservation's
// identity (Rust's Arc<()>).
type foregroundHolder struct {
	label     string
	sessionID string
	gate      *RecoveryGate
	handles   atomic.Int64
}

func (a *ForegroundAdmission) Clone() *ForegroundAdmission {
	a.holder.handles.Add(1)
	return &ForegroundAdmission{holder: a.holder}
}

// Release gives this handle up; releasing it twice does nothing.
func (a *ForegroundAdmission) Release() {
	if a == nil || !a.released.CompareAndSwap(false, true) {
		return
	}
	if a.holder.handles.Add(-1) == 0 {
		a.holder.gate.release(a.holder.sessionID, a.holder)
	}
}

// Admission is HoldsSession: a new handle on this admission.
func (a *ForegroundAdmission) Admission() SessionAdmission {
	return SessionAdmission{foreground: a.Clone()}
}

// BackgroundWork is what a background admission is for. Background
// operations exclude each other and every connection lease; foreground
// lifecycle operations preempt each background holder.
type BackgroundWork int

const (
	// BackgroundWorkWorkerReplacement stops and replaces the worker, so it
	// shares the session with nothing.
	BackgroundWorkWorkerReplacement BackgroundWork = iota
	BackgroundWorkRelayRecovery
	BackgroundWorkWorkerObservation
	BackgroundWorkDelegationDelivery
	BackgroundWorkQuotaScheduling
)

// Label is the operation named in admission wait notices.
func (w BackgroundWork) Label() string {
	switch w {
	case BackgroundWorkWorkerReplacement:
		return "worker upgrade"
	case BackgroundWorkRelayRecovery:
		return "relay recovery"
	case BackgroundWorkWorkerObservation:
		return "worker readiness observation"
	case BackgroundWorkDelegationDelivery:
		return "delegation delivery"
	case BackgroundWorkQuotaScheduling:
		return "quota scheduling"
	}
	return fmt.Sprintf("background work %d", int(w))
}

// RecoveryRefusal (Rust's recovery_gate::Refusal) says why an admission was
// refused and who holds the session.
//
// Admission for the background work that acts on a session's live worker is
// shared rather than per-coordinator so that one place decides which kinds
// of work may run together on a session, and a foreground lifecycle
// operation preempts all of them.
type RecoveryRefusal struct {
	Cause   RefusalCause
	Holders []string
	// WorkerOwned: a holder that starts or replaces this session's worker (a
	// foreground lifecycle or a worker replacement) is among the holders, so
	// that holder observes the worker's boot and reports its failure.
	WorkerOwned bool
}

type RefusalCause int

const (
	RefusalCauseClosed RefusalCause = iota
	RefusalCauseDaemonDraining
	RefusalCauseSessionHeld
)

func daemonDrainingRefusal() *RecoveryRefusal {
	return &RecoveryRefusal{Cause: RefusalCauseDaemonDraining, Holders: []string{"daemon handoff"}}
}

// RecoveryGate is the per-session admission. Build one with NewRecoveryGate.
type RecoveryGate struct {
	mu    sync.Mutex
	state recoveryGateState
	// closed is closed by Close.
	closed    chan struct{}
	closeOnce sync.Once
	// busy is which sessions are busy, for waiters. Published from inside
	// the gate so every holder updates it, whatever started the work.
	busy *Watch[recoveryBusy]
}

// recoveryBusy is one publication of the busy sessions; version counts
// publications so a receiver can tell it has missed one.
type recoveryBusy struct {
	version  uint64
	sessions []string
}

func NewRecoveryGate() *RecoveryGate {
	return &RecoveryGate{
		state: recoveryGateState{
			busy:         map[string][]backgroundHolder{},
			reservations: map[string][]*reservation{},
			leases:       map[string]string{},
		},
		closed: make(chan struct{}),
		busy:   NewWatch(recoveryBusy{sessions: []string{}}),
	}
}

type recoveryGateState struct {
	closed bool
	// busy is the in-flight work per session, each holder with the cancel
	// flag its executor watches, so a foreground lifecycle operation can
	// preempt it instead of waiting. A session's entry is never empty.
	busy         map[string][]backgroundHolder
	reservations map[string][]*reservation
	leases       map[string]string
	version      uint64
}

// reservationKind: reviews exclude automatic work except accepted replies,
// but cannot hold up a person's lifecycle or daemon handoff. Their worker
// work survives it.
type reservationKind int

const (
	reservationForeground reservationKind = iota
	reservationForegroundDelegationDrain
	reservationReview
)

func (k reservationKind) runsBeside(other reservationKind) bool {
	return k == reservationReview || other == reservationReview
}

type reservation struct {
	identity *foregroundHolder
	label    string
	kind     reservationKind
}

type backgroundHolder struct {
	work BackgroundWork
	// cancelled is also the holder's identity.
	cancelled *atomic.Bool
}

// recoveryNotices is what a foreground admission asks of its executor while
// it waits: whether to give up, and where to say what it is waiting for.
// Every CommandExecutor provides both.
type recoveryNotices interface {
	CancellationRequested() bool
	NotifyNotice(notice string)
}

// RecoveryRunBackground runs disposable background work under the session's
// background admission. Lifecycle reservations preempt it before they touch
// the worker; work must return once its context is cancelled. Cancelling ctx
// also releases admission. false when the work was refused or preempted.
func RecoveryRunBackground[T any](g *RecoveryGate, ctx context.Context, sessionID string, kind BackgroundWork, work func(ctx context.Context) T) (T, bool) {
	return RecoveryRunBackgroundOwned(g, ctx, sessionID, kind, func(ctx context.Context, attempt *RecoveryAttempt) T {
		attempt.Release()
		return work(ctx)
	})
}

// RecoveryRunBackgroundOwned is RecoveryRunBackground for work that outlives
// the call awaiting it, such as a job queued on a session actor: work
// receives a handle on the admission and may hand it to that job, which
// then holds the slot until it has finished and released it, whether or not
// this call is still waiting. work must release the handle it is given.
func RecoveryRunBackgroundOwned[T any](g *RecoveryGate, ctx context.Context, sessionID string, kind BackgroundWork, work func(ctx context.Context, attempt *RecoveryAttempt) T) (T, bool) {
	var zero T
	admission, refusal := g.TryStart(sessionID, kind)
	if refusal != nil {
		slog.Debug("background work deferred", "session_id", sessionID, "holders", refusal.Holders)
		return zero, false
	}
	cancelled := admission.Cancellation()
	// Held here as well, so work that ignores the admission still runs
	// under it for as long as this call lives.
	defer admission.Release()
	workCtx, cancelWork := context.WithCancel(ctx)
	defer cancelWork()
	handed := admission.Clone()
	type outcome struct {
		value     T
		recovered any
	}
	done := make(chan outcome, 1)
	go func() {
		var result outcome
		defer func() {
			result.recovered = recover()
			done <- result
		}()
		result.value = work(workCtx, handed)
	}()
	ticker := time.NewTicker(recoveryCancellationPoll)
	defer ticker.Stop()
	for {
		// Cancellation wins a tie, as Rust's biased select does.
		if cancelled.Load() || ctx.Err() != nil {
			// Rust drops the work future before the admission; here the work
			// is cancelled and waited for.
			cancelWork()
			<-done
			return zero, false
		}
		select {
		case result := <-done:
			if result.recovered != nil {
				panic(result.recovered)
			}
			if cancelled.Load() {
				return zero, false
			}
			return result.value, true
		case <-ticker.C:
		case <-ctx.Done():
		}
	}
}

// Reserve holds the session for a foreground operation at once, without
// waiting for background holders.
func (g *RecoveryGate) Reserve(sessionID string) *RecoveryReservation {
	return &RecoveryReservation{admission: g.reserveLabelled(sessionID, "foreground operation")}
}

func (g *RecoveryGate) reserveLabelled(sessionID, label string) *ForegroundAdmission {
	g.mu.Lock()
	defer g.mu.Unlock()
	return g.reserveLocked(sessionID, label, reservationForeground)
}

// reserveLocked records a reservation. Call it with the gate locked.
func (g *RecoveryGate) reserveLocked(sessionID, label string, kind reservationKind) *ForegroundAdmission {
	holder := &foregroundHolder{label: label, sessionID: sessionID, gate: g}
	holder.handles.Store(1)
	g.state.reservations[sessionID] = append(g.state.reservations[sessionID], &reservation{identity: holder, label: label, kind: kind})
	return &ForegroundAdmission{holder: holder}
}

// EnterForeground waits for the session's background holders to stop,
// preempting them, and holds the session for a foreground lifecycle.
func (g *RecoveryGate) EnterForeground(ctx context.Context, sessionID, label string, _ LifecycleOrigin, cancelled *atomic.Bool, notices recoveryNotices) (*ForegroundAdmission, error) {
	return g.enter(ctx, sessionID, label, cancelled, notices, reservationForeground)
}

// TryEnterForeground claims a foreground admission without waiting or
// preempting anyone.
//
// A review reservation shares the session. A foreground reservation, a
// delegation drain, background holders, a relay lease, a closed gate, or a
// daemon handoff refuse at once. Background work keeps running.
func (g *RecoveryGate) TryEnterForeground(sessionID, label string, _ LifecycleOrigin) (*ForegroundAdmission, *RecoveryRefusal) {
	if UpgradeIsDraining() {
		return nil, daemonDrainingRefusal()
	}
	g.mu.Lock()
	defer g.mu.Unlock()
	if refusal := g.state.foregroundTryRefusal(sessionID); refusal != nil {
		return nil, refusal
	}
	return g.reserveLocked(sessionID, label, reservationForeground), nil
}

// ReserveWhenIdle holds the session for a review. Reviews exclude
// background work except delegation replies without preempting it. They
// share the session with foreground lifecycles, whose worker actions already
// cancel or refuse overlapping review capture/selection requests. Fails
// only when ctx ends.
func (g *RecoveryGate) ReserveWhenIdle(ctx context.Context, sessionID string) (*RecoveryReservation, error) {
	admission, err := g.enter(ctx, sessionID, "turn review", new(atomic.Bool), ProcessExecutor{}, reservationReview)
	if err != nil {
		return nil, err
	}
	return &RecoveryReservation{admission: admission}, nil
}

func (g *RecoveryGate) enter(ctx context.Context, sessionID, label string, cancelled *atomic.Bool, notices recoveryNotices, kind reservationKind) (*ForegroundAdmission, error) {
	var admission *ForegroundAdmission
	var previous []string
	ticker := time.NewTicker(recoveryCancellationPoll)
	defer ticker.Stop()
	for {
		if cancelled.Load() || notices.CancellationRequested() {
			admission.Release()
			return nil, errors.New("operation cancelled while waiting for session admission")
		}
		if err := ctx.Err(); err != nil {
			admission.Release()
			return nil, err
		}
		changed := g.busy.Changed()
		g.mu.Lock()
		state := &g.state
		if admission == nil &&
			// A waiting review does not preempt background work, so
			// compatible work must remain admissible until the review can
			// start.
			(kind == reservationForeground || len(state.busy[sessionID]) == 0) &&
			!slices.ContainsFunc(state.reservations[sessionID], func(holder *reservation) bool {
				return !kind.runsBeside(holder.kind)
			}) {
			admission = g.reserveLocked(sessionID, label, kind)
			if kind == reservationForeground {
				for _, holder := range state.busy[sessionID] {
					holder.cancelled.Store(true)
				}
			}
		}
		holders := []string{}
		for _, holder := range state.busy[sessionID] {
			holders = append(holders, holder.work.Label())
		}
		if admission == nil {
			for _, holder := range state.reservations[sessionID] {
				if !kind.runsBeside(holder.kind) {
					holders = append(holders, holder.label)
				}
			}
		}
		g.mu.Unlock()
		if len(holders) == 0 && admission != nil {
			return admission, nil
		}
		if !slices.Equal(holders, previous) {
			notices.NotifyNotice(fmt.Sprintf("Waiting for %s to stop", strings.Join(holders, " and ")))
			previous = holders
		}
		select {
		case <-changed:
		case <-ticker.C:
		case <-ctx.Done():
		}
	}
}

func (g *RecoveryGate) release(sessionID string, identity *foregroundHolder) {
	g.mu.Lock()
	defer g.mu.Unlock()
	holders, ok := g.state.reservations[sessionID]
	if !ok {
		return
	}
	holders = slices.DeleteFunc(holders, func(holder *reservation) bool { return holder.identity == identity })
	if len(holders) == 0 {
		delete(g.state.reservations, sessionID)
	} else {
		g.state.reservations[sessionID] = holders
	}
	g.publishBusyLocked()
}

// TryStart claims the session for kind with an identity-bearing handle
// whose cancellation flag the executor watches, or a labelled refusal when
// work that kind cannot run beside or a reservation already holds it, or a
// daemon upgrade is waiting.
//
// The work does not hold up a daemon upgrade: it is cancelled when the
// daemon exits, and the next daemon starts it again. Starting it while a
// handoff waits would only waste it.
func (g *RecoveryGate) TryStart(sessionID string, kind BackgroundWork) (*RecoveryAttempt, *RecoveryRefusal) {
	if UpgradeIsDraining() {
		return nil, daemonDrainingRefusal()
	}
	g.mu.Lock()
	defer g.mu.Unlock()
	if refusal := g.state.sessionRefusal(sessionID, kind); refusal != nil {
		return nil, refusal
	}
	cancelled := new(atomic.Bool)
	g.state.busy[sessionID] = append(g.state.busy[sessionID], backgroundHolder{work: kind, cancelled: cancelled})
	g.publishBusyLocked()
	admission := &recoveryAdmission{gate: g, sessionID: sessionID, cancelled: cancelled}
	admission.handles.Store(1)
	return &RecoveryAttempt{admission: admission}, nil
}

func (g *RecoveryGate) finish(sessionID string, identity *atomic.Bool) {
	g.mu.Lock()
	defer g.mu.Unlock()
	holders, ok := g.state.busy[sessionID]
	if !ok {
		return
	}
	before := len(holders)
	holders = slices.DeleteFunc(holders, func(holder backgroundHolder) bool { return holder.cancelled == identity })
	if len(holders) == before {
		return
	}
	if len(holders) == 0 {
		delete(g.state.busy, sessionID)
	} else {
		g.state.busy[sessionID] = holders
	}
	g.publishBusyLocked()
}

// publishBusyLocked publishes the busy sessions. Call it with the gate
// locked.
func (g *RecoveryGate) publishBusyLocked() {
	g.state.version++
	g.busy.SendReplace(recoveryBusy{version: g.state.version, sessions: g.state.busySessions()})
}

func (s *recoveryGateState) busySessions() []string {
	sessions := make([]string, 0, len(s.busy))
	for session := range s.busy {
		sessions = append(sessions, session)
	}
	slices.Sort(sessions)
	return sessions
}

func (g *RecoveryGate) IsClosed() bool {
	g.mu.Lock()
	defer g.mu.Unlock()
	return g.state.closed
}

// Closed returns once the gate is closed, or with ctx's error.
func (g *RecoveryGate) Closed(ctx context.Context) error {
	select {
	case <-g.closed:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

// Subscribe is a receiver of the busy sessions that has seen the current
// publication.
func (g *RecoveryGate) Subscribe() *RecoveryBusyReceiver {
	return &RecoveryBusyReceiver{watch: g.busy, seen: g.busy.Borrow().version}
}

func (g *RecoveryGate) IsBusy(sessionID string) bool {
	g.mu.Lock()
	defer g.mu.Unlock()
	_, busy := g.state.busy[sessionID]
	return busy
}

// ForegroundReservationLabels is the foreground reservation labels for
// sessionID.
//
// TryEnterForeground reports daemon handoff before it looks at this session,
// so a test that has closed the upgrade gate cannot use that call to see
// whether the session admission was released.
func (g *RecoveryGate) ForegroundReservationLabels(sessionID string) []string {
	g.mu.Lock()
	defer g.mu.Unlock()
	labels := []string{}
	for _, holder := range g.state.reservations[sessionID] {
		if holder.kind == reservationForeground {
			labels = append(labels, holder.label)
		}
	}
	return labels
}

func (s *recoveryGateState) sessionRefusal(sessionID string, kind BackgroundWork) *RecoveryRefusal {
	// Replies finish the work a review accepted and the work a foreground
	// Move explicitly yields to while draining. Neither reservation can
	// defer these completions for the lifetime of the review/drain.
	_, leased := s.leases[sessionID]
	reserved := slices.ContainsFunc(s.reservations[sessionID], func(holder *reservation) bool {
		switch holder.kind {
		case reservationForeground:
			return true
		case reservationReview:
			return kind != BackgroundWorkDelegationDelivery && kind != BackgroundWorkRelayRecovery &&
				kind != BackgroundWorkWorkerObservation
		case reservationForegroundDelegationDrain:
			return kind != BackgroundWorkDelegationDelivery
		}
		return false
	})
	if !(s.closed || leased || reserved || len(s.busy[sessionID]) > 0) {
		return nil
	}
	holders := []string{}
	for _, holder := range s.busy[sessionID] {
		holders = append(holders, holder.work.Label())
	}
	for _, holder := range s.reservations[sessionID] {
		holders = append(holders, holder.label)
	}
	if s.closed {
		holders = append(holders, "daemon shutdown")
	}
	return &RecoveryRefusal{Cause: s.refusalCause(), Holders: holders, WorkerOwned: s.workerOwned(sessionID)}
}

func (s *recoveryGateState) refusalCause() RefusalCause {
	if s.closed {
		return RefusalCauseClosed
	}
	return RefusalCauseSessionHeld
}

func (s *recoveryGateState) workerOwned(sessionID string) bool {
	return slices.ContainsFunc(s.reservations[sessionID], func(holder *reservation) bool {
		return holder.kind == reservationForeground
	}) || slices.ContainsFunc(s.busy[sessionID], func(holder backgroundHolder) bool {
		return holder.work == BackgroundWorkWorkerReplacement
	})
}

// foregroundTryRefusal reports whether a non-waiting foreground admission
// can start. Reviews do not count: they share the session with a person's
// lifecycle.
func (s *recoveryGateState) foregroundTryRefusal(sessionID string) *RecoveryRefusal {
	reservations := s.reservations[sessionID]
	blockingReservation := slices.ContainsFunc(reservations, func(holder *reservation) bool {
		return !reservationForeground.runsBeside(holder.kind)
	})
	busy := len(s.busy[sessionID]) > 0
	lease, leased := s.leases[sessionID]
	if !(s.closed || leased || blockingReservation || busy) {
		return nil
	}
	holders := []string{}
	for _, holder := range s.busy[sessionID] {
		holders = append(holders, holder.work.Label())
	}
	for _, holder := range reservations {
		if !reservationForeground.runsBeside(holder.kind) {
			holders = append(holders, holder.label)
		}
	}
	if leased {
		holders = append(holders, lease)
	}
	if s.closed {
		holders = append(holders, "daemon shutdown")
	}
	return &RecoveryRefusal{Cause: s.refusalCause(), Holders: holders, WorkerOwned: s.workerOwned(sessionID)}
}

// WaitForBackgroundRelease observes the same admission decision as TryStart
// without claiming resources. A global publication can wake this; only this
// session's release returns true. false when the gate closed or ctx ended.
func (g *RecoveryGate) WaitForBackgroundRelease(ctx context.Context, sessionID string, kind BackgroundWork, changed *RecoveryBusyReceiver) bool {
	for {
		g.mu.Lock()
		refusal := g.state.sessionRefusal(sessionID, kind)
		g.mu.Unlock()
		switch {
		case refusal == nil:
			return true
		case refusal.Cause == RefusalCauseClosed:
			return false
		}
		if changed.Changed(ctx) != nil {
			return false
		}
	}
}

// WaitForLeaseReturn: reviews coexist with foreground lifecycles, so their
// exchanges wait for the owner's physical lease return before trying the
// worker again.
func (g *RecoveryGate) WaitForLeaseReturn(ctx context.Context, sessionID string, changed *RecoveryBusyReceiver) error {
	g.mu.Lock()
	held := g.state.sessionRefusal(sessionID, BackgroundWorkWorkerObservation) != nil
	g.mu.Unlock()
	if !held && !changed.HasChanged() {
		return fmt.Errorf("worker reported lifecycle contention without an owner for session %s", sessionID)
	}
	if !g.WaitForBackgroundRelease(ctx, sessionID, BackgroundWorkWorkerObservation, changed) {
		if err := ctx.Err(); err != nil {
			return err
		}
		return errors.New("session owner is closed")
	}
	return nil
}

// CancelBusy asks all in-flight work for this session, if any, to stop.
func (g *RecoveryGate) CancelBusy(sessionID string) {
	g.mu.Lock()
	defer g.mu.Unlock()
	for _, holder := range g.state.busy[sessionID] {
		holder.cancelled.Store(true)
	}
}

// Close closes background admission and cancels every admitted attempt in
// one transition. Existing handles retain ownership until their executors
// have settled.
func (g *RecoveryGate) Close() {
	g.mu.Lock()
	g.state.closed = true
	for _, holders := range g.state.busy {
		for _, holder := range holders {
			holder.cancelled.Store(true)
		}
	}
	g.publishBusyLocked()
	g.mu.Unlock()
	g.closeOnce.Do(func() { close(g.closed) })
}

func (g *RecoveryGate) BusySessions() []string {
	g.mu.Lock()
	defer g.mu.Unlock()
	return g.state.busySessions()
}

// RecoveryBusyReceiver follows the gate's busy sessions (Rust's
// watch::Receiver): it remembers the last publication it saw.
type RecoveryBusyReceiver struct {
	watch *Watch[recoveryBusy]
	seen  uint64
}

// Borrow is the current busy sessions, without marking them seen.
func (r *RecoveryBusyReceiver) Borrow() []string { return r.watch.Borrow().sessions }

// HasChanged reports whether a publication arrived since this receiver last
// saw one.
func (r *RecoveryBusyReceiver) HasChanged() bool { return r.watch.Borrow().version != r.seen }

// Changed waits for a publication this receiver has not seen and marks it
// seen, or returns ctx's error.
func (r *RecoveryBusyReceiver) Changed(ctx context.Context) error {
	for {
		changed := r.watch.Changed()
		if current := r.watch.Borrow(); current.version != r.seen {
			r.seen = current.version
			return nil
		}
		select {
		case <-changed:
		case <-ctx.Done():
			return ctx.Err()
		}
	}
}

// WaitFor waits until predicate accepts the busy sessions, marking what it
// read seen, and returns them.
func (r *RecoveryBusyReceiver) WaitFor(ctx context.Context, predicate func(sessions []string) bool) ([]string, error) {
	for {
		changed := r.watch.Changed()
		current := r.watch.Borrow()
		r.seen = current.version
		if predicate(current.sessions) {
			return current.sessions, nil
		}
		select {
		case <-changed:
		case <-ctx.Done():
			return nil, ctx.Err()
		}
	}
}

// RecoveryAttempt is a background admission. The cancel flag also
// identifies this particular admission. Only its owner can release the
// slot, including when an executor or coordinator unwinds. Clones share it;
// it ends when the last handle is released.
type RecoveryAttempt struct {
	admission *recoveryAdmission
	released  atomic.Bool
}

type recoveryAdmission struct {
	gate      *RecoveryGate
	sessionID string
	cancelled *atomic.Bool
	handles   atomic.Int64
}

func (a *RecoveryAttempt) Clone() *RecoveryAttempt {
	a.admission.handles.Add(1)
	return &RecoveryAttempt{admission: a.admission}
}

// Release gives this handle up; releasing it twice does nothing.
func (a *RecoveryAttempt) Release() {
	if a == nil || !a.released.CompareAndSwap(false, true) {
		return
	}
	if a.admission.handles.Add(-1) == 0 {
		a.admission.gate.finish(a.admission.sessionID, a.admission.cancelled)
	}
}

func (a *RecoveryAttempt) Cancellation() *atomic.Bool { return a.admission.cancelled }

// Cancelled reads the cancellation flag (Rust: Deref to AtomicBool).
func (a *RecoveryAttempt) Cancelled() bool { return a.admission.cancelled.Load() }

// Admission is HoldsSession: a new handle on this admission.
func (a *RecoveryAttempt) Admission() SessionAdmission {
	return SessionAdmission{background: a.Clone()}
}

// RecoveryRunBlocking runs work on its own goroutine with a handle on the
// admission. Both the waiter and the executor own admission: cancelling ctx
// releases the waiter's handle and cannot release a running executor; a
// panicking executor cannot release its slot before the supervisor applies
// the failure outcome. The returned handle is attempt, for the caller to
// release once it has settled; nil when ctx ended first.
func RecoveryRunBlocking[T any](attempt *RecoveryAttempt, ctx context.Context, work func(attempt *RecoveryAttempt) T) (T, error, *RecoveryAttempt) {
	executing := attempt.Clone()
	handle := SpawnBlocking(func() T {
		defer executing.Release()
		return work(executing)
	})
	value, err := handle.Join(ctx)
	if ctx.Err() != nil && err == ctx.Err() {
		attempt.Release()
		return value, err, nil
	}
	return value, err, attempt
}

// SessionAdmission is an admitted operation, explicitly carried across
// executor boundaries: one of a foreground or a background admission.
type SessionAdmission struct {
	foreground *ForegroundAdmission
	background *RecoveryAttempt
}

// HoldsSession hands out a new handle on the admission it holds.
type HoldsSession interface {
	Admission() SessionAdmission
}

// Admission is HoldsSession: a new handle on the same admission.
func (s SessionAdmission) Admission() SessionAdmission { return s.Clone() }

func (s SessionAdmission) Clone() SessionAdmission {
	if s.foreground != nil {
		return SessionAdmission{foreground: s.foreground.Clone()}
	}
	return SessionAdmission{background: s.background.Clone()}
}

// Release gives this handle up.
func (s SessionAdmission) Release() {
	s.foreground.Release()
	s.background.Release()
}

// Foreground is the foreground admission, if this is one.
func (s SessionAdmission) Foreground() (*ForegroundAdmission, bool) {
	return s.foreground, s.foreground != nil
}

// Background is the background admission, if this is one.
func (s SessionAdmission) Background() (*RecoveryAttempt, bool) {
	return s.background, s.background != nil
}

func (s SessionAdmission) GateAndSession() (*RecoveryGate, string) {
	if s.foreground != nil {
		return s.foreground.holder.gate, s.foreground.holder.sessionID
	}
	return s.background.admission.gate, s.background.admission.sessionID
}

func (s SessionAdmission) SessionID() string {
	_, session := s.GateAndSession()
	return session
}

// DrainDelegation turns this foreground admission into a delegation drain
// until the drain is closed.
func (s SessionAdmission) DrainDelegation() (*ForegroundDelegationDrain, error) {
	admission := s.foreground
	if admission == nil {
		return nil, errors.New("delegation drain requires foreground ownership")
	}
	gate := admission.holder.gate
	gate.mu.Lock()
	defer gate.mu.Unlock()
	index := slices.IndexFunc(gate.state.reservations[admission.holder.sessionID], func(holder *reservation) bool {
		return holder.identity == admission.holder
	})
	if index < 0 {
		return nil, errors.New("foreground admission has ended")
	}
	holder := gate.state.reservations[admission.holder.sessionID][index]
	if holder.kind != reservationForeground {
		return nil, errors.New("foreground is already draining delegation")
	}
	holder.kind = reservationForegroundDelegationDrain
	gate.publishBusyLocked()
	return &ForegroundDelegationDrain{admission: admission.Clone()}, nil
}

// Lease waits for the session's relay lease and takes it for this
// admission, or returns ctx's error.
func (s SessionAdmission) Lease(ctx context.Context, sessionID string) (*RelayLeaseAdmission, error) {
	gate, admitted := s.GateAndSession()
	if sessionID != admitted {
		return nil, errors.New("relay lease admission belongs to another session")
	}
	for {
		changed := gate.busy.Changed()
		gate.mu.Lock()
		state := &gate.state
		var label string
		if s.foreground != nil {
			index := slices.IndexFunc(state.reservations[sessionID], func(holder *reservation) bool {
				return holder.identity == s.foreground.holder
			})
			if index < 0 {
				gate.mu.Unlock()
				return nil, errors.New("foreground admission has ended")
			}
			if state.reservations[sessionID][index].kind != reservationForeground {
				gate.mu.Unlock()
				return nil, errors.New("foreground relay ownership is yielded to delegation drain")
			}
			label = s.foreground.holder.label
		} else {
			index := slices.IndexFunc(state.busy[sessionID], func(holder backgroundHolder) bool {
				return holder.cancelled == s.background.admission.cancelled
			})
			if index < 0 {
				gate.mu.Unlock()
				return nil, errors.New("relay lease admission has ended")
			}
			holder := state.busy[sessionID][index]
			if holder.cancelled.Load() {
				gate.mu.Unlock()
				return nil, fmt.Errorf("%s was cancelled before taking its relay lease", holder.work.Label())
			}
			label = holder.work.Label()
		}
		_, leased := state.leases[sessionID]
		if !leased && (s.foreground == nil || len(state.busy[sessionID]) == 0) {
			state.leases[sessionID] = label
			gate.mu.Unlock()
			return &RelayLeaseAdmission{admission: s.Clone(), Label: label}, nil
		}
		gate.mu.Unlock()
		select {
		case <-changed:
		case <-ctx.Done():
			return nil, ctx.Err()
		}
	}
}

// ForegroundDelegationDrain is a temporary phase of the same foreground
// identity. Closing the phase refuses new delivery and preempts admitted
// delivery; source reacquisition waits for its holders and leases to
// return. Close it on every path, cancellation and unwinding included.
type ForegroundDelegationDrain struct {
	admission *ForegroundAdmission
	closed    atomic.Bool
}

// Close ends the drain (Rust's Drop): the reservation is foreground again
// and admitted background work is cancelled.
func (d *ForegroundDelegationDrain) Close() {
	if d == nil || !d.closed.CompareAndSwap(false, true) {
		return
	}
	holder := d.admission.holder
	gate := holder.gate
	gate.mu.Lock()
	for _, reservation := range gate.state.reservations[holder.sessionID] {
		if reservation.identity == holder {
			reservation.kind = reservationForeground
			break
		}
	}
	for _, background := range gate.state.busy[holder.sessionID] {
		background.cancelled.Store(true)
	}
	gate.publishBusyLocked()
	gate.mu.Unlock()
	d.admission.Release()
}

// RelayLeaseAdmission is retained by the actor until it has applied the
// lease return. Admission cannot end before the relay is available.
type RelayLeaseAdmission struct {
	admission SessionAdmission
	Label     string
	released  atomic.Bool
}

// Release returns the lease (Rust's Drop), then the admission it held.
func (l *RelayLeaseAdmission) Release() {
	if l == nil || !l.released.CompareAndSwap(false, true) {
		return
	}
	gate, session := l.admission.GateAndSession()
	gate.mu.Lock()
	delete(gate.state.leases, session)
	gate.publishBusyLocked()
	gate.mu.Unlock()
	l.admission.Release()
}

// ObservationSender is the sending side of a per-session latest-state
// mailbox shared by the two background policies. A capacity-one wake
// channel never queues an observation history. Clones share it; the
// receiver sees the end once every sender is closed.
type ObservationSender[T any] struct {
	channel *observationChannel[T]
	closed  atomic.Bool
}

// ObservationReceiver is the receiving side of an observation channel.
type ObservationReceiver[T any] struct {
	channel *observationChannel[T]
}

type observationChannel[T any] struct {
	mu      sync.Mutex
	values  map[string]T
	ready   []string
	wake    chan struct{}
	senders int
	// receiverClosed: closure discards disposable observations.
	receiverClosed bool
}

func NewObservationChannel[T any]() (*ObservationSender[T], *ObservationReceiver[T]) {
	channel := &observationChannel[T]{values: map[string]T{}, wake: make(chan struct{}, 1), senders: 1}
	return &ObservationSender[T]{channel: channel}, &ObservationReceiver[T]{channel: channel}
}

func (s *ObservationSender[T]) Clone() *ObservationSender[T] {
	s.channel.mu.Lock()
	s.channel.senders++
	s.channel.mu.Unlock()
	return &ObservationSender[T]{channel: s.channel}
}

// Close gives this sender up; the receiver sees the end after the last.
func (s *ObservationSender[T]) Close() {
	if !s.closed.CompareAndSwap(false, true) {
		return
	}
	s.channel.mu.Lock()
	defer s.channel.mu.Unlock()
	s.channel.senders--
	if s.channel.senders == 0 {
		close(s.channel.wake)
	}
}

// Send stores observation as the session's latest, merging the one still
// pending into it.
func (s *ObservationSender[T]) Send(sessionID string, observation T, merge func(previous T, observation *T)) {
	if s.closed.Load() {
		return
	}
	channel := s.channel
	channel.mu.Lock()
	defer channel.mu.Unlock()
	if channel.receiverClosed {
		return
	}
	if previous, ok := channel.values[sessionID]; ok {
		merge(previous, &observation)
	} else {
		channel.ready = append(channel.ready, sessionID)
	}
	channel.values[sessionID] = observation
	// Full means a wake is already pending.
	select {
	case channel.wake <- struct{}{}:
	default:
	}
}

func (r *ObservationReceiver[T]) TryRecv() (T, bool) {
	channel := r.channel
	channel.mu.Lock()
	defer channel.mu.Unlock()
	var zero T
	if len(channel.ready) == 0 {
		return zero, false
	}
	session := channel.ready[0]
	channel.ready = channel.ready[1:]
	value, ok := channel.values[session]
	delete(channel.values, session)
	return value, ok
}

// Recv waits for the next session's latest observation; false once every
// sender is closed and nothing is pending, or when ctx ends.
func (r *ObservationReceiver[T]) Recv(ctx context.Context) (T, bool) {
	for {
		if observation, ok := r.TryRecv(); ok {
			return observation, true
		}
		select {
		case _, open := <-r.channel.wake:
			if !open {
				return r.TryRecv()
			}
		case <-ctx.Done():
			var zero T
			return zero, false
		}
	}
}

// Close drops the receiver: later observations are discarded.
func (r *ObservationReceiver[T]) Close() {
	r.channel.mu.Lock()
	r.channel.receiverClosed = true
	r.channel.mu.Unlock()
}
