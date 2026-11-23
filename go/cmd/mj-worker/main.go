// Port of mj-worker-bin/src/main.rs: the executable's entry point. The
// command line and everything it runs are in package workerbin, so its tests
// can call them; this file only does what Rust's main does before anything
// else: raise the open-file limit and install the build identity.
//
// Rust's musl builds also swap in mimalloc as the global allocator; Go's
// allocator has no such switch, so nothing replaces it here.

// Command mj-worker is the portable target-side entry point for Mjolnir
// session workers.
package main

import (
	"os"

	"github.com/BrokkAi/mjolnir/go/internal/buildid"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/workerbin"
)

// workerBuildStamp keeps the worker stamp in the executable: the controller
// reads the identity from the file's bytes (core.WorkerBuildFromBytes)
// before it runs a worker, as Rust's `black_box(WORKER_BUILD_STAMP)` keeps it.
var workerBuildStamp string

func main() {
	// First, before any child: the harness and its tools inherit it.
	core.RaiseOpenFileLimit()
	core.Install(buildid.Identity())
	// A build without go/scripts/build.sh's stamp must not run as a worker;
	// WorkerBuildStamp panics in one.
	workerBuildStamp = buildid.WorkerBuildStamp()
	os.Exit(workerbin.Main())
}
