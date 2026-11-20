// Port of mj-worker/src/review.rs.
//
// Target-owned reviewer services: the push-review Git capture
// (review_capture.go) and the repository-local pre-push gate
// (review_hook.go). Rust compiles them on Unix only; they use only Git and
// portable file operations here, so the Go files build everywhere and the
// worker runtime decides where they run.

package worker
