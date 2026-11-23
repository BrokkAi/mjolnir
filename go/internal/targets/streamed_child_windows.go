//go:build windows

// Port of mj-targets/src/streamed_child.rs: Windows has no process group to
// set (Rust sets one only under `cfg(unix)`).

package targets

import "os/exec"

func setProxyProcessGroup(*exec.Cmd) {}
