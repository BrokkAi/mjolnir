// Port of mj-chat/src/theme.rs.
//
// Shared terminal colors and chrome for the dashboard and conversation.
//
// Rust keeps the palette and symbol set in force in thread-locals that
// WithTheme and WithSymbols swap for one synchronous render. Go has no
// thread-locals, so they are process-wide here: render on one goroutine
// (the Bubble Tea loop) and do not run tests that select different themes
// in parallel.
package chat

import (
	"fmt"
	"math"
	"os"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"unicode"
	"unicode/utf8"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// SymbolSet, ThemeChoice and UiTheme are re-exported from core's config.
type (
	SymbolSet   = core.SymbolSet
	ThemeChoice = core.ThemeChoice
	UiTheme     = core.UiTheme
)

// Palette is the semantic colors always paired with the palette's painted
// surfaces.
type Palette struct {
	Background       Color
	SurfaceRaised    Color
	Surface          Color
	Selection        Color
	Text             Color
	Muted            Color
	Border           Color
	Accent           Color
	Secondary        Color
	Success          Color
	Warning          Color
	Error            Color
	SessionError     Color
	SessionActivity  Color
	SessionAttention Color
	SessionIdle      Color
	ActivityDim      Color
	// Pins are the colors pinned sessions cycle through.
	Pins [8]Color
	// ReverseVideo: the palette leaves every background to the terminal, so
	// selection, focus, and filled chips are drawn with reverse video, bold,
	// and underline instead of painted surfaces.
	ReverseVideo bool
}

// GoString is Rust's derived Debug:
// `Palette { background: Reset, ..., reverse_video: true }`.
func (p Palette) GoString() string {
	pins := make([]string, len(p.Pins))
	for i, pin := range p.Pins {
		pins[i] = pin.GoString()
	}
	fields := []struct {
		name  string
		color Color
	}{
		{"background", p.Background},
		{"surface_raised", p.SurfaceRaised},
		{"surface", p.Surface},
		{"selection", p.Selection},
		{"text", p.Text},
		{"muted", p.Muted},
		{"border", p.Border},
		{"accent", p.Accent},
		{"secondary", p.Secondary},
		{"success", p.Success},
		{"warning", p.Warning},
		{"error", p.Error},
		{"session_error", p.SessionError},
		{"session_activity", p.SessionActivity},
		{"session_attention", p.SessionAttention},
		{"session_idle", p.SessionIdle},
		{"activity_dim", p.ActivityDim},
	}
	var text strings.Builder
	text.WriteString("Palette { ")
	for _, field := range fields {
		text.WriteString(field.name + ": " + field.color.GoString() + ", ")
	}
	fmt.Fprintf(&text, "pins: [%s], reverse_video: %t }", strings.Join(pins, ", "), p.ReverseVideo)
	return text.String()
}

var darkPins = [8]Color{
	rgb(100, 215, 235),
	rgb(190, 160, 250),
	rgb(130, 210, 180),
	rgb(235, 160, 205),
	rgb(145, 175, 250),
	rgb(205, 195, 135),
	rgb(125, 215, 215),
	rgb(215, 175, 175),
}

var lightPins = [8]Color{
	rgb(0, 105, 135),
	rgb(112, 65, 165),
	rgb(40, 110, 95),
	rgb(155, 65, 115),
	rgb(65, 85, 165),
	rgb(115, 95, 40),
	rgb(0, 110, 115),
	rgb(125, 75, 80),
}

var midnight = Palette{
	// Neutral graphite leaves color to actions, authorship, and session state.
	Background:       rgb(15, 18, 20),
	SurfaceRaised:    rgb(32, 39, 43),
	Surface:          rgb(22, 27, 30),
	Selection:        rgb(42, 58, 57),
	Text:             rgb(239, 237, 232),
	Muted:            rgb(151, 163, 163),
	Border:           rgb(62, 74, 78),
	Accent:           rgb(157, 229, 202),
	Secondary:        rgb(158, 194, 238),
	Success:          rgb(157, 229, 202),
	Warning:          rgb(232, 189, 123),
	Error:            rgb(244, 154, 165),
	SessionError:     rgb(244, 154, 165),
	SessionActivity:  rgb(232, 189, 123),
	SessionAttention: rgb(240, 205, 145),
	SessionIdle:      rgb(158, 194, 238),
	ActivityDim:      rgb(67, 44, 48),
	Pins:             darkPins,
	ReverseVideo:     false,
}

var light = Palette{
	Background:       rgb(242, 241, 237),
	SurfaceRaised:    rgb(233, 231, 226),
	Surface:          rgb(251, 250, 247),
	Selection:        rgb(214, 229, 220),
	Text:             rgb(38, 51, 46),
	Muted:            rgb(89, 101, 95),
	Border:           rgb(179, 186, 180),
	Accent:           rgb(35, 104, 77),
	Secondary:        rgb(54, 95, 146),
	Success:          rgb(40, 108, 77),
	Warning:          rgb(133, 83, 19),
	Error:            rgb(169, 61, 80),
	SessionError:     rgb(169, 61, 80),
	SessionActivity:  rgb(133, 83, 19),
	SessionAttention: rgb(133, 83, 19),
	SessionIdle:      rgb(54, 95, 146),
	ActivityDim:      rgb(220, 183, 187),
	Pins:             lightPins,
	ReverseVideo:     false,
}

var darcula = Palette{
	// IntelliJ Darcula's charcoal editor and panel anchors.
	Background:    rgb(43, 43, 43),
	SurfaceRaised: rgb(60, 63, 65),
	Surface:       rgb(49, 51, 53),
	Selection:     rgb(39, 55, 78),
	Text:          rgb(169, 183, 198),
	Muted:         rgb(159, 170, 183),
	Border:        rgb(126, 135, 143),
	// Brighter variants preserve contrast over the blue-gray selection surface.
	Accent:           rgb(112, 183, 255),
	Secondary:        rgb(205, 169, 255),
	Success:          rgb(136, 231, 155),
	Warning:          rgb(255, 198, 109),
	Error:            rgb(255, 155, 155),
	SessionError:     rgb(255, 155, 155),
	SessionActivity:  rgb(255, 198, 109),
	SessionAttention: rgb(255, 220, 96),
	SessionIdle:      rgb(145, 220, 255),
	ActivityDim:      rgb(104, 56, 76),
	Pins:             darkPins,
	ReverseVideo:     false,
}

var highContrast = Palette{
	Background:    rgb(0, 0, 0),
	SurfaceRaised: rgb(32, 32, 32),
	Surface:       rgb(0, 0, 0),
	// Selected rows retain status colors, so their surface must support red too.
	Selection:        rgb(38, 38, 38),
	Text:             rgb(255, 255, 255),
	Muted:            rgb(190, 190, 190),
	Border:           rgb(230, 230, 230),
	Accent:           rgb(26, 235, 255),
	Secondary:        rgb(255, 150, 255),
	Success:          rgb(80, 166, 97),
	Warning:          rgb(255, 191, 102),
	Error:            rgb(255, 80, 80),
	SessionError:     rgb(255, 80, 80),
	SessionActivity:  rgb(255, 191, 102),
	SessionAttention: rgb(255, 220, 96),
	SessionIdle:      rgb(140, 220, 255),
	ActivityDim:      rgb(64, 0, 32),
	Pins:             darkPins,
	ReverseVideo:     false,
}

var highContrastLight = Palette{
	Background:    rgb(255, 255, 255),
	SurfaceRaised: rgb(232, 232, 232),
	Surface:       rgb(255, 255, 255),
	Selection:     rgb(222, 222, 222),
	Text:          rgb(0, 0, 0),
	Muted:         rgb(64, 64, 64),
	Border:        rgb(32, 32, 32),
	// Every role reaches 7:1 on every surface.
	Accent:           rgb(0, 60, 168),
	Secondary:        rgb(127, 0, 128),
	Success:          rgb(0, 82, 0),
	Warning:          rgb(111, 54, 0),
	Error:            rgb(146, 0, 0),
	SessionError:     rgb(146, 0, 0),
	SessionActivity:  rgb(111, 54, 0),
	SessionAttention: rgb(111, 54, 0),
	SessionIdle:      rgb(0, 60, 168),
	ActivityDim:      rgb(222, 179, 179),
	Pins:             lightPins,
	ReverseVideo:     false,
}

// The colorblind palettes keep Midnight's and Light's surfaces and carry
// status on the blue-yellow axis, which protanopia and deuteranopia keep:
// success is blue, warnings and activity yellow, and failures orange (dark)
// or deep crimson (light), which also differs from the yellow in lightness.
// The status and session colors were chosen by simulating protanopia,
// deuteranopia, and tritanopia: every pair stays at least 9 OKLab units
// (x100) apart under all three. Pins follow Okabe and Ito's palette.

var colorblindDark = Palette{
	Background:       rgb(15, 18, 20),
	SurfaceRaised:    rgb(32, 39, 43),
	Surface:          rgb(22, 27, 30),
	Selection:        rgb(38, 50, 62),
	Text:             rgb(239, 237, 232),
	Muted:            rgb(151, 163, 163),
	Border:           rgb(62, 74, 78),
	Accent:           rgb(86, 180, 233),
	Secondary:        rgb(186, 213, 233),
	Success:          rgb(86, 180, 233),
	Warning:          rgb(240, 228, 66),
	Error:            rgb(229, 127, 79),
	SessionError:     rgb(229, 127, 79),
	SessionActivity:  rgb(240, 228, 66),
	SessionAttention: rgb(207, 147, 175),
	SessionIdle:      rgb(86, 180, 233),
	ActivityDim:      rgb(62, 42, 33),
	Pins: [8]Color{
		rgb(86, 180, 233),
		rgb(230, 159, 0),
		rgb(0, 158, 115),
		rgb(240, 228, 66),
		rgb(204, 121, 167),
		rgb(44, 138, 204),
		rgb(215, 96, 5),
		rgb(200, 200, 200),
	},
	ReverseVideo: false,
}

var colorblindLight = Palette{
	Background:       rgb(242, 241, 237),
	SurfaceRaised:    rgb(233, 231, 226),
	Surface:          rgb(251, 250, 247),
	Selection:        rgb(224, 231, 241),
	Text:             rgb(38, 51, 46),
	Muted:            rgb(89, 101, 95),
	Border:           rgb(179, 186, 180),
	Accent:           rgb(0, 106, 170),
	Secondary:        rgb(70, 16, 82),
	Success:          rgb(0, 106, 170),
	Warning:          rgb(132, 90, 15),
	Error:            rgb(127, 4, 30),
	SessionError:     rgb(127, 4, 30),
	SessionActivity:  rgb(132, 90, 15),
	SessionAttention: rgb(94, 50, 104),
	SessionIdle:      rgb(0, 106, 170),
	ActivityDim:      rgb(206, 168, 173),
	Pins: [8]Color{
		rgb(0, 113, 177),
		rgb(159, 92, 0),
		rgb(0, 124, 84),
		rgb(161, 82, 127),
		rgb(95, 80, 170),
		rgb(187, 70, 0),
		rgb(123, 109, 0),
		rgb(60, 60, 60),
	},
	ReverseVideo: false,
}

// Catppuccin paints its base color, so the dashboard meets the padding of a
// terminal running the same flavor. Mocha's colors are its own; Latte's
// accents sit near 3:1 on its base and are deepened in hue to 4.5:1.

var catppuccinMocha = Palette{
	// Base, mantle, and overlay 2 at 20% over base.
	Background:       rgb(30, 30, 46),
	SurfaceRaised:    rgb(24, 24, 37),
	Surface:          rgb(30, 30, 46),
	Selection:        rgb(53, 55, 72),
	Text:             rgb(205, 214, 244),
	Muted:            rgb(166, 173, 200),
	Border:           rgb(108, 112, 134),
	Accent:           rgb(203, 166, 247),
	Secondary:        rgb(137, 180, 250),
	Success:          rgb(166, 227, 161),
	Warning:          rgb(249, 226, 175),
	Error:            rgb(243, 139, 168),
	SessionError:     rgb(243, 139, 168),
	SessionActivity:  rgb(249, 226, 175),
	SessionAttention: rgb(250, 179, 135),
	SessionIdle:      rgb(137, 180, 250),
	ActivityDim:      rgb(77, 54, 73),
	Pins: [8]Color{
		rgb(137, 220, 235),
		rgb(203, 166, 247),
		rgb(166, 227, 161),
		rgb(245, 194, 231),
		rgb(137, 180, 250),
		rgb(249, 226, 175),
		rgb(148, 226, 213),
		rgb(242, 205, 205),
	},
	ReverseVideo: false,
}

var catppuccinLatte = Palette{
	// Base, mantle, and overlay 2 at 15% over base.
	Background:       rgb(239, 241, 245),
	SurfaceRaised:    rgb(230, 233, 239),
	Surface:          rgb(239, 241, 245),
	Selection:        rgb(222, 224, 230),
	Text:             rgb(76, 79, 105),
	Muted:            rgb(92, 95, 119),
	Border:           rgb(156, 160, 176),
	Accent:           rgb(128, 45, 229),
	Secondary:        rgb(11, 85, 227),
	Success:          rgb(0, 115, 0),
	Warning:          rgb(153, 77, 0),
	Error:            rgb(198, 0, 48),
	SessionError:     rgb(198, 0, 48),
	SessionActivity:  rgb(153, 77, 0),
	SessionAttention: rgb(153, 77, 0),
	SessionIdle:      rgb(11, 85, 227),
	ActivityDim:      rgb(226, 166, 184),
	Pins: [8]Color{
		rgb(0, 119, 141),
		rgb(136, 57, 239),
		rgb(22, 126, 0),
		rgb(176, 64, 149),
		rgb(25, 97, 240),
		rgb(165, 89, 0),
		rgb(0, 120, 127),
		rgb(174, 78, 80),
	},
	ReverseVideo: false,
}

// The Windows Terminal palettes keep the scheme's own background so the
// window padding the terminal paints matches the dashboard canvas. Scheme
// colors that fall under 4.5:1 on a surface are lifted or deepened in hue.

var campbell = Palette{
	Background:    rgb(12, 12, 12),
	SurfaceRaised: rgb(34, 34, 34),
	Surface:       rgb(19, 19, 19),
	// Campbell's blue, darkened to carry every status color.
	Selection:        rgb(23, 42, 84),
	Text:             rgb(204, 204, 204),
	Muted:            rgb(152, 152, 152),
	Border:           rgb(78, 78, 78),
	Accent:           rgb(97, 214, 214),
	Secondary:        rgb(97, 156, 255),
	Success:          rgb(22, 198, 12),
	Warning:          rgb(193, 156, 0),
	Error:            rgb(240, 108, 118),
	SessionError:     rgb(240, 108, 118),
	SessionActivity:  rgb(193, 156, 0),
	SessionAttention: rgb(249, 241, 165),
	SessionIdle:      rgb(97, 156, 255),
	ActivityDim:      rgb(60, 20, 24),
	Pins:             darkPins,
	ReverseVideo:     false,
}

var oneHalfDark = Palette{
	Background:       rgb(40, 44, 52),
	SurfaceRaised:    rgb(50, 56, 66),
	Surface:          rgb(44, 49, 58),
	Selection:        rgb(52, 64, 84),
	Text:             rgb(220, 223, 228),
	Muted:            rgb(166, 173, 184),
	Border:           rgb(90, 99, 116),
	Accent:           rgb(94, 186, 197),
	Secondary:        rgb(105, 179, 240),
	Success:          rgb(152, 195, 121),
	Warning:          rgb(229, 192, 123),
	Error:            rgb(233, 149, 156),
	SessionError:     rgb(233, 149, 156),
	SessionActivity:  rgb(229, 192, 123),
	SessionAttention: rgb(240, 208, 150),
	SessionIdle:      rgb(105, 179, 240),
	ActivityDim:      rgb(74, 48, 56),
	Pins:             darkPins,
	ReverseVideo:     false,
}

var oneHalfLight = Palette{
	Background:       rgb(250, 250, 250),
	SurfaceRaised:    rgb(240, 240, 241),
	Surface:          rgb(255, 255, 255),
	Selection:        rgb(222, 230, 242),
	Text:             rgb(56, 58, 66),
	Muted:            rgb(99, 101, 110),
	Border:           rgb(160, 161, 167),
	Accent:           rgb(1, 108, 154),
	Secondary:        rgb(166, 38, 164),
	Success:          rgb(56, 113, 55),
	Warning:          rgb(135, 92, 1),
	Error:            rgb(173, 65, 55),
	SessionError:     rgb(173, 65, 55),
	SessionActivity:  rgb(135, 92, 1),
	SessionAttention: rgb(135, 92, 1),
	SessionIdle:      rgb(7, 110, 131),
	ActivityDim:      rgb(236, 200, 196),
	Pins:             lightPins,
	ReverseVideo:     false,
}

// Solarized keeps its base tones and accent hues. Its accents sit near 3:1 on
// its own background, so each is lifted (dark) or deepened (light) to 4.5:1.

var solarizedDark = Palette{
	Background:       rgb(0, 43, 54),
	SurfaceRaised:    rgb(7, 54, 66),
	Surface:          rgb(3, 47, 59),
	Selection:        rgb(12, 66, 80),
	Text:             rgb(238, 232, 213),
	Muted:            rgb(159, 171, 171),
	Border:           rgb(88, 110, 117),
	Accent:           rgb(91, 183, 176),
	Secondary:        rgb(105, 175, 224),
	Success:          rgb(162, 177, 61),
	Warning:          rgb(199, 165, 61),
	Error:            rgb(236, 142, 141),
	SessionError:     rgb(236, 142, 141),
	SessionActivity:  rgb(199, 165, 61),
	SessionAttention: rgb(225, 151, 120),
	SessionIdle:      rgb(105, 175, 224),
	ActivityDim:      rgb(58, 42, 50),
	Pins:             darkPins,
	ReverseVideo:     false,
}

var solarizedLight = Palette{
	Background:       rgb(253, 246, 227),
	SurfaceRaised:    rgb(238, 232, 213),
	Surface:          rgb(255, 250, 236),
	Selection:        rgb(229, 222, 196),
	Text:             rgb(7, 54, 66),
	Muted:            rgb(81, 98, 105),
	Border:           rgb(147, 161, 161),
	Accent:           rgb(28, 108, 102),
	Secondary:        rgb(28, 101, 153),
	Success:          rgb(89, 103, 0),
	Warning:          rgb(121, 92, 0),
	Error:            rgb(183, 42, 39),
	SessionError:     rgb(183, 42, 39),
	SessionActivity:  rgb(121, 92, 0),
	SessionAttention: rgb(168, 62, 18),
	SessionIdle:      rgb(28, 101, 153),
	ActivityDim:      rgb(240, 205, 195),
	Pins:             lightPins,
	ReverseVideo:     false,
}

// The Terminal palettes name slots of the terminal's 16-color table, so the
// user's scheme decides the actual shades. Each slot takes the variant that
// stays readable across the common schemes: Campbell's normal blue and
// magenta are too dark on black, and Solarized maps several bright colors to
// its grays. Muted text and borders take fixed grays from the 256-color ramp,
// which no scheme redefines: bright black, the usual choice, is Solarized's
// background highlight and too dim on One Half Dark and Tango. See
// ansiIndexed for why every slot is a 256-color index.

var terminalDark = Palette{
	Background:       ColorReset,
	SurfaceRaised:    ColorReset,
	Surface:          ColorReset,
	Selection:        ColorReset,
	Text:             ColorReset,
	Muted:            ansiIndexed(246),
	Border:           ansiIndexed(243),
	Accent:           ansiIndexed(6),
	Secondary:        ansiIndexed(12),
	Success:          ansiIndexed(2),
	Warning:          ansiIndexed(3),
	Error:            ansiIndexed(9),
	SessionError:     ansiIndexed(9),
	SessionActivity:  ansiIndexed(3),
	SessionAttention: ansiIndexed(3),
	SessionIdle:      ansiIndexed(12),
	ActivityDim:      ansiIndexed(1),
	Pins: [8]Color{
		ansiIndexed(6),
		ansiIndexed(13),
		ansiIndexed(2),
		ansiIndexed(3),
		ansiIndexed(12),
		ansiIndexed(9),
		ansiIndexed(14),
		ansiIndexed(7),
	},
	ReverseVideo: true,
}

var terminalLight = Palette{
	Background:    ColorReset,
	SurfaceRaised: ColorReset,
	Surface:       ColorReset,
	Selection:     ColorReset,
	Text:          ColorReset,
	Muted:         ansiIndexed(242),
	Border:        ansiIndexed(248),
	// Light schemes keep their bright colors pale, so only the normal eight
	// and the two blacks carry text.
	Accent:           ansiIndexed(4),
	Secondary:        ansiIndexed(5),
	Success:          ansiIndexed(2),
	Warning:          ansiIndexed(3),
	Error:            ansiIndexed(1),
	SessionError:     ansiIndexed(1),
	SessionActivity:  ansiIndexed(3),
	SessionAttention: ansiIndexed(3),
	SessionIdle:      ansiIndexed(4),
	ActivityDim:      ansiIndexed(1),
	Pins: [8]Color{
		ansiIndexed(4),
		ansiIndexed(5),
		ansiIndexed(2),
		ansiIndexed(1),
		ansiIndexed(6),
		ansiIndexed(3),
		ansiIndexed(0),
		ansiIndexed(8),
	},
	ReverseVideo: true,
}

// mono has no colors: every slot is the terminal's own default, and the style
// functions below carry focus and selection with bold and reverse video.
var mono = Palette{
	Background:       ColorReset,
	SurfaceRaised:    ColorReset,
	Surface:          ColorReset,
	Selection:        ColorReset,
	Text:             ColorReset,
	Muted:            ColorReset,
	Border:           ColorReset,
	Accent:           ColorReset,
	Secondary:        ColorReset,
	Success:          ColorReset,
	Warning:          ColorReset,
	Error:            ColorReset,
	SessionError:     ColorReset,
	SessionActivity:  ColorReset,
	SessionAttention: ColorReset,
	SessionIdle:      ColorReset,
	ActivityDim:      ColorReset,
	Pins:             [8]Color{ColorReset, ColorReset, ColorReset, ColorReset, ColorReset, ColorReset, ColorReset, ColorReset},
	ReverseVideo:     true,
}

// themeScope is the palette and symbol set in force (Rust's thread-local
// CURRENT and SYMBOLS).
var themeScope = struct {
	sync.Mutex
	current core.UiTheme
	symbols core.SymbolSet
}{current: core.UiThemeMidnight, symbols: core.SymbolSetUnicode}

// Current is the palette in force.
func Current() core.UiTheme {
	themeScope.Lock()
	defer themeScope.Unlock()
	return themeScope.current
}

// ReverseVideo reports whether the palette in force leaves backgrounds to the
// terminal, so styles carry selection and focus with reverse video, bold,
// and underline.
func ReverseVideo() bool { return ThemePalette().ReverseVideo }

// noColorOnce reads NO_COLOR once: the answer cannot change while the
// process runs.
var noColorOnce = sync.OnceValue(func() bool {
	value, ok := os.LookupEnv("NO_COLOR")
	return ok && value != ""
})

// NoColorRequested reports whether `NO_COLOR` (https://no-color.org) is set
// to a non-empty value.
func NoColorRequested() bool { return noColorOnce() }

// Rgb is a terminal's background as red, green, and blue.
type Rgb struct {
	R, G, B uint8
}

// GoString is Rust's Debug of the tuple: `(255, 255, 255)`.
func (c Rgb) GoString() string { return fmt.Sprintf("(%d, %d, %d)", c.R, c.G, c.B) }

// terminalBackground is what the terminal answered when asked for its
// background: nil when it did not answer. Unset until a terminal surface
// asks, which happens once.
var terminalBackground struct {
	once  sync.Once
	set   atomic.Bool
	value *Rgb
}

// TerminalBackgroundOrProbe asks for the terminal's background with probe the
// first time it is called in this process, and returns that answer every
// time. The probe must run before anything else reads the terminal's input.
func TerminalBackgroundOrProbe(probe func() *Rgb) *Rgb {
	terminalBackground.once.Do(func() {
		terminalBackground.value = probe()
		terminalBackground.set.Store(true)
	})
	return terminalBackground.value
}

// TerminalBackground is the background the terminal reported, if it was
// asked and answered.
func TerminalBackground() *Rgb {
	if !terminalBackground.set.Load() {
		return nil
	}
	return terminalBackground.value
}

// ThemeReason is why a theme is on screen, which Settings reports beside the
// choice.
type ThemeReason interface {
	isThemeReason()
	GoString() string
}

// ThemeReasonConfigured: the configured palette.
type ThemeReasonConfigured struct{}

// ThemeReasonNoColor: `NO_COLOR` is set; the configured choice returns once
// it is unset.
type ThemeReasonNoColor struct{}

// ThemeReasonTerminalBackground: Auto, from the background the terminal
// reported.
type ThemeReasonTerminalBackground struct{ Background Rgb }

// ThemeReasonUnreported: Auto, with no background reported: Midnight, the
// default.
type ThemeReasonUnreported struct{}

func (ThemeReasonConfigured) isThemeReason()         {}
func (ThemeReasonNoColor) isThemeReason()            {}
func (ThemeReasonTerminalBackground) isThemeReason() {}
func (ThemeReasonUnreported) isThemeReason()         {}

func (ThemeReasonConfigured) GoString() string { return "Configured" }
func (ThemeReasonNoColor) GoString() string    { return "NoColor" }
func (r ThemeReasonTerminalBackground) GoString() string {
	return "TerminalBackground(" + r.Background.GoString() + ")"
}
func (ThemeReasonUnreported) GoString() string { return "Unreported" }

// Resolution is the palette a choice resolved to, and why.
type Resolution struct {
	Theme  core.UiTheme
	Reason ThemeReason
}

// GoString is Rust's Debug:
// `Resolution { theme: Light, reason: TerminalBackground((255, 255, 255)) }`.
func (r Resolution) GoString() string {
	return "Resolution { theme: " + r.Theme.GoString() + ", reason: " + r.Reason.GoString() + " }"
}

// EffectiveTheme is the theme to draw with: the configured one, unless
// `NO_COLOR` asks for none, which wins over every configured choice.
func EffectiveTheme(configured core.ThemeChoice) core.UiTheme {
	return Resolve(configured, NoColorRequested(), TerminalBackground()).Theme
}

// Resolve is the one decision from a choice to a palette, with the process
// facts passed in. Auto draws whichever of Midnight and Light has text that
// contrasts more with the terminal's background.
func Resolve(configured core.ThemeChoice, noColor bool, background *Rgb) Resolution {
	if noColor {
		return Resolution{Theme: core.UiThemeMono, Reason: ThemeReasonNoColor{}}
	}
	if fixed, ok := configured.(core.ThemeChoiceFixed); ok {
		return Resolution{Theme: fixed.Value, Reason: ThemeReasonConfigured{}}
	}
	if background == nil {
		return Resolution{Theme: core.DefaultUiTheme(), Reason: ThemeReasonUnreported{}}
	}
	canvas := rgb(background.R, background.G, background.B)
	theme := core.UiThemeLight
	if contrast(midnight.Text, canvas) >= contrast(light.Text, canvas) {
		theme = core.UiThemeMidnight
	}
	return Resolution{Theme: theme, Reason: ThemeReasonTerminalBackground{Background: *background}}
}

// luminance is the WCAG relative luminance of an RGB color.
func luminance(c Color) float64 {
	r, g, b, ok := c.RGB()
	if !ok {
		panic("contrast is measured between explicit RGB colors")
	}
	sum := 0.0
	for i, channel := range [3]uint8{r, g, b} {
		value := float64(channel) / 255.0
		if value <= 0.04045 {
			value /= 12.92
		} else {
			value = math.Pow((value+0.055)/1.055, 2.4)
		}
		sum += [3]float64{0.2126, 0.7152, 0.0722}[i] * value
	}
	return sum
}

// contrast is the WCAG contrast ratio between two RGB colors, from 1 to 21.
func contrast(foreground, background Color) float64 {
	a := luminance(foreground)
	b := luminance(background)
	return (math.Max(a, b) + 0.05) / (math.Min(a, b) + 0.05)
}

// PaletteFor is the palette a theme draws with.
func PaletteFor(theme core.UiTheme) *Palette {
	switch theme {
	case core.UiThemeLight:
		return &light
	case core.UiThemeDarcula:
		return &darcula
	case core.UiThemeHighContrast:
		return &highContrast
	case core.UiThemeHighContrastLight:
		return &highContrastLight
	case core.UiThemeColorblindDark:
		return &colorblindDark
	case core.UiThemeColorblindLight:
		return &colorblindLight
	case core.UiThemeCatppuccinMocha:
		return &catppuccinMocha
	case core.UiThemeCatppuccinLatte:
		return &catppuccinLatte
	case core.UiThemeCampbell:
		return &campbell
	case core.UiThemeOneHalfDark:
		return &oneHalfDark
	case core.UiThemeOneHalfLight:
		return &oneHalfLight
	case core.UiThemeSolarizedDark:
		return &solarizedDark
	case core.UiThemeSolarizedLight:
		return &solarizedLight
	case core.UiThemeTerminalDark:
		return &terminalDark
	case core.UiThemeTerminalLight:
		return &terminalLight
	case core.UiThemeMono:
		return &mono
	default:
		return &midnight
	}
}

// Glyphs are the glyphs one symbol set draws with. Every site that draws a
// status mark, a control, a rule, or a separator reads these through
// ThemeGlyphs, so switching the set changes all of them together.
//
// Every Unicode glyph must take the same columns in every common terminal;
// `every_unicode_glyph_takes_the_same_columns_in_every_terminal` lists the
// rules and why.
type Glyphs struct {
	Pin           string
	Pinned        string
	Working       string
	Waiting       string
	Important     string
	Unread        string
	Idle          string
	Unknown       string
	Unreachable   string
	Failed        string
	Starting      string
	Resuming      string
	Moving        string
	Checkpointing string
	Stopping      string
	Stopped       string
	Destroying    string
	// Selected is the caret before the selected row.
	Selected             string
	Ellipsis             string
	RowMenu              string
	WorkspaceMenu        string
	Close                string
	SizeMinimized        string
	SizeStandard         string
	SizeMaximized        string
	Warning              string
	Spark                string
	Rule                 string
	None                 string
	FooterSeparator      string
	FooterGroupSeparator string
	Bullet               string
	Check                string
	Running              string
	Pending              string
	Dropdown             string
	// Dropup closes what a Dropdown mark opened.
	Dropup string
	// Navigate opens another view rather than a choice menu.
	Navigate    string
	ScrollTrack string
	ScrollThumb string
	// ScrollMark marks a point of interest on a scrollbar track, such as a
	// user message.
	ScrollMark     string
	BarFull        string
	BarLeft        string
	BarRight       string
	ArrowsVertical string
	RoleGutter     string
	// Microphone is the microphone chip on the composer's top border.
	Microphone string
	// CheckboxChecked and CheckboxUnchecked are a checkbox's mark when
	// checked and when not.
	CheckboxChecked   string
	CheckboxUnchecked string
	// The marks a transcript header draws for each kind of message. A system
	// message uses Rule and a tool row uses the tool-status glyphs, so only
	// the kinds without a mark of their own are listed here.
	RoleUser         string
	RoleMessage      string
	RoleAgent        string
	RoleThought      string
	RolePlan         string
	RolePlanProposal string
	// ToolOutput leads the first output row under an inline tool call.
	ToolOutput string
}

var UnicodeGlyphs = Glyphs{
	Pin:                  "◇",
	Pinned:               "◆",
	Working:              "◐",
	Waiting:              "?",
	Important:            "!",
	Unread:               "✓",
	Idle:                 "○",
	Unknown:              "·",
	Unreachable:          "×",
	Failed:               "×",
	Starting:             "↑",
	Resuming:             "↻",
	Moving:               "⇄",
	Checkpointing:        "▣",
	Stopping:             "↓",
	Stopped:              "■",
	Destroying:           "⊗",
	Selected:             "› ",
	Ellipsis:             "…",
	RowMenu:              " ⋯ ",
	WorkspaceMenu:        " ≡ ",
	Close:                " × ",
	SizeMinimized:        "▁",
	SizeStandard:         "▄",
	SizeMaximized:        "□",
	Warning:              "!",
	Spark:                "✦",
	Rule:                 "─",
	None:                 "—",
	FooterSeparator:      " · ",
	FooterGroupSeparator: " │ ",
	Bullet:               "•",
	Check:                "✓",
	Running:              "●",
	Pending:              "○",
	Dropdown:             "▾",
	Dropup:               "▴",
	Navigate:             "›",
	ScrollTrack:          "│",
	ScrollThumb:          "▐",
	ScrollMark:           "━",
	BarFull:              "█",
	BarLeft:              "▕",
	BarRight:             "▏",
	ArrowsVertical:       "↑↓",
	RoleGutter:           "│ ",
	Microphone:           "mic",
	CheckboxChecked:      "☒",
	CheckboxUnchecked:    "☐",
	RoleUser:             "❯",
	RoleMessage:          "←",
	RoleAgent:            "●",
	RoleThought:          "○",
	RolePlan:             "◇",
	RolePlanProposal:     "◈",
	ToolOutput:           "└ ",
}

var ASCIIGlyphs = Glyphs{
	Pin:                  "+",
	Pinned:               "*",
	Working:              "*",
	Waiting:              "?",
	Important:            "!",
	Unread:               "+",
	Idle:                 "-",
	Unknown:              ".",
	Unreachable:          "x",
	Failed:               "x",
	Starting:             "^",
	Resuming:             "~",
	Moving:               "<>",
	Checkpointing:        "#",
	Stopping:             "v",
	Stopped:              "=",
	Destroying:           "X",
	Selected:             "> ",
	Ellipsis:             "..",
	RowMenu:              " . ",
	WorkspaceMenu:        " = ",
	Close:                " x ",
	SizeMinimized:        "-",
	SizeStandard:         "=",
	SizeMaximized:        "+",
	Warning:              "!",
	Spark:                "*",
	Rule:                 "-",
	None:                 "-",
	FooterSeparator:      " - ",
	FooterGroupSeparator: " | ",
	Bullet:               "*",
	Check:                "x",
	Running:              "*",
	Pending:              "o",
	Dropdown:             "v",
	Dropup:               "^",
	Navigate:             ">",
	ScrollTrack:          "|",
	ScrollThumb:          "#",
	ScrollMark:           "-",
	BarFull:              "#",
	BarLeft:              "[",
	BarRight:             "]",
	ArrowsVertical:       "^v",
	RoleGutter:           "| ",
	Microphone:           "mic",
	CheckboxChecked:      "[x]",
	CheckboxUnchecked:    "[ ]",
	RoleUser:             ">",
	RoleMessage:          "<",
	RoleAgent:            "*",
	RoleThought:          "o",
	RolePlan:             "-",
	RolePlanProposal:     "+",
	ToolOutput:           "`- ",
}

// ThemeGlyphs is the glyph set in force (Rust's theme::glyphs; renamed
// because the type Glyphs takes the name).
func ThemeGlyphs() *Glyphs {
	if ASCII() {
		return &ASCIIGlyphs
	}
	return &UnicodeGlyphs
}

// ASCII reports whether the ASCII set is in force.
func ASCII() bool {
	themeScope.Lock()
	defer themeScope.Unlock()
	return themeScope.symbols == core.SymbolSetAscii
}

// WithSymbols selects a symbol set for synchronous rendering, restoring it
// even on panic.
func WithSymbols(symbols core.SymbolSet, render func()) {
	themeScope.Lock()
	previous := themeScope.symbols
	themeScope.symbols = symbols
	themeScope.Unlock()
	defer func() {
		themeScope.Lock()
		themeScope.symbols = previous
		themeScope.Unlock()
	}()
	render()
}

// ASCIIFallback is the one-cell ASCII stand-in for a Unicode mark that dialog
// titles, hints, notices and Settings fields spell as literal text. The
// glyph table covers the marks the dashboard draws itself; prose such as
// "Enter opens · Esc closes" or "Saving…" cannot reach it without every
// string asking for its separator, so the frame is folded once after drawing
// instead. The replacements are one cell wide so no layout changes.
func ASCIIFallback(mark rune) (string, bool) {
	switch mark {
	case '·', '—', '–', '−', '─', '▁':
		return "-", true
	case '…', '⋯':
		return ".", true
	case '▾', '↓':
		return ASCIIGlyphs.Stopping, true
	case '↑':
		return ASCIIGlyphs.Starting, true
	case '→', '›', '❯', '▸':
		return ">", true
	case '←', '‹':
		return "<", true
	case '✓':
		return ASCIIGlyphs.Unread, true
	case '×':
		return ASCIIGlyphs.Failed, true
	case '▪':
		return ASCIIGlyphs.SizeStandard, true
	case '□', '◇', '◈':
		return "+", true
	case '◆', '•', '●', '◐':
		return "*", true
	case '○':
		return ASCIIGlyphs.Pending, true
	case '⚠':
		return "!", true
	case '│', '▕', '▏':
		return "|", true
	case '█', '▐':
		return "#", true
	case '■':
		return "=", true
	}
	return "", false
}

// FoldBufferToASCII replaces every Unicode mark in buffer that has an ASCII
// stand-in. Called after a frame is drawn while the ASCII symbol set is in
// force.
func FoldBufferToASCII(buffer *uv.Buffer) {
	for _, line := range buffer.Lines {
		for x := range line {
			cell := &line[x]
			mark, size := utf8.DecodeRuneInString(cell.Content)
			if size == 0 || size != len(cell.Content) {
				continue
			}
			if replacement, ok := ASCIIFallback(mark); ok {
				cell.Content = replacement
			}
		}
	}
}

// detectedSymbols is SymbolsFor's guess, made once.
var detectedSymbols = sync.OnceValue(func() core.SymbolSet {
	var term *string
	if value, ok := os.LookupEnv("TERM"); ok {
		term = &value
	}
	var locale *string
	for _, name := range []string{"LC_ALL", "LC_CTYPE", "LANG"} {
		if value, ok := os.LookupEnv(name); ok && value != "" {
			locale = &value
			break
		}
	}
	return SymbolsForEnvironment(term, locale)
})

// SymbolsFor is the symbol set to draw with: the configured one, or a guess
// from the terminal when nothing is configured. The Linux console and a
// locale without UTF-8 cannot show the Unicode set.
func SymbolsFor(configured *core.SymbolSet) core.SymbolSet {
	if configured != nil {
		return *configured
	}
	return detectedSymbols()
}

// SymbolsForEnvironment is SymbolsFor's guess, from the terminal name and the
// locale.
func SymbolsForEnvironment(term, locale *string) core.SymbolSet {
	if term != nil && *term == "linux" {
		return core.SymbolSetAscii
	}
	if locale != nil && !strings.Contains(strings.ToLower(*locale), "utf") {
		return core.SymbolSetAscii
	}
	return core.SymbolSetUnicode
}

// asciiBorder is an all-ASCII border for terminals that cannot draw box
// characters.
var asciiBorder = BorderSet{
	TopLeft:          "+",
	TopRight:         "+",
	BottomLeft:       "+",
	BottomRight:      "+",
	VerticalLeft:     "|",
	VerticalRight:    "|",
	HorizontalTop:    "-",
	HorizontalBottom: "-",
}

// ThemePalette is the palette in force (Rust's theme::palette; renamed
// because the type Palette takes the name).
func ThemePalette() *Palette { return PaletteFor(Current()) }

// WithTheme selects a palette for synchronous rendering, restoring it even
// on panic. Keep asynchronous work outside this scope.
func WithTheme(theme core.UiTheme, render func()) {
	themeScope.Lock()
	previous := themeScope.current
	themeScope.current = theme
	themeScope.Unlock()
	defer func() {
		themeScope.Lock()
		themeScope.current = previous
		themeScope.Unlock()
	}()
	render()
}

// rgb: the shared theme paints both foreground and background, so its RGB
// contrast does not depend on the terminal palette.
func rgb(red, green, blue uint8) Color { return ColorRGB(red, green, blue) }

// ansiIndexed is a 256-color index (`38;5;n`). Indexes 0-15 are the
// terminal's own scheme colors, written this way rather than as SGR 30-37
// because Windows Terminal, xterm and Alacritty draw bold text in the bright
// variant of a 30-37 color; on a light scheme that turns a bold blue title
// pale. (Rust's theme::ansi; renamed so it cannot clash with the x/ansi
// import in this package.)
func ansiIndexed(index uint8) Color { return ColorIndexed(index) }

// Base is the canvas beneath panels and modal halos.
func Base() Style {
	return Style{}.Foreground(ThemePalette().Text).Background(ThemePalette().Background)
}

// Actionable is neutral enabled-control text; shape and surface convey
// clickability.
func Actionable() Style { return Style{}.Foreground(ThemePalette().Text) }

// ActionableChip is a clickable chip that retains its surface and monochrome
// affordance.
func ActionableChip() Style { return Selection(false).Patch(Actionable()) }

func Muted() Style { return Style{}.Foreground(ThemePalette().Muted) }

func Border(focused bool) Style {
	switch {
	case focused && Current() == core.UiThemeHighContrast:
		return Style{}.Foreground(ThemePalette().Accent).Add(ModifierBold)
	// The terminal's own colors have no edge quieter than the border, so
	// focus is the foreground in bold.
	case focused && ReverseVideo():
		return Style{}.Foreground(ThemePalette().Text).Add(ModifierBold)
	// Titles carry the accent. A neutral edge keeps large panes quiet.
	case focused:
		return Style{}.Foreground(ThemePalette().Muted)
	default:
		return Style{}.Foreground(ThemePalette().Border)
	}
}

func Title(focused bool) Style {
	fg := ThemePalette().Text
	if focused {
		fg = ThemePalette().Accent
	}
	return Style{}.Foreground(fg).Add(ModifierBold)
}

func Selection(focused bool) Style {
	if ReverseVideo() {
		// Without a painted surface, reverse video is the selection.
		style := Style{}.Add(ModifierReversed)
		if focused {
			return style.Add(ModifierBold)
		}
		return style
	}
	if focused {
		return Style{}.Foreground(ThemePalette().Accent).Background(ThemePalette().Selection).Add(ModifierBold)
	}
	return Style{}.Foreground(ThemePalette().Text).Background(ThemePalette().Selection)
}

// Raised is the background of a raised surface: a selected row, an armed
// control, a modal's title bar. Reverse video without painted surfaces.
func Raised() Style {
	if ReverseVideo() {
		return Style{}.Add(ModifierReversed)
	}
	return Style{}.Background(ThemePalette().SurfaceRaised)
}

// ActiveControl is the style of a control that is switched on, such as the
// active pane size chip: a selection surface in a painted theme, reverse
// video otherwise.
func ActiveControl() Style {
	if ReverseVideo() {
		return Style{}.Add(ModifierReversed | ModifierBold)
	}
	return Style{}.Foreground(ThemePalette().Accent).Background(ThemePalette().Selection).Add(ModifierBold)
}

// FocusControl is a focused or armed action. Filled color is reserved for a
// direct action; selected content uses the quieter Selection surface.
func FocusControl() Style { return Filled(ThemePalette().Accent).Add(ModifierBold) }

// Filled is a chip filled with c and lettered in the canvas color. Without a
// painted canvas, reverse video swaps the terminal's own background in as
// the lettering, which keeps the chip readable on any scheme.
func Filled(c Color) Style {
	if ReverseVideo() {
		return Style{}.Foreground(c).Add(ModifierReversed)
	}
	return Style{}.Foreground(ThemePalette().Background).Background(c)
}

// Field is an inset text field, with an uninterrupted focus surface for
// legibility. Without painted surfaces an underline keeps editable text
// distinct.
func Field(focused bool) Style {
	if ReverseVideo() {
		if focused {
			return Style{}.Add(ModifierUnderlined)
		}
		return Style{}
	}
	bg := ThemePalette().Background
	if focused {
		bg = ThemePalette().Selection
	}
	return Style{}.Foreground(ThemePalette().Text).Background(bg)
}

// KeyHint emphasizes a key name without painting it as a selected control.
func KeyHint() Style { return Style{}.Foreground(ThemePalette().Secondary).Add(ModifierBold) }

// HintDescription is description text beside a key hint. Panel titles are
// bold, so the description must remove BOLD explicitly; patching a plain
// muted style over a bold title style cannot clear the modifier.
func HintDescription() Style { return Muted().Remove(ModifierBold) }

// Panel is a rounded panel; it keeps identical content geometry regardless
// of focus.
func Panel(focused bool) Block {
	block := NewBlock().
		WithBorders(BordersAll).
		WithStyle(Style{}.Foreground(ThemePalette().Text).Background(ThemePalette().Surface)).
		WithBorderStyle(Border(focused)).
		WithTitleStyle(Title(focused))
	if ASCII() {
		return block.WithBorderSet(asciiBorder)
	}
	return block.WithBorderType(BorderTypeRounded)
}

func Modal() Block {
	return Panel(true).WithStyle(Style{}.Foreground(ThemePalette().Text).Background(ThemePalette().SurfaceRaised))
}

// Hints distinguishes keys from their descriptions without changing hint
// spacing.
//
// The text is cut at the footer separators in force, so the ASCII set's
// ` - ` never splits a hyphenated key such as `Shift-Enter`.
func Hints(text string) Line {
	var spans []Span
	separators := []string{FooterGroupSeparator(), FooterSeparator()}
	for rest := text; rest != ""; {
		// The earliest separator wins; at the same offset, the shorter one,
		// as Rust's min over (offset, length) picks.
		at, length := -1, 0
		for _, separator := range separators {
			offset := strings.Index(rest, separator)
			if offset < 0 {
				continue
			}
			if at < 0 || offset < at || (offset == at && len(separator) < length) {
				at, length = offset, len(separator)
			}
		}
		hint, separator := rest, ""
		if at >= 0 {
			hint, separator = rest[:at], rest[at:at+length]
		}
		leading := len(hint) - len(strings.TrimLeftFunc(hint, unicode.IsSpace))
		keyEnd := len(hint)
		if offset := strings.IndexFunc(hint[leading:], unicode.IsSpace); offset >= 0 {
			keyEnd = leading + offset
		}
		spans = append(spans,
			StyledSpan(hint[:leading], Muted()),
			StyledSpan(hint[leading:keyEnd], KeyHint()),
			StyledSpan(hint[keyEnd:]+separator, Muted()),
		)
		rest = rest[len(hint)+len(separator):]
	}
	return LineFrom(spans...)
}

// FooterSeparator is the text between two hints of one footer group.
func FooterSeparator() string { return ThemeGlyphs().FooterSeparator }

// FooterGroupSeparator is the text between two footer groups.
func FooterGroupSeparator() string { return ThemeGlyphs().FooterGroupSeparator }

// PrefixBanner is the footer row drawn while the prefix key is waiting for
// the key that completes a chord. prefix is the resolved prefix label and
// paletteKeys names the keys that open command search.
func PrefixBanner(prefix string, paletteKeys *string) Line {
	separator := FooterSeparator()
	hint := " esc cancel" + separator + prefix + " send"
	if paletteKeys != nil {
		hint += separator + *paletteKeys + " palette"
	}
	return LineFrom(
		StyledSpan(" PREFIX ", FocusControl()),
		StyledSpan(hint, Muted()),
	)
}

// FitFooter keeps complete footer hints within the terminal width. The
// prefix chords give way before the pane's own hints; protected names the
// hints that survive longest.
func FitFooter(pane, chords, functions []string, width int, protected func(string) bool) string {
	identity := func(text string) string { return text }
	return FooterItemsText(
		FitFooterItems([3][]string{pane, chords, functions}, width, identity, protected),
		identity,
	)
}

// FitFooterItems fits structured command hints while retaining their
// identities for hit testing.
//
// Whole segments are dropped rather than truncated, because half a hint
// names a key that does not exist. They give way from the right-hand group
// first, and from the right within a group, so the reader keeps the few keys
// that work right here and loses the long list that answers from anywhere —
// a list the palette holds in full. A hint protected accepts is dropped only
// once nothing else is left, which is how the palette stays visible on the
// narrowest terminal.
func FitFooterItems[T any](groups [3][]T, width int, label func(T) string, protected func(T) bool) [3][]T {
	// The groups are edited below; keep the caller's slices intact.
	for i := range groups {
		groups[i] = slices.Clone(groups[i])
	}
	for {
		if displayWidth(FooterItemsText(groups, label)) <= width {
			return groups
		}
		victimGroup, victimIndex := -1, -1
		for group := len(groups) - 1; group >= 0 && victimGroup < 0; group-- {
			for index := len(groups[group]) - 1; index >= 0; index-- {
				if !protected(groups[group][index]) {
					victimGroup, victimIndex = group, index
					break
				}
			}
		}
		if victimGroup < 0 {
			// Only protected hints are left: give up the leading one, so the
			// last hint standing is the one the table ranked last.
			for group := range groups {
				if len(groups[group]) > 0 {
					victimGroup, victimIndex = group, 0
					break
				}
			}
		}
		if victimGroup < 0 {
			return groups
		}
		groups[victimGroup] = slices.Delete(groups[victimGroup], victimIndex, victimIndex+1)
	}
}

// FooterItem is one keyed footer hint, Rust's `(K, String)`.
type FooterItem[K any] struct {
	Key  K
	Text string
}

// FitPrefixedFooterItems fits footer hints whose chord group (the middle
// one) is read after a prefix.
//
// prefix — `ctrl+b then ` — is prepended to the chord group's first hint.
// That hint is the first unprotected chord the fitter drops, and once it is
// gone the survivor would read `: / ? palette` as if `:` alone opened the
// palette. So when the labeled hint did not survive, the label moves to the
// first chord that did, and the fit runs again so the label's width is paid
// for. A row too narrow for the label and the palette together keeps the
// unlabeled palette: a reachable key beats a complete sentence.
//
// Both the dashboard footer and the composer footer go through this one
// function, so the two cannot disagree about when the prefix is named.
func FitPrefixedFooterItems[K any](groups [3][]FooterItem[K], width int, prefix string, protected func(K) bool) [3][]FooterItem[K] {
	label := func(item FooterItem[K]) string { return item.Text }
	isProtected := func(item FooterItem[K]) bool { return protected(item.Key) }
	labeled := func(groups [3][]FooterItem[K]) bool {
		return len(groups[1]) == 0 || strings.HasPrefix(groups[1][0].Text, prefix)
	}
	groups[1] = slices.Clone(groups[1])
	if len(groups[1]) > 0 {
		groups[1][0].Text = prefix + groups[1][0].Text
	}
	fitted := FitFooterItems(groups, width, label, isProtected)
	if labeled(fitted) {
		return fitted
	}
	relabeled := fitted
	relabeled[1] = slices.Clone(fitted[1])
	if len(relabeled[1]) > 0 {
		relabeled[1][0].Text = prefix + relabeled[1][0].Text
	}
	relabeled = FitFooterItems(relabeled, width, label, isProtected)
	if labeled(relabeled) && len(relabeled[1]) > 0 {
		return relabeled
	}
	return fitted
}

// FooterItemsText renders the same complete segments used to register
// footer controls.
func FooterItemsText[T any](groups [3][]T, label func(T) string) string {
	var rendered []string
	for _, group := range groups {
		if len(group) == 0 {
			continue
		}
		labels := make([]string, len(group))
		for i, item := range group {
			labels[i] = label(item)
		}
		rendered = append(rendered, strings.Join(labels, FooterSeparator()))
	}
	return strings.Join(rendered, FooterGroupSeparator())
}

// PinLabel: stable pin names remain distinct when colors repeat or color is
// disabled.
func PinLabel(id uint32) string {
	var label []byte
	for {
		label = append(label, byte('A'+id%26))
		if id < 26 {
			break
		}
		id = id/26 - 1
	}
	for i, j := 0, len(label)-1; i < j; i, j = i+1, j-1 {
		label[i], label[j] = label[j], label[i]
	}
	return string(label)
}

func PinColor(id uint32) Color { return ThemePalette().Pins[id%8] }
