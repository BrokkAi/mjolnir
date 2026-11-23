//go:build !linux && !darwin && !windows

package githubrelevance

// Port of mj-github-relevance/src/github_student/inference.rs: sysinfo
// reports nothing on other hosts, which runtime.rs's target() refuses
// before any model loads.
func availableMemory() (uint64, error) { return 0, nil }
