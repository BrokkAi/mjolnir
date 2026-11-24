// Port of mj-daemon/src/pollers/types.rs (only WorkerDiagnosisTracker's
// state so far).

package daemon

// workerDiagnosisEpisode is one stretch of a worker failing.
type workerDiagnosisEpisode struct {
	id        uint64
	error     string
	diagnosed bool
}

// WorkerDiagnosisTracker decides when a failing worker is diagnosed: once
// per failure episode. The zero value is Rust's Default.
type WorkerDiagnosisTracker struct {
	nextEpisode uint64
	current     map[string]workerDiagnosisEpisode
	pending     map[string]uint64
}
