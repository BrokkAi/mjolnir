// Port of mj-chat/src/chat/events.rs (test-only items).

package chat

import "encoding/json"

// applySessionUpdate projects one typed ACP update into stable transcript
// items without a receive time (Rust's #[cfg(test)] apply_session_update).
func (s *ChatState) applySessionUpdate(seq uint64, update json.RawMessage) {
	s.applySessionUpdateAt(seq, nil, update)
}
