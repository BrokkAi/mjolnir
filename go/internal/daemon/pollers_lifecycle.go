// Port of mj-daemon/src/pollers/lifecycle.rs (only LifecycleSuccess so far).

package daemon

import "github.com/BrokkAi/mjolnir/go/internal/core"

// LifecycleSuccess is how a lifecycle operation ended when it succeeded.
type LifecycleSuccess interface{ isLifecycleSuccess() }

type LifecycleSuccessCreated struct{}

type LifecycleSuccessResumed struct {
	ProfileID string
	TargetID  string
}

type LifecycleSuccessMoved struct{ Outcome core.MoveOutcome }

type LifecycleSuccessClosed struct{}

type LifecycleSuccessDestroyedStopped struct{}

type LifecycleSuccessForceDestroyed struct{}

func (LifecycleSuccessCreated) isLifecycleSuccess()          {}
func (LifecycleSuccessResumed) isLifecycleSuccess()          {}
func (LifecycleSuccessMoved) isLifecycleSuccess()            {}
func (LifecycleSuccessClosed) isLifecycleSuccess()           {}
func (LifecycleSuccessDestroyedStopped) isLifecycleSuccess() {}
func (LifecycleSuccessForceDestroyed) isLifecycleSuccess()   {}
