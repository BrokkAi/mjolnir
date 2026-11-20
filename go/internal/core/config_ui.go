// Port of mj-core/src/config/ui.rs.
//
// Terminal appearance settings.

package core

import (
	"encoding/json"
	"fmt"
	"strings"
)

type SpinnerStyle string

const (
	// SpinnerStylePulse: a bright dot glides across a faint row
	// (typing-indicator feel).
	SpinnerStylePulse SpinnerStyle = "pulse"
	// SpinnerStyleWave: an undulating braille ribbon rolls across the strip.
	SpinnerStyleWave SpinnerStyle = "wave"
	// SpinnerStyleBars: vertical bars bounce like an audio equalizer.
	SpinnerStyleBars SpinnerStyle = "bars"
	// SpinnerStyleShimmer: the whole row breathes brightness in unison
	// (calmest).
	SpinnerStyleShimmer SpinnerStyle = "shimmer"
	// SpinnerStyleGlobe: a lit sphere rotates in place, carrying its dark
	// side into view.
	SpinnerStyleGlobe SpinnerStyle = "globe"
	// SpinnerStyleScan: a lit head sweeps to one wall and back, trailing a
	// fading tail. The default.
	SpinnerStyleScan SpinnerStyle = "scan"
)

// SpinnerStyleAll is every spinner, in the command palette's cycle order.
var SpinnerStyleAll = [6]SpinnerStyle{
	SpinnerStylePulse, SpinnerStyleWave, SpinnerStyleBars, SpinnerStyleShimmer, SpinnerStyleGlobe, SpinnerStyleScan,
}

// DefaultSpinnerStyle is SpinnerStyle's Default.
func DefaultSpinnerStyle() SpinnerStyle { return SpinnerStyleScan }

func (s SpinnerStyle) AsStr() string { return string(s) }

func (s SpinnerStyle) IsDefault() bool { return s == DefaultSpinnerStyle() }

// Next is the next animation in the command palette's stable cycle.
func (s SpinnerStyle) Next() SpinnerStyle {
	index := 0
	for i, style := range SpinnerStyleAll {
		if style == s {
			index = i
			break
		}
	}
	return SpinnerStyleAll[(index+1)%len(SpinnerStyleAll)]
}

func (s SpinnerStyle) String() string { return string(s) }

// ParseSpinnerStyle is FromStr for SpinnerStyle.
func ParseSpinnerStyle(value string) (SpinnerStyle, error) {
	for _, style := range SpinnerStyleAll {
		if string(style) == value {
			return style, nil
		}
	}
	names := make([]string, len(SpinnerStyleAll))
	for i, style := range SpinnerStyleAll {
		names[i] = string(style)
	}
	return "", fmt.Errorf("unknown spinner %s; expected one of: %s", debugString(value), strings.Join(names, ", "))
}

func (s *SpinnerStyle) UnmarshalText(text []byte) error {
	for _, style := range SpinnerStyleAll {
		if string(style) == string(text) {
			*s = style
			return nil
		}
	}
	return serdeUnknownVariant(string(text), "pulse", "wave", "bars", "shimmer", "globe", "scan")
}

// UiTheme is a color palette for the terminal dashboard and conversation.
type UiTheme string

const (
	UiThemeMidnight     UiTheme = "midnight"
	UiThemeLight        UiTheme = "light"
	UiThemeDarcula      UiTheme = "darcula"
	UiThemeHighContrast UiTheme = "high-contrast"
	// UiThemeHighContrastLight: black on white, for readers who need more
	// contrast than Light gives.
	UiThemeHighContrastLight UiTheme = "high-contrast-light"
	// UiThemeColorblindDark: status in blue, yellow, and orange on
	// Midnight's surfaces, chosen to stay apart for red-green and blue-yellow
	// color blindness.
	UiThemeColorblindDark UiTheme = "colorblind-dark"
	// UiThemeColorblindLight: as UiThemeColorblindDark, on Light's surfaces.
	UiThemeColorblindLight UiTheme = "colorblind-light"
	// UiThemeCatppuccinMocha: Catppuccin's dark Mocha flavor, painting its
	// own base color.
	UiThemeCatppuccinMocha UiTheme = "catppuccin-mocha"
	// UiThemeCatppuccinLatte: Catppuccin's light Latte flavor, painting its
	// own base color.
	UiThemeCatppuccinLatte UiTheme = "catppuccin-latte"
	// UiThemeCampbell: Windows Terminal's default scheme.
	UiThemeCampbell UiTheme = "campbell"
	// UiThemeOneHalfDark: Windows Terminal's One Half Dark scheme.
	UiThemeOneHalfDark UiTheme = "one-half-dark"
	// UiThemeOneHalfLight: Windows Terminal's One Half Light scheme.
	UiThemeOneHalfLight UiTheme = "one-half-light"
	// UiThemeSolarizedDark: Solarized's dark scheme, as Windows Terminal
	// ships it.
	UiThemeSolarizedDark UiTheme = "solarized-dark"
	// UiThemeSolarizedLight: Solarized's light scheme, as Windows Terminal
	// ships it.
	UiThemeSolarizedLight UiTheme = "solarized-light"
	// UiThemeTerminalDark: the terminal's own background, foreground, and 16
	// ANSI colors, picked for a dark scheme. Selection and focus use reverse
	// video.
	UiThemeTerminalDark UiTheme = "terminal-dark"
	// UiThemeTerminalLight: as UiThemeTerminalDark, with ANSI colors picked
	// for a light scheme.
	UiThemeTerminalLight UiTheme = "terminal-light"
	// UiThemeMono: no colors at all: the terminal's own foreground and
	// background, with bold and reverse video carrying focus and selection.
	// Chosen automatically when the NO_COLOR environment variable is set.
	UiThemeMono UiTheme = "mono"
)

// UiThemeAll is every palette, in display order.
var UiThemeAll = [17]UiTheme{
	UiThemeMidnight, UiThemeLight, UiThemeDarcula, UiThemeHighContrast, UiThemeHighContrastLight,
	UiThemeColorblindDark, UiThemeColorblindLight, UiThemeCatppuccinMocha, UiThemeCatppuccinLatte,
	UiThemeCampbell, UiThemeOneHalfDark, UiThemeOneHalfLight, UiThemeSolarizedDark, UiThemeSolarizedLight,
	UiThemeTerminalDark, UiThemeTerminalLight, UiThemeMono,
}

// DefaultUiTheme is UiTheme's Default.
func DefaultUiTheme() UiTheme { return UiThemeMidnight }

func (t UiTheme) Label() string {
	switch t {
	case UiThemeMidnight:
		return "Midnight"
	case UiThemeLight:
		return "Light"
	case UiThemeDarcula:
		return "Darcula"
	case UiThemeHighContrast:
		return "High Contrast"
	case UiThemeHighContrastLight:
		return "High Contrast Light"
	case UiThemeColorblindDark:
		return "Colorblind Dark"
	case UiThemeColorblindLight:
		return "Colorblind Light"
	case UiThemeCatppuccinMocha:
		return "Catppuccin Mocha"
	case UiThemeCatppuccinLatte:
		return "Catppuccin Latte"
	case UiThemeCampbell:
		return "Campbell"
	case UiThemeOneHalfDark:
		return "One Half Dark"
	case UiThemeOneHalfLight:
		return "One Half Light"
	case UiThemeSolarizedDark:
		return "Solarized Dark"
	case UiThemeSolarizedLight:
		return "Solarized Light"
	case UiThemeTerminalDark:
		return "Terminal Dark"
	case UiThemeTerminalLight:
		return "Terminal Light"
	case UiThemeMono:
		return "Monochrome"
	}
	return string(t)
}

// UnmarshalText accepts every palette name and the `dracula` alias.
func (t *UiTheme) UnmarshalText(text []byte) error {
	name := string(text)
	if name == "dracula" {
		*t = UiThemeDarcula
		return nil
	}
	for _, theme := range UiThemeAll {
		if string(theme) == name {
			*t = theme
			return nil
		}
	}
	names := make([]string, len(UiThemeAll))
	for i, theme := range UiThemeAll {
		names[i] = string(theme)
	}
	return serdeUnknownVariant(name, names...)
}

// ThemeChoice is the `theme` setting: a palette, or Auto, which picks
// Midnight or Light from the background the terminal reports. Only
// mj_chat::theme::effective_theme turns a choice into a palette, so a render
// scope never holds Auto.
//
// It is written as `auto` or the palette's own name, so every file written
// before Auto existed, aliases included, reads unchanged. Fields of this type
// are read with UnmarshalThemeChoiceJSON.
type ThemeChoice interface {
	isThemeChoice()
	Label() string
	MarshalJSON() ([]byte, error)
}

type ThemeChoiceAuto struct{}

type ThemeChoiceFixed struct{ Value UiTheme }

func (ThemeChoiceAuto) isThemeChoice()  {}
func (ThemeChoiceFixed) isThemeChoice() {}

func (ThemeChoiceAuto) Label() string    { return "Auto" }
func (t ThemeChoiceFixed) Label() string { return t.Value.Label() }

func (ThemeChoiceAuto) MarshalJSON() ([]byte, error)    { return []byte(`"auto"`), nil }
func (t ThemeChoiceFixed) MarshalJSON() ([]byte, error) { return json.Marshal(string(t.Value)) }

// UnmarshalThemeChoiceJSON reads a ThemeChoice.
func UnmarshalThemeChoiceJSON(data []byte) (ThemeChoice, error) {
	var name string
	if err := json.Unmarshal(data, &name); err != nil {
		return nil, fmt.Errorf("invalid type: %s, expected a string", serdeUnexpected(data))
	}
	if name == "auto" {
		return ThemeChoiceAuto{}, nil
	}
	var theme UiTheme
	if err := theme.UnmarshalText([]byte(name)); err != nil {
		return nil, fmt.Errorf("%v, or `auto`", err)
	}
	return ThemeChoiceFixed{Value: theme}, nil
}

// ThemeChoiceAll is Auto first, then every palette in UiThemeAll order.
func ThemeChoiceAll() []ThemeChoice {
	choices := []ThemeChoice{ThemeChoiceAuto{}}
	for _, theme := range UiThemeAll {
		choices = append(choices, ThemeChoiceFixed{Value: theme})
	}
	return choices
}

// DefaultThemeChoice is ThemeChoice's Default.
func DefaultThemeChoice() ThemeChoice { return ThemeChoiceFixed{Value: DefaultUiTheme()} }

func themeChoiceIsDefault(choice ThemeChoice) bool {
	return choice == nil || choice == DefaultThemeChoice()
}

type SessionsSide string

const (
	SessionsSideLeft  SessionsSide = "left"
	SessionsSideRight SessionsSide = "right"
)

// DefaultSessionsSide is SessionsSide's Default.
func DefaultSessionsSide() SessionsSide { return SessionsSideLeft }

func (s SessionsSide) isDefault() bool { return s == DefaultSessionsSide() }

func (s *SessionsSide) UnmarshalText(text []byte) error {
	switch SessionsSide(text) {
	case SessionsSideLeft, SessionsSideRight:
		*s = SessionsSide(text)
		return nil
	}
	return serdeUnknownVariant(string(text), "left", "right")
}

// AdvancedConfig holds settings that are useful while diagnosing or tuning
// the client surface.
//
// The section is optional on disk so configurations written before it was
// introduced retain their existing representation and behavior.
type AdvancedConfig struct {
	DetailedActivityClocks bool         `json:"detailed_activity_clocks" toml:"detailed_activity_clocks"`
	SessionOrder           SessionOrder `json:"session_order" toml:"session_order"`
	// Symbols is which glyphs the dashboard draws with. Nil decides from the
	// terminal: ASCII on the Linux console or a locale without UTF-8,
	// Unicode otherwise.
	Symbols    *SymbolSet `json:"symbols,omitempty" toml:"symbols,omitempty"`
	ToolOutput ToolOutput `json:"tool_output" toml:"tool_output"`
	// ToolOutputLines is how many output rows an inline tool call shows, the
	// omission marker included. Zero shows none.
	ToolOutputLines int `json:"tool_output_lines" toml:"tool_output_lines"`
	// AutoAfkMinutes enters AFK after this many minutes without operator
	// input. Zero disables automatic AFK.
	AutoAfkMinutes uint32 `json:"auto_afk_minutes" toml:"auto_afk_minutes"`
}

// DefaultAdvancedConfig is AdvancedConfig's Default.
func DefaultAdvancedConfig() AdvancedConfig {
	return AdvancedConfig{
		SessionOrder:    DefaultSessionOrder(),
		ToolOutput:      DefaultToolOutput(),
		ToolOutputLines: DefaultToolOutputLines,
	}
}

func (a AdvancedConfig) MarshalJSON() ([]byte, error) {
	var object serdeObject
	object.fieldUnless(isFalse(a.DetailedActivityClocks), "detailed_activity_clocks", a.DetailedActivityClocks)
	object.fieldUnless(a.SessionOrder.isDefault(), "session_order", a.SessionOrder)
	object.fieldUnless(a.Symbols == nil, "symbols", a.Symbols)
	object.fieldUnless(a.ToolOutput.isDefault(), "tool_output", a.ToolOutput)
	object.fieldUnless(isDefaultToolOutputLines(a.ToolOutputLines), "tool_output_lines", a.ToolOutputLines)
	object.fieldUnless(isZero(a.AutoAfkMinutes), "auto_afk_minutes", a.AutoAfkMinutes)
	return object.finish()
}

func (a *AdvancedConfig) UnmarshalJSON(data []byte) error {
	type plain AdvancedConfig
	value := plain(DefaultAdvancedConfig())
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	*a = AdvancedConfig(value)
	return nil
}

const DefaultToolOutputLines = 5

func isDefaultToolOutputLines(lines int) bool { return lines == DefaultToolOutputLines }

func isZero(value uint32) bool { return value == 0 }

// ToolOutput is how the conversation draws tool calls.
//
// Grouped folds a run of finished calls into one row of short names and keeps
// only the newest thought of the run. Inline gives every call its own row with
// its full command and a few lines of its output, and keeps every thought, the
// way Codex draws a turn.
type ToolOutput string

const (
	ToolOutputGrouped ToolOutput = "grouped"
	ToolOutputInline  ToolOutput = "inline"
)

// DefaultToolOutput is ToolOutput's Default.
func DefaultToolOutput() ToolOutput { return ToolOutputGrouped }

func (t ToolOutput) isDefault() bool { return t == DefaultToolOutput() }

func (t *ToolOutput) UnmarshalText(text []byte) error {
	switch ToolOutput(text) {
	case ToolOutputGrouped, ToolOutputInline:
		*t = ToolOutput(text)
		return nil
	}
	return serdeUnknownVariant(string(text), "grouped", "inline")
}

// SymbolSet is the character set the dashboard draws status symbols, borders,
// and separators with.
type SymbolSet string

const (
	SymbolSetUnicode SymbolSet = "unicode"
	SymbolSetAscii   SymbolSet = "ascii"
)

func (s *SymbolSet) UnmarshalText(text []byte) error {
	switch SymbolSet(text) {
	case SymbolSetUnicode, SymbolSetAscii:
		*s = SymbolSet(text)
		return nil
	}
	return serdeUnknownVariant(string(text), "unicode", "ascii")
}

// NotifyConfig is how the dashboard tells the person about a session they are
// not looking at: one that asked a question, failed, or finished with an
// unread answer.
//
// Terminal rings the terminal bell and works over SSH and inside a
// multiplexer, which is why it is the default. System also posts a desktop
// notification through `osascript` on macOS, `notify-send` on Linux, or a
// PowerShell toast on Windows. Off reports nothing. Independently of the mode,
// Title keeps the terminal window title showing how many sessions are waiting.
type NotifyConfig struct {
	Mode NotifyMode `json:"mode" toml:"mode"`
	// Bell is whether a notification also rings the terminal bell. Off leaves
	// the system notification alone as the only signal.
	Bell bool `json:"bell" toml:"bell"`
	// DelaySeconds is how long a session must stay in need of a person
	// before it is reported, so a question the agent answers itself does not
	// ring.
	DelaySeconds uint64 `json:"delay_seconds" toml:"delay_seconds"`
	// Title is whether the terminal title shows the waiting and unread counts.
	Title bool `json:"title" toml:"title"`
}

// DefaultNotifyConfig is NotifyConfig's Default.
func DefaultNotifyConfig() NotifyConfig {
	return NotifyConfig{Mode: NotifyModeTerminal, Bell: true, DelaySeconds: 2, Title: true}
}

func (n *NotifyConfig) UnmarshalJSON(data []byte) error {
	type plain NotifyConfig
	value := plain(DefaultNotifyConfig())
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	*n = NotifyConfig(value)
	return nil
}

func (n NotifyConfig) isDefault() bool { return n == DefaultNotifyConfig() }

type NotifyMode string

const (
	NotifyModeOff      NotifyMode = "off"
	NotifyModeTerminal NotifyMode = "terminal"
	NotifyModeSystem   NotifyMode = "system"
)

// DefaultNotifyMode is NotifyMode's Default.
func DefaultNotifyMode() NotifyMode { return NotifyModeTerminal }

func (n *NotifyMode) UnmarshalText(text []byte) error {
	switch NotifyMode(text) {
	case NotifyModeOff, NotifyModeTerminal, NotifyModeSystem:
		*n = NotifyMode(text)
		return nil
	}
	return serdeUnknownVariant(string(text), "off", "terminal", "system")
}

// SessionOrder is how the Sessions pane orders its rows.
//
// Project groups sessions under a heading per project, in creation order,
// which keeps related work together. Priority drops the headings and lists the
// sessions that need a person first: waiting for input, then failed, then
// unread, then working, then idle, newest activity first within a level.
type SessionOrder string

const (
	SessionOrderProject  SessionOrder = "project"
	SessionOrderPriority SessionOrder = "priority"
)

// DefaultSessionOrder is SessionOrder's Default.
func DefaultSessionOrder() SessionOrder { return SessionOrderProject }

func (s SessionOrder) isDefault() bool { return s == DefaultSessionOrder() }

func (s *SessionOrder) UnmarshalText(text []byte) error {
	switch SessionOrder(text) {
	case SessionOrderProject, SessionOrderPriority:
		*s = SessionOrder(text)
		return nil
	}
	return serdeUnknownVariant(string(text), "project", "priority")
}

func (a AdvancedConfig) isDefault() bool {
	return a.DetailedActivityClocks == false && a.SessionOrder == DefaultSessionOrder() && a.Symbols == nil &&
		a.ToolOutput == DefaultToolOutput() && a.ToolOutputLines == DefaultToolOutputLines && a.AutoAfkMinutes == 0
}
