// Port of mj-chat/src/chat/active/render.rs.

package chat

import (
	"strings"

	uv "github.com/charmbracelet/ultraviolet"
)

// renderFullFrame draws a chat across a whole frame, the way the combined
// surface lays it out when nothing else is competing for the rows.
//
// Only tests use this: the real surface owns the layout and calls renderIn
// with the bands it chose.
func renderFullFrame(frame *Frame, chat *ChatState, transcriptSelected bool) {
	panic("unported: mj-chat/src/chat/active/render.rs renderFullFrame")
}

// renderIn draws the transcript and the composer into regions.
//
// promptFocused says whether the composer owns the keyboard; only then does
// it draw a cursor and an accent border. transcriptSelected says the
// selection engine still owns a selection on the transcript, so its row space
// has to stay frozen for this frame.
func renderIn(frame *Frame, chat *ChatState, regions ChatRegions, promptFocused bool, transcriptSelected bool) {
	panic("unported: mj-chat/src/chat/active/render.rs renderIn")
}

// renderChatFooter draws the one-row footer under the conversation: the
// reverse-i-search prompt when one is open, else the shared notice, else the
// hotkey hints for the composer.
func renderChatFooter(frame *Frame, footer ChatFooter, chat *ChatState, promptFocused bool) {
	panic("unported: mj-chat/src/chat/active/render.rs renderChatFooter")
}

// testFooter is the footer tests draw. Only tests use this.
func testFooter(area uv.Rectangle) ChatFooter {
	return ChatFooter{
		Area:        area,
		Chords:      []string{"b panes", "q detach", ": / ? palette"},
		ChordPrefix: "ctrl+b then ",
		Functions:   []string{},
	}
}

// renderComposerBand draws the composer band: the title, borders,
// queued-prompt previews, input, and cursor. The full chat render calls this
// for its ordinary prompt, and hosts call it through ChatState.DrawPromptBand
// to show the real composer while a session is not attached.
//
// note adds a left-aligned line to the bottom border — the standby prompt's
// cancel chord; the ordinary render passes nil.
func renderComposerBand(frame *Frame, promptArea uv.Rectangle, chat *ChatState, promptFocused bool, note *Line) {
	panic("unported: mj-chat/src/chat/active/render.rs renderComposerBand")
}

// promptTitle is the prompt's title text. Only tests use this.
func promptTitle(chat *ChatState) string {
	title := strings.Join(promptTitleParts(chat), " · ")
	if title == "" {
		return ""
	}
	return " " + title + " "
}

func promptTitleParts(chat *ChatState) []string {
	panic("unported: mj-chat/src/chat/active/render.rs promptTitleParts")
}

// promptTitleLine: the microphone chip owns the prompt border's upper-left
// corner, followed by model, effort, and state. Activity owns the upper-right
// corner (nil when there is none). A full configured spinner is used when
// both titles fit; the one-column frame keeps narrow prompts readable. The
// model and effort spans are returned as clickable chips that open their
// value selector.
func promptTitleLine(chat *ChatState, promptArea uv.Rectangle) (Line, *Line, []configChipArea) {
	panic("unported: mj-chat/src/chat/active/render.rs promptTitleLine")
}

// promptBottomQueueControl: queue state and the hint for controlling the
// current turn live together on the prompt's bottom border. This stays
// independent of background tasks, so a queued prompt remains visible even
// when there is no task button.
func promptBottomQueueControl(chat *ChatState) *Line {
	panic("unported: mj-chat/src/chat/active/render.rs promptBottomQueueControl")
}

func renderBackgroundTaskDialog(frame *Frame, area uv.Rectangle, chat *ChatState) {
	panic("unported: mj-chat/src/chat/active/render.rs renderBackgroundTaskDialog")
}

// promptContentWidth: the composer keeps a `>` gutter on the left and one
// cell of space on the right.
func promptContentWidth(width int) int {
	return max(width-5, 1)
}

// promptBottomTitles is the shared navigation controls on ordinary composers
// and synchronous review prompts.
func promptBottomTitles(chat *ChatState, promptArea uv.Rectangle, promptFocused bool) []Line {
	panic("unported: mj-chat/src/chat/active/render.rs promptBottomTitles")
}
