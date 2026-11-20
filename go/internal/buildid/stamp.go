// Port of the WORKER_BUILD_STAMP constant of mj-build-id/src/lib.rs.
//
// go/scripts/build.sh replaces this file through `go build -overlay` with
// one whose constant is the stamp of the build it makes, so the stamp's
// bytes, NULs included, are in the executable as one literal. `-ldflags -X`
// cannot set them: a command-line argument cannot hold a NUL byte.

package buildid

// workerBuildStampLiteral is the worker stamp,
// "\x00MJ-WORKER-BUILD:<WorkerBuildID>\x00". It is empty in a build made
// without go/scripts/build.sh, which therefore cannot serve as a worker.
const workerBuildStampLiteral = ""
