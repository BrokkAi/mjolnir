// Port of brokk-sessionwiki 0.34.28 src/adapters/claude_code.rs.

package brokksessionwiki

// ClaudeCode reads Claude Code transcripts.
type ClaudeCode struct {
	unportedAdapter
}

// ClaudeCodeInHome is the transcripts under one Claude Code home.
func ClaudeCodeInHome(home string) *ClaudeCode {
	panic("unported: brokk-sessionwiki src/adapters/claude_code.rs ClaudeCode::in_home")
}
