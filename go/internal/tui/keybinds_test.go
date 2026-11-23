// Port of mj-tui/src/keybinds.rs (test-only items and tests).

package tui

import (
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"unicode"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// comboKeyEvent is the key press a combination stands for, for tests and
// for the documentation screenshots.
func comboKeyEvent(combo core.KeyCombo) tea.KeyPressMsg {
	var code rune
	switch name := combo.Name.(type) {
	case core.KeyNameChar:
		code = name.Value
	case core.KeyNameEnter:
		code = tea.KeyEnter
	case core.KeyNameEsc:
		code = tea.KeyEscape
	case core.KeyNameTab:
		code = tea.KeyTab
	case core.KeyNameBackspace:
		code = tea.KeyBackspace
	case core.KeyNameDelete:
		code = tea.KeyDelete
	case core.KeyNameInsert:
		code = tea.KeyInsert
	case core.KeyNameHome:
		code = tea.KeyHome
	case core.KeyNameEnd:
		code = tea.KeyEnd
	case core.KeyNamePageUp:
		code = tea.KeyPgUp
	case core.KeyNamePageDown:
		code = tea.KeyPgDown
	case core.KeyNameLeft:
		code = tea.KeyLeft
	case core.KeyNameRight:
		code = tea.KeyRight
	case core.KeyNameUp:
		code = tea.KeyUp
	case core.KeyNameDown:
		code = tea.KeyDown
	case core.KeyNameF:
		code = tea.KeyF1 + rune(name.Value) - 1
	}
	var modifiers tea.KeyMod
	if combo.Modifiers.Ctrl {
		modifiers |= tea.ModCtrl
	}
	if combo.Modifiers.Alt {
		modifiers |= tea.ModAlt
	}
	if combo.Modifiers.Shift {
		modifiers |= tea.ModShift
	}
	if combo.Modifiers.Super {
		modifiers |= tea.ModSuper
	}
	press := tea.KeyPressMsg{Code: code, Mod: modifiers}
	// A terminal reports a printable key's text when no command modifier
	// claims it, which is how the panes read a typed character.
	if _, char := combo.Name.(core.KeyNameChar); char && unicode.IsPrint(code) &&
		modifiers&(tea.ModCtrl|tea.ModAlt|tea.ModSuper) == 0 {
		press.Text = string(code)
	}
	return press
}

func keybindsPlain(character rune) tea.KeyPressMsg {
	return key(character)
}

func keybindsRequireRoute(t *testing.T, got, want KeyRoute, context ...any) {
	t.Helper()
	if !keybindsRoutesEqual(got, want) {
		t.Fatalf("route = %#v, want %#v %s", got, want, fmt.Sprint(context...))
	}
}

// keybindsRoutesEqual is KeyRoute's derived PartialEq.
func keybindsRoutesEqual(left, right KeyRoute) bool {
	leftCommand, leftIsCommand := left.(KeyRouteCommand)
	rightCommand, rightIsCommand := right.(KeyRouteCommand)
	if leftIsCommand || rightIsCommand {
		return leftIsCommand && rightIsCommand && leftCommand.ID == rightCommand.ID &&
			wizardOptionalEqual(leftCommand.Index, rightCommand.Index)
	}
	return left == right
}

func keybindsPaletteRoute() KeyRoute {
	return KeyRouteCommand{ID: CommandIdPalette}
}

func TestSimpleModeHasNoPrefixAndLeavesCtrlBToTheFocusedSurface(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.simpleMode = true
	dashboard.keybinds = core.SimpleKeybinds()

	ctrlB := tea.KeyPressMsg{Code: 'b', Mod: tea.ModCtrl}
	keybindsRequireRoute(t, dashboard.RouteBoundKey(ctrlB), KeyRouteForward{})
	if dashboard.PrefixPending() {
		t.Fatal("no prefix is armed")
	}

	ctrlQ := tea.KeyPressMsg{Code: 'q', Mod: tea.ModCtrl}
	keybindsRequireRoute(t, dashboard.RouteBoundKey(ctrlQ), KeyRouteCommand{ID: CommandIdQuitDetach})
	if dashboard.PrefixPending() {
		t.Fatal("no prefix is armed")
	}

	ctrlF1 := tea.KeyPressMsg{Code: tea.KeyF1, Mod: tea.ModCtrl}
	ctrlP := tea.KeyPressMsg{Code: 'p', Mod: tea.ModCtrl}
	for _, pressed := range []tea.KeyPressMsg{ctrlF1, ctrlP} {
		keybindsRequireRoute(t, dashboard.RouteBoundKey(pressed), keybindsPaletteRoute())
	}

	dormantPrefix := tea.KeyPressMsg{Code: tea.KeyF12, Mod: tea.ModCtrl}
	keybindsRequireRoute(t, dashboard.RouteBoundKey(dormantPrefix), KeyRouteForward{})
	if dashboard.PrefixPending() {
		t.Fatal("no prefix is armed")
	}
}

func TestAConfigReloadDoesNotRestorePrefixBindingsInSimpleMode(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.simpleMode = true
	dashboard.keybinds = core.SimpleKeybinds()
	reloaded := config()
	reloaded.Keys.Prefix = "ctrl+a"
	reloaded.Keys.Detach = core.BindingConfigFrom("prefix+q")
	dashboard.SetConfig(reloaded)

	ctrlB := tea.KeyPressMsg{Code: 'b', Mod: tea.ModCtrl}
	keybindsRequireRoute(t, dashboard.RouteBoundKey(ctrlB), KeyRouteForward{})
	if dashboard.PrefixPending() {
		t.Fatal("no prefix is armed")
	}
	ctrlQ := tea.KeyPressMsg{Code: 'q', Mod: tea.ModCtrl}
	command, ok := dashboard.RouteBoundKey(ctrlQ).(KeyRouteCommand)
	if !ok || command.ID != CommandIdQuitDetach {
		t.Fatalf("ctrl+q routes to %#v", command)
	}
}

func TestPressingThePrefixTwiceForwardsTheLiteralKeyAndDisarms(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	keybindsRequireRoute(t, dashboard.RouteBoundKey(prefixKey()), KeyRouteConsumed{})
	keybindsRequireRoute(t, dashboard.RouteBoundKey(prefixKey()), KeyRouteForward{})
	if dashboard.PrefixPending() {
		t.Fatal("the doubled prefix disarms")
	}
	if notice, ok := dashboard.notices.Current(); ok {
		t.Fatalf("notice = %q", notice)
	}
}

func TestEscCancelsAPendingPrefixWithoutForwarding(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.RouteBoundKey(prefixKey())
	keybindsRequireRoute(t, dashboard.RouteBoundKey(key(tea.KeyEscape)), KeyRouteConsumed{})
	if dashboard.PrefixPending() {
		t.Fatal("Esc disarms the prefix")
	}
	if notice, ok := dashboard.notices.Current(); ok {
		t.Fatalf("notice = %q", notice)
	}
}

func TestControlPunctuationPrefixesAcceptLegacyAndEnhancedTerminalEvents(t *testing.T) {
	for _, testCase := range []struct {
		prefix   string
		legacy   rune
		enhanced rune
	}{
		{`ctrl+\`, '4', '\\'},
		{"ctrl+]", '5', ']'},
		{"ctrl+^", '6', '^'},
		{"ctrl+_", '7', '_'},
	} {
		for _, character := range []rune{testCase.legacy, testCase.enhanced} {
			dashboard := dashboardWithSession(runningSession())
			configured := config()
			configured.Keys.Prefix = testCase.prefix
			dashboard.SetConfig(configured)
			event := tea.KeyPressMsg{Code: character, Mod: tea.ModCtrl}
			keybindsRequireRoute(t, dashboard.RouteBoundKey(event), KeyRouteConsumed{})
			if !dashboard.PrefixPending() {
				t.Fatalf("%s: %c", testCase.prefix, character)
			}
			keybindsRequireRoute(t, dashboard.RouteBoundKey(keybindsPlain('?')), keybindsPaletteRoute())
			if dashboard.PrefixPending() {
				t.Fatal("the command disarms the prefix")
			}
			keybindsRequireRoute(t, dashboard.RouteBoundKey(event), KeyRouteConsumed{})
			keybindsRequireRoute(t, dashboard.RouteBoundKey(event), KeyRouteForward{})
			if dashboard.PrefixPending() {
				t.Fatal("the doubled prefix disarms")
			}
		}
	}
}

func TestShiftAndUppercaseLettersMatchTheSameBinding(t *testing.T) {
	for _, pressed := range []tea.KeyPressMsg{
		{Code: 'N'},
		{Code: 'N', Mod: tea.ModShift},
		{Code: 'n', Mod: tea.ModShift},
	} {
		dashboard := dashboardWithSession(runningSession())
		dashboard.RouteBoundKey(prefixKey())
		keybindsRequireRoute(t, dashboard.RouteBoundKey(pressed), KeyRouteCommand{ID: CommandIdWorkspaces}, pressed)
	}
}

func TestColonAndQuestionMarkMatchWithOrWithoutTheShiftFlag(t *testing.T) {
	for _, testCase := range []struct {
		pressed  tea.KeyPressMsg
		expected CommandId
	}{
		{tea.KeyPressMsg{Code: ':'}, CommandIdPalette},
		{tea.KeyPressMsg{Code: ':', Mod: tea.ModShift}, CommandIdPalette},
		{tea.KeyPressMsg{Code: '?'}, CommandIdPalette},
		{tea.KeyPressMsg{Code: '?', Mod: tea.ModShift}, CommandIdPalette},
	} {
		dashboard := dashboardWithSession(runningSession())
		keybindsRequireRoute(t, dashboard.RouteBoundKey(testCase.pressed), KeyRouteForward{}, testCase.pressed)
		dashboard.RouteBoundKey(prefixKey())
		keybindsRequireRoute(t, dashboard.RouteBoundKey(testCase.pressed), KeyRouteCommand{ID: testCase.expected}, testCase.pressed)
	}
}

func TestDecodedLegacyHelpBindingsArePaletteAliases(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "config.toml")
	configText := fmt.Sprintf(
		"version = %d\n\n[keys]\nprefix = \"ctrl+a\"\nhelp = [\"ctrl+alt+p\", \"prefix+x\", \"prefix+f1\"]\npalette = \"\"\n",
		core.ConfigVersion,
	)
	if err := os.WriteFile(path, []byte(configText), 0o600); err != nil {
		t.Fatal(err)
	}
	loaded, err := core.ConfigLoadFrom(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := loaded.Keys.Help.Entries(); !slices.Equal(got, []string{"ctrl+alt+p", "prefix+x", "prefix+f1"}) {
		t.Fatalf("help entries = %q", got)
	}
	if len(loaded.Keys.Palette.Entries()) != 0 {
		t.Fatalf("palette entries = %q", loaded.Keys.Palette.Entries())
	}
	if err := loaded.SaveTo(path); err != nil {
		t.Fatal(err)
	}
	loaded, err = core.ConfigLoadFrom(path)
	if err != nil {
		t.Fatal(err)
	}
	dashboard := dashboardWithSession(runningSession())
	dashboard.SetConfig(loaded.WithLocalTargets())

	if got := dashboard.keyLabels(CommandIdPalette); !slices.Equal(got, []string{"?", "ctrl+alt+p", "ctrl+a x", "ctrl+a f1"}) {
		t.Fatalf("palette key labels = %q", got)
	}
	if footer, ok := dashboard.footerKey(CommandIdPalette); !ok || footer != "x / f1" {
		t.Fatalf("palette footer key = %q, %v", footer, ok)
	}
	if direct, ok := dashboard.directPaletteFooterKey(); !ok || direct != "ctrl+alt+p" {
		t.Fatalf("direct palette footer key = %q, %v", direct, ok)
	}
	footer := combinedFooterText(dashboard, 200)
	for _, expected := range []string{"ctrl+a then g sessions", "x / f1 palette", "ctrl+alt+p palette"} {
		if !strings.Contains(footer, expected) {
			t.Fatalf("footer lacks %q: %s", expected, footer)
		}
	}
	if strings.Contains(footer, "ctrl+a then ctrl+alt+p") {
		t.Fatalf("footer: %s", footer)
	}
	prefix := tea.KeyPressMsg{Code: 'a', Mod: tea.ModCtrl}
	for _, pressed := range []tea.KeyPressMsg{key('x'), {Code: tea.KeyF1}} {
		keybindsRequireRoute(t, dashboard.RouteBoundKey(prefix), KeyRouteConsumed{})
		keybindsRequireRoute(t, dashboard.RouteBoundKey(pressed), keybindsPaletteRoute())
	}
	keybindsRequireRoute(t, dashboard.RouteBoundKey(tea.KeyPressMsg{Code: 'p', Mod: tea.ModCtrl | tea.ModAlt}), keybindsPaletteRoute())

	dashboard.RouteBoundKey(prefix)
	keybindsRequireRoute(t, dashboard.RouteBoundKey(tea.KeyPressMsg{Code: tea.KeyF12}), KeyRouteConsumed{})
	if notice, ok := dashboard.notices.Current(); !ok || notice != "ctrl+a f12 is not bound; ctrl+alt+p opens the command palette" {
		t.Fatalf("notice = %q, %v", notice, ok)
	}

	disabled := fmt.Sprintf(
		"version = %d\n\n[keys]\nprefix = \"ctrl+a\"\nhelp = \"\"\npalette = \"\"\nworkspace_manager = \"prefix+?\"\n",
		core.ConfigVersion,
	)
	if err := os.WriteFile(path, []byte(disabled), 0o600); err != nil {
		t.Fatal(err)
	}
	loaded, err = core.ConfigLoadFrom(path)
	if err != nil {
		t.Fatal(err)
	}
	dashboard.SetConfig(loaded.WithLocalTargets())
	dashboard.RouteBoundKey(prefix)
	banner := prefixBannerLine(dashboard)
	if text := banner.String(); text != " PREFIX  esc cancel · ctrl+a send" {
		t.Fatalf("banner = %q", text)
	}
	keybindsRequireRoute(t, dashboard.RouteBoundKey(keybindsPlain('?')), KeyRouteCommand{ID: CommandIdWorkspaces})
	dashboard.RouteBoundKey(prefix)
	keybindsRequireRoute(t, dashboard.RouteBoundKey(tea.KeyPressMsg{Code: tea.KeyF12}), KeyRouteConsumed{})
	if notice, ok := dashboard.notices.Current(); !ok || notice != "ctrl+a f12 is not bound" {
		t.Fatalf("notice = %q, %v", notice, ok)
	}

	duplicate := fmt.Sprintf(
		"version = %d\n\n[keys]\nprefix = \"ctrl+a\"\nhelp = \"prefix+x\"\npalette = \"prefix+x\"\n",
		core.ConfigVersion,
	)
	if err := os.WriteFile(path, []byte(duplicate), 0o600); err != nil {
		t.Fatal(err)
	}
	loaded, err = core.ConfigLoadFrom(path)
	if err != nil {
		t.Fatal(err)
	}
	dashboard.SetConfig(loaded.WithLocalTargets())
	count := 0
	for _, label := range dashboard.keyLabels(CommandIdPalette) {
		if label == "ctrl+a x" {
			count++
		}
	}
	if count != 1 {
		t.Fatalf("ctrl+a x appears %d times", count)
	}
	keybindsRequireRoute(t, dashboard.RouteBoundKey(prefix), KeyRouteConsumed{})
	keybindsRequireRoute(t, dashboard.RouteBoundKey(key('x')), keybindsPaletteRoute())
}

func TestAMousePressCancelsAPendingPrefix(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.RouteBoundKey(prefixKey())
	if !dashboard.PrefixPending() {
		t.Fatal("the prefix is armed")
	}
	dashboard.HandleMouse(tea.MouseClickMsg{Button: tea.MouseLeft, X: 0, Y: 0})
	if dashboard.PrefixPending() {
		t.Fatal("a mouse press disarms the prefix")
	}
}
