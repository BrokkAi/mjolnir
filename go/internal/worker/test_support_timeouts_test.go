// Port of mj-worker/src/test_support/timeouts.rs.

// Hang guards shared by unit and real-worker integration fixtures.

package worker

import "time"

// ioTimeout covers interpreter startup and temporary-file I/O, which can
// exceed five seconds when worker suites run concurrently. This is a hang
// guard, not an application SLA.
const ioTimeout = 30 * time.Second
