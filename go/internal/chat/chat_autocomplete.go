// Port of mj-chat/src/chat/autocomplete.rs.

// Slash commands: what they parse to, what the popup offers, and how a
// chosen completion lands back in the composer.

package chat

import (
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
	panic("unported: mj-chat/src/chat/autocomplete.rs moveAutocomplete")
}

func (c *ChatState) acceptAutocomplete() bool {
	panic("unported: mj-chat/src/chat/autocomplete.rs acceptAutocomplete")
}

func (c *ChatState) updateAutocomplete() {
	panic("unported: mj-chat/src/chat/autocomplete.rs updateAutocomplete")
}

func (c *ChatState) setAutocomplete(autocomplete *autocomplete) {
	if !c.autocomplete.equal(autocomplete) {
		c.autocomplete = autocomplete
	}
}

// configOptionKeys is each user-settable config key with the label its
// option advertises, as (key, label) pairs.
func (c *ChatState) configOptionKeys() [][2]string {
	panic("unported: mj-chat/src/chat/autocomplete.rs configOptionKeys")
}

func (c *ChatState) rebuildCommandChoices() {
	panic("unported: mj-chat/src/chat/autocomplete.rs rebuildCommandChoices")
}

func (c *ChatState) setConfigOptions(options []schema.SessionConfigOption) {
	panic("unported: mj-chat/src/chat/autocomplete.rs setConfigOptions")
}

func (c *ChatState) showHelp() {
	panic("unported: mj-chat/src/chat/autocomplete.rs showHelp")
}

func valueAutocomplete(query string, values []core.SessionConfigChoice, key string, command string) *autocomplete {
	panic("unported: mj-chat/src/chat/autocomplete.rs valueAutocomplete")
}

// matchingIndices is the indices of values whose primary field starts with
// query, case-insensitively; when none does, those whose primary or
// secondary field contains it.
func matchingIndices[T any](values []T, query string, fields func(value *T) (primary string, secondary *string)) []int {
	panic("unported: mj-chat/src/chat/autocomplete.rs matchingIndices")
}

func builtinCommandChoices() []commandChoice {
	panic("unported: mj-chat/src/chat/autocomplete.rs builtinCommandChoices")
}

// parseLocalCommand is the local command prompt names and its arguments, or
// false when the prompt is not one.
func parseLocalCommand(prompt string) (localCommand, string, bool) {
	panic("unported: mj-chat/src/chat/autocomplete.rs parseLocalCommand")
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
	panic("unported: mj-chat/src/chat/autocomplete.rs renderAutocomplete")
}

func autocompleteRow(chat *ChatState, kind autocompleteKind, index int) (string, bool) {
	panic("unported: mj-chat/src/chat/autocomplete.rs autocompleteRow")
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
