// Port of mj-github-relevance/src/github_student/concurrent.rs.

// Concurrent CPU Run adapter. No session configuration is changed after
// loading.
//
// Rust calls ORT's raw C API here (its Output and Shape guards release the
// per-call output and its metadata, and checked turns a status into an
// error) because ort's safe Run needs exclusive access. The Go binding in
// onnxruntime.go owns those releases and status checks, and its run is the
// raw Run, so only score remains.

package githubrelevance

// score is the student's probability for one tokenized pair on a CPU
// session. ORT explicitly permits concurrent Run on one immutable session
// (https://onnxruntime.ai/docs/reference/high-level-design.html). Only CPU
// sessions enter this adapter; DirectML sessions are run one at a time
// under their gpu's lock.
func score(session *ortSession, ids, mask []int64) (float64, error) {
	panic("unported: mj-github-relevance/src/github_student/concurrent.rs score")
}
