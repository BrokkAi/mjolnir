// Port of the golden_cases module of mj-chat/src/chat/tests.rs.

package chat

import (
	"encoding/json"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
	"github.com/BrokkAi/acp-go/schema"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const (
	goldenWidth  = 100
	goldenHeight = 24
)

func goldenRendered(chat *ChatState, width, height int) string {
	// The task dialog prints elapsed time. A future start keeps that
	// user-facing clock at 0s in every checked-in render.
	for i := range chat.sessionActivity.BackgroundCommands {
		chat.sessionActivity.BackgroundCommands[i].StartedAtMs = math.MaxInt64
	}
	terminal := newComponentTerminal(width, height)
	terminal.draw(func(frame *Frame) { renderFullFrame(frame, chat, false) })
	return strings.Join(BufferLines(terminal.buffer), "\n")
}

// goldenState appends one labelled render, the action that led to it (nil
// for none), and the details read from the chat after drawing.
func goldenState(output *strings.Builder, label string, chat *ChatState, width, height int, action ChatAction, details func(*ChatState) []string) {
	if output.Len() > 0 {
		output.WriteString("\n")
	}
	fmt.Fprintf(output, "=== %s (%dx%d) ===\n", label, width, height)
	output.WriteString(goldenRendered(chat, width, height))
	output.WriteString("\n")
	if action != nil {
		fmt.Fprintf(output, "action: %s\n", chatActionDebug(action))
	}
	for _, detail := range details(chat) {
		output.WriteString(detail + "\n")
	}
}

func noDetails(*ChatState) []string { return nil }

func saveChatGolden(t *testing.T, name, output string) {
	t.Helper()
	core.AssertPlatformGolden(t, "testdata", name, output)
}

// chatActionDebug is Rust's derived Debug of a ChatAction, for the variants
// these goldens print.
func chatActionDebug(action ChatAction) string {
	switch action := action.(type) {
	case ChatActionNone:
		return "None"
	case ChatActionPrompt:
		return "Prompt(" + rustDebugString(action.Text) + ")"
	case ChatActionSetConfig:
		return fmt.Sprintf("SetConfig { key: %s, value: %s }", rustDebugString(action.Key), rustDebugString(action.Value))
	case ChatActionRemoveQueuedPrompt:
		return fmt.Sprintf("RemoveQueuedPrompt { id: %s, text: %s, kind: %s }",
			rustDebugString(action.ID), rustDebugString(action.Text), debugQueuedCommandKind(action.Kind))
	case ChatActionPlanCommand:
		return fmt.Sprintf("PlanCommand { original: %s, control: %s, requested_active: %t, prompt: %s }",
			rustDebugString(action.Original), debugPlanControl(action.Control), action.RequestedActive, debugOptionalText(action.Prompt))
	case ChatActionStopBackgroundTask:
		return fmt.Sprintf("StopBackgroundTask { id: %s }", rustDebugString(action.ID))
	case ChatActionAttach:
		return fmt.Sprintf("Attach { path: %s, command: %s }", debugPastedPath(action.Path), rustDebugString(action.Command))
	case ChatActionToggleVoice:
		return "ToggleVoice"
	case ChatActionOpenSubagents:
		return "OpenSubagents"
	case ChatActionOpenReviewers:
		return "OpenReviewers"
	case ChatActionPasteFromClipboard:
		return "PasteFromClipboard"
	case ChatActionCancel:
		return "Cancel"
	case ChatActionQuitDetach:
		return "QuitDetach"
	case ChatActionStartTurnReview:
		return "StartTurnReview"
	case ChatActionCycleFocus:
		return fmt.Sprintf("CycleFocus { reverse: %t }", action.Reverse)
	}
	return fmt.Sprintf("%#v", action)
}

func chatActionsDebug(actions []ChatAction) string {
	parts := make([]string, 0, len(actions))
	for _, action := range actions {
		parts = append(parts, chatActionDebug(action))
	}
	return "[" + strings.Join(parts, ", ") + "]"
}

func debugQueuedCommandKind(kind core.QueuedCommandKind) string {
	switch kind := kind.(type) {
	case nil, core.QueuedCommandKindPrompt:
		return "Prompt"
	case core.QueuedCommandKindSetConfig:
		return fmt.Sprintf("SetConfig { key: %s, value: %s }", rustDebugString(kind.Key), rustDebugString(kind.Value))
	}
	return fmt.Sprintf("%#v", kind)
}

func debugPlanControl(control PlanControl) string {
	switch control := control.(type) {
	case core.PlanControlSetConfig:
		return fmt.Sprintf("SetConfig { key: %s, value: %s }", rustDebugString(control.Key), rustDebugString(control.Value))
	case core.PlanControlSetSessionMode:
		return fmt.Sprintf("SetSessionMode { mode_id: %s }", rustDebugString(control.ModeID))
	case core.PlanControlRestoreExecutionMode:
		return "RestoreExecutionMode"
	}
	return fmt.Sprintf("%#v", control)
}

func debugPastedPath(path PastedPath) string {
	switch path := path.(type) {
	case PastedPathLocal:
		return "Local(" + rustDebugString(path.Path) + ")"
	case PastedPathWindows:
		return "Windows(" + rustDebugString(path.Path) + ")"
	}
	return fmt.Sprintf("%#v", path)
}

func debugOptionalText(text *string) string {
	if text == nil {
		return "None"
	}
	return "Some(" + rustDebugString(*text) + ")"
}

func debugOptionalRect(area *uv.Rectangle) string {
	if area == nil {
		return "None"
	}
	return "Some(" + debugRect(*area) + ")"
}

func debugStrings(values []string) string {
	parts := make([]string, 0, len(values))
	for _, value := range values {
		parts = append(parts, rustDebugString(value))
	}
	return "[" + strings.Join(parts, ", ") + "]"
}

func debugRenderMode(mode client.TranscriptRenderMode) string {
	if mode == client.TranscriptRenderModeRaw {
		return "Raw"
	}
	return "Rich"
}

func debugPlanReviewFollowup(followup *PlanReviewFollowup) string {
	if followup == nil {
		return "None"
	}
	control := "None"
	if followup.control != nil {
		control = "Some(" + debugPlanControl(followup.control) + ")"
	}
	return fmt.Sprintf("Some(PlanReviewFollowup { desired_active: %t, control: %s, prompt: %s })",
		followup.desiredActive, control, debugOptionalText(followup.prompt))
}

// harnessDebugNames are Rust's Debug names of the harness kinds these
// goldens print.
var harnessDebugNames = map[core.HarnessKind]string{
	core.HarnessKindClaude: "Claude",
	core.HarnessKindKimi:   "Kimi",
}

func TestGoldenChatImagePromptComposer(t *testing.T) {
	var output strings.Builder

	chat := newTestChat()
	chat.setPromptImagesSupported(true)
	chat.setInput("compare  and this")
	chat.inputCursor = len("compare ")
	chat.handleClipboardContent(ClipboardContentImage{Image: testImage()})
	goldenState(&output, "paste image at cursor", chat, goldenWidth, goldenHeight, nil, func(chat *ChatState) []string {
		return []string{
			"draft: " + chat.input,
			fmt.Sprintf("image count: %d", len(chat.inputImages)),
		}
	})
	chat.HandleKey(key(tea.KeyEnd))
	chat.handleClipboardContent(ClipboardContentImage{Image: testImage()})
	payload := chat.draftPayload()
	action := chat.HandleKey(key(tea.KeyEnter))
	submitted := chat.takeSubmittingImages()
	submittedMatches := rustEqual(submitted, payload.Images)
	blocks := payload.ContentBlocks()
	markerBlocksMatch := len(blocks) == 4 &&
		blocks[0].Text != nil && blocks[1].Image != nil &&
		blocks[2].Text != nil && blocks[3].Image != nil
	goldenState(&output, "submit image markers at their cursor positions", chat, goldenWidth, goldenHeight, action, func(*ChatState) []string {
		return []string{
			fmt.Sprintf("submitted image count: %d", len(submitted)),
			fmt.Sprintf("submitted images match draft payload: %t", submittedMatches),
			fmt.Sprintf("marker content-block layout preserved: %t", markerBlocksMatch),
			"payload: " + payload.Text,
		}
	})

	chat = newTestChat()
	chat.setPromptImagesSupported(true)
	chat.setInput("keep ")
	chat.handleClipboardContent(ClipboardContentImage{Image: testImage()})
	cursorDetail := func(chat *ChatState) []string { return []string{fmt.Sprintf("cursor: %d", chat.inputCursor)} }
	goldenState(&output, "image marker insertion", chat, goldenWidth, goldenHeight, nil, cursorDetail)
	end := chat.inputCursor
	chat.HandleKey(key(tea.KeyLeft))
	goldenState(&output, "left skips the whole marker", chat, goldenWidth, goldenHeight, nil, cursorDetail)
	chat.HandleKey(key(tea.KeyRight))
	goldenState(&output, "right skips the whole marker", chat, goldenWidth, goldenHeight, nil, func(chat *ChatState) []string {
		return []string{
			fmt.Sprintf("cursor at original marker end: %t", chat.inputCursor == end),
			fmt.Sprintf("cursor: %d", chat.inputCursor),
		}
	})
	chat.HandleKey(key(tea.KeyBackspace))
	goldenState(&output, "backspace removes the whole marker", chat, goldenWidth, goldenHeight, nil, func(chat *ChatState) []string {
		return []string{
			fmt.Sprintf("cursor after deletion: %d", chat.inputCursor),
			fmt.Sprintf("remaining images: %d", len(chat.inputImages)),
		}
	})
	chat.handleClipboardContent(ClipboardContentImage{Image: testImage()})
	chat.HandleKey(key(tea.KeyLeft))
	chat.HandleKey(key(tea.KeyDelete))
	goldenState(&output, "delete removes the whole marker", chat, goldenWidth, goldenHeight, nil, func(chat *ChatState) []string {
		return []string{fmt.Sprintf("remaining images: %d", len(chat.inputImages))}
	})

	chat = newTestChat()
	chat.setPromptImagesSupported(true)
	chat.handleClipboardContent(ClipboardContentImage{Image: testImage()})
	chat.HandleKey(ctrl('u'))
	goldenState(&output, "kill removes image with its marker", chat, goldenWidth, goldenHeight, nil, func(chat *ChatState) []string {
		return []string{fmt.Sprintf("images after kill: %d", len(chat.inputImages))}
	})
	chat.HandleKey(ctrl('y'))
	chat.HandleKey(ctrl('y'))
	goldenState(&output, "yank copies and renumbers images", chat, goldenWidth, goldenHeight, nil, func(chat *ChatState) []string {
		return []string{
			fmt.Sprintf("image count: %d", len(chat.inputImages)),
			fmt.Sprintf("copies share image data: %t", chat.inputImages[0].Image.Equal(&chat.inputImages[1].Image)),
		}
	})

	chat = newTestChat()
	chat.setPromptImagesSupported(true)
	chat.handleClipboardContent(ClipboardContentImage{Image: testImage()})
	goldenState(&output, "numbered marker and paste hint", chat, goldenWidth, goldenHeight, nil, func(*ChatState) []string {
		return []string{"clipboard action: PasteFromClipboard"}
	})
	chat.HandleKey(key(tea.KeyBackspace))
	goldenState(&output, "marker removed from composer", chat, goldenWidth, goldenHeight, nil, func(chat *ChatState) []string {
		return []string{fmt.Sprintf("marker remains: %t", strings.Contains(chat.input, "[image"))}
	})

	for _, command := range []string{"!pwd ", "/help ", "/model ", "/plan inspect "} {
		chat := newTestChat()
		chat.setPromptImagesSupported(true)
		chat.setInput(command)
		chat.handleClipboardContent(ClipboardContentImage{Image: testImage()})
		before := chat.draftPayload()
		action := chat.HandleKey(key(tea.KeyEnter))
		goldenState(&output, fmt.Sprintf("image retained by %s", rustDebugString(command)), chat, goldenWidth, goldenHeight, action, func(chat *ChatState) []string {
			after := chat.draftPayload()
			return []string{
				fmt.Sprintf("draft unchanged: %t", rustEqual(after, before)),
				fmt.Sprintf("image count: %d", len(chat.inputImages)),
			}
		})
	}

	chat = newTestChat()
	chat.setPromptImagesSupported(true)
	action = chat.handleTerminalPaste("")
	goldenState(&output, "empty terminal paste requests clipboard", chat, goldenWidth, goldenHeight, action, noDetails)
	chat.handleClipboardContent(ClipboardContentImage{Image: testImage()})
	action = chat.HandleKey(key(tea.KeyEnter))
	submitted = chat.takeSubmittingImages()
	goldenState(&output, "clipboard image becomes a prompt attachment", chat, goldenWidth, goldenHeight, action, func(*ChatState) []string {
		return []string{fmt.Sprintf("submitted image count: %d", len(submitted))}
	})

	chat = newTestChat()
	var actions []ChatAction
	for _, text := range []string{"hello", " \t\n", "world"} {
		actions = append(actions, chat.handleTerminalPaste(text))
	}
	chat.handleClipboardContent(ClipboardContentText{Text: ""})
	goldenState(&output, "text paste bypasses clipboard image lookup", chat, goldenWidth, goldenHeight, ChatActionNone{}, func(chat *ChatState) []string {
		return []string{
			"text-paste actions: " + chatActionsDebug(actions),
			"text remains: " + rustDebugString(chat.input),
		}
	})

	chat = newTestChat()
	chat.setConfigOptions([]schema.SessionConfigOption{selectConfigOption("model", "small", []string{"small", "large"})})
	chat.openConfigPicker("model")
	action = chat.handleTerminalPaste("")
	goldenState(&output, "empty paste stays in the active config picker", chat, goldenWidth, goldenHeight, action, func(chat *ChatState) []string {
		return []string{fmt.Sprintf("picker active: %t", chat.configPickerActive())}
	})

	chat = newTestChat()
	chat.setPromptImagesSupported(true)
	chat.handleClipboardContent(ClipboardContentImage{Image: testImage()})
	chat.handlePaste("/attach /workspace/second.png")
	action = chat.HandleKey(key(tea.KeyEnter))
	goldenState(&output, "attach command retains existing marker", chat, goldenWidth, goldenHeight, action, func(chat *ChatState) []string {
		return []string{
			fmt.Sprintf("markers before reservation: %d", len(chat.inputImages)),
			"draft after command: " + rustDebugString(chat.input),
		}
	})
	chat.reserveAttachment(0)
	goldenState(&output, "reserved attachment appends a numbered marker", chat, goldenWidth, goldenHeight, nil, func(chat *ChatState) []string {
		return []string{fmt.Sprintf("markers: %d", len(chat.inputImages))}
	})

	path := filepath.Join(t.TempDir(), "notes.txt")
	if err := os.WriteFile(path, []byte("plain text"), 0o644); err != nil {
		t.Fatalf("write text file: %v", err)
	}
	_, installErr := installPath("attach-text", path)
	if installErr == nil {
		t.Fatal("a text file was installed as an image")
	}
	message := installErr.Error()
	chat = newTestChat()
	chat.handlePaste("/attach notes.txt")
	action = chat.HandleKey(key(tea.KeyEnter))
	goldenState(&output, "text file attachment feedback", chat, goldenWidth, goldenHeight, action, func(*ChatState) []string {
		return []string{
			fmt.Sprintf("installer feedback includes image-only explanation: %t", strings.Contains(message, "only image files can be attached")),
			fmt.Sprintf("installer mentions marker: %t", strings.Contains(message, "marker")),
		}
	})

	saveChatGolden(t, "chat-image-prompt-composer", output.String())
}

func TestGoldenChatComposerDraftLifecycle(t *testing.T) {
	var output strings.Builder

	saved := freshlyOpenedChat("saved before opening")
	goldenState(&output, "fresh chat restores saved draft", saved, goldenWidth, goldenHeight, nil, func(chat *ChatState) []string {
		return []string{"draft: " + rustDebugString(chat.Draft())}
	})
	empty := freshlyOpenedChat("")
	goldenState(&output, "fresh chat without saved draft", empty, goldenWidth, goldenHeight, nil, func(chat *ChatState) []string {
		return []string{fmt.Sprintf("draft empty: %t", chat.Draft() == "")}
	})

	var config core.Config
	if err := json.Unmarshal([]byte(`{"version": 0}`), &config); err != nil {
		t.Fatalf("default config: %v", err)
	}
	chat := ChatStateStandby("session-1", &config, SessionHeaderIdentity{}, NewNotices())
	chat.SetDraft("alpha beta")
	chat.HandleKey(ctrl('a'))
	chat.HandleKey(ctrl('k'))
	chat.HandleKey(ctrl('y'))
	chat.Paste("…\r\nsecond")
	goldenState(&output, "standby composer readline and normalized paste", chat, goldenWidth, goldenHeight, nil, func(chat *ChatState) []string {
		return []string{
			"draft: " + rustDebugString(chat.Draft()),
			fmt.Sprintf("cursor: %d", chat.inputCursor),
		}
	})
	chat.setInput("/help")
	action := chat.HandleKey(key(tea.KeyEnter))
	goldenState(&output, "standby command remains an editable draft", chat, goldenWidth, goldenHeight, action, func(chat *ChatState) []string {
		return []string{
			"draft: " + rustDebugString(chat.Draft()),
			fmt.Sprintf("queued prompts: %d", len(chat.QueuedPromptTexts())),
		}
	})
	chat.setInput("alpha beta…\nsecond")
	action = chat.HandleKey(key(tea.KeyEnter))
	goldenState(&output, "standby prompt is queued for the host", chat, goldenWidth, goldenHeight, action, func(chat *ChatState) []string {
		return []string{
			fmt.Sprintf("draft empty: %t", chat.Draft() == ""),
			"queued prompts: " + debugStrings(chat.QueuedPromptTexts()),
		}
	})
	chat.RemoveQueuedPromptText("alpha beta…\nsecond")
	chat.setInput("/mod")
	chat.updateAutocomplete()
	goldenState(&output, "standby composer does not offer slash autocomplete", chat, goldenWidth, goldenHeight, nil, func(chat *ChatState) []string {
		return []string{fmt.Sprintf("autocomplete active: %t", chat.autocomplete != nil)}
	})

	chat.setInput("describe ")
	chat.handleClipboardContent(ClipboardContentImage{Image: testImage()})
	composer := chat.TakeLiveComposer()
	if composer == nil {
		t.Fatal("no live composer to hand off")
	}
	adopted := ChatStateStandby("session-1", &config, SessionHeaderIdentity{}, NewNotices())
	adopted.AdoptLiveComposer(composer)
	action = adopted.HandleKey(key(tea.KeyEnter))
	goldenState(&output, "image staged before capability discovery survives live handoff and queues with its marker", adopted, goldenWidth, goldenHeight, action, func(chat *ChatState) []string {
		prompt, ok := chat.LatestStartupPrompt()
		if !ok {
			t.Fatal("no startup prompt queued")
		}
		return []string{fmt.Sprintf("queued images: %d", len(prompt.Payload.Images))}
	})

	saveChatGolden(t, "chat-composer-draft-lifecycle", output.String())
}

func TestGoldenChatVoicePromptControl(t *testing.T) {
	var output strings.Builder

	chat := newTestChat()
	unavailable := chat.DictationToggleAction()
	keyAction := chat.HandleKey(alt('v'))
	goldenState(&output, "voice unavailable", chat, goldenWidth, goldenHeight, unavailable, func(chat *ChatState) []string {
		return []string{
			"Alt-V action: " + chatActionDebug(keyAction),
			fmt.Sprintf("draft empty: %t", chat.input == ""),
		}
	})
	chat.setVoiceAvailable(true)
	action := chat.DictationToggleAction()
	keyAction = chat.HandleKey(alt('v'))
	goldenState(&output, "voice available, host owns Alt-V", chat, goldenWidth, goldenHeight, action, func(*ChatState) []string {
		return []string{"Alt-V action: " + chatActionDebug(keyAction)}
	})

	chat = newTestChat()
	chat.setVoiceAvailable(true)
	buttonArea := func(chat *ChatState) []string {
		return []string{"button area: " + debugOptionalRect(chat.voiceButtonArea)}
	}
	goldenState(&output, "enabled microphone and rendered hitbox", chat, goldenWidth, goldenHeight, nil, buttonArea)
	if chat.voiceButtonArea == nil {
		t.Fatal("rendered voice button")
	}
	area := *chat.voiceButtonArea
	down := chat.HandleMouse(formMouseDown(area.Min.X+1, area.Min.Y))
	up := chat.HandleMouse(formMouseUp(area.Min.X+1, area.Min.Y))
	goldenState(&output, "mouse activates microphone on release", chat, goldenWidth, goldenHeight, up, func(*ChatState) []string {
		return []string{"press action: " + chatActionDebug(down)}
	})

	chat = newTestChat()
	inert := uv.Rect(10, 8, 4, 1)
	chat.voiceButtonArea = &inert
	disabled := chat.HandleMouse(formMouseDown(11, 8))
	goldenState(&output, "disabled microphone click is inert", chat, goldenWidth, goldenHeight, disabled, buttonArea)

	chat = newTestChat()
	chat.voiceActive = true
	chat.setInput("dictated draft")
	action = chat.HandleKey(key(tea.KeyEnter))
	goldenState(&output, "Enter preserves a draft during active voice capture", chat, goldenWidth, goldenHeight, action, func(chat *ChatState) []string {
		return []string{"draft: " + rustDebugString(chat.input)}
	})

	for _, geometry := range []struct {
		width  int
		prompt uv.Rectangle
	}{
		{4, uv.Rect(0, 0, 4, 3)},
		{5, uv.Rect(0, 0, 5, 3)},
		{30, uv.Rect(4, 2, 30, 5)},
	} {
		button, ok := voiceButtonArea(geometry.prompt)
		buttonDebug := "None"
		if ok {
			buttonDebug = "Some(" + debugRect(button) + ")"
		}
		chat := newTestChat()
		chat.setVoiceAvailable(true)
		goldenState(&output, fmt.Sprintf("prompt width %d microphone geometry", geometry.width), chat, geometry.width, 8, nil, func(*ChatState) []string {
			return []string{"button area: " + buttonDebug}
		})
	}

	saveChatGolden(t, "chat-voice-prompt-control", output.String())
}

func TestGoldenChatBackgroundTaskDialog(t *testing.T) {
	var output strings.Builder
	chat := newTestChat()
	chat.setInput("keep this draft")
	chat.setSessionActivity(client.SessionActivity{
		BackgroundCommands: []core.BackgroundCommand{backgroundTask("task-main", "cargo test --workspace", true)},
	})
	initial := goldenRendered(chat, 100, 24)
	if chat.taskControlArea == nil {
		t.Fatal("initial task control")
	}
	initialTaskX := chat.taskControlArea.Min.X
	goldenState(&output, "task count in prompt border", chat, 100, 24, nil, func(chat *ChatState) []string {
		return []string{
			fmt.Sprintf("task label visible: %t", strings.Contains(initial, "Tasks (1)")),
			"draft: " + rustDebugString(chat.input),
			fmt.Sprintf("initial task control x: %d", initialTaskX),
		}
	})
	chat.markPromptSubmitted("continue")
	steering := true
	chat.steeringSupported = &steering
	chat.targetedTurnControlSupported = true
	chat.activePromptID = stringPtr("active")
	chat.turnControls = &core.TurnControls{
		Status: core.TurnControlStatusReady,
		Primary: &core.TurnControlAction{
			Kind: core.TurnControlKindSteer,
			Command: core.RelayCommandSteer{
				ActivePromptID: "active",
				QueuedPromptID: "next",
			},
		},
		Interrupt: core.RelayCommandCancelTurnFor{ActivePromptID: "active"},
		Message:   nil,
	}
	chat.queuedPrompts = append(chat.queuedPrompts, queued("next", "follow up"))
	for _, width := range []int{32, 48, 56, 80} {
		goldenState(&output, fmt.Sprintf("queued task border width %d", width), chat, width, 24, nil, func(chat *ChatState) []string {
			return []string{
				"task control area: " + debugOptionalRect(chat.taskControlArea),
				fmt.Sprintf("queued text: %d", len(chat.QueuedPromptTexts())),
			}
		})
	}
	goldenState(&output, "queued task border width 100", chat, 100, 24, nil, func(chat *ChatState) []string {
		if chat.taskControlArea == nil {
			t.Fatal("task control after layout")
		}
		area := *chat.taskControlArea
		return []string{
			"task control area: " + debugRect(area),
			fmt.Sprintf("task control shifted right: %t", area.Min.X > initialTaskX),
		}
	})

	if chat.taskControlArea == nil {
		t.Fatal("task control hitbox")
	}
	taskArea := *chat.taskControlArea
	press := formMouseDown(taskArea.Min.X, taskArea.Min.Y)
	mouseClaimed := chat.ComponentHandlesMouse(press)
	action := chat.HandleMouse(press)
	goldenState(&output, "mouse opens task dialog", chat, 100, 24, action, func(chat *ChatState) []string {
		return []string{
			fmt.Sprintf("dialog open: %t", chat.TaskDialogOpen()),
			fmt.Sprintf("draft cursor: %d", chat.inputCursor),
			fmt.Sprintf("chat surface claims click: %t", mouseClaimed),
		}
	})
	if chat.taskDialogArea == nil {
		t.Fatal("task dialog geometry")
	}
	dialogInner := *chat.taskDialogArea
	dismissX, dismissY := dialogInner.Min.X+1, max(dialogInner.Min.Y-1, 0)
	outsideClaimed := chat.ComponentHandlesMouse(formMouseDown(dismissX, dismissY))
	down := chat.HandleMouse(formMouseDown(dismissX, dismissY))
	up := chat.HandleMouse(formMouseUp(dismissX, dismissY))
	goldenState(&output, "outside mouse click dismisses task dialog", chat, 100, 24, up, func(chat *ChatState) []string {
		return []string{
			"press action: " + chatActionDebug(down),
			fmt.Sprintf("dialog open: %t", chat.TaskDialogOpen()),
			fmt.Sprintf("chat surface claims outside click: %t", outsideClaimed),
		}
	})
	focus := chat.HandleKey(key(tea.KeyDown))
	open := chat.HandleKey(key(tea.KeyEnter))
	goldenState(&output, "keyboard opens dialog without replacing draft", chat, 100, 24, open, func(chat *ChatState) []string {
		return []string{
			"focus action: " + chatActionDebug(focus),
			"draft: " + rustDebugString(chat.input),
		}
	})
	closeAction := chat.HandleKey(key(tea.KeyEscape))
	goldenState(&output, "Escape closes task dialog", chat, 100, 24, closeAction, func(chat *ChatState) []string {
		return []string{"draft: " + rustDebugString(chat.input)}
	})

	chat.openTaskDialog()
	chat.sessionActivity.BackgroundCommands[0].Command = "cargo test --all-targets --all-features --workspace"
	scrollDetail := func(chat *ChatState) []string { return []string{fmt.Sprintf("scroll: %d", chat.taskDialogScroll)} }
	for _, height := range []int{1, 4, 8} {
		goldenState(&output, fmt.Sprintf("narrow task dialog height %d", height), chat, 24, height, nil, scrollDetail)
	}
	for range 20 {
		chat.HandleKey(key(tea.KeyDown))
	}
	goldenState(&output, "wrapped task command scroll tail", chat, 24, 8, nil, scrollDetail)
	chat.sessionActivity.BackgroundCommands[0].Command = "cargo test " + strings.Repeat("--feature example ", 40) + "FINAL_ARGUMENT"
	goldenRendered(chat, 80, 12)
	for range 100 {
		chat.HandleKey(key(tea.KeyDown))
	}
	goldenState(&output, "long command retains its final argument after scrolling", chat, 80, 12, nil, scrollDetail)
	chat.setSessionActivity(client.SessionActivity{})
	goldenState(&output, "empty task dialog", chat, 80, 12, nil, noDetails)

	chat = newTestChat()
	chat.setSessionActivity(client.SessionActivity{
		BackgroundCommands: []core.BackgroundCommand{backgroundTask("read-only", "codex exec", false)},
	})
	chat.openTaskDialog()
	tab := chat.HandleKey(key(tea.KeyTab))
	enter := chat.HandleKey(key(tea.KeyEnter))
	goldenState(&output, "read-only row has no Stop control", chat, 80, 16, enter, func(chat *ChatState) []string {
		return []string{
			"Tab action: " + chatActionDebug(tab),
			fmt.Sprintf("dialog remains open: %t", chat.TaskDialogOpen()),
		}
	})

	chat = newTestChat()
	commands := make([]core.BackgroundCommand, 0, 8)
	for index := range 8 {
		commands = append(commands, backgroundTask(fmt.Sprintf("task-%d", index), fmt.Sprintf("work-%d", index), true))
	}
	chat.setSessionActivity(client.SessionActivity{BackgroundCommands: commands})
	chat.openTaskDialog()
	goldenRendered(chat, 40, 8)
	if chat.taskDialogArea == nil {
		t.Fatal("dialog geometry")
	}
	inner := *chat.taskDialogArea
	chat.HandleMouse(formScrollDown(inner.Min.X, inner.Min.Y))
	goldenState(&output, "mouse scrolls task rows", chat, 40, 8, nil, scrollDetail)
	chat.HandleKey(key(tea.KeyPgUp))
	goldenState(&output, "PageUp returns to first task rows", chat, 40, 8, nil, scrollDetail)
	x, y := max(inner.Max.X-2, 0), inner.Min.Y
	down = chat.HandleMouse(formMouseDown(x, y))
	stop := chat.HandleMouse(formMouseUp(x, y))
	goldenState(&output, "mouse stops first visible task", chat, 40, 8, stop, func(*ChatState) []string {
		return []string{"press action: " + chatActionDebug(down)}
	})

	saveChatGolden(t, "chat-background-task-dialog", output.String())
}

func TestGoldenChatSubagentsPromptControl(t *testing.T) {
	var output strings.Builder
	chat := newTestChat()
	controlDetail := func(chat *ChatState) []string {
		return []string{"control: " + debugOptionalRect(chat.subagentControlArea)}
	}
	goldenState(&output, "subagent control absent by default", chat, goldenWidth, goldenHeight, nil, controlDetail)
	chat.SetSubagentCount(1)
	goldenState(&output, "first child enables prompt control", chat, goldenWidth, goldenHeight, nil, func(chat *ChatState) []string {
		return []string{fmt.Sprintf("control available: %t", chat.subagentControlArea != nil)}
	})

	chat = newTestChat()
	chat.setInput("keep this draft")
	chat.SetSubagentCount(2)
	chat.SetSubagentWorkingCount(1)
	goldenState(&output, "active subagent count and preserved draft", chat, goldenWidth, goldenHeight, nil, controlDetail)
	if chat.subagentControlArea == nil {
		t.Fatal("subagent hitbox")
	}
	area := *chat.subagentControlArea
	press := formMouseDown(area.Min.X, area.Min.Y)
	mouseClaimed := chat.ComponentHandlesMouse(press)
	mouseAction := chat.HandleMouse(press)
	goldenState(&output, "mouse opens subagent panel", chat, goldenWidth, goldenHeight, mouseAction, func(*ChatState) []string {
		return []string{fmt.Sprintf("chat surface claims click: %t", mouseClaimed)}
	})
	down := chat.HandleKey(key(tea.KeyDown))
	enter := chat.HandleKey(key(tea.KeyEnter))
	goldenState(&output, "keyboard opens subagent panel", chat, goldenWidth, goldenHeight, enter, func(chat *ChatState) []string {
		return []string{
			"focus action: " + chatActionDebug(down),
			"draft: " + rustDebugString(chat.input),
		}
	})

	saveChatGolden(t, "chat-subagents-prompt-control", output.String())
}

func TestGoldenChatQueuedPromptEditing(t *testing.T) {
	var output strings.Builder
	queueDetail := func(chat *ChatState) []string {
		return []string{
			"draft: " + rustDebugString(chat.input),
			"remaining queue: " + debugStrings(chat.QueuedPromptTexts()),
		}
	}
	chat := newTestChat()
	chat.queuedPrompts = append(chat.queuedPrompts, queued("queued-1", "first"), queued("queued-2", "second"))
	action := chat.HandleKey(chord(tea.KeyUp, tea.ModAlt))
	goldenState(&output, "Alt-Up peels newest queued prompt", chat, goldenWidth, goldenHeight, action, queueDetail)

	chat = newTestChat()
	for _, prompt := range []struct{ id, text string }{
		{"queued-1", "first"},
		{"queued-2", "second"},
		{"queued-3", "third"},
	} {
		chat.queuedPrompts = append(chat.queuedPrompts, queued(prompt.id, prompt.text))
	}
	for _, step := range []struct {
		label string
		event tea.KeyPressMsg
	}{
		{"Up peels newest", key(tea.KeyUp)},
		{"Ctrl-P peels next", ctrl('p')},
		{"Up peels oldest", key(tea.KeyUp)},
		{"Up with empty queue", key(tea.KeyUp)},
	} {
		action := chat.HandleKey(step.event)
		goldenState(&output, step.label, chat, goldenWidth, goldenHeight, action, queueDetail)
		chat.clearInput()
	}

	chat = newTestChat()
	session := core.MaterializedSessionEmpty("1234567890")
	session.AppliedEventOrdinal = 5
	session.QueuedPrompts = append(session.QueuedPrompts, core.MaterializedQueuedPrompt{
		AcceptedOrdinal: nil,
		CommandID:       "queued-config",
		Kind:            core.QueuedCommandKindSetConfig{Key: "model", Value: "sonnet"},
		Content:         []any{map[string]any{"type": "text", "text": "/model sonnet"}},
		QueuedAtMs:      10,
	})
	chat.ApplyMaterialized(&session, nil, nil)
	goldenState(&output, "queued config before editing", chat, goldenWidth, goldenHeight, nil, func(chat *ChatState) []string {
		return []string{"queue label: " + chat.queuedPrompts[0].queueLabel()}
	})
	action = chat.HandleKey(ctrl('p'))
	goldenState(&output, "Ctrl-P peels queued config into composer", chat, goldenWidth, goldenHeight, action, func(chat *ChatState) []string {
		return []string{
			"draft: " + rustDebugString(chat.input),
			fmt.Sprintf("queue empty: %t", len(chat.queuedPrompts) == 0),
		}
	})
	chat.phase = core.WorkerPhaseRunning
	chat.setConfigOptions([]schema.SessionConfigOption{selectConfigOption("model", "opus", []string{"opus", "sonnet"})})
	action = chat.HandleKey(key(tea.KeyEnter))
	goldenState(&output, "resubmitted config retains its command", chat, goldenWidth, goldenHeight, action, noDetails)

	saveChatGolden(t, "chat-queued-prompt-editing", output.String())
}

func TestGoldenChatConfigSlashCommands(t *testing.T) {
	var output strings.Builder
	submit := func(chat *ChatState, input string) ChatAction {
		chat.setInput(input)
		return chat.HandleKey(key(tea.KeyEnter))
	}
	chat := newTestChat()
	chat.setConfigOptions([]schema.SessionConfigOption{
		selectConfigOption("model", "gpt-5.6", []string{"gpt-5.6", "gpt-5.6-luna"}),
		selectConfigOption("effort", "high", []string{"high", "xhigh"}),
	})
	goldenState(&output, "model selector", chat, goldenWidth, goldenHeight, submit(chat, "/model gpt-5.6-luna"), noDetails)
	goldenState(&output, "effort selector", chat, goldenWidth, goldenHeight, submit(chat, "/effort xhigh"), noDetails)

	chat = newTestChat()
	chat.setConfigOptions([]schema.SessionConfigOption{fastModeOption("off")})
	goldenState(&output, "fast toggles on", chat, goldenWidth, goldenHeight, submit(chat, "/fast"), noDetails)
	chat.setConfigOptions([]schema.SessionConfigOption{fastModeOption("on")})
	goldenState(&output, "fast toggles off", chat, goldenWidth, goldenHeight, submit(chat, "/fast"), noDetails)
	goldenState(&output, "fast argument usage", chat, goldenWidth, goldenHeight, submit(chat, "/fast on"), noDetails)

	chat = newTestChat()
	goldenState(&output, "fast unavailable for active model", chat, goldenWidth, goldenHeight, submit(chat, "/fast"), noDetails)

	chat = newTestChat()
	chat.phase = core.WorkerPhaseRunning
	goldenState(&output, "busy model command usage", chat, goldenWidth, goldenHeight, submit(chat, "/model"), noDetails)
	chat.setConfigOptions([]schema.SessionConfigOption{selectConfigOption("model", "opus", []string{"opus", "sonnet"})})
	action := submit(chat, "/model sonnet")
	goldenState(&output, "busy agent accepts queued config update", chat, goldenWidth, goldenHeight, action, func(chat *ChatState) []string {
		return []string{fmt.Sprintf("draft cleared: %t", chat.input == "")}
	})
	chat.phase = core.WorkerPhaseClosing
	goldenState(&output, "closing worker refuses config update", chat, goldenWidth, goldenHeight, submit(chat, "/model sonnet"), noDetails)

	chat = newTestChat()
	goldenState(&output, "missing model value shows usage", chat, goldenWidth, goldenHeight, submit(chat, "/model"), noDetails)

	saveChatGolden(t, "chat-config-slash-commands", output.String())
}

func TestGoldenChatPlanCommandCapabilities(t *testing.T) {
	var output strings.Builder
	submit := func(chat *ChatState, input string) ChatAction {
		chat.setInput(input)
		return chat.HandleKey(key(tea.KeyEnter))
	}

	chat := grokChat()
	goldenState(&output, "Grok enters plan mode", chat, goldenWidth, goldenHeight, submit(chat, "/plan"), noDetails)
	chat.planCommandPending = false
	goldenState(&output, "Grok exits plan mode", chat, goldenWidth, goldenHeight, submit(chat, "/plan"), noDetails)

	chat = grokChat()
	goldenState(&output, "explicit plan off", chat, goldenWidth, goldenHeight, submit(chat, "/plan off"), noDetails)
	goldenState(&output, "explicit plan on", chat, goldenWidth, goldenHeight, submit(chat, "/plan ON"), noDetails)
	chat.planCommandPending = false
	goldenState(&output, "unrecognized plan suffix becomes prompt", chat, goldenWidth, goldenHeight, submit(chat, "/plan sideways"), noDetails)

	chat = newTestChat()
	chat.setConfigOptions([]schema.SessionConfigOption{modeConfigOption("default", []string{"default", "plan"})})
	goldenState(&output, "advertised mode config", chat, goldenWidth, goldenHeight, submit(chat, "/plan"), noDetails)

	chat = grokChat()
	chat.setConfigOptions([]schema.SessionConfigOption{modeConfigOption("default", []string{"default", "act"})})
	goldenState(&output, "Grok trusted mode fallback", chat, goldenWidth, goldenHeight, submit(chat, "/plan"), noDetails)

	chat = newTestChat()
	chat.SetHarnessKind(core.HarnessKindMuse)
	advertise(chat, 1, []string{"plan"})
	goldenState(&output, "Muse advertised plan skill", chat, goldenWidth, goldenHeight, submit(chat, "/plan the migration"), noDetails)

	chat = newTestChat()
	chat.SetHarnessKind(core.HarnessKindMuse)
	advertise(chat, 1, []string{"plan"})
	modeOption := selectConfigOption("mode", "default", []string{"default", "readOnly", "plan"})
	category := schema.SessionConfigOptionCategoryMode
	modeOption.Category = &category
	chat.setConfigOptions([]schema.SessionConfigOption{
		modeOption,
		selectConfigOption("approval_mode", "allowAll", []string{"allowAll", "promptUnmatched", "onRequest", "denyUnmatched"}),
	})
	goldenState(&output, "Muse plan mode selector", chat, goldenWidth, goldenHeight, submit(chat, "/plan the migration"), noDetails)

	chat = grokChat()
	advertise(chat, 1, []string{"plan"})
	goldenState(&output, "unified plan command wins over skill", chat, goldenWidth, goldenHeight, submit(chat, "/plan the migration"), noDetails)

	chat = newTestChat()
	goldenState(&output, "no compatible plan surface", chat, goldenWidth, goldenHeight, submit(chat, "/plan"), noDetails)

	chat = newTestChat()
	chat.SetHarnessKind(core.HarnessKindCodex)
	chat.setConfigOptions([]schema.SessionConfigOption{
		selectConfigOption("mode", "read-only", []string{"read-only", "full-access"}),
		selectConfigOption("collaboration_mode", "default", []string{"default", "plan"}),
	})
	action := submit(chat, "/plan inspect the migration")
	goldenState(&output, "Codex collaboration mode preserves permission mode", chat, goldenWidth, goldenHeight, action, func(chat *ChatState) []string {
		return []string{"prompt history: " + debugStrings(chat.promptHistory)}
	})

	for _, harness := range []core.HarnessKind{core.HarnessKindClaude, core.HarnessKindKimi} {
		chat := newTestChat()
		chat.SetHarnessKind(harness)
		chat.setConfigOptions([]schema.SessionConfigOption{selectConfigOption("mode", "default", []string{"default", "plan"})})
		action := submit(chat, "/plan")
		goldenState(&output, harnessDebugNames[harness]+" exact mode config", chat, goldenWidth, goldenHeight, action, noDetails)
	}

	chat = newTestChat()
	chat.SetHarnessKind(core.HarnessKindGrok)
	goldenState(&output, "Grok set mode without catalogue", chat, goldenWidth, goldenHeight, submit(chat, "/plan"), noDetails)

	chat = grokChat()
	chat.SetHarnessKind(core.HarnessKindMuse)
	for _, command := range []string{"/plan design it", "/implement"} {
		action := submit(chat, command)
		goldenState(&output, "unsupported plan command "+command, chat, goldenWidth, goldenHeight, action, noDetails)
	}

	chat = grokChat()
	chat.finishPlanModeChange(true)
	goldenState(&output, "implement exits plan and carries instruction", chat, goldenWidth, goldenHeight, submit(chat, "/implement start with the parser"), noDetails)

	chat = grokChat()
	chat.finishPlanModeChange(true)
	request := core.ElicitationRequest{
		ID:             "plan-review-1",
		Message:        "review",
		Title:          nil,
		Description:    nil,
		Fields:         []core.ElicitationField{},
		ToolPermission: nil,
	}
	for _, choice := range []struct {
		action   string
		feedback *string
	}{
		{"implement", nil},
		{"revise", stringPtr("add tests")},
		{"exit", nil},
	} {
		content := map[string]core.ElicitationValue{"action": core.ElicitationValueString(choice.action)}
		if choice.feedback != nil {
			content["feedback"] = core.ElicitationValueString(*choice.feedback)
		}
		response := core.ElicitationResponse{Action: core.ElicitationResponseAccept, Content: content}
		followup := chat.planReviewFollowup(&request, &response)
		goldenState(&output, "plan review choice "+choice.action, chat, goldenWidth, goldenHeight, nil, func(*ChatState) []string {
			return []string{"followup: " + debugPlanReviewFollowup(followup)}
		})
	}

	chat = grokChat()
	chat.phase = core.WorkerPhaseRunning
	goldenState(&output, "plan waits for idle agent", chat, goldenWidth, goldenHeight, submit(chat, "/plan"), noDetails)

	saveChatGolden(t, "chat-plan-command-capabilities", output.String())
}

func TestGoldenChatTranscriptRendering(t *testing.T) {
	var output strings.Builder

	chat := newTestChat()
	chat.setInput("inspect renderer")
	chat.ToggleRenderMode()
	renderModeDetail := func(chat *ChatState) []string {
		return []string{"render mode: " + debugRenderMode(chat.renderMode)}
	}
	goldenState(&output, "raw transcript and composer state", chat, goldenWidth, goldenHeight, nil, renderModeDetail)
	chat.ToggleRenderMode()
	for _, event := range []tea.KeyPressMsg{alt('t'), ctrl('t'), alt('v')} {
		action := chat.HandleKey(event)
		goldenState(&output, "retired transcript shortcut remains local", chat, goldenWidth, goldenHeight, action, renderModeDetail)
	}

	runtime, err := json.Marshal(core.RuntimeEventSessionUpdate{Update: json.RawMessage(`{
		"sessionUpdate": "agent_message_chunk",
		"content": {"type": "text", "text": "done"}
	}`)})
	if err != nil {
		t.Fatal(err)
	}
	events := []core.SequencedEvent{
		{
			Seq:          1,
			RecordedAtMs: nil,
			RequestID:    stringPtr("prompt-1"),
			Event: core.WorkerEventPromptAccepted{
				RequestID:   "prompt-1",
				Text:        "work",
				Attachments: []core.Attachment{},
			},
		},
		{
			Seq:          2,
			RecordedAtMs: nil,
			RequestID:    nil,
			Event:        core.WorkerEventAdapter{Kind: "session_update", Payload: runtime},
		},
	}
	initial := snapshot()
	initial.LatestSeq = 2
	replayed := NewChatState(&initial, events)
	goldenState(&output, "replayed user and agent transcript", replayed, goldenWidth, goldenHeight, nil, noDetails)

	question, err := core.ElicitationRequestFromACPParams("question-1", json.RawMessage(`{
		"mode": "form", "sessionId": "session-1", "message": "Choose an architecture",
		"requestedSchema": {"type": "object", "properties": {
			"architecture": {"type": "string", "title": "Architecture", "oneOf": [
				{"const": "thin", "title": "Thin callers"}
			]}
		}}
	}`))
	if err != nil {
		t.Fatal(err)
	}
	reply := question.ReplyText(&core.ElicitationResponse{
		Action:  core.ElicitationResponseAccept,
		Content: map[string]core.ElicitationValue{"architecture": core.ElicitationValueString("thin")},
	})
	resolved, err := json.Marshal(core.RuntimeEventElicitationResolved{
		ElicitationID: question.ID,
		Action:        "accept",
		Reply:         &reply,
	})
	if err != nil {
		t.Fatal(err)
	}
	answeredEvents := append(append([]core.SequencedEvent(nil), events...), core.SequencedEvent{
		Seq:   3,
		Event: core.WorkerEventAdapter{Kind: "elicitation_resolved", Payload: resolved},
	})
	initial.LatestSeq = 3
	answered := NewChatState(&initial, answeredEvents)
	goldenState(&output, "elicitation reply echoed as a user message after replay", answered, goldenWidth, goldenHeight, nil, noDetails)

	chat = newTestChat()
	chat.applySessionUpdate(1, json.RawMessage(`{"sessionUpdate":"tool_call", "toolCallId":"grep-config", "title":"grep config", "status":"pending"}`))
	goldenState(&output, "pending tool call title", chat, goldenWidth, goldenHeight, nil, noDetails)
	chat.applySessionUpdate(2, json.RawMessage(`{"sessionUpdate":"tool_call_update", "toolCallId":"grep-config", "status":"completed", "content":[{"type":"content", "content":{"type":"text", "text":"noise"}}]}`))
	goldenState(&output, "completed tool call keeps title and suppresses update noise", chat, goldenWidth, goldenHeight, nil, func(chat *ChatState) []string {
		return []string{"entry text: " + rustDebugString(chat.entries[0].Text)}
	})

	chat = newTestChat()
	for _, plan := range []struct {
		seq    uint64
		status string
	}{{1, "pending"}, {2, "completed"}} {
		update, _ := json.Marshal(map[string]any{
			"sessionUpdate": "plan",
			"entries":       []any{map[string]any{"content": "inspect renderer", "priority": "high", "status": plan.status}},
		})
		chat.applySessionUpdate(plan.seq, update)
		goldenState(&output, "plan entry "+plan.status, chat, goldenWidth, goldenHeight, nil, func(chat *ChatState) []string {
			return []string{
				fmt.Sprintf("plan entries: %d", len(chat.entries[0].Plan)),
				fmt.Sprintf("completed: %t", chat.entries[0].Plan[0].Status == core.PlanStatusCompleted),
			}
		})
	}

	saveChatGolden(t, "chat-transcript-rendering", output.String())
}
