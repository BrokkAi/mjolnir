// Port of mj-worker/src/acp/operator_away.rs: its test-only items.

package worker

// pendingForBridge opens a bridge scope with classification disabled.
func (o *OperatorAway) pendingForBridge() PendingElicitations {
	return o.pendingForBridgeWithSource(VerdictSourceDisabled{}, nil)
}

// DefaultPendingElicitations is the test-only PendingElicitations::default():
// a bridge of a fresh presence owner.
func DefaultPendingElicitations() PendingElicitations {
	return (&OperatorAway{}).pendingForBridge()
}
