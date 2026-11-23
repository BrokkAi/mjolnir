// Port of mj-chat/src/chat/autocomplete.rs.

// Slash commands: what they parse to, what the popup offers, and how a
// chosen completion lands back in the composer.

package chat

import (
	"fmt"
	"slices"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/schema"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// localCommand is a slash command the chat handles itself. Variants are
// comparable, so commands compare with ==.
type localCommand interface {
	isLocalCommand()
}

type localCommandClear struct{}

// localCommandGoalControl is `/goal <action>`.
type localCommandGoalControl core.GoalControlAction

type localCommandHelp struct{}
type localCommandDetach struct{}
type localCommandModel struct{}
type localCommandEffort struct{}
type localCommandConfig struct{}
type localCommandFast struct{}
type localCommandPlan struct{}
type localCommandImplement struct{}
type localCommandReview struct{}
type localCommandAttach struct{}

func (localCommandClear) isLocalCommand()       {}
func (localCommandGoalControl) isLocalCommand() {}
func (localCommandHelp) isLocalCommand()        {}
func (localCommandDetach) isLocalCommand()      {}
func (localCommandModel) isLocalCommand()       {}
func (localCommandEffort) isLocalCommand()      {}
func (localCommandConfig) isLocalCommand()      {}
func (localCommandFast) isLocalCommand()        {}
func (localCommandPlan) isLocalCommand()        {}
func (localCommandImplement) isLocalCommand()   {}
func (localCommandReview) isLocalCommand()      {}
func (localCommandAttach) isLocalCommand()      {}

type commandSource int

const (
	commandSourceHel commandSource = iota
	commandSourceAgent
)

type commandChoice struct {
	name        string
	description string
	inputHint   *string
	source      commandSource
}

// equal is Rust's derived PartialEq.
func (c *commandChoice) equal(other *commandChoice) bool {
	if c.name != other.name || c.description != other.description || c.source != other.source {
		return false
	}
	if c.inputHint == nil || other.inputHint == nil {
		return c.inputHint == nil && other.inputHint == nil
	}
	return *c.inputHint == *other.inputHint
}

// commandChoicesEqual compares two choice lists element by element.
func commandChoicesEqual(a, b []commandChoice) bool {
	if len(a) != len(b) {
		return false
	}
	for index := range a {
		if !a[index].equal(&b[index]) {
			return false
		}
	}
	return true
}

// autocompleteKind is what the popup completes. Variants are comparable.
type autocompleteKind interface {
	isAutocompleteKind()
}

type autocompleteKindCommands struct{}
type autocompleteKindConfigKeys struct{}
type autocompleteKindConfigValues struct {
	key     string
	command string
}

func (autocompleteKindCommands) isAutocompleteKind()     {}
func (autocompleteKindConfigKeys) isAutocompleteKind()   {}
func (autocompleteKindConfigValues) isAutocompleteKind() {}

type autocomplete struct {
	kind     autocompleteKind
	selected int
	matches  []int
}

// equal is Rust's derived PartialEq over Option<Autocomplete>: nil equals
// only nil.
func (a *autocomplete) equal(other *autocomplete) bool {
	if a == nil || other == nil {
		return a == nil && other == nil
	}
	if a.kind != other.kind || a.selected != other.selected || len(a.matches) != len(other.matches) {
		return false
	}
	for index := range a.matches {
		if a.matches[index] != other.matches[index] {
			return false
		}
	}
	return true
}

func (c *ChatState) moveAutocomplete(delta int) {
	autocomplete := c.autocomplete
	if autocomplete == nil {
		return
	}
	length := len(autocomplete.matches)
	if length == 0 {
		return
	}
	var selected int
	if delta < 0 {
		selected = autocomplete.selected - 1
		if autocomplete.selected == 0 {
			selected = length - 1
		}
	} else {
		selected = (autocomplete.selected + 1) % length
	}
	if autocomplete.selected != selected {
		autocomplete.selected = selected
	}
}

func (c *ChatState) acceptAutocomplete() bool {
	if c.autocomplete == nil {
		return false
	}
	autocomplete := *c.autocomplete
	if autocomplete.selected < 0 || autocomplete.selected >= len(autocomplete.matches) {
		return false
	}
	index := autocomplete.matches[autocomplete.selected]
	var value string
	found := false
	switch kind := autocomplete.kind.(type) {
	case autocompleteKindCommands:
		if index >= 0 && index < len(c.commandChoices) {
			value = "/" + c.commandChoices[index].name + " "
			found = true
		}
	case autocompleteKindConfigKeys:
		if keys := c.configOptionKeys(); index >= 0 && index < len(keys) {
			value = "/config " + keys[index][0] + " "
			found = true
		}
	case autocompleteKindConfigValues:
		if values := c.configValues(kind.key); index >= 0 && index < len(values) {
			if kind.command == "config" {
				value = "/config " + kind.key + " " + values[index].Value
			} else {
				value = "/" + kind.command + " " + values[index].Value
			}
			found = true
		}
	}
	if !found {
		return false
	}
	c.setInput(value)
	c.setAutocomplete(nil)
	return true
}

func (c *ChatState) updateAutocomplete() {
	// A standby composer has no session to answer commands, and its host
	// does not run the overlay pass that draws the popup, so offering
	// completion would only open an invisible state that eats arrows.
	if c.standby {
		c.setAutocomplete(nil)
		return
	}
	if len(c.inputImages) > 0 {
		c.setAutocomplete(nil)
		return
	}
	if c.historySearch != nil || c.inputCursor != len(c.input) {
		c.setAutocomplete(nil)
		return
	}
	if query, ok := strings.CutPrefix(c.input, "/model "); ok {
		c.setAutocomplete(valueAutocomplete(query, c.modelValues, "model", "model"))
		return
	}
	if query, ok := strings.CutPrefix(c.input, "/effort "); ok {
		c.setAutocomplete(valueAutocomplete(query, c.effortValues, "effort", "effort"))
		return
	}
	if query, ok := strings.CutPrefix(c.input, "/config "); ok {
		key, value, hasValue := splitOnceWhitespace(query)
		if hasValue {
			values := c.configValues(key)
			c.setAutocomplete(valueAutocomplete(value, values, key, "config"))
		} else {
			matches := matchingIndices(c.configOptionKeys(), key, func(option *[2]string) (string, *string) {
				return option[0], &option[1]
			})
			if len(matches) == 0 {
				c.setAutocomplete(nil)
			} else {
				c.setAutocomplete(&autocomplete{kind: autocompleteKindConfigKeys{}, selected: 0, matches: matches})
			}
		}
		return
	}
	query, ok := strings.CutPrefix(c.input, "/")
	if !ok {
		c.setAutocomplete(nil)
		return
	}
	if strings.ContainsFunc(query, unicode.IsSpace) && !strings.HasPrefix(query, "goal ") {
		c.setAutocomplete(nil)
		return
	}
	// A fully typed command is ready to submit. Leaving its popup open would
	// make Enter re-complete the text instead of running it, which matters
	// most for the bare /model and /effort selectors.
	for _, command := range c.commandChoices {
		if command.name == query {
			c.setAutocomplete(nil)
			return
		}
	}
	matches := matchingIndices(c.commandChoices, query, func(command *commandChoice) (string, *string) {
		return command.name, &command.description
	})
	if len(matches) == 0 {
		c.setAutocomplete(nil)
		return
	}
	c.setAutocomplete(&autocomplete{kind: autocompleteKindCommands{}, selected: 0, matches: matches})
}

func (c *ChatState) setAutocomplete(autocomplete *autocomplete) {
	if !c.autocomplete.equal(autocomplete) {
		c.autocomplete = autocomplete
	}
}

// configOptionKeys is each user-settable config key with the label its
// option advertises, as (key, label) pairs.
func (c *ChatState) configOptionKeys() [][2]string {
	keys := [][2]string{}
	options := c.acpSurface.ConfigOptions()
	for index := range options {
		option := &options[index]
		key := configOptionKey(option)
		if !c.configKeyUserSettable(key) {
			continue
		}
		if slices.ContainsFunc(keys, func(existing [2]string) bool { return existing[0] == key }) {
			continue
		}
		if len(c.configValues(key)) == 0 {
			continue
		}
		keys = append(keys, [2]string{key, option.Name})
	}
	return keys
}

func (c *ChatState) rebuildCommandChoices() {
	commands := builtinCommandChoices()
	hint := func(text string) *string { return &text }
	if len(c.configOptionKeys()) > 0 {
		commands = append(commands, commandChoice{
			name:        "config",
			description: "change any setting advertised by the active agent",
			inputHint:   hint("key [value]"),
			source:      commandSourceHel,
		})
	}
	if c.clearContextSupported {
		commands = append(commands, commandChoice{
			name:        "clear",
			description: "start a new conversation and clear pending mail, keeping workspace and history",
			source:      commandSourceHel,
		})
	}
	if c.supportsFastMode() {
		commands = append(commands, commandChoice{
			name:        "fast",
			description: "toggle Codex Fast mode",
			source:      commandSourceHel,
		})
	}
	if c.supportsPlanMode() {
		commands = append(commands,
			commandChoice{
				name:        "plan",
				description: "toggle plan mode",
				inputHint:   hint("message"),
				source:      commandSourceHel,
			},
			commandChoice{
				name:        "implement",
				description: "leave plan mode and implement",
				inputHint:   hint("instruction"),
				source:      commandSourceHel,
			})
	}
	for _, action := range core.GoalControlActionAll {
		if !c.goalState.Supports(action) {
			continue
		}
		var description string
		switch action {
		case core.GoalControlActionPause:
			description = "pause goal continuation, preserving the goal"
		case core.GoalControlActionResume:
			description = "resume the existing goal"
		case core.GoalControlActionClear:
			description = "remove the goal without interrupting the current turn"
		}
		commands = append(commands, commandChoice{
			name:        "goal " + action.AsStr(),
			description: description,
			source:      commandSourceHel,
		})
	}
	for _, command := range c.acpSurface.AgentCommands() {
		name := strings.TrimSpace(command.Name)
		lowered := textASCIILower(name)
		reserved := (lowered == "fast" || lowered == "plan" || lowered == "implement") &&
			!(textASCIIEqualFold(name, "plan") && c.acpSurface.ForwardsPlanCommand())
		duplicate := slices.ContainsFunc(commands, func(existing commandChoice) bool {
			return textASCIIEqualFold(existing.name, name)
		})
		if name == "" || reserved || duplicate {
			continue
		}
		var inputHint *string
		if command.Input != nil {
			inputHint = hint(command.Input.Hint)
		}
		commands = append(commands, commandChoice{
			name:        name,
			description: strings.TrimSpace(command.Description),
			inputHint:   inputHint,
			source:      commandSourceAgent,
		})
	}
	if !commandChoicesEqual(c.commandChoices, commands) {
		c.commandChoices = commands
	}
	c.updateAutocomplete()
}

func (c *ChatState) setConfigOptions(options []schema.SessionConfigOption) {
	c.acpSurface.SetConfigOptions(options)
	modelValues := core.SessionConfigChoices(options, "model")
	effortValues := core.SessionConfigChoices(options, "effort")
	if !sessionConfigChoicesEqual(c.modelValues, modelValues) || !sessionConfigChoicesEqual(c.effortValues, effortValues) {
		c.modelValues = modelValues
		c.effortValues = effortValues
	}
	c.rebuildCommandChoices()
}

// sessionConfigChoicesEqual is Rust's derived PartialEq over two choice
// lists.
func sessionConfigChoicesEqual(a, b []core.SessionConfigChoice) bool {
	return slices.EqualFunc(a, b, func(left, right core.SessionConfigChoice) bool {
		return left.Value == right.Value && left.Name == right.Name &&
			optionalStringsEqual(left.Description, right.Description)
	})
}

// optionalStringsEqual compares two Rust `Option<String>` values.
func optionalStringsEqual(a, b *string) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	return *a == *b
}

func (c *ChatState) showHelp() {
	lines := make([]string, 0, len(c.commandChoices))
	for _, command := range c.commandChoices {
		lines = append(lines, fmt.Sprintf("/%s%s — %s [%s]",
			command.name, commandHint(command.inputHint), command.description, commandSourceLabel(command.source)))
	}
	c.entries = append(c.entries, core.ChatEntryPlain(c.latestSeq, core.ChatRoleSystem, fmt.Sprintf(
		"Clipboard: %s paste text/image (Ctrl-Alt-V if intercepted by your terminal) · Backspace/Delete remove image markers\n\nAvailable commands:\n!<command> — run a Bash command in this session [mj]\n%s",
		PasteShortcut, strings.Join(lines, "\n"))))
}

// commandHint is a command's input hint as ` <hint>`, or nothing.
func commandHint(inputHint *string) string {
	if inputHint == nil {
		return ""
	}
	return " <" + *inputHint + ">"
}

func commandSourceLabel(source commandSource) string {
	if source == commandSourceAgent {
		return "agent"
	}
	return "mj"
}

func valueAutocomplete(query string, values []core.SessionConfigChoice, key string, command string) *autocomplete {
	// A bare command submits into the full value selector; the inline popup
	// only completes a partially typed value.
	if query == "" || slices.ContainsFunc(values, func(choice core.SessionConfigChoice) bool { return choice.Value == query }) {
		return nil
	}
	matches := matchingIndices(values, query, func(choice *core.SessionConfigChoice) (string, *string) {
		return choice.Value, &choice.Name
	})
	if len(matches) == 0 {
		return nil
	}
	return &autocomplete{
		kind:     autocompleteKindConfigValues{key: key, command: command},
		selected: 0,
		matches:  matches,
	}
}

// matchingIndices is the indices of values whose primary field starts with
// query, case-insensitively; when none does, those whose primary or
// secondary field contains it.
func matchingIndices[T any](values []T, query string, fields func(value *T) (primary string, secondary *string)) []int {
	query = strings.ToLower(query)
	prefix := []int{}
	for index := range values {
		primary, _ := fields(&values[index])
		if strings.HasPrefix(strings.ToLower(primary), query) {
			prefix = append(prefix, index)
		}
	}
	if len(prefix) > 0 {
		return prefix
	}
	contains := []int{}
	for index := range values {
		primary, secondary := fields(&values[index])
		if strings.Contains(strings.ToLower(primary), query) ||
			(secondary != nil && strings.Contains(strings.ToLower(*secondary), query)) {
			contains = append(contains, index)
		}
	}
	return contains
}

func builtinCommandChoices() []commandChoice {
	builtins := []struct {
		name        string
		description string
		inputHint   string
	}{
		{"help", "show available Mjolnir and agent commands", ""},
		{"detach", "leave Mjolnir without stopping the worker", ""},
		{"model", "change the active model, queued while the agent is busy", "value"},
		{"effort", "change the active reasoning effort, queued while the agent is busy", "value"},
		{"review", "review committed work now, or report how review is configured", "status"},
		{"attach", "add an image file (PNG, JPEG or WebP) to the prompt", "path"},
	}
	choices := make([]commandChoice, 0, len(builtins))
	for _, builtin := range builtins {
		var inputHint *string
		if builtin.inputHint != "" {
			text := builtin.inputHint
			inputHint = &text
		}
		choices = append(choices, commandChoice{
			name:        builtin.name,
			description: builtin.description,
			inputHint:   inputHint,
			source:      commandSourceHel,
		})
	}
	return choices
}

// parseLocalCommand is the local command prompt names and its arguments, or
// false when the prompt is not one.
func parseLocalCommand(prompt string) (localCommand, string, bool) {
	name, args, ok := parseSlashCommand(prompt)
	if !ok {
		return nil, "", false
	}
	var command localCommand
	switch name {
	case "goal":
		action, ok := core.GoalControlActionParse(args)
		if !ok {
			return nil, "", false
		}
		command = localCommandGoalControl(action)
	case "clear":
		command = localCommandClear{}
	case "help":
		command = localCommandHelp{}
	case "detach":
		command = localCommandDetach{}
	case "model":
		command = localCommandModel{}
	case "effort":
		command = localCommandEffort{}
	case "config":
		command = localCommandConfig{}
	case "fast":
		command = localCommandFast{}
	case "plan":
		command = localCommandPlan{}
	case "implement":
		command = localCommandImplement{}
	case "review":
		command = localCommandReview{}
	case "attach":
		command = localCommandAttach{}
	default:
		return nil, "", false
	}
	return command, args, true
}

func promptInvokesCommand(prompt string, expected string) bool {
	name, _, ok := parseSlashCommand(prompt)
	return ok && name == expected
}

// parseSlashCommand splits `/name args` into its name and trimmed arguments.
func parseSlashCommand(prompt string) (string, string, bool) {
	command, ok := strings.CutPrefix(prompt, "/")
	if !ok {
		return "", "", false
	}
	index := strings.IndexFunc(command, unicode.IsSpace)
	if index < 0 {
		return command, "", true
	}
	_, width := utf8.DecodeRuneInString(command[index:])
	return command[:index], strings.TrimSpace(command[index+width:]), true
}

// renderAutocomplete draws the popup over the prompt and reports the rows it
// covers, so the caller can register them as a selectable surface.
func renderAutocomplete(frame *Frame, promptArea uv.Rectangle, chat *ChatState) (uv.Rectangle, bool) {
	autocomplete := chat.autocomplete
	if autocomplete == nil {
		return uv.Rectangle{}, false
	}
	visible := min(len(autocomplete.matches), 8)
	if visible == 0 {
		return uv.Rectangle{}, false
	}
	var title string
	switch autocomplete.kind.(type) {
	case autocompleteKindCommands:
		title = " commands · ↑/↓ select · Tab/Enter accept "
	case autocompleteKindConfigKeys:
		title = " settings · ↑/↓ select · Tab/Enter accept "
	default:
		title = " values · ↑/↓ select · Tab/Enter accept "
	}
	_, inner, ok := AutocompletePopupRender(frame, frame.Area(), promptArea, promptArea.Dx(),
		len(autocomplete.matches), ComboBoxDefaultMaxVisibleRows, title, PopupSideAbove)
	if !ok {
		return uv.Rectangle{}, false
	}
	start := max(autocomplete.selected-max(visible-1, 0), 0)
	end := min(start+visible, len(autocomplete.matches))
	items := make([]ListItem, 0, visible)
	for offset, index := range autocomplete.matches[start:end] {
		row, ok := autocompleteRow(chat, autocomplete.kind, index)
		if !ok {
			continue
		}
		style := Style{}
		if start+offset == autocomplete.selected {
			style = Selection(true)
		}
		items = append(items, NewListItem(RawText(truncateToWidth(row, inner.Dx()))).WithStyle(style))
	}
	frame.RenderWidget(NewList(items), inner)
	return inner, true
}

func autocompleteRow(chat *ChatState, kind autocompleteKind, index int) (string, bool) {
	switch kind := kind.(type) {
	case autocompleteKindCommands:
		if index < 0 || index >= len(chat.commandChoices) {
			return "", false
		}
		command := chat.commandChoices[index]
		return fmt.Sprintf("/%s%s  — %s [%s]",
			command.name, commandHint(command.inputHint), command.description, commandSourceLabel(command.source)), true
	case autocompleteKindConfigKeys:
		keys := chat.configOptionKeys()
		if index < 0 || index >= len(keys) {
			return "", false
		}
		return fmt.Sprintf("/config %s  — %s", keys[index][0], keys[index][1]), true
	case autocompleteKindConfigValues:
		values := chat.configValues(kind.key)
		if index < 0 || index >= len(values) {
			return "", false
		}
		return configValueRow(&values[index])
	}
	return "", false
}

func configChoiceName(choice *core.SessionConfigChoice) string {
	if strings.TrimSpace(choice.Name) == "" {
		return choice.Value
	}
	return choice.Name
}

func configValueRow(choice *core.SessionConfigChoice) (string, bool) {
	return configChoiceName(choice), true
}
