// Port of mj-server/src/server.rs (only the items the CLI uses so far).

package server

// UpgradeHeader is the header, valued `pending`, that marks a 503 as "this
// daemon is being replaced by an automatic upgrade; ask the next one". The
// request was not accepted, or was a read the shutdown ended, so asking
// again repeats nothing.
const UpgradeHeader = "x-mj-upgrade"
