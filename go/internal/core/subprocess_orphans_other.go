// Port of mj-core/src/subprocess.rs: spawn_child off Linux, where there is
// no orphan reaper (Rust's orphans module is Linux only).

//go:build !linux

package core

import "os/exec"

func spawnChild(command *exec.Cmd) error { return command.Start() }
