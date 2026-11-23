//go:build !windows

package githubrelevance

// Port of mj-github-relevance/src/github_student/runtime.rs: Rust defines
// the DirectX 12 probe only on Windows.
func directx12Available() bool { return false }

// checkWindowsRuntime: only Windows needs the Visual C++ runtime.
func checkWindowsRuntime() error { return nil }

// preloadWindowsLibraries: only Windows preloads the runtime's DLLs.
func preloadWindowsLibraries(string) error { return nil }
