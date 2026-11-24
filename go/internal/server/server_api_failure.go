// Port of mj-server/src/server/api/failure.rs (only the failure codes so far).

package server

// DaemonHandoffCode is the failure code of ApiFailure::handoff: the daemon
// is being replaced by an automatic upgrade.
const DaemonHandoffCode = "daemon_handoff"
