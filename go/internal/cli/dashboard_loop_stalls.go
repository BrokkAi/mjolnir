// Port of mj-cli/src/dashboard/loop_stalls.rs.
//
// Logs a dashboard loop pass that kept input waiting.
//
// A pass runs from the moment the loop's select returns to the moment the
// next one starts waiting; input cannot be read in between. Polling the arm
// futures inside the select runs on the same thread and blocks input too,
// so the CPU spent there counts as well. On Linux the thread's scheduler
// statistics split a slow pass into time on a CPU and time spent runnable
// but waiting for one, which is the difference between work the loop did and
// a host too busy to run it.
//
// The scheduler statistics are the OS thread's. Rust's loop runs inside
// block_on on the main thread and never moves; a goroutine does move, so
// the goroutine that runs the loop must hold runtime.LockOSThread for the
// samples to describe it.

package cli

import (
	"log/slog"
	"time"
)

// loopStallsStall: a pass this long, or this much CPU inside one select, is
// a freeze a person can see.
const loopStallsStall = 100 * time.Millisecond

// loopStallsLogInterval: at most one stall line per interval; a host under
// heavy load can stall every pass.
const loopStallsLogInterval = time.Second

// loopStallsPhase is the part of a loop pass the time is charged to.
type loopStallsPhase int

const (
	// loopStallsPhaseArm is the body of the select arm that woke the loop.
	loopStallsPhaseArm loopStallsPhase = iota
	loopStallsPhaseDrainFeeds
	loopStallsPhasePersistence
	loopStallsPhaseActions
	loopStallsPhaseHousekeeping
	loopStallsPhaseDraw
	loopStallsPhaseNotifications
)

const loopStallsPhases = 7

type loopStallsMeter struct {
	sched        *schedstat
	arm          string
	currentPhase loopStallsPhase
	phaseStarted time.Time
	phases       [loopStallsPhases]time.Duration
	passStarted  time.Time
	passSample   *loopStallsSample
	selectSample *loopStallsSample
	// selectCPU is the CPU the thread used inside the select that started
	// this pass.
	selectCPU *time.Duration
	limiter   loopStallsLimiter
}

// newLoopStallsMeter opens the scheduler statistics of the calling thread,
// so it has to be built on the thread that runs the loop (see the file
// comment).
func newLoopStallsMeter() loopStallsMeter {
	sched := openSchedstat()
	now := time.Now()
	return loopStallsMeter{
		sched:        sched,
		arm:          "start",
		currentPhase: loopStallsPhaseHousekeeping,
		phaseStarted: now,
		passStarted:  now,
		passSample:   sched.read(),
	}
}

// phase charges the time since the last switch to the current phase and
// moves on to phase.
func (m *loopStallsMeter) phase(phase loopStallsPhase) {
	m.charge()
	m.currentPhase = phase
}

func (m *loopStallsMeter) charge() time.Time {
	now := time.Now()
	m.phases[m.currentPhase] += now.Sub(m.phaseStarted)
	m.phaseStarted = now
	return now
}

// selectStarted: the loop is about to wait. This ends the pass, which is
// logged when it kept input waiting.
func (m *loopStallsMeter) selectStarted() {
	now := m.charge()
	sample := m.sched.read()
	total := now.Sub(m.passStarted)
	var pass *loopStallsSample
	if sample != nil && m.passSample != nil {
		since := sample.since(*m.passSample)
		pass = &since
	}
	if total >= loopStallsStall || (m.selectCPU != nil && *m.selectCPU >= loopStallsStall) {
		m.report(now, total, pass)
	}
	m.selectSample = sample
}

// woke: the select returned through the arm named arm; this starts the next
// pass.
func (m *loopStallsMeter) woke(arm string) {
	now := time.Now()
	sample := m.sched.read()
	m.selectCPU = nil
	if sample != nil && m.selectSample != nil {
		cpu := sample.since(*m.selectSample).cpu
		m.selectCPU = &cpu
	}
	m.arm = arm
	m.currentPhase = loopStallsPhaseArm
	m.phaseStarted = now
	m.phases = [loopStallsPhases]time.Duration{}
	m.passStarted = now
	m.passSample = sample
}

// forgetPass starts the pass over from now: the time before it was not
// spent reading input, such as startup before the loop or a handover of the
// terminal to another program.
func (m *loopStallsMeter) forgetPass() {
	now := time.Now()
	m.phaseStarted = now
	m.phases = [loopStallsPhases]time.Duration{}
	m.passStarted = now
	m.passSample = m.sched.read()
	m.selectCPU = nil
}

func (m *loopStallsMeter) report(now time.Time, total time.Duration, pass *loopStallsSample) {
	suppressed, ok := m.limiter.admit(now, total)
	if !ok {
		return
	}
	ms := func(duration time.Duration) uint64 { return uint64(duration.Milliseconds()) }
	phase := func(phase loopStallsPhase) uint64 { return ms(m.phases[phase]) }
	// tracing leaves a None field out of the line.
	attrs := []any{"arm", m.arm, "total_ms", ms(total)}
	if pass != nil {
		attrs = append(attrs, "cpu_ms", ms(pass.cpu), "runqueue_ms", ms(pass.runqueue))
	}
	if m.selectCPU != nil {
		attrs = append(attrs, "select_cpu_ms", ms(*m.selectCPU))
	}
	attrs = append(attrs,
		"arm_ms", phase(loopStallsPhaseArm),
		"drain_feeds_ms", phase(loopStallsPhaseDrainFeeds),
		"persistence_ms", phase(loopStallsPhasePersistence),
		"actions_ms", phase(loopStallsPhaseActions),
		"housekeeping_ms", phase(loopStallsPhaseHousekeeping),
		"draw_ms", phase(loopStallsPhaseDraw),
		"notifications_ms", phase(loopStallsPhaseNotifications),
		"suppressed", suppressed.count,
		"suppressed_max_ms", ms(suppressed.max),
	)
	slog.Warn("the dashboard loop was busy", attrs...)
}

// loopStallsLimiter holds stall lines to one per loopStallsLogInterval and
// counts the ones it held.
type loopStallsLimiter struct {
	last *time.Time
	held loopStallsSuppressed
}

type loopStallsSuppressed struct {
	count uint64
	max   time.Duration
}

// admit is whether a stall at now is logged; when it is, what was held since
// the last line.
func (l *loopStallsLimiter) admit(now time.Time, total time.Duration) (loopStallsSuppressed, bool) {
	if l.last != nil && now.Sub(*l.last) < loopStallsLogInterval {
		l.held.count++
		l.held.max = max(l.held.max, total)
		return loopStallsSuppressed{}, false
	}
	l.last = &now
	held := l.held
	l.held = loopStallsSuppressed{}
	return held, true
}

type loopStallsSample struct {
	cpu      time.Duration
	runqueue time.Duration
}

func (s loopStallsSample) since(start loopStallsSample) loopStallsSample {
	return loopStallsSample{
		cpu:      max(s.cpu-start.cpu, 0),
		runqueue: max(s.runqueue-start.runqueue, 0),
	}
}
