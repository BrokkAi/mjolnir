// Port of mj-worker/src/worker_runtime/unix.rs: a Unix worker other than
// Linux or macOS has no suspend signal, so its wait never ends (Rust's
// pending future) and the finish is unreachable.

//go:build unix && !linux && !darwin

package worker

import (
	"context"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type SuspendSignal struct{}

func InstallSuspendSignal(string) (*SuspendSignal, error) { return nil, nil }

func waitForSuspend(ctx context.Context, _ *SuspendSignal) (core.SuspendRequest, error) {
	<-ctx.Done()
	return core.SuspendRequest{}, ctx.Err()
}

func finishSuspend(context.Context, string, string, core.SuspendRequest) error {
	panic("unreachable: an unsupported suspend signal is always pending")
}
