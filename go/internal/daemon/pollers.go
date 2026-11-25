// Port of mj-daemon/src/pollers.rs (only what the dashboard holds so far).

// Package daemon is the daemon crate (mj-daemon).
//
// Only the background feeds a dashboard consumes are here so far, declared
// from the Rust source at 3dde0c11b for the Go dashboard (mj-cli re-exports
// them as `crate::pollers`): the wait-then-drain Feed, the remote dashboard
// worker poller and the runtime projections it publishes. The daemon itself
// arrives with milestone 3, in the same files.
//
// Background feeds for the control surfaces. Everything here runs off the
// event loop and reports back over a channel; the loop that consumes them
// never blocks. See Feed for the wait-then-drain shape they all share.
package daemon
