// Test-only setup of mj-chat/src/golden.rs.

package chat

// Rust normalizes clocked activity in buffer_lines under #[cfg(test)].
func init() { bufferLinesTestFilter = normalizeClockedActivity }
