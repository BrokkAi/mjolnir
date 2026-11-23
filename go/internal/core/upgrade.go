// Port of mj-core/src/upgrade.rs.

// Admission for process-owned work during an automatic daemon handoff.
// Waiting never cancels existing work. The last idle check and closing
// admission are one decision, so new work cannot race the process exit.
//
// The handoff waits only for short control operations. Work that can take
// minutes but is safe to stop, such as a recovery copy, the preparation for
// a worker upgrade, or a SessionWiki sync, does not hold admission: the
// handoff cancels it and the next daemon starts it again. Once a handoff is
// waiting, the gate drains: it refuses new deferrable work, so the set of
// holders can only shrink. While it waits, the daemon log names each blocker
// with its count and age. A hold may carry a Describe handle, which the
// notice asks for the work's identity and current stage when it renders, so
// the owner of that fact stays the only place that records it.

package core

import (
	"context"
	"fmt"
	"log/slog"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// drainLapse is how long draining lasts after the most recent handoff
// attempt. The upgrading client retries every quarter second, so a lapse
// means it gave up, and deferrable work may start again.
const drainLapse = 10 * time.Second

// waitReportInterval is how often a waiting handoff names its blockers again
// in the daemon log.
const waitReportInterval = 30 * time.Second

// namedHolds is how many holds of one label a notice describes before it
// counts the rest.
const namedHolds = 3

const startupRecoveryLabel = "daemon startup recovery"

// DestroyLabel is the label a session destroy holds admission under.
const DestroyLabel = "session destroy"

// HandoffRefusalCode: a refused control admission has made no mutation and
// may wait for handoff.
const HandoffRefusalCode = "daemon_handoff"

// DestroyWaitBound is how long a stop or handoff waits for destroys in
// flight before it goes ahead and abandons them. The client waits longer
// than this for the daemon to exit (STOP_DRAIN_TIMEOUT).
const DestroyWaitBound = 60 * time.Second

// Gate decides daemon admission during an automatic handoff. Build one with
// NewGate.
type Gate struct {
	mu    sync.Mutex
	state upgradeState
	// notify is closed and replaced to wake async waiters (Rust's Notify).
	notify chan struct{}
	// condvar is closed and replaced to wake blocked admissions (Rust's
	// Condvar).
	condvar chan struct{}
	// logger is where the gate reports waits and abandoned destroys; nil is
	// slog's default. A test gives a gate its own, as Rust's tests install a
	// thread's own subscriber.
	logger *slog.Logger
}

func (g *Gate) log() *slog.Logger {
	if g.logger != nil {
		return g.logger
	}
	return slog.Default()
}

func NewGate() *Gate {
	return &Gate{
		state:   upgradeState{active: map[string]map[uint64]*upgradeHold{}},
		notify:  make(chan struct{}),
		condvar: make(chan struct{}),
	}
}

type upgradeState struct {
	closed bool
	// destroying is the sessions whose destroy holds admission, one entry
	// per hold.
	destroying []string
	// destroyWaitStarted is when a stop or handoff first found a destroy
	// still running.
	destroyWaitStarted *time.Time
	// destroyWaitBound lets tests shorten DestroyWaitBound.
	destroyWaitBound *time.Duration
	// drainRequested is when the most recent handoff attempt found work
	// still running.
	drainRequested *time.Time
	// handoffWait is when the current handoff wait began, and when it last
	// named its blockers in the log.
	handoffWait *[2]time.Time
	// active is each hold by label, keyed by admission order.
	active   map[string]map[uint64]*upgradeHold
	nextHold uint64
	// resumableWaiters is test synchronization only; never part of
	// admission policy.
	resumableWaiters int
}

type upgradeHold struct {
	began     time.Time
	describer Describe
}

// HoldDetail is what a hold is doing, as its owner reports it when a notice
// renders.
type HoldDetail struct {
	// Subject is which work this is, such as `create e5dfbd99`.
	Subject string
	// Stage is what the work is doing now, if its owner knows.
	Stage *string
}

// Describe is held by an admission so a waiting handoff can name the work.
// The implementation reads the live fact from its owner on every call; the
// gate never copies it.
type Describe interface {
	Describe() HoldDetail
}

// destroyWaitHolds reports whether the destroys still running should be
// waited for. Refuses new deferrable work while it does.
func (s *upgradeState) destroyWaitHolds(logger *slog.Logger) bool {
	if s.destroyWaitStarted == nil {
		now := time.Now()
		s.destroyWaitStarted = &now
	}
	bound := DestroyWaitBound
	if s.destroyWaitBound != nil {
		bound = *s.destroyWaitBound
	}
	if time.Since(*s.destroyWaitStarted) < bound {
		return true
	}
	logger.Warn("daemon stops with session destroys unfinished; each session comes back and needs `mj destroy` again",
		"sessions", fmt.Sprintf("%q", s.destroying), "bound_seconds", bound.Seconds())
	return false
}

// holdsOnlyDestroysOrNothing is true when every active hold is a destroy.
func (s *upgradeState) holdsOnlyDestroysOrNothing() bool {
	for label := range s.active {
		if label != DestroyLabel {
			return false
		}
	}
	return true
}

func (s *upgradeState) draining() bool {
	return s.drainRequested != nil && time.Since(*s.drainRequested) < drainLapse
}

func (s *upgradeState) blockers() []Blocker {
	labels := make([]string, 0, len(s.active))
	for label := range s.active {
		labels = append(labels, label)
	}
	slices.Sort(labels)
	blockers := []Blocker{}
	for _, label := range labels {
		holds := s.active[label]
		order := make([]uint64, 0, len(holds))
		for hold := range holds {
			order = append(order, hold)
		}
		slices.Sort(order)
		if len(order) == 0 {
			continue
		}
		// Admission order is start order, so the first hold is oldest.
		blocker := Blocker{Label: label, Count: len(holds), Age: time.Since(holds[order[0]].began)}
		for _, id := range order {
			hold := holds[id]
			if hold.describer == nil {
				continue
			}
			if len(blocker.named) == namedHolds {
				break
			}
			blocker.named = append(blocker.named, namedHold{describer: hold.describer, age: time.Since(hold.began)})
		}
		blockers = append(blockers, blocker)
	}
	return blockers
}

// refusedHandoffReport decides whether a refused handoff reports its
// blockers now: when the wait begins, then every waitReportInterval, so a
// long wait always names its cause. The caller logs the report after it
// releases the gate, because describing a hold reads other owners' state.
func (s *upgradeState) refusedHandoffReport() *refusedHandoff {
	now := time.Now()
	if s.handoffWait == nil || !s.draining() {
		// A wait whose client stopped asking has ended; this is a new one.
		s.handoffWait = &[2]time.Time{now, now}
		return &refusedHandoff{blockers: s.blockers()}
	}
	started, reported := s.handoffWait[0], s.handoffWait[1]
	if now.Sub(reported) >= waitReportInterval {
		s.handoffWait = &[2]time.Time{started, now}
		waited := uint64(time.Since(started).Seconds())
		return &refusedHandoff{blockers: s.blockers(), waitedSeconds: &waited}
	}
	return nil
}

type refusedHandoff struct {
	blockers []Blocker
	// waitedSeconds is nil for the first report of a wait.
	waitedSeconds *uint64
}

func (r *refusedHandoff) log(logger *slog.Logger) {
	blockers := describeBlockers(r.blockers)
	if r.waitedSeconds == nil {
		logger.Info("daemon upgrade handoff is waiting for daemon-owned work", "blockers", blockers)
		return
	}
	logger.Info("daemon upgrade handoff is still waiting for daemon-owned work",
		"blockers", blockers, "waited_seconds", *r.waitedSeconds)
}

// Blocker is one label of work holding admission open: how many holds, how
// long the oldest has run, and the oldest few holds that can describe
// themselves.
type Blocker struct {
	Label string
	Count int
	Age   time.Duration
	named []namedHold
}

type namedHold struct {
	describer Describe
	age       time.Duration
}

func upgradeFormatAge(age time.Duration) string {
	seconds := uint64(age / time.Second)
	if seconds < 60 {
		return fmt.Sprintf("%ds", seconds)
	}
	return fmt.Sprintf("%dm %02ds", seconds/60, seconds%60)
}

// countedLabel is the label, with the count appended when it is held more
// than once.
func (b *Blocker) countedLabel() string {
	if b.Count == 1 {
		return b.Label
	}
	return fmt.Sprintf("%s x%d", b.Label, b.Count)
}

// String renders `SessionWiki sync (12s)`, `session lifecycle x2 (oldest 1m
// 05s)`, and, for holds that describe themselves, `session lifecycle x5:
// create e5dfbd99 (install worker binary, 84m 42s); and 4 more`. Rendering
// asks each described hold for its live detail, so call it without the gate
// held.
func (b Blocker) String() string {
	if len(b.named) == 0 {
		age := upgradeFormatAge(b.Age)
		if b.Count == 1 {
			return fmt.Sprintf("%s (%s)", b.Label, age)
		}
		return fmt.Sprintf("%s x%d (oldest %s)", b.Label, b.Count, age)
	}
	var text strings.Builder
	text.WriteString(b.Label)
	if b.Count > 1 {
		fmt.Fprintf(&text, " x%d", b.Count)
	}
	for index, hold := range b.named {
		detail := hold.describer.Describe()
		age := upgradeFormatAge(hold.age)
		separator := "; "
		if index == 0 {
			separator = ": "
		}
		if detail.Stage != nil {
			fmt.Fprintf(&text, "%s%s (%s, %s)", separator, detail.Subject, *detail.Stage, age)
		} else {
			fmt.Fprintf(&text, "%s%s (%s)", separator, detail.Subject, age)
		}
	}
	if more := b.Count - len(b.named); more > 0 {
		fmt.Fprintf(&text, "; and %d more", more)
	}
	return text.String()
}

func describeBlockers(blockers []Blocker) string {
	texts := make([]string, len(blockers))
	for i, blocker := range blockers {
		texts[i] = blocker.String()
	}
	return strings.Join(texts, ", ")
}

// UpgradeWork is one admitted operation (Rust's upgrade::Work). Clones share
// it: the operation is counted once however many holders have a clone, and
// it ends when the last clone is released. Release every handle, including
// clones, on every path.
type UpgradeWork struct {
	registration *upgradeRegistration
	released     atomic.Bool
}

type upgradeRegistration struct {
	gate       *Gate
	label      string
	hold       uint64
	destroying *string
	handles    atomic.Int64
}

// Clone is another handle on the same operation.
func (w *UpgradeWork) Clone() *UpgradeWork {
	w.registration.handles.Add(1)
	return &UpgradeWork{registration: w.registration}
}

// Release gives this handle up; the operation ends with its last handle.
// Releasing a handle twice does nothing.
func (w *UpgradeWork) Release() {
	if w == nil || !w.released.CompareAndSwap(false, true) {
		return
	}
	if w.registration.handles.Add(-1) == 0 {
		w.registration.drop()
	}
}

// drop is Rust's Drop for Registration.
func (r *upgradeRegistration) drop() {
	gate := r.gate
	gate.mu.Lock()
	state := &gate.state
	if r.destroying != nil {
		if index := slices.Index(state.destroying, *r.destroying); index >= 0 {
			state.destroying = slices.Delete(state.destroying, index, index+1)
		}
	}
	if len(state.destroying) == 0 {
		state.destroyWaitStarted = nil
	}
	holds, ok := state.active[r.label]
	if !ok {
		gate.mu.Unlock()
		panic("registered upgrade work")
	}
	delete(holds, r.hold)
	notify := false
	if len(holds) == 0 {
		delete(state.active, r.label)
		notify = r.label == startupRecoveryLabel
	}
	if notify {
		close(gate.notify)
		gate.notify = make(chan struct{})
	}
	gate.mu.Unlock()
}

// publishLocked wakes every waiter. Call it with the gate locked.
func (g *Gate) publishLocked() {
	close(g.notify)
	g.notify = make(chan struct{})
	close(g.condvar)
	g.condvar = make(chan struct{})
}

func handoffUnderway() error {
	return RefusalPrecondition("daemon upgrade handoff is underway").WithCode(HandoffRefusalCode)
}

// EnterAfterDraining admits resumable work, which owns no admission while
// handoff drains. The same lock decides whether to resume control or yield
// to a closed daemon.
func (g *Gate) EnterAfterDraining(label string) (*UpgradeWork, error) {
	g.mu.Lock()
	defer g.mu.Unlock()
	for {
		if g.state.closed {
			return nil, handoffUnderway()
		}
		if !g.state.draining() {
			return g.registerAs(label, nil, nil), nil
		}
		// This is the owner's existing lapse deadline, not a polling
		// interval or a bound on the resumable operation.
		deadline := g.state.drainRequested.Add(drainLapse)
		trackWaiters := testing.Testing() || TestFixturesEnabled()
		if trackWaiters {
			g.state.resumableWaiters++
			close(g.notify)
			g.notify = make(chan struct{})
		}
		wake := g.condvar
		g.mu.Unlock()
		timer := time.NewTimer(time.Until(deadline))
		select {
		case <-wake:
		case <-timer.C:
		}
		timer.Stop()
		g.mu.Lock()
		if trackWaiters {
			g.state.resumableWaiters--
		}
	}
}

// WaitAfterDraining waits, without holding daemon admission, until the drain
// lapses (true) or the gate closes (false), on this owner's exact drain
// deadline and publication. Resumable background observers use it. A
// cancelled ctx also answers false: the observer is not to resume.
func (g *Gate) WaitAfterDraining(ctx context.Context) bool {
	for {
		g.mu.Lock()
		changed := g.notify
		if g.state.closed {
			g.mu.Unlock()
			return false
		}
		if !g.state.draining() {
			g.mu.Unlock()
			return true
		}
		deadline := g.state.drainRequested.Add(drainLapse)
		g.mu.Unlock()
		timer := time.NewTimer(time.Until(deadline))
		select {
		case <-changed:
		case <-timer.C:
		case <-ctx.Done():
			timer.Stop()
			return false
		}
		timer.Stop()
	}
}

func (g *Gate) waitForStartupRecovery(ctx context.Context) error {
	for {
		g.mu.Lock()
		changed := g.notify
		_, held := g.state.active[startupRecoveryLabel]
		g.mu.Unlock()
		if !held {
			return nil
		}
		select {
		case <-changed:
		case <-ctx.Done():
			return ctx.Err()
		}
	}
}

// WaitForResumableWaiterForTest returns once an EnterAfterDraining call is
// waiting.
func (g *Gate) WaitForResumableWaiterForTest(ctx context.Context) error {
	for {
		g.mu.Lock()
		notified := g.notify
		waiting := g.state.resumableWaiters > 0
		g.mu.Unlock()
		if waiting {
			return nil
		}
		select {
		case <-notified:
		case <-ctx.Done():
			return ctx.Err()
		}
	}
}

// ExpireDrainForTest ends the current drain as if its client stopped
// asking.
func (g *Gate) ExpireDrainForTest() {
	g.mu.Lock()
	expired := time.Now().Add(-drainLapse)
	g.state.drainRequested = &expired
	g.publishLocked()
	g.mu.Unlock()
}

func (g *Gate) IsOpen() bool {
	g.mu.Lock()
	defer g.mu.Unlock()
	return !g.state.closed
}

// IsDraining reports whether a handoff is waiting, or has already happened.
// Deferrable work that has not started yet should not start.
func (g *Gate) IsDraining() bool {
	g.mu.Lock()
	defer g.mu.Unlock()
	return g.state.closed || g.state.draining()
}

// BeginDraining publishes a drain from the same owner that decides
// admission. Queued, unaccepted requests release their outer client hold
// when this wakes.
func (g *Gate) BeginDraining() {
	g.mu.Lock()
	now := time.Now()
	g.state.drainRequested = &now
	g.publishLocked()
	g.mu.Unlock()
}

// WaitUntilDraining returns once IsDraining is true, or with ctx's error.
func (g *Gate) WaitUntilDraining(ctx context.Context) error {
	for {
		g.mu.Lock()
		notified := g.notify
		draining := g.state.closed || g.state.draining()
		g.mu.Unlock()
		if draining {
			return nil
		}
		select {
		case <-notified:
		case <-ctx.Done():
			return ctx.Err()
		}
	}
}

// Enter admits a short control operation. The handoff waits for it. Refused
// only once the handoff has happened.
func (g *Gate) Enter(label string) (*UpgradeWork, error) {
	return g.EnterDescribed(label, nil)
}

// EnterDescribed is Enter for work that can name itself in the wait notice.
func (g *Gate) EnterDescribed(label string, describer Describe) (*UpgradeWork, error) {
	g.mu.Lock()
	defer g.mu.Unlock()
	if g.state.closed {
		return nil, handoffUnderway()
	}
	return g.registerAs(label, nil, describer), nil
}

// EnterUnlessDraining admits work the handoff waits for but that can be put
// off. Refused while a handoff is waiting, so this work cannot keep
// extending the wait.
func (g *Gate) EnterUnlessDraining(label string) (*UpgradeWork, error) {
	return g.EnterUnlessDrainingDescribed(label, nil)
}

// EnterUnlessDrainingDescribed is EnterUnlessDraining for work that can name
// itself.
func (g *Gate) EnterUnlessDrainingDescribed(label string, describer Describe) (*UpgradeWork, error) {
	g.mu.Lock()
	defer g.mu.Unlock()
	if g.state.closed {
		return nil, handoffUnderway()
	}
	if g.state.draining() {
		return nil, RefusalPrecondition("a daemon upgrade handoff is waiting").WithCode(HandoffRefusalCode)
	}
	return g.registerAs(label, nil, describer), nil
}

// EnterDestroy admits the destroy of sessionID. The handoff and the
// graceful stop wait for it, but only up to DestroyWaitBound.
func (g *Gate) EnterDestroy(sessionID string) (*UpgradeWork, error) {
	return g.enterDestroyMode(sessionID, false)
}

func (g *Gate) EnterDestroyUnlessDraining(sessionID string) (*UpgradeWork, error) {
	return g.enterDestroyMode(sessionID, true)
}

func (g *Gate) enterDestroyMode(sessionID string, deferrable bool) (*UpgradeWork, error) {
	g.mu.Lock()
	defer g.mu.Unlock()
	if g.state.closed || (deferrable && g.state.draining()) {
		return nil, handoffUnderway()
	}
	g.state.destroying = append(g.state.destroying, sessionID)
	return g.registerAs(DestroyLabel, &sessionID, nil), nil
}

// registerAs records a hold. Call it with the gate locked.
func (g *Gate) registerAs(label string, destroying *string, describer Describe) *UpgradeWork {
	hold := g.state.nextHold
	g.state.nextHold++
	holds, ok := g.state.active[label]
	if !ok {
		holds = map[uint64]*upgradeHold{}
		g.state.active[label] = holds
	}
	holds[hold] = &upgradeHold{began: time.Now(), describer: describer}
	registration := &upgradeRegistration{gate: g, label: label, hold: hold, destroying: destroying}
	registration.handles.Store(1)
	return &UpgradeWork{registration: registration}
}

// gateWithDestroyWaitBound is a gate whose destroys hold a stop for bound.
func gateWithDestroyWaitBound(bound time.Duration) *Gate {
	gate := NewGate()
	gate.state.destroyWaitBound = &bound
	return gate
}

// DestroysHoldTheStop reports whether a stop should still wait for
// destroys. Starts the wait on the first call that finds one. Once the bound
// has passed it logs the destroys it abandons and answers no.
func (g *Gate) DestroysHoldTheStop() bool {
	g.mu.Lock()
	if len(g.state.destroying) == 0 {
		g.mu.Unlock()
		return false
	}
	now := time.Now()
	g.state.drainRequested = &now
	holds := g.state.destroyWaitHolds(g.log())
	g.publishLocked()
	g.mu.Unlock()
	return holds
}

// ActiveLabels is the work holding admission open, sorted by label, with a
// count appended when a label is held more than once.
func (g *Gate) ActiveLabels() []string {
	blockers := g.Blockers()
	labels := make([]string, len(blockers))
	for i := range blockers {
		labels[i] = blockers[i].countedLabel()
	}
	return labels
}

// Blockers is the work holding admission open, sorted by label, with how
// long each label's oldest hold has run. A waiting handoff names these.
func (g *Gate) Blockers() []Blocker {
	g.mu.Lock()
	defer g.mu.Unlock()
	return g.state.blockers()
}

// TryClose closes admission when nothing but abandoned destroys holds it.
// Called only for automatic replacement, never explicit Stop. A refusal
// starts or extends draining.
func (g *Gate) TryClose() bool {
	g.mu.Lock()
	if g.state.closed {
		g.mu.Unlock()
		return true
	}
	if !g.state.holdsOnlyDestroysOrNothing() ||
		(len(g.state.destroying) > 0 && g.state.destroyWaitHolds(g.log())) {
		report := g.state.refusedHandoffReport()
		now := time.Now()
		g.state.drainRequested = &now
		g.publishLocked()
		g.mu.Unlock()
		if report != nil {
			report.log(g.log())
		}
		return false
	}
	// Logged on every close, so a handoff's start is on record even when
	// nothing made it wait.
	waited := 0.0
	if g.state.handoffWait != nil {
		waited = time.Since(g.state.handoffWait[0]).Seconds()
		g.state.handoffWait = nil
	}
	g.log().Info("daemon upgrade handoff closed admission; the daemon is shutting down", "waited_seconds", waited)
	g.state.closed = true
	g.publishLocked()
	g.mu.Unlock()
	return true
}

var processGate = sync.OnceValue(NewGate)

// UpgradeGate is the process's admission gate (Rust's upgrade::gate).
func UpgradeGate() *Gate { return processGate() }

func UpgradeActivity(label string) (*UpgradeWork, error) { return UpgradeGate().Enter(label) }

func UpgradeStartupRecovery() (*UpgradeWork, error) { return UpgradeActivity(startupRecoveryLabel) }

// UpgradeWaitForStartupRecovery observes the startup hold used by handoff
// without taking admission.
func UpgradeWaitForStartupRecovery(ctx context.Context) error {
	return UpgradeGate().waitForStartupRecovery(ctx)
}

func UpgradeActivityUnlessDraining(label string) (*UpgradeWork, error) {
	return UpgradeGate().EnterUnlessDraining(label)
}

func UpgradeActivityDescribed(label string, describer Describe) (*UpgradeWork, error) {
	return UpgradeGate().EnterDescribed(label, describer)
}

func UpgradeActivityUnlessDrainingDescribed(label string, describer Describe) (*UpgradeWork, error) {
	return UpgradeGate().EnterUnlessDrainingDescribed(label, describer)
}

func UpgradeDestroyActivity(sessionID string) (*UpgradeWork, error) {
	return UpgradeGate().EnterDestroy(sessionID)
}

func UpgradeDestroyActivityUnlessDraining(sessionID string) (*UpgradeWork, error) {
	return UpgradeGate().EnterDestroyUnlessDraining(sessionID)
}

func UpgradeIsDraining() bool { return UpgradeGate().IsDraining() }

func UpgradeActiveLabels() []string { return UpgradeGate().ActiveLabels() }

// UpgradeBlockers is what a waiting handoff names: each blocking label with
// its count and age, and the oldest few holds that can describe themselves.
func UpgradeBlockers() []Blocker { return UpgradeGate().Blockers() }
