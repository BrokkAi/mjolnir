// Test-only items of mj-chat/src/chat/transcript/render.rs.

package chat

// transcriptLines renders every row of the transcript. Rendering surfaces
// are all incremental now, so this exists only for tests that assert on the
// whole projection.
func transcriptLines(chat *ChatState, width int) []Line {
	prepareRenderCache(
		chat.entries,
		&chat.renderCache,
		width,
		chat.renderMode,
		chat.toolDisplay,
		chat.expandedToolCalls,
		chat.expandedMailboxEvents,
	)
	var lines []Line
	for index := range chat.entries {
		lines = append(lines, cachedEntryLines(chat.entries, &chat.renderCache, index)...)
	}
	trailing := chat.trailingEntries()
	for index := range trailing {
		lines = append(lines, chat.renderTrailingEntry(&trailing[index], width)...)
	}
	if len(lines) == 0 {
		lines = append(lines, emptyTranscriptRow(chat.transcriptLoading))
	}
	return lines
}
