// Port of mj-worker-launch/src/lib.rs.

// Package workerlaunch is what a session's worker is launched with, planned
// from the configuration and the session records: its launch configuration,
// its files on the target, its recovery plan, and the command that
// reconnects to it.
//
// These are functions over core.Config and core.StateState, so the session
// runtime can plan a recovery and the controller a launch from the same
// code. The package builds on worker staging (package workerstaging) and the
// build caches (package buildcache). It is the Go port of the
// mj-worker-launch crate (`brokk-mj-worker-launch`, library
// `mj_worker_launch`).
//
// The Rust crate's modules all live in this one package, one file per Rust
// module: worker_launch.rs is worker_launch.go, local_profile_homes.rs is
// local_profile_homes.go and launch_timing.rs is launch_timing.go. Rust's
// crate-private re-exports (`mj_store::{database, worker_lifecycle}`,
// `mj_targets::{aws_assume_role, aws_credentials, targets}`) have no Go
// equivalent: the files import packages store and targets directly. Rust's
// `targets::*` items that mj-targets re-exports from mj_core::targets are in
// package core (core.CommandSpec, core.TargetsTargetLocator, core.WorkerRoot,
// core.CommandOnLocator, core.ProcessExecutor, ...).
//
// Rust's worker_lifecycle task-local owner is carried by a context.Context
// (see store.Current and store.Require): every function here that reads it,
// directly or through another, takes ctx first.
//
// Library replacements: tracing is log/slog, sysinfo's process listing is
// read from the platform directly (local_profile_homes_processes_*.go), and
// tempfile's staging directory is os.MkdirTemp.
//
// Rust's test-only `enable_test_fixtures_on_startup!` belongs in an init
// function of this package's test files.
package workerlaunch

import (
	"strings"
)

// lossyText is Rust's String::from_utf8_lossy for command output.
func lossyText(data []byte) string {
	return strings.ToValidUTF8(string(data), "�")
}

// rustLines is Rust's str::lines: lines split at `\n`, a trailing `\r`
// removed from each, and no empty last line after a final newline.
func rustLines(text string) []string {
	if text == "" {
		return nil
	}
	lines := strings.Split(strings.TrimSuffix(text, "\n"), "\n")
	for index, line := range lines {
		lines[index] = strings.TrimSuffix(line, "\r")
	}
	return lines
}
