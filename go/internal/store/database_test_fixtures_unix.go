// Port of the Unix-only part of mj-store/src/database/test_fixtures.rs.

//go:build unix

package store

// WorkerTestObservation is the shared owner for fixtures exercising process
// mutation without a daemon: a worker-observation admission from
// WorkerTestGate, nil when the gate refuses it.
func WorkerTestObservation(id string) (*WorkerPermit, error) {
	panic("unported: mj-store/src/database/test_fixtures.rs worker_test_observation")
}
