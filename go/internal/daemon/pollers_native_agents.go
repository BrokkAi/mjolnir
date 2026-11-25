// Port of mj-daemon/src/pollers/native_agents.rs (only RuntimeFeedHealth so
// far).

package daemon

// RuntimeFeedHealth is what failed in the last runtime feed refresh and
// native agent load.
type RuntimeFeedHealth struct {
	RefreshError *string
	NativeError  *string
}

// Error is the refresh error, else the native agent error.
func (h RuntimeFeedHealth) Error() *string {
	if h.RefreshError != nil {
		return h.RefreshError
	}
	return h.NativeError
}
