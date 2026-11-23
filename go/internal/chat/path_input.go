// Port of mj-chat/src/path_input.rs.

// Path editing reuses the text editor; resolution happens only on apply.

package chat

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// completion is the completion popup state owned by one path field.
//
// requested holds the text a reply is still expected for, so a reply that
// arrives after the user kept typing can be dropped.
type completion struct {
	candidates []string
	selected   int
	truncated  bool
	requested  *string
}

func (c *completion) clearPopup() {
	c.candidates = c.candidates[:0]
	c.selected = 0
	c.truncated = false
}

// PathInput is a path field's text and its completion popup. The embedded
// TextInput stands in for Rust's Deref/DerefMut to the editor, so its
// methods (Value, SetValue, Cursor, String, ...) apply to the field.
type PathInput struct {
	TextInput
	completion completion
}

func NewPathInput() PathInput {
	return PathInput{TextInput: NewTextInput()}
}

// PathInputFromValue is the field holding value (Rust's from_value and its
// From<String>/From<&str> conversions).
func PathInputFromValue(value string) PathInput {
	return PathInput{TextInput: TextInputFromValue(value)}
}

// Resolve expands the field's text against home, which may be absent.
func (p *PathInput) Resolve(home *string) (string, error) {
	return core.ExpandHome(p.Value(), home)
}

// ApplyLocal expands the field's text on this machine and writes the result
// back into the field.
func (p *PathInput) ApplyLocal() (string, error) {
	path, err := core.ExpandLocal(p.Value())
	if err != nil {
		return "", err
	}
	p.SetValue(path)
	return path, nil
}

// Completions are the candidates currently offered by the popup.
func (p *PathInput) Completions() []string {
	return p.completion.candidates
}

// CompletionSelected is the highlighted candidate.
func (p *PathInput) CompletionSelected() int {
	return p.completion.selected
}

// IsCompleting reports whether a popup of candidates is open.
func (p *PathInput) IsCompleting() bool {
	return len(p.completion.candidates) > 0
}

// CompletionTruncated reports whether the host had more matches than it was
// willing to send.
func (p *PathInput) CompletionTruncated() bool {
	return p.completion.truncated
}

// CompletionPending is the text a completion reply is still expected for.
func (p *PathInput) CompletionPending() (string, bool) {
	if p.completion.requested == nil {
		return "", false
	}
	return *p.completion.requested, true
}

// ControlKind is the control declaration this field registers with a form.
func (p *PathInput) ControlKind() ControlKind {
	return ControlKindPathField{
		Len:      len(p.completion.candidates),
		Selected: p.completion.selected,
		Expanded: p.IsCompleting(),
	}
}

// RequestCompletion records a completion request for the current text,
// returning the prefix to ask the host about. An empty field, or one already
// waiting for a reply about this exact text, asks for nothing.
func (p *PathInput) RequestCompletion() (string, bool) {
	value := p.Value()
	if value == "" || (p.completion.requested != nil && *p.completion.requested == value) {
		return "", false
	}
	p.completion.requested = &value
	return value, true
}

// ApplyCompletion applies a host reply. A reply for text the field no longer
// holds is dropped and reported as unapplied.
func (p *PathInput) ApplyCompletion(prefix string, reply core.PathCompletion) bool {
	p.completion.requested = nil
	if p.Value() != prefix {
		return false
	}
	if reply.Insert != nil && *reply.Insert != prefix {
		p.SetValue(*reply.Insert)
	}
	if len(reply.Candidates) > 1 {
		p.completion.candidates = reply.Candidates
		p.completion.selected = 0
		p.completion.truncated = reply.Truncated
	} else {
		p.completion.clearPopup()
	}
	return true
}

// SelectCompletion moves the popup highlight, clamped to the candidates on
// offer.
func (p *PathInput) SelectCompletion(index int) {
	if len(p.completion.candidates) == 0 {
		p.completion.selected = 0
	} else {
		p.completion.selected = min(index, len(p.completion.candidates)-1)
	}
}

// AcceptCompletion replaces the text with the highlighted candidate and
// closes the popup.
func (p *PathInput) AcceptCompletion() bool {
	if p.completion.selected >= len(p.completion.candidates) {
		return false
	}
	p.SetValue(p.completion.candidates[p.completion.selected])
	p.completion.clearPopup()
	p.completion.requested = nil
	return true
}

// DismissCompletion closes the popup and forgets any pending request.
func (p *PathInput) DismissCompletion() {
	p.completion.clearPopup()
	p.completion.requested = nil
}
