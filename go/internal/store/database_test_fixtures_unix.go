// Port of the Unix-only part of mj-store/src/database/test_fixtures.rs.

//go:build unix

package store

import (
	"context"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// WorkerTestObservation is the shared owner for fixtures exercising process
// mutation without a daemon: a worker-observation admission from
// WorkerTestGate, nil when the gate refuses it. Inside an operation that ctx
// carries for id, the permit is that operation's own (Rust's
// WorkerPermit::acquire consults the current owner).
func WorkerTestObservation(ctx context.Context, id string) (*WorkerPermit, error) {
	admission, refusal := WorkerTestGate().TryStart(id, core.BackgroundWorkWorkerObservation)
	if refusal != nil {
		return nil, nil
	}
	// The permit holds its own handle on the admission; Rust drops this one
	// when the function returns.
	defer admission.Release()
	return WorkerPermitAcquire(ctx, id, admission)
}
