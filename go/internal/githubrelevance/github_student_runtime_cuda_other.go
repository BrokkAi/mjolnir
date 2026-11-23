//go:build !linux || !amd64 || android

package githubrelevance

// Port of mj-github-relevance/src/github_student/runtime.rs: only x86-64
// Linux probes CUDA in select.
func probeCUDA() (missing []string, driver bool) {
	return nil, false
}
