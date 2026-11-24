// Part of the port of mj-worker-staging/src/worker_binary/tests.rs: the
// hand-written command executor it and install.rs's test modules share,
// standing in for their one-off CommandExecutor impls.

package workerstaging

import (
	"io"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Rust: `#[cfg(test)] mj_core::enable_test_fixtures_on_startup!();` in
// lib.rs. Another init of this package's tests may enable them too; doing
// so twice is harmless.
func init() { core.EnableTestFixtures() }

// workerTestExecutor is a CommandExecutor whose behaviour is a set of
// functions. Each field overrides the trait method it names. Unset fields
// keep what the Rust test impls write by hand: the no-deadline methods
// forward to their deadline-bound forms, cancellation without a deadline is
// ordinary cancellation, and the deadline pause is empty.
type workerTestExecutor struct {
	core.CommandExecutorDefaults
	execute                         func(*core.CommandSpec) (core.CommandOutput, error)
	executeWithStdin                func(*core.CommandSpec, io.Reader) (core.CommandOutput, error)
	executeWithoutDeadline          func(*core.CommandSpec) (core.CommandOutput, error)
	executeWithStdinWithoutDeadline func(*core.CommandSpec, io.Reader) (core.CommandOutput, error)
	cancelled                       func() bool
	cancelledWithoutDeadline        func() bool
	pauseDeadline                   func() *core.DeadlinePause
	yieldAdmission                  func() bool
	endResumable                    func(label string) error
	notice                          func(notice string)
	stageStarted                    func(stage core.ProvisionStage)
	stageFinished                   func(stage core.ProvisionStage)
}

func (e *workerTestExecutor) Execute(command *core.CommandSpec) (core.CommandOutput, error) {
	if e.execute == nil {
		panic("workerTestExecutor: no execute")
	}
	return e.execute(command)
}

func (e *workerTestExecutor) ExecuteCleanup(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.Execute(command)
}

func (e *workerTestExecutor) ExecuteCleanupWithTimeout(command *core.CommandSpec, _ time.Duration) (core.CommandOutput, error) {
	return e.ExecuteCleanup(command)
}

func (e *workerTestExecutor) ExecuteWithStdin(command *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
	if e.executeWithStdin == nil {
		return e.CommandExecutorDefaults.ExecuteWithStdin(command, input)
	}
	return e.executeWithStdin(command, input)
}

func (e *workerTestExecutor) ExecuteWithoutDeadline(command *core.CommandSpec) (core.CommandOutput, error) {
	if e.executeWithoutDeadline != nil {
		return e.executeWithoutDeadline(command)
	}
	return e.Execute(command)
}

func (e *workerTestExecutor) ExecuteWithStdinWithoutDeadline(command *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
	if e.executeWithStdinWithoutDeadline != nil {
		return e.executeWithStdinWithoutDeadline(command, input)
	}
	return e.ExecuteWithStdin(command, input)
}

func (e *workerTestExecutor) CancellationRequested() bool {
	if e.cancelled != nil {
		return e.cancelled()
	}
	return false
}

func (e *workerTestExecutor) CancellationRequestedWithoutDeadline() bool {
	if e.cancelledWithoutDeadline != nil {
		return e.cancelledWithoutDeadline()
	}
	return e.CancellationRequested()
}

func (e *workerTestExecutor) PauseDeadline() *core.DeadlinePause {
	if e.pauseDeadline != nil {
		return e.pauseDeadline()
	}
	return core.DeadlinePauseNone()
}

func (e *workerTestExecutor) YieldAdmissionIfResumable() bool {
	if e.yieldAdmission != nil {
		return e.yieldAdmission()
	}
	return false
}

func (e *workerTestExecutor) EndResumableWork(label string) error {
	if e.endResumable != nil {
		return e.endResumable(label)
	}
	return nil
}

func (e *workerTestExecutor) NotifyNotice(notice string) {
	if e.notice != nil {
		e.notice(notice)
	}
}

func (e *workerTestExecutor) StageStarted(stage core.ProvisionStage) {
	if e.stageStarted != nil {
		e.stageStarted(stage)
	}
}

func (e *workerTestExecutor) StageFinished(stage core.ProvisionStage) {
	if e.stageFinished != nil {
		e.stageFinished(stage)
	}
}

// fixedOutput is an executor result with status and the given output.
func commandOutputOf(status int, stdout, stderr string) core.CommandOutput {
	return core.CommandOutput{Status: status, Stdout: []byte(stdout), Stderr: []byte(stderr)}
}
