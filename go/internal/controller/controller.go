// Port of mj-controller/src/controller.rs (only the store ownership guard
// so far).

// Package controller is the daemon's session controller (mj-controller).
//
// Only ControllerStoreGuard is here so far, declared from the Rust source at
// 3dde0c11b because the `mj` CLI probes store ownership with it before it
// starts or replaces a daemon. The rest of the crate arrives with the Go
// daemon (milestone 3), in the same files. Items mj-controller re-exports
// from other crates are used from their own Go packages
// (store.CheckReadCompatibility, store.DatabasePath,
// workerstaging.WarmWorkerBinarySources, ...).
package controller

import (
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ControllerStoreGuard is the machine-wide advisory lock for one controller
// data store. This prevents a dashboard, server, or CLI lifecycle command
// from concurrently acting as a second controller against the same SQLite
// state and relay sessions.
//
// Rust releases it on drop; Go callers Release it (or defer Drop) on every
// path.
type ControllerStoreGuard struct {
	file *core.FileLock
}

// StoreAlreadyOwnedError is returned when another controller owns the
// store.
type StoreAlreadyOwnedError struct {
	DataDir string
}

func (e *StoreAlreadyOwnedError) Error() string {
	return fmt.Sprintf("another Mjolnir controller is already using %s; stop it before starting this command", e.DataDir)
}

// ControllerStoreGuardAcquire takes the guard for this instance's data
// directory, or fails with StoreAlreadyOwnedError.
func ControllerStoreGuardAcquire() (*ControllerStoreGuard, error) {
	return controllerStoreGuardAcquireAt(core.DataDir())
}

func controllerStoreGuardAcquireAt(directory string) (*ControllerStoreGuard, error) {
	guard, err := controllerStoreGuardTryAcquireAt(directory)
	if err != nil {
		return nil, err
	}
	if guard == nil {
		return nil, &StoreAlreadyOwnedError{DataDir: directory}
	}
	return guard, nil
}

// ControllerStoreGuardTryAcquire probes exclusivity without treating an
// owner that is still exiting as an error: nil, nil when another process
// holds the store.
func ControllerStoreGuardTryAcquire() (*ControllerStoreGuard, error) {
	return controllerStoreGuardTryAcquireAt(core.DataDir())
}

func controllerStoreGuardTryAcquireAt(directory string) (*ControllerStoreGuard, error) {
	panic("unported: mj-controller/src/controller.rs ControllerStoreGuard::try_acquire_at")
}

// Release releases the store now and reports an unlock failure.
func (g *ControllerStoreGuard) Release() error {
	return g.file.Release()
}

// Drop is Rust's Drop: release if still held, logging a failure.
func (g *ControllerStoreGuard) Drop() {
	g.file.Drop()
}
