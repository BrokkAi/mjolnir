// Port of mj-worker-review/src/lib.rs.

// Package workerreview holds the target-owned reviewer services: the
// push-review Git capture (capture.go) and the repository-local pre-push
// gate (hook.go). It is the Go port of the mj-worker-review crate
// (`brokk-mj-worker-review`, library `mj_worker_review`), which depends on
// core, checkpoint and review; the worker package imports it.
//
// Rust compiles the crate on Unix only; these files use only Git and
// portable file operations, so they build everywhere and the worker runtime
// decides where they run. The code was first ported while these modules
// were part of mj-worker's package, so names keep the module prefixes they
// had there (captureRun is capture::run).
package workerreview
