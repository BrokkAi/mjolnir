// Port of mj-client/src/settings/view.rs.

// A settings page rendered as data, for a surface that draws its own
// controls, and the one entry point that applies an edit such a surface
// sends. An edit is accepted only through a control the page offers, so a
// surface cannot change a setting the page shows as read only.

package client

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

// SettingsPage is one rendered settings page.
type SettingsPage struct {
	Path []string `json:"path"`
	// Breadcrumb is the page names from the first page down to this one;
	// segment n names the page at the first n keys of Path.
	Breadcrumb []string         `json:"breadcrumb"`
	Help       string           `json:"help"`
	Groups     []SettingsGroup  `json:"groups"`
	Actions    []SettingsAction `json:"actions,omitempty"`
	// Add is adding an entry, on a page that lists entries.
	Add *AddEntry `json:"add,omitempty"`
	// Removable is whether this page is an entry that may be removed.
	Removable bool `json:"removable"`
	// Notices is what this page cannot do here, and where it can be done.
	Notices []string `json:"notices,omitempty"`
}

func (p SettingsPage) MarshalJSON() ([]byte, error) {
	type plain SettingsPage
	value := plain(p)
	if value.Path == nil {
		value.Path = []string{}
	}
	if value.Breadcrumb == nil {
		value.Breadcrumb = []string{}
	}
	if value.Groups == nil {
		value.Groups = []SettingsGroup{}
	}
	return feedMarshalJSON(value)
}

func (p *SettingsPage) UnmarshalJSON(data []byte) error {
	type plain SettingsPage
	return feedDecodeStruct(data, (*plain)(p), false, "path", "breadcrumb", "help", "groups", "removable")
}

type SettingsAction struct {
	ID             string  `json:"id"`
	Label          string  `json:"label"`
	DisabledReason *string `json:"disabled_reason,omitempty"`
}

func (a *SettingsAction) UnmarshalJSON(data []byte) error {
	type plain SettingsAction
	return feedDecodeStruct(data, (*plain)(a), false, "id", "label")
}

type AddEntry struct {
	Label string `json:"label"`
	// Named is whether the new entry needs a name; a list grows without one.
	Named bool `json:"named"`
}

func (e *AddEntry) UnmarshalJSON(data []byte) error {
	type plain AddEntry
	return feedDecodeStruct(data, (*plain)(e), false, "label", "named")
}

type SettingsGroup struct {
	Heading *string       `json:"heading,omitempty"`
	Rows    []SettingsRow `json:"rows"`
}

func (g SettingsGroup) MarshalJSON() ([]byte, error) {
	type plain SettingsGroup
	value := plain(g)
	if value.Rows == nil {
		value.Rows = []SettingsRow{}
	}
	return feedMarshalJSON(value)
}

func (g *SettingsGroup) UnmarshalJSON(data []byte) error {
	type plain SettingsGroup
	return feedDecodeStruct(data, (*plain)(g), false, "rows")
}

// SettingsRow is one row of a page. Its control's fields are flattened into
// the row's object, after the row's own fields.
type SettingsRow struct {
	Path    []string
	Label   string
	Help    string
	Control SettingsControl
}

func (r SettingsRow) MarshalJSON() ([]byte, error) {
	if r.Control == nil {
		return nil, errors.New("a settings row has no control")
	}
	path := r.Path
	if path == nil {
		path = []string{}
	}
	head, err := feedMarshalJSON(struct {
		Path  []string `json:"path"`
		Label string   `json:"label"`
		Help  string   `json:"help,omitempty"`
	}{path, r.Label, r.Help})
	if err != nil {
		return nil, err
	}
	control, err := feedMarshalJSON(r.Control)
	if err != nil {
		return nil, err
	}
	if !bytes.HasPrefix(control, []byte("{")) || string(control) == "{}" {
		return nil, fmt.Errorf("a settings control serialized as %s, not a tagged object", control)
	}
	return append(append(head[:len(head)-1], ','), control[1:]...), nil
}

func (r *SettingsRow) UnmarshalJSON(data []byte) error {
	var row struct {
		Path  []string `json:"path"`
		Label string   `json:"label"`
		Help  string   `json:"help"`
	}
	if err := feedDecodeStruct(data, &row, false, "path", "label"); err != nil {
		return err
	}
	control, err := UnmarshalSettingsControlJSON(data)
	if err != nil {
		return err
	}
	*r = SettingsRow{Path: row.Path, Label: row.Label, Help: row.Help, Control: control}
	return nil
}

// SettingsControl is how a row is edited. Internally tagged on `control`,
// snake_case.
type SettingsControl interface {
	isSettingsControl()
}

// SettingsControlPage opens the page at the row's path.
type SettingsControlPage struct{ Summary string }

type SettingsControlCheckbox struct{ Checked bool }

// SettingsControlChoice holds the current value and the offered ones
// (jsonvalue values).
type SettingsControlChoice struct {
	Value   any
	Choices []SettingsChoice
}

type SettingsControlText struct {
	Value string
	// Placeholder is what an empty field means.
	Placeholder string
	Number      bool
}

// SettingsControlHidden is a literal value the surface may replace but is
// never sent.
type SettingsControlHidden struct{}

type SettingsControlReadOnly struct{ Text string }

func (SettingsControlPage) isSettingsControl()     {}
func (SettingsControlCheckbox) isSettingsControl() {}
func (SettingsControlChoice) isSettingsControl()   {}
func (SettingsControlText) isSettingsControl()     {}
func (SettingsControlHidden) isSettingsControl()   {}
func (SettingsControlReadOnly) isSettingsControl() {}

func (c SettingsControlPage) MarshalJSON() ([]byte, error) {
	return feedMarshalJSON(struct {
		Control string `json:"control"`
		Summary string `json:"summary"`
	}{"page", c.Summary})
}

func (c SettingsControlCheckbox) MarshalJSON() ([]byte, error) {
	return feedMarshalJSON(struct {
		Control string `json:"control"`
		Checked bool   `json:"checked"`
	}{"checkbox", c.Checked})
}

func (c SettingsControlChoice) MarshalJSON() ([]byte, error) {
	value, err := jsonvalue.Marshal(c.Value)
	if err != nil {
		return nil, err
	}
	choices := c.Choices
	if choices == nil {
		choices = []SettingsChoice{}
	}
	return feedMarshalJSON(struct {
		Control string           `json:"control"`
		Value   json.RawMessage  `json:"value"`
		Choices []SettingsChoice `json:"choices"`
	}{"choice", value, choices})
}

func (c SettingsControlText) MarshalJSON() ([]byte, error) {
	return feedMarshalJSON(struct {
		Control     string `json:"control"`
		Value       string `json:"value"`
		Placeholder string `json:"placeholder"`
		Number      bool   `json:"number"`
	}{"text", c.Value, c.Placeholder, c.Number})
}

func (SettingsControlHidden) MarshalJSON() ([]byte, error) {
	return []byte(`{"control":"hidden"}`), nil
}

func (c SettingsControlReadOnly) MarshalJSON() ([]byte, error) {
	return feedMarshalJSON(struct {
		Control string `json:"control"`
		Text    string `json:"text"`
	}{"read_only", c.Text})
}

// UnmarshalSettingsControlJSON decodes a control from its tagged object;
// fields of the surrounding row are ignored.
func UnmarshalSettingsControlJSON(data []byte) (SettingsControl, error) {
	tag, err := feedTag(data, "control")
	if err != nil {
		return nil, err
	}
	switch tag {
	case "page":
		var value struct {
			Summary string `json:"summary"`
		}
		err := feedDecodeStruct(data, &value, false, "summary")
		return SettingsControlPage{Summary: value.Summary}, err
	case "checkbox":
		var value struct {
			Checked bool `json:"checked"`
		}
		err := feedDecodeStruct(data, &value, false, "checked")
		return SettingsControlCheckbox{Checked: value.Checked}, err
	case "choice":
		var value struct {
			Value   json.RawMessage  `json:"value"`
			Choices []SettingsChoice `json:"choices"`
		}
		if err := feedDecodeStruct(data, &value, false, "value", "choices"); err != nil {
			return nil, err
		}
		current, err := jsonvalue.Parse(value.Value)
		if err != nil {
			return nil, err
		}
		return SettingsControlChoice{Value: current, Choices: value.Choices}, nil
	case "text":
		var value struct {
			Value       string `json:"value"`
			Placeholder string `json:"placeholder"`
			Number      bool   `json:"number"`
		}
		err := feedDecodeStruct(data, &value, false, "value", "placeholder", "number")
		return SettingsControlText{Value: value.Value, Placeholder: value.Placeholder, Number: value.Number}, err
	case "hidden":
		return SettingsControlHidden{}, nil
	case "read_only":
		var value struct {
			Text string `json:"text"`
		}
		err := feedDecodeStruct(data, &value, false, "text")
		return SettingsControlReadOnly{Text: value.Text}, err
	}
	return nil, fmt.Errorf("unknown variant `%s`, expected one of `page`, `checkbox`, `choice`, `text`, `hidden`, `read_only`", tag)
}

// SettingsChoice is one offered value (a jsonvalue value) and its label.
type SettingsChoice struct {
	Value any
	Label string
}

func (c SettingsChoice) MarshalJSON() ([]byte, error) {
	value, err := jsonvalue.Marshal(c.Value)
	if err != nil {
		return nil, err
	}
	return feedMarshalJSON(struct {
		Value json.RawMessage `json:"value"`
		Label string          `json:"label"`
	}{value, c.Label})
}

func (c *SettingsChoice) UnmarshalJSON(data []byte) error {
	var choice struct {
		Value json.RawMessage `json:"value"`
		Label string          `json:"label"`
	}
	if err := feedDecodeStruct(data, &choice, false, "value", "label"); err != nil {
		return err
	}
	value, err := jsonvalue.Parse(choice.Value)
	if err != nil {
		return err
	}
	*c = SettingsChoice{Value: value, Label: choice.Label}
	return nil
}

// SettingsEdit is an edit a surface asks for, addressed by the path of the
// row or page it came from. Internally tagged on `kind`, snake_case; unknown
// fields are refused.
type SettingsEdit interface {
	isSettingsEdit()
}

// SettingsEditSet types Text into a text row, or picks Choice on a choice
// row. Choice nil is Rust's None; a non-nil Choice may hold nil (JSON
// null).
type SettingsEditSet struct {
	Path   []string
	Text   *string
	Choice *any
}

type SettingsEditToggle struct{ Path []string }

// SettingsEditAdd adds an entry to the page at Path.
type SettingsEditAdd struct {
	Path []string
	Name *string
}

// SettingsEditRemove removes the entry whose page is Path.
type SettingsEditRemove struct{ Path []string }

type SettingsEditAction struct{ ID string }

func (SettingsEditSet) isSettingsEdit()    {}
func (SettingsEditToggle) isSettingsEdit() {}
func (SettingsEditAdd) isSettingsEdit()    {}
func (SettingsEditRemove) isSettingsEdit() {}
func (SettingsEditAction) isSettingsEdit() {}

// settingsEditPath writes a path as serde writes a Vec: [] when empty.
func settingsEditPath(path []string) []string {
	if path == nil {
		return []string{}
	}
	return path
}

func (e SettingsEditSet) MarshalJSON() ([]byte, error) {
	var choice json.RawMessage
	if e.Choice != nil {
		var err error
		if choice, err = jsonvalue.Marshal(*e.Choice); err != nil {
			return nil, err
		}
	}
	return feedMarshalJSON(struct {
		Kind   string          `json:"kind"`
		Path   []string        `json:"path"`
		Text   *string         `json:"text,omitempty"`
		Choice json.RawMessage `json:"choice,omitempty"`
	}{"set", settingsEditPath(e.Path), e.Text, choice})
}

func (e SettingsEditToggle) MarshalJSON() ([]byte, error) {
	return feedMarshalJSON(struct {
		Kind string   `json:"kind"`
		Path []string `json:"path"`
	}{"toggle", settingsEditPath(e.Path)})
}

func (e SettingsEditAdd) MarshalJSON() ([]byte, error) {
	return feedMarshalJSON(struct {
		Kind string   `json:"kind"`
		Path []string `json:"path"`
		Name *string  `json:"name,omitempty"`
	}{"add", settingsEditPath(e.Path), e.Name})
}

func (e SettingsEditRemove) MarshalJSON() ([]byte, error) {
	return feedMarshalJSON(struct {
		Kind string   `json:"kind"`
		Path []string `json:"path"`
	}{"remove", settingsEditPath(e.Path)})
}

func (e SettingsEditAction) MarshalJSON() ([]byte, error) {
	return feedMarshalJSON(struct {
		Kind string `json:"kind"`
		ID   string `json:"id"`
	}{"action", e.ID})
}

// UnmarshalSettingsEditJSON decodes an edit, refusing fields its variant
// does not have.
func UnmarshalSettingsEditJSON(data []byte) (SettingsEdit, error) {
	tag, err := feedTag(data, "kind")
	if err != nil {
		return nil, err
	}
	switch tag {
	case "set":
		var value struct {
			Kind   string          `json:"kind"`
			Path   []string        `json:"path"`
			Text   *string         `json:"text"`
			Choice json.RawMessage `json:"choice"`
		}
		if err := feedDecodeStruct(data, &value, true, "path"); err != nil {
			return nil, err
		}
		edit := SettingsEditSet{Path: value.Path, Text: value.Text}
		if len(value.Choice) > 0 && string(bytes.TrimSpace(value.Choice)) != "null" {
			choice, err := jsonvalue.Parse(value.Choice)
			if err != nil {
				return nil, err
			}
			edit.Choice = &choice
		}
		return edit, nil
	case "toggle":
		var value struct {
			Kind string   `json:"kind"`
			Path []string `json:"path"`
		}
		err := feedDecodeStruct(data, &value, true, "path")
		return SettingsEditToggle{Path: value.Path}, err
	case "add":
		var value struct {
			Kind string   `json:"kind"`
			Path []string `json:"path"`
			Name *string  `json:"name"`
		}
		err := feedDecodeStruct(data, &value, true, "path")
		return SettingsEditAdd{Path: value.Path, Name: value.Name}, err
	case "remove":
		var value struct {
			Kind string   `json:"kind"`
			Path []string `json:"path"`
		}
		err := feedDecodeStruct(data, &value, true, "path")
		return SettingsEditRemove{Path: value.Path}, err
	case "action":
		var value struct {
			Kind string `json:"kind"`
			ID   string `json:"id"`
		}
		err := feedDecodeStruct(data, &value, true, "id")
		return SettingsEditAction{ID: value.ID}, err
	}
	return nil, fmt.Errorf("unknown variant `%s`, expected one of `set`, `toggle`, `add`, `remove`, `action`", tag)
}

// EditResult is what an accepted edit did: the page to show next, and
// anything else it changed that the person should be told.
type EditResult struct {
	Page   []string
	Notice *string
}

// themeLabel is the theme as a surface that cannot see a terminal reports
// it.
func themeLabel(value, draft any) string {
	return ChoiceLabel([]string{"theme"}, value, draft)
}

func displayed(path []string, value, draft any) string {
	panic("unported: mj-client/src/settings/view.rs displayed")
}

// readOnly: settings whose changes the terminal checks against something
// only it measures or discovers, shown here as they stand.
func readOnly(path []string, draft any) bool {
	panic("unported: mj-client/src/settings/view.rs read_only")
}

func notices(path []string) []string {
	panic("unported: mj-client/src/settings/view.rs notices")
}

func row(draft any, pagePath []string, parent any, key string, path []string) SettingsRow {
	panic("unported: mj-client/src/settings/view.rs row")
}

// RenderPage renders the page at path of draft.
func RenderPage(draft any, path []string) (SettingsPage, error) {
	panic("unported: mj-client/src/settings/view.rs render_page")
}

// Apply applies change, sent from the page at page, to draft.
//
// The page is rendered first and the edit must match a control it offers:
// text or a listed choice for a typed or chosen row, a toggle for a
// checkbox, Add where the page lists entries, Remove on a removable entry.
func Apply(draft *any, page []string, change SettingsEdit) (EditResult, error) {
	panic("unported: mj-client/src/settings/view.rs apply")
}
