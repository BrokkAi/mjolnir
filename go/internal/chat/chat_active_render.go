// Port of mj-chat/src/chat/active/render.rs.

package chat

import (
	"fmt"
	"math"
	"slices"
	"strings"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// renderFullFrame draws a chat across a whole frame, the way the combined
// surface lays it out when nothing else is competing for the rows.
//
// Only tests use this: the real surface owns the layout and calls renderIn
// with the bands it chose.
func renderFullFrame(frame *Frame, chat *ChatState, transcriptSelected bool) {
	inner := frame.Area()
	promptHeight := min(chat.DesiredPromptHeight(inner.Dx()), max(inner.Dy()-6, 3))
	chunks := VerticalLayout(
		ConstraintMin(5),
		ConstraintLength(promptHeight),
		ConstraintLength(1),
	).Split(inner)
	footer := testFooter(chunks[2])
	renderIn(frame, chat, ChatRegions{
		Transcript:    chunks[0],
		Prompt:        chunks[1],
		Footer:        &footer,
		Overlay:       inner,
		TitleControls: 0,
		TitleLead:     0,
		PaneFocused:   false,
	}, true, transcriptSelected)
}

// renderIn draws the transcript and the composer into regions.
//
// promptFocused says whether the composer owns the keyboard; only then does
// it draw a cursor and an accent border. transcriptSelected says the
// selection engine still owns a selection on the transcript, so its row space
// has to stay frozen for this frame.
func renderIn(frame *Frame, chat *ChatState, regions ChatRegions, promptFocused bool, transcriptSelected bool) {
	chat.footerCommandAreas = chat.footerCommandAreas[:0]
	chat.voiceForm.BeginFrame()
	if chat.composerReplaced() {
		chat.voiceForm.CancelPointer()
	}
	chat.frameSurfaces.Clear()
	if chat.earlier != nil {
		renderEarlier(frame, regions.Overlay, chat.earlier)
		return
	}
	// Dialogs and the completion popup are centred in this conversation's
	// own overlay rectangle, not in the band the transcript happens to have
	// been given.
	inner := regions.Overlay
	transcriptArea := regions.Transcript
	promptArea := regions.Prompt

	// An open question replaces the composer, but only with the natural
	// height of its current page (up to half of the combined conversation
	// bands). The transcript is rendered into the rows left above it so its
	// viewport, scrollbar, and selection row space describe what is
	// visible. This branch deliberately runs before the ordinary
	// prompt/review drawing: no hidden prompt or autocomplete surface may
	// survive underneath the question, and both visible scrollable surfaces
	// remain registered.
	if chat.elicitation != nil {
		combined := uv.Rect(
			transcriptArea.Min.X,
			transcriptArea.Min.Y,
			transcriptArea.Dx(),
			max(promptArea.Max.Y-transcriptArea.Min.Y, 0),
		)
		questionHeight := min(chat.elicitation.naturalHeight(combined.Dx()), combined.Dy()/2)
		transcriptHeight := max(combined.Dy()-questionHeight, 0)
		questionArea := uv.Rect(combined.Min.X, max(combined.Max.Y-questionHeight, 0), combined.Dx(), questionHeight)
		upperTranscript := uv.Rect(combined.Min.X, combined.Min.Y, combined.Dx(), transcriptHeight)

		chat.voiceButtonArea = nil
		chat.configChipAreas = chat.configChipAreas[:0]
		chat.taskControlArea = nil
		chat.subagentControlArea = nil
		chat.reviewerControlArea = nil
		chat.taskDialogArea = nil
		chat.voiceForm.CancelPointer()
		chat.voiceForm.EndFrame(VoiceControlMicrophone)

		renderTranscript(frame, upperTranscript, chat, transcriptSelected,
			regions.TitleControls, regions.TitleLead, regions.PaneFocused)
		if questionHeight > 0 && chat.elicitation != nil {
			renderElicitationIn(frame, chat.elicitation, &chat.frameSurfaces, questionArea, promptFocused)
		}
		if regions.Footer != nil {
			renderChatFooter(frame, *regions.Footer, chat, promptFocused)
		}
		return
	}

	chat.voiceButtonArea = nil
	chat.configChipAreas = chat.configChipAreas[:0]
	chat.taskControlArea = nil
	chat.subagentControlArea = nil
	chat.reviewerControlArea = nil
	chat.taskDialogArea = nil
	renderTranscript(frame, transcriptArea, chat, transcriptSelected,
		regions.TitleControls, regions.TitleLead, regions.PaneFocused)
	renderComposerBand(frame, promptArea, chat, promptFocused, nil)
	if regions.Footer != nil {
		renderChatFooter(frame, *regions.Footer, chat, promptFocused)
	}
	// The popup overlays the prompt and whatever sits above it, so it
	// registers last and wins the cells it covers.
	if popup, ok := renderAutocomplete(frame, promptArea, chat); ok {
		chat.frameSurfaces.Push(SurfaceFrameFixed(SurfaceIdAutocompletePopup{}, popup))
	}
	// A new elicitation can arrive while the task list is open. Keep the
	// task dialog state so it can reappear afterwards, but let the question
	// render and receive input on top of it.
	if chat.turnControlDialogOpen && chat.elicitation == nil {
		turnControlRender(frame, inner, chat)
		return
	}
	if chat.taskDialogOpen && chat.elicitation == nil {
		renderBackgroundTaskDialog(frame, inner, chat)
		return
	}
	if body, ok := renderConfigPicker(frame, inner, chat); ok {
		// The selector owns this pane's interaction, so the chat behind it
		// stops being selectable. An elicitation dialog still draws over
		// it, matching the key routing that lets the dialog win.
		chat.frameSurfaces.Clear()
		chat.frameSurfaces.Push(SurfaceFrameFixed(SurfaceIdModalBody{}, body))
	}
}

// renderChatFooter draws the one-row footer under the conversation: the
// reverse-i-search prompt when one is open, else the shared notice, else the
// hotkey hints for the composer.
func renderChatFooter(frame *Frame, footer ChatFooter, chat *ChatState, promptFocused bool) {
	// The host only hands the footer to the chat while the composer has
	// focus, so promptFocused is normally true here; the other arm keeps
	// the row honest if it ever is not. The three groups are the
	// dashboard's: what the composer answers, the chords that answer from
	// anywhere, then the function keys. Only the first group changes with
	// what the composer is doing.
	footerArea := footer.Area
	// A host banner owns the whole row: it is reporting a state the
	// composer's own hints would only bury.
	if footer.Banner != nil {
		chat.footerCommandAreas = chat.footerCommandAreas[:0]
		frame.RenderWidget(
			NewParagraph(TextFrom(*footer.Banner)).WithStyle(Base().Foreground(ThemePalette().Muted)),
			footerArea,
		)
		return
	}
	// Built with FooterSeparator() rather than a literal dot, so the split
	// below finds the same glyph the ASCII symbol set draws.
	sep := FooterSeparator()
	queuedKeys := "Up/Ctrl-P edit last queued" + sep + "Enter send/queue" + sep + "Ctrl-R history" + sep +
		"Shift-Enter newline" + sep + chat.turnControlIntent().escapeHint()
	var composerKeys string
	switch {
	case !promptFocused:
		composerKeys = "Tab pane" + sep + "PgUp/PgDn transcript"
	case chat.voiceActive:
		composerKeys = "Listening… click the microphone to stop" + sep + "PgUp/PgDn transcript"
	case len(chat.queuedPrompts) > 0:
		composerKeys = queuedKeys
	default:
		composerKeys = "Tab pane" + sep + PasteShortcut + " paste" + sep + "Enter send" + sep +
			"Ctrl-R history" + sep + "Shift-Enter newline"
	}
	var composerItems, chordItems, functionItems []FooterItem[*int]
	for _, text := range strings.Split(composerKeys, FooterSeparator()) {
		composerItems = append(composerItems, FooterItem[*int]{Text: text})
	}
	for index, text := range footer.Chords {
		command := index
		chordItems = append(chordItems, FooterItem[*int]{Key: &command, Text: text})
	}
	for index, text := range footer.Functions {
		command := len(footer.Chords) + index
		functionItems = append(functionItems, FooterItem[*int]{Key: &command, Text: text})
	}
	groups := FitPrefixedFooterItems(
		[3][]FooterItem[*int]{composerItems, chordItems, functionItems},
		footerArea.Dx(),
		footer.ChordPrefix,
		// The host ranks its hints and puts the palette aliases last so
		// they stay visible when the row narrows.
		func(command *int) bool {
			return command != nil && *command+2 >= len(footer.Chords)+len(footer.Functions)
		},
	)
	defaultFooter := FooterItemsText(groups, func(item FooterItem[*int]) string { return item.Text })
	var searchFooter *string
	if chat.historySearch != nil {
		text := historySearchFooter(chat.historySearch)
		searchFooter = &text
	}
	notice, hasNotice := chat.notices.Current()
	shown := defaultFooter
	switch {
	case searchFooter != nil:
		shown = *searchFooter
	case hasNotice:
		shown = notice
	}
	chat.footerCommandAreas = chat.footerCommandAreas[:0]
	if searchFooter == nil && !hasNotice {
		x := footerArea.Min.X
		for _, group := range groups {
			if len(group) == 0 {
				continue
			}
			if x > footerArea.Min.X {
				x += displayWidth(FooterGroupSeparator())
			}
			for index, item := range group {
				if index > 0 {
					x += displayWidth(FooterSeparator())
				}
				width := displayWidth(item.Text)
				if item.Key != nil {
					chat.footerCommandAreas = append(chat.footerCommandAreas, FooterCommandArea{
						Command: *item.Key,
						Area:    uv.Rect(x, footerArea.Min.Y, width, footerArea.Dy()),
						Text:    item.Text,
					})
				}
				x += width
			}
		}
	}
	// Notices keep a warm accent; navigation hints remain quiet.
	footerColor := ThemePalette().Muted
	if searchFooter == nil && hasNotice {
		footerColor = ThemePalette().Warning
	}
	var line Line
	if searchFooter == nil && !hasNotice {
		line = Hints(shown)
	} else {
		line = RawLine(shown)
	}
	frame.RenderWidget(NewParagraph(TextFrom(line)).WithStyle(Base().Foreground(footerColor)), footerArea)
	if search := chat.historySearch; search != nil && chat.elicitation == nil && footerArea.Dx() > 0 {
		prefix := "reverse-i-search [" + historyScopeName(search.scope) + "]: "
		column := displayWidth(prefix) + displayWidth(search.query.Value())
		frame.SetCursorPosition(uv.Pos(footerArea.Min.X+min(column, max(footerArea.Dx()-1, 0)), footerArea.Min.Y))
	}
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
	if feedback, ok := composerFeedback(chat); ok && promptArea.Dy() > 3 {
		feedbackArea := uv.Rect(promptArea.Min.X, promptArea.Min.Y, promptArea.Dx(), 1)
		frame.RenderWidget(
			NewParagraph(TextFrom(TruncateLineToWidth(RawLine(sanitizeTerminalText(feedback)), promptArea.Dx()))).
				WithStyle(Muted()),
			feedbackArea,
		)
		promptArea = uv.Rect(promptArea.Min.X, promptArea.Min.Y+1, promptArea.Dx(), promptArea.Dy()-1)
	}
	promptWidth := promptContentWidth(promptArea.Dx())
	promptTitle, activityTitle, configChips := promptTitleLine(chat, promptArea)
	chat.configChipAreas = configChips
	promptBlock := Panel(promptFocused).
		WithStyle(Style{}.Foreground(ThemePalette().Text).Background(ThemePalette().SurfaceRaised)).
		WithPadding(Padding{Left: 2, Right: 1, Top: 0, Bottom: 0}).
		WithTitle(promptTitle)
	// The input owns the accent edge; supporting panels use quieter chrome.
	if promptFocused {
		promptBlock = promptBlock.WithBorderStyle(Title(true))
	}
	if activityTitle != nil {
		promptBlock = promptBlock.WithTitle(activityTitle.RightAligned())
	}
	for _, title := range promptBottomTitles(chat, promptArea, promptFocused) {
		promptBlock = promptBlock.WithTitleBottom(title)
	}
	if note != nil {
		promptBlock = promptBlock.WithTitleBottom(*note)
	}
	promptInner := promptBlock.Inner(promptArea)
	chat.promptContentWidth = promptWidth
	chat.voiceButtonArea = nil
	if area, ok := voiceButtonArea(promptArea); ok {
		chat.voiceButtonArea = &area
	}
	visibleQueue := chat.queuedPrompts[max(len(chat.queuedPrompts)-3, 0):]
	promptLines := make([]Line, 0, len(visibleQueue)+1)
	for index, queued := range visibleQueue {
		promptLines = append(promptLines, LineFrom(StyledSpan(
			truncateToWidth(
				fmt.Sprintf("%s %d: %s", queued.queueLabel(), index+1, queuedPromptPreview(queued.text)),
				promptInner.Dx(),
			),
			Style{}.Foreground(ThemePalette().Muted),
		)))
	}
	if len(promptLines) > 0 {
		promptLines = append(promptLines, Line{})
	}
	queueRows := len(promptLines)
	switch {
	case chat.historySearch != nil:
		promptLines = append(promptLines, highlightedInputLines(chat.input, chat.historySearch.query.Value())...)
	case chat.input == "":
		placeholder := "Message the agent, or / for commands"
		switch {
		case chat.standby:
			placeholder = "Type a draft · sending opens when the session is live"
		case chat.phase == core.WorkerPhaseRunning:
			placeholder = "Add a follow-up while the agent works…"
		case len(chat.entries) == 0 && chat.unconvertedPrefix == 0 && !chat.transcriptLoading:
			placeholder = "What would you like to build?"
		}
		promptLines = append(promptLines, LineFrom(StyledSpan(placeholder, Muted())))
	default:
		for _, line := range strings.Split(chat.input, "\n") {
			promptLines = append(promptLines, RawLine(line))
		}
	}
	_, cursorRow := InputCursorVisualPosition(chat.input, chat.inputCursor, promptWidth)
	cursorRow += queueRows
	contentHeight := max(promptInner.Dy(), 1)
	inputScroll := max(cursorRow+1-contentHeight, 0)
	frame.RenderWidget(
		NewParagraph(TextFrom(promptLines...)).
			WithStyle(Style{}.Foreground(ThemePalette().Text).Background(ThemePalette().SurfaceRaised)).
			WithWrap(Wrap{Trim: false}).
			WithScroll(inputScroll, 0).
			WithBlock(promptBlock),
		promptArea,
	)
	if markerRow := queueRows - inputScroll; markerRow >= 0 && markerRow < promptInner.Dy() {
		frame.RenderWidget(
			StyledLine(">", Title(promptFocused)),
			uv.Rect(max(promptInner.Min.X-2, 0), promptInner.Min.Y+markerRow, 1, 1),
		)
	}
	if chat.voiceButtonArea != nil {
		buttonArea := *chat.voiceButtonArea
		chat.voiceForm.Register(VoiceControlMicrophone, ControlKindButton{}, buttonArea, chat.voiceAvailable || chat.voiceActive)
		frame.RenderWidget(voiceButtonLine(chat.voiceAvailable, chat.voiceActive), buttonArea)
	}
	chat.voiceForm.EndFrame(VoiceControlMicrophone)
	chat.frameSurfaces.Push(SurfaceFrameFixed(SurfaceIdPromptInput{}, promptInner))
	// The cursor belongs to whatever has focus, so the composer only shows
	// one while the keyboard is driving it.
	if chat.historySearch == nil && promptFocused && chat.elicitation == nil {
		SetInputCursor(frame, promptInner, chat.input, chat.inputCursor, queueRows, inputScroll)
	}
}

// composerFeedback is the status row the composer band shows above its
// border: shared feedback, then running operations, then the connection,
// then the review.
func composerFeedback(chat *ChatState) (string, bool) {
	if feedback, ok := chat.feedback.Current(); ok {
		return feedback, true
	}
	if len(chat.operationFeedback) > 0 {
		keys := make([]string, 0, len(chat.operationFeedback))
		for key := range chat.operationFeedback {
			keys = append(keys, key)
		}
		slices.Sort(keys)
		values := make([]string, 0, len(keys))
		for _, key := range keys {
			values = append(values, chat.operationFeedback[key])
		}
		return strings.Join(values, " · "), true
	}
	if chat.connectionFeedback != nil {
		return *chat.connectionFeedback, true
	}
	return chat.turnReviewStatus()
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
	var parts []string
	if model, ok := chat.currentModel(); ok {
		parts = append(parts, model)
	}
	if effort, ok := chat.currentEffort(); ok {
		parts = append(parts, effort)
	}
	if chat.fastModeActive() {
		parts = append(parts, "Fast")
	}
	if chat.planModeActive() {
		parts = append(parts, "PLAN MODE")
		return parts
	}
	switch chat.phase {
	case core.WorkerPhaseRunning:
		// The spinner already indicates an ordinary running turn.
		if chat.pursuingGoal() {
			parts = append(parts, "Pursuing goal")
		}
	case core.WorkerPhaseClosing:
		parts = append(parts, "Agent shutdown")
	case core.WorkerPhaseClosed:
		parts = append(parts, "Agent ended")
	}
	return parts
}

// promptTitleLine: the microphone chip owns the prompt border's upper-left
// corner, followed by model, effort, and state. Activity owns the upper-right
// corner (nil when there is none). A full configured spinner is used when
// both titles fit; the one-column frame keeps narrow prompts readable. The
// model and effort spans are returned as clickable chips that open their
// value selector.
func promptTitleLine(chat *ChatState, promptArea uv.Rectangle) (Line, *Line, []configChipArea) {
	parts := promptTitleParts(chat)
	model, hasModel := chat.currentModel()
	effort, hasEffort := chat.currentEffort()
	prefixCount := 0
	if hasModel {
		prefixCount++
	}
	if hasEffort {
		prefixCount++
	}
	suffix := strings.Join(parts[min(prefixCount, len(parts)):], " · ")
	// A blank cell after the corner, as other border titles have, then the
	// microphone chip, which voiceButtonArea places on the same cells.
	spans := []Span{RawSpan(" "), RawSpan(" " + voiceButtonGlyph() + " ")}
	// The title begins just inside the border corner; each chip keeps the
	// exact cells of its span, and only a chip that fits inside the border
	// is kept.
	chipX := promptArea.Min.X + 1 + LineFrom(spans...).Width()
	activityReserve := 0
	if chat.NeedsAnimation() {
		activityReserve = 4
	}
	chipLimit := max(promptArea.Max.X-(1+activityReserve), 0)
	var chips []configChipArea
	for _, chip := range []struct {
		key     string
		value   string
		present bool
	}{{"model", model, hasModel}, {"effort", effort, hasEffort}} {
		if !chip.present {
			continue
		}
		advertised := chat.advertisedConfigValues(chip.key)
		enabled := len(advertised) > 0 &&
			chat.phase != core.WorkerPhaseClosing && chat.phase != core.WorkerPhaseClosed
		label := chip.value
		for index := range advertised {
			if advertised[index].Value == chip.value {
				label = configChoiceName(&advertised[index])
				break
			}
		}
		separator := " "
		if chip.key == "effort" && hasModel {
			separator = "· "
		}
		text := label + " "
		if enabled {
			text = label + " " + ThemeGlyphs().Dropdown + " "
		}
		width := displayWidth(separator) + displayWidth(text)
		// Draw and register complete controls together, never a clipped
		// active fragment.
		if chipX+width > chipLimit {
			break
		}
		spans = append(spans, StyledSpan(separator, Muted()))
		chipX += displayWidth(separator)
		if enabled {
			chips = append(chips, configChipArea{
				key:  chip.key,
				area: uv.Rect(chipX, promptArea.Min.Y, displayWidth(text), 1),
			})
		}
		style := Muted()
		if enabled {
			style = ActionableChip()
		}
		spans = append(spans, StyledSpan(text, style))
		chipX += width - displayWidth(separator)
	}
	if suffix != "" {
		spans = append(spans, RawSpan(" "+suffix+" "))
	}
	leftWidth := LineFrom(spans...).Width()
	var activityTitle *Line
	if chat.NeedsAnimation() {
		full := chat.ActivitySpinner()
		maxTitleWidth := max(promptArea.Dx()-2, 0)
		spinner := full
		if leftWidth+full.Width()+3 > maxTitleWidth {
			spinner = LineFrom(CompactSpan(chat.spinnerStyle, ElapsedMs()))
		}
		title := []Span{RawSpan(" ")}
		title = append(title, spinner.Spans...)
		title = append(title, RawSpan(" "))
		line := LineFrom(title...)
		activityTitle = &line
	}
	return LineFrom(spans...), activityTitle, chips
}

// promptBottomQueueControl: queue state and the hint for controlling the
// current turn live together on the prompt's bottom border. This stays
// independent of background tasks, so a queued prompt remains visible even
// when there is no task button.
func promptBottomQueueControl(chat *ChatState) *Line {
	var labels []string
	if len(chat.queuedPrompts) > 0 {
		labels = append(labels, fmt.Sprintf("%d queued", len(chat.queuedPrompts)))
	}
	if chat.promptInFlight ||
		chat.harnessTurnStoppable() ||
		chat.sessionActivity.CapacityRetry != nil ||
		chat.sessionActivity.QuotaRecovery != nil {
		labels = append(labels, chat.turnControlIntent().escapeHint())
	}
	if len(labels) == 0 {
		return nil
	}
	line := LineFrom(StyledSpan(" "+strings.Join(labels, " · "), Muted()))
	return &line
}

// taskStopRow is one Stop button the background-task dialog draws: its
// control, the wrapped row it sits on, its label, whether it is enabled, its
// width, and the opaque task id it stops.
type taskStopRow struct {
	control     BackgroundTaskControl
	row         int
	label       string
	enabled     bool
	buttonWidth int
	id          string
}

func renderBackgroundTaskDialog(frame *Frame, area uv.Rectangle, chat *ChatState) {
	commands := slices.Clone(chat.sessionActivity.BackgroundCommands)
	height := min(max(min(len(commands)+4, math.MaxUint16), 1), max(area.Dy(), 1))
	width := min(min(max(max(area.Dx()-4, 0), 32), 96), max(area.Dx(), 1))
	popup := centeredModalRectFixed(frame, width, height, area)
	inner := RectInner(popup, Margin{Horizontal: 1, Vertical: 1})
	now := core.EpochSeconds()
	contentWidth := max(inner.Dx(), 1)
	var stopRows []taskStopRow
	var lines []Line
	if len(commands) == 0 {
		lines = append(lines, LineFrom(StyledSpan("No background tasks remain.", Muted())))
	} else {
		for index, command := range commands {
			started := uint64(max(command.StartedAtMs, 0) / 1_000)
			elapsed := client.FormatClock(now - min(started, now))
			prefix := fmt.Sprintf("%8s  ", elapsed)
			prefixWidth := displayWidth(prefix)
			pending := chat.backgroundStopPending(command.ID)
			label := "[Stop]"
			if pending {
				label = "Interrupting…"
			}
			buttonWidth := displayWidth("  " + label + "  ")
			// Keep the acknowledgement state visible even if the provider
			// revokes the affordance before its task-disappeared snapshot.
			drawButton := (command.CanStop || pending) && contentWidth >= buttonWidth+prefixWidth+2
			textWidth := contentWidth
			if drawButton {
				textWidth = max(contentWidth-(buttonWidth+1), 1)
			}
			start := len(lines)
			text := strings.Join(strings.Fields(sanitizeTerminalText(command.Command)), " ")
			lines = append(lines, WrapStyledLine(RawLine(prefix+text), textWidth, prefixWidth)...)
			if drawButton {
				stopRows = append(stopRows, taskStopRow{
					control:     BackgroundTaskControlStop(index),
					row:         start,
					label:       label,
					enabled:     !pending,
					buttonWidth: buttonWidth,
					id:          command.ID,
				})
			}
		}
	}
	visible := max(inner.Dy(), 1)
	totalLines := len(lines)
	maxScroll := max(totalLines-visible, 0)
	chat.taskDialogMaxScroll = maxScroll
	chat.taskDialogScroll = min(chat.taskDialogScroll, maxScroll)
	if chat.taskDialogScroll > 0 {
		lines = lines[min(chat.taskDialogScroll, len(lines)):]
	}
	lines = lines[:min(len(lines), visible)]
	chat.taskDialogForm.BeginFrame()
	title := DismissibleModalTitle(&chat.taskDialogForm, popup, "Background tasks", Title(true), true)
	block := Panel(true).
		WithTitle(title).
		WithTitleBottom(LineFrom(StyledSpan(" Esc close ", Muted())).RightAligned())
	frame.RenderWidget(
		NewParagraph(TextFrom(lines...)).
			WithStyle(Base().Foreground(ThemePalette().Text)).
			WithWrap(Wrap{Trim: false}).
			WithBlock(block),
		popup,
	)
	chat.taskDialogControlIDs = chat.taskDialogControlIDs[:0]
	for _, stop := range stopRows {
		if stop.row < chat.taskDialogScroll || stop.row >= chat.taskDialogScroll+visible {
			continue
		}
		chat.taskDialogControlIDs = append(chat.taskDialogControlIDs, taskDialogControlID{control: stop.control, id: stop.id})
		y := inner.Min.Y + (stop.row - chat.taskDialogScroll)
		buttonArea := uv.Rect(
			max(inner.Max.X-min(stop.buttonWidth, math.MaxUint16), 0),
			y,
			min(min(stop.buttonWidth, math.MaxUint16), inner.Dx()),
			1,
		)
		ButtonRender(frame, buttonArea, stop.label, stop.enabled, &chat.taskDialogForm, stop.control)
	}
	if geometry, ok := NewScrollbarGeometry(
		uv.Rect(inner.Max.X, inner.Min.Y, 1, inner.Dy()),
		totalLines,
		chat.taskDialogScroll,
		visible,
	); ok {
		RenderScrollbar(frame, geometry)
	}
	taskDialogArea := inner
	chat.taskDialogArea = &taskDialogArea
	chat.frameSurfaces.Clear()
	chat.frameSurfaces.Push(SurfaceFrameFixed(SurfaceIdModalBody{}, inner))
	chat.taskDialogForm.EndFrame(BackgroundTaskControlStop(0))
}

// promptContentWidth: the composer keeps a `>` gutter on the left and one
// cell of space on the right.
func promptContentWidth(width int) int {
	return max(width-5, 1)
}

// promptBottomTitles is the shared navigation controls on ordinary composers
// and synchronous review prompts.
func promptBottomTitles(chat *ChatState, promptArea uv.Rectangle, promptFocused bool) []Line {
	chat.taskControlArea = nil
	chat.subagentControlArea = nil
	chat.reviewerControlArea = nil
	bottomWidth := max(promptArea.Dx()-2, 0)
	queueControl := promptBottomQueueControl(chat)
	var taskLabel, subagentLabel, reviewerLabel *string
	if count := chat.backgroundTaskCount(); count > 0 {
		label := fmt.Sprintf(" Tasks (%d) %s ", count, ThemeGlyphs().Navigate)
		taskLabel = &label
	}
	// Sub-agents appear once the session has at least one child.
	subagentsReady := chat.subagentCount > 0
	if subagentsReady {
		label := fmt.Sprintf(" Subagents%s%d %s ", FooterSeparator(), chat.subagentWorkingCount, ThemeGlyphs().Navigate)
		subagentLabel = &label
	}
	if chat.reviewers.Total > 0 {
		label := fmt.Sprintf(" Reviewers%s%d %s ", FooterSeparator(), chat.reviewers.Open, ThemeGlyphs().Navigate)
		reviewerLabel = &label
	}
	labelWidth := func(label *string) int {
		if label == nil {
			return 0
		}
		return displayWidth(*label)
	}
	reviewerWidth := labelWidth(reviewerLabel)
	var commandHints *Line
	if promptFocused && promptArea.Dx() >= 56 {
		// A standby composer cannot send, so the hint says what Enter does
		// there instead of advertising a send that would be refused.
		var hints Line
		if chat.standby {
			hints = LineFrom(
				StyledSpan(" Enter ", KeyHint()),
				StyledSpan(" keeps the draft ", HintDescription()),
			).RightAligned()
		} else {
			hints = LineFrom(
				StyledSpan(" Enter ", KeyHint()),
				StyledSpan(" send  ", HintDescription()),
				StyledSpan(" / ", KeyHint()),
				StyledSpan(" commands ", HintDescription()),
			).RightAligned()
		}
		commandHints = &hints
	}
	queueWidth := 0
	if queueControl != nil {
		queueWidth = queueControl.Width()
	}
	taskWidth := labelWidth(taskLabel)
	subagentWidth := labelWidth(subagentLabel)
	commandWidth := 0
	if commandHints != nil {
		commandWidth = commandHints.Width()
	}
	taskSeparatorWidth := 0
	if queueControl != nil && taskLabel != nil {
		taskSeparatorWidth = 2
	}
	// Fit queue/control text first, then a complete task button, then hints.
	leftWithTask := queueWidth + taskSeparatorWidth + taskWidth
	showTask := taskLabel != nil && leftWithTask <= bottomWidth
	leftWidth := queueWidth
	if showTask {
		leftWidth = leftWithTask
	}
	separatorAfter := func(width int) int {
		if width > 0 {
			return 2
		}
		return 0
	}
	subagentSeparatorWidth := separatorAfter(leftWidth)
	showSubagents := subagentLabel != nil && leftWidth+subagentSeparatorWidth+subagentWidth <= bottomWidth
	if showSubagents {
		leftWidth += subagentSeparatorWidth + subagentWidth
	}
	reviewerSeparatorWidth := separatorAfter(leftWidth)
	showReviewers := reviewerLabel != nil && leftWidth+reviewerSeparatorWidth+reviewerWidth <= bottomWidth
	if showReviewers {
		leftWidth += reviewerSeparatorWidth + reviewerWidth
	}
	hintGap := 0
	if leftWidth > 0 {
		hintGap = 1
	}
	showCommandHints := commandHints != nil && leftWidth+hintGap+commandWidth <= bottomWidth
	var bottomSpans []Span
	bottomLeftWidth := 0
	if queueControl != nil {
		bottomLeftWidth = queueWidth
		bottomSpans = append(bottomSpans, queueControl.Spans...)
	}
	if showTask {
		taskStart := bottomLeftWidth + taskSeparatorWidth
		if bottomLeftWidth > 0 {
			bottomSpans = append(bottomSpans, RawSpan(" ·"))
		}
		area := uv.Rect(promptArea.Min.X+1+taskStart, max(promptArea.Max.Y-1, 0), taskWidth, 1)
		chat.taskControlArea = &area
		style := ActionableChip()
		if chat.taskControlFocused() {
			style = FocusControl()
		}
		// The surface and navigation glyph advertise the action.
		bottomSpans = append(bottomSpans, StyledSpan(*taskLabel, style))
		bottomLeftWidth = taskStart + taskWidth
	}
	if showSubagents {
		subagentStart := bottomLeftWidth + separatorAfter(bottomLeftWidth)
		if bottomLeftWidth > 0 {
			bottomSpans = append(bottomSpans, RawSpan(" ·"))
		}
		if subagentsReady {
			area := uv.Rect(promptArea.Min.X+1+subagentStart, max(promptArea.Max.Y-1, 0), subagentWidth, 1)
			chat.subagentControlArea = &area
		}
		style := ActionableChip()
		if chat.subagentControlFocused() {
			style = FocusControl()
		}
		// The surface and navigation glyph advertise the action.
		bottomSpans = append(bottomSpans, StyledSpan(*subagentLabel, style))
		bottomLeftWidth = subagentStart + subagentWidth
	}
	if showReviewers {
		start := bottomLeftWidth + separatorAfter(bottomLeftWidth)
		if bottomLeftWidth > 0 {
			bottomSpans = append(bottomSpans, RawSpan(" ·"))
		}
		area := uv.Rect(promptArea.Min.X+1+start, max(promptArea.Max.Y-1, 0), reviewerWidth, 1)
		chat.reviewerControlArea = &area
		style := ActionableChip()
		if chat.promptControlFocus != nil && *chat.promptControlFocus == PromptControlReviewers {
			style = FocusControl()
		}
		bottomSpans = append(bottomSpans, StyledSpan(*reviewerLabel, style))
	}
	var titles []Line
	if len(bottomSpans) > 0 {
		titles = append(titles, LineFrom(bottomSpans...).LeftAligned())
	}
	if showCommandHints {
		titles = append(titles, *commandHints)
	}
	return titles
}
