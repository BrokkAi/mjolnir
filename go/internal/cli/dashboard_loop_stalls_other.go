// Port of mj-cli/src/dashboard/loop_stalls.rs (Schedstat off Linux).

//go:build !linux

package cli

// schedstat has no source off Linux; openSchedstat is always nil.
type schedstat struct{}

func openSchedstat() *schedstat {
	return nil
}

func (s *schedstat) read() *loopStallsSample {
	return nil
}
