// Port of mj-chat/src/chat/status.rs (test-only items).

package chat

func (c *ChatState) currentMode() (string, bool) {
	return c.acpSurface.CurrentMode()
}
