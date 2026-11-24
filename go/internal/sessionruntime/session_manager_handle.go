// Port of mj-session-runtime/src/session_manager/handle.rs (only the type a
// dashboard holds so far).

package sessionruntime

// SessionManagerControl is the cloneable command side of a session manager:
// its recovery gate, credential wakes, model policies, CPU table, command
// channel and launch registrations. The fields arrive with the session
// manager port.
type SessionManagerControl struct{}
