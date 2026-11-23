// Tests of mj-chat/src/theme.rs.

package chat

import (
	"fmt"
	"reflect"
	"strings"
	"testing"
	"unicode/utf8"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestThemeScopesRestoreColorsAfterNestedRenderingAndPanics(t *testing.T) {
	original := Current()
	WithTheme(core.UiThemeLight, func() {
		if Base().Bg != light.Background {
			t.Fatalf("base background = %#v", Base().Bg)
		}
		panicked := func() (recovered any) {
			defer func() { recovered = recover() }()
			WithTheme(core.UiThemeDarcula, func() {
				if Base().Fg != darcula.Text {
					t.Fatalf("base foreground = %#v", Base().Fg)
				}
				panic("interrupted render")
			})
			return nil
		}()
		if panicked == nil {
			t.Fatal("the nested render did not panic")
		}
		if Base().Bg != light.Background {
			t.Fatalf("base background after the panic = %#v", Base().Bg)
		}
	})
	if Current() != original {
		t.Fatalf("theme after the scope = %v, want %v", Current(), original)
	}
}

func renderedThemeRows(label string, width int, rows []Line, metadata string) string {
	height := len(rows)
	buffer := uv.NewBuffer(width, height)
	frame := NewFrame(buffer)
	frame.RenderWidget(NewParagraph(TextFrom(rows...)), frame.Area())
	output := fmt.Sprintf("=== %s (%dx%d) ===\n%s\n", label, width, height, metadata)
	output += strings.Join(BufferLines(buffer), "\n")
	output += "\n"
	return output
}

// optionRgbDebug is Rust's Debug of an Option<Rgb>.
func optionRgbDebug(background *Rgb) string {
	if background == nil {
		return "None"
	}
	return "Some(" + background.GoString() + ")"
}

func TestGoldenThemeRendering(t *testing.T) {
	var output strings.Builder
	for _, set := range []struct {
		name   string
		glyphs *Glyphs
	}{{"Unicode", &UnicodeGlyphs}, {"ASCII", &ASCIIGlyphs}} {
		fmt.Fprintf(&output, "%s attention: needs_input=%s important=%s unread=%s failed=%s unreachable=%s\n",
			set.name, set.glyphs.Waiting, set.glyphs.Important, set.glyphs.Unread, set.glyphs.Failed, set.glyphs.Unreachable)
	}

	lightChoice := core.ThemeChoiceFixed{Value: core.UiThemeLight}
	monoTheme := Resolve(lightChoice, true, nil).Theme
	var monoStyles string
	var monoRows []Line
	WithTheme(monoTheme, func() {
		monoStyles = fmt.Sprintf(
			"NO_COLOR theme: %#v; color-enabled light remains: %#v; base: %#v; selection: %#v; active control: %#v",
			monoTheme, Resolve(lightChoice, false, nil).Theme, Base(), Selection(true), ActiveControl())
		monoRows = []Line{LineFrom(
			StyledSpan("selected", Selection(true)),
			RawSpan(" "),
			StyledSpan("active", ActiveControl()),
			RawSpan(" "),
			StyledSpan("canvas", Base()),
		)}
	})
	output.WriteString(renderedThemeRows("NO_COLOR monochrome controls", 40, monoRows, monoStyles))

	// Auto against backgrounds terminals actually report: xterm's white,
	// Solarized's two bases, Catppuccin Mocha's base, a mid gray, and a
	// terminal that does not answer. NO_COLOR still wins.
	output.WriteString("=== Auto resolution ===\n")
	for _, background := range []*Rgb{
		{255, 255, 255},
		{253, 246, 227},
		{0, 43, 54},
		{30, 30, 46},
		{128, 128, 128},
		nil,
	} {
		fmt.Fprintf(&output, "%s: %#v\n", optionRgbDebug(background), Resolve(core.ThemeChoiceAuto{}, false, background))
	}
	fmt.Fprintf(&output, "NO_COLOR over a light background: %#v\n",
		Resolve(core.ThemeChoiceAuto{}, true, &Rgb{255, 255, 255}))

	// A terminal palette's contrast belongs to the user's scheme, so its
	// record is the styles that carry selection and focus.
	for _, theme := range []core.UiTheme{core.UiThemeTerminalDark, core.UiThemeTerminalLight} {
		var rows []Line
		var styles string
		WithTheme(theme, func() {
			styles = fmt.Sprintf(
				"selection: %#v; raised: %#v; active control: %#v; focus control: %#v; field: %#v; focused border: %#v",
				Selection(true), Raised(), ActiveControl(), FocusControl(), Field(true), Border(true))
			palette := ThemePalette()
			rows = []Line{LineFrom(
				StyledSpan("selected", Selection(true)),
				RawSpan(" "),
				StyledSpan("focus", FocusControl()),
				RawSpan(" "),
				StyledSpan("muted", Muted()),
				RawSpan(" "),
				StyledSpan("error", Style{}.Foreground(palette.Error)),
			)}
			styles = styles + "\n" + palette.GoString()
		})
		output.WriteString(renderedThemeRows(theme.GoString()+" controls", 40, rows, styles))
	}

	for _, theme := range core.UiThemeAll {
		palette := PaletteFor(theme)
		if palette.ReverseVideo {
			continue
		}
		foregrounds := []struct {
			name  string
			color Color
		}{
			{"text", palette.Text},
			{"muted", palette.Muted},
			{"accent", palette.Accent},
			{"secondary", palette.Secondary},
			{"success", palette.Success},
			{"warning", palette.Warning},
			{"error", palette.Error},
			{"session_error", palette.SessionError},
			{"session_activity", palette.SessionActivity},
			{"session_attention", palette.SessionAttention},
			{"session_idle", palette.SessionIdle},
		}
		backgrounds := []struct {
			name  string
			color Color
		}{
			{"background", palette.Background},
			{"surface", palette.Surface},
			{"surface_raised", palette.SurfaceRaised},
			{"selection", palette.Selection},
		}
		var rows []Line
		var details strings.Builder
		fmt.Fprintf(&details, "landmarks: background=%#v; surface=%#v; raised=%#v; selection=%#v\n",
			palette.Background, palette.Surface, palette.SurfaceRaised, palette.Selection)
		for _, foreground := range foregrounds {
			for _, background := range backgrounds {
				rows = append(rows, LineFrom(StyledSpan(
					foreground.name+" on "+background.name+": Aa",
					Style{}.Foreground(foreground.color).Background(background.color),
				)))
				ratio := contrast(foreground.color, background.color)
				// A regenerated golden would accept an unreadable pair.
				if ratio < 4.5 {
					t.Errorf("%#v %s on %s is %.2f:1, under 4.5:1", theme, foreground.name, background.name, ratio)
				}
				fmt.Fprintf(&details, "%s/%s: fg=%#v bg=%#v contrast=%.2f\n",
					foreground.name, background.name, foreground.color, background.color, ratio)
			}
		}
		output.WriteString(renderedThemeRows(theme.GoString()+" palette and contrast surfaces", 56, rows, details.String()))
	}
	core.AssertGolden(t, "testdata", "theme-rendering", output.String())
}

func TestAsciiSymbolsFollowTheConsoleAndTheLocaleAndSwapEveryGlyph(t *testing.T) {
	text := func(value string) *string { return &value }
	cases := []struct {
		term, locale *string
		want         core.SymbolSet
	}{
		{text("linux"), text("en_US.UTF-8"), core.SymbolSetAscii},
		{text("xterm"), text("C"), core.SymbolSetAscii},
		{text("xterm"), text("en_US.utf8"), core.SymbolSetUnicode},
		{text("xterm"), nil, core.SymbolSetUnicode},
	}
	for _, c := range cases {
		if got := SymbolsForEnvironment(c.term, c.locale); got != c.want {
			t.Errorf("SymbolsForEnvironment(%v, %v) = %v, want %v", c.term, c.locale, got, c.want)
		}
	}
	ascii := core.SymbolSetAscii
	if got := SymbolsFor(&ascii); got != core.SymbolSetAscii {
		t.Errorf("SymbolsFor(ascii) = %v", got)
	}
	// Every field is printed, so a glyph added without an ASCII column is
	// caught here instead of on a console without UTF-8.
	table := fmt.Sprintf("%#v", ASCIIGlyphs)
	for _, character := range table {
		if character >= utf8.RuneSelf {
			t.Fatalf("the ASCII glyph table is not ASCII: %s", table)
		}
	}
	WithSymbols(core.SymbolSetAscii, func() {
		for _, character := range ThemeGlyphs().Working {
			if character >= utf8.RuneSelf {
				t.Errorf("working glyph %q is not ASCII", ThemeGlyphs().Working)
			}
		}
		if FooterSeparator() != " - " {
			t.Errorf("footer separator = %q", FooterSeparator())
		}
		// A hyphenated key survives the ASCII separator.
		line := Hints("Shift-Enter newline - Tab pane | ctrl+b then c create")
		if got := line.String(); got != "Shift-Enter newline - Tab pane | ctrl+b then c create" {
			t.Errorf("hint text = %q", got)
		}
		if line.Spans[1].Content != "Shift-Enter" {
			t.Errorf("first key = %q", line.Spans[1].Content)
		}
	})
	if FooterSeparator() != " · " {
		t.Errorf("footer separator after the scope = %q", FooterSeparator())
	}
}

// ratatui writes a run of changed cells without moving the cursor, so a
// glyph that a terminal draws wider than unicode-width measures it moves
// every later cell of the run, and at the right edge the overflow wraps onto
// the next row, where it stays. Every glyph the dashboard draws must
// therefore take the same columns in every common terminal:
//
//   - No zero-width code point (variation selector, joiner, combining mark).
//     Windows Terminal before 1.22, and its "Windows Console" text
//     measurement mode since, give every code point a column of its own.
//   - No emoji without default emoji presentation. Terminals disagree on
//     whether to draw those as one-column text or two-column emoji.
//   - No code point whose East Asian Width changed in Unicode 16, which a
//     terminal with an older table draws one column narrower.
//
// East Asian Ambiguous characters stay allowed: Windows Terminal, ConPTY and
// unicode-width all draw them one column wide unless a terminal is set to
// draw them wide, and the box-drawing characters of every border are
// Ambiguous too.
//
// Hard-won: #1243: a glyph measured 1 column by ratatui and 2 by Windows Terminal shifted composer rows over the Sessions pane
func TestEveryUnicodeGlyphTakesTheSameColumnsInEveryTerminal(t *testing.T) {
	// Every field of the table is checked: reflection walks them all, so a
	// glyph added to the table is checked without being listed here.
	table := reflect.ValueOf(UnicodeGlyphs)
	// Changed from Neutral to Wide in Unicode 16.0: the trigrams, monograms
	// and digrams, the Yijing hexagrams, and the Tai Xuan Jing symbols.
	widthChanged := [][2]rune{
		{0x2630, 0x2637},
		{0x268A, 0x268F},
		{0x4DC0, 0x4DFF},
		{0x1D300, 0x1D356},
	}
	var offending []string
	for field := range table.NumField() {
		name := table.Type().Field(field).Name
		glyph := table.Field(field).String()
		for _, mark := range glyph {
			problem := ""
			switch width := displayWidth(string(mark)); {
			case width == 0:
				problem = "is a zero-width code point"
			case width == 1 && displayWidth(string(mark)+"️") == 2:
				problem = "is an emoji without default emoji presentation"
			default:
				for _, span := range widthChanged {
					if mark >= span[0] && mark <= span[1] {
						problem = "changed East Asian Width in Unicode 16"
					}
				}
			}
			if problem != "" {
				offending = append(offending, fmt.Sprintf("%s %q: U+%04X %s", name, glyph, mark, problem))
			}
		}
	}
	if len(offending) > 0 {
		t.Fatal(strings.Join(offending, "\n"))
	}
}

func TestPinLabelsRemainUniqueAfterTheColorPaletteRepeats(t *testing.T) {
	for id, want := range map[uint32]string{0: "A", 25: "Z", 26: "AA", 51: "AZ", 52: "BA"} {
		if got := PinLabel(id); got != want {
			t.Errorf("PinLabel(%d) = %q, want %q", id, got, want)
		}
	}
	for _, selected := range []core.UiTheme{core.UiThemeLight, core.UiThemeMidnight, core.UiThemeMono} {
		WithTheme(selected, func() {
			if PinColor(0) != PinColor(8) {
				t.Errorf("%v: pin colors do not repeat after eight", selected)
			}
			if selected == core.UiThemeMono && PinColor(0) != ThemePalette().Text {
				t.Errorf("mono pin color = %#v", PinColor(0))
			}
		})
	}
}
