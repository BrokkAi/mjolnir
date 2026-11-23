// Port of the test-only helpers of mj-worker/src/acp/drive.rs.

package worker

import (
	"context"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// drive runs one bridge's protocol over transport. Tests own their
// connection registry here; production holds it through process teardown.
func drive(ctx context.Context, transport ByteStreams, spec LaunchSpec, requests <-chan CommandRequest, events chan<- core.RuntimeEvent, opened *Shared[*OpenedSession], replacingPreviousBridge bool) (SessionRestart, error) {
	var firstReplacedUnusedNativeSessionID *string
	return driveBridge(ctx, transport, spec, requests, events, opened, BridgeSessionOpen{
		replacingPreviousBridge:            replacingPreviousBridge,
		replaceUnusedCodexThread:           shouldReplaceUnusedCodexThread(&spec),
		firstReplacedUnusedNativeSessionID: &firstReplacedUnusedNativeSessionID,
	})
}

func driveBridge(ctx context.Context, transport ByteStreams, spec LaunchSpec, requests <-chan CommandRequest, events chan<- core.RuntimeEvent, opened *Shared[*OpenedSession], sessionOpen BridgeSessionOpen) (SessionRestart, error) {
	guardian := guardianForLaunch(&spec, nil)
	terminals, err := NewTerminalRegistry()
	if err != nil {
		return nil, err
	}
	restart, err := driveWithTerminals(ctx, transport, spec, requests, events, opened, sessionOpen,
		BridgeOwned{terminals: terminals, guardian: guardian})
	if guardian != nil {
		guardian.shutdown(context.WithoutCancel(ctx))
	}
	return restart, err
}
