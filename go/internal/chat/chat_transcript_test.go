// Tests of mj-chat/src/chat/transcript.rs (chat/transcript/tests.rs) and its
// test-only items.

package chat

func (s *TranscriptSnapshot) richTail(width int, maximumLines int) []Line {
	lines, _ := s.RichTailScrolled(width, maximumLines, 0, DefaultToolDisplay())
	return lines
}
