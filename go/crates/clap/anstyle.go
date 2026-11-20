// Port of anstyle 1.0.14 src/style.rs, src/color.rs, src/effect.rs and
// src/reset.rs
//
// clap re-exports the crate as `clap::builder::styling`, which is where the
// names below come from.

package clap

import (
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// anstyleReset is anstyle's RESET.
const anstyleReset = "\x1B[0m"

// BuilderStylingReset is anstyle::Reset: reset terminal formatting.
type BuilderStylingReset struct{}

// Render is Reset::render: render the ANSI code.
func (BuilderStylingReset) Render() string { return anstyleReset }

// String is the Display form: the ANSI code.
func (BuilderStylingReset) String() string { return anstyleReset }

// FmtDisplay is `impl Display for Reset`.
func (BuilderStylingReset) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(anstyleReset) }

// BuilderStylingEffects is anstyle::Effects: a set of text styles.
type BuilderStylingEffects uint16

// The effects, as the associated constants of anstyle::Effects.
const (
	BuilderStylingEffectsBold   BuilderStylingEffects = 1 << 0
	BuilderStylingEffectsDimmed BuilderStylingEffects = 1 << 1
	// BuilderStylingEffectsItalic is not widely supported. Sometimes treated
	// as inverse or blink.
	BuilderStylingEffectsItalic BuilderStylingEffects = 1 << 2
	// BuilderStylingEffectsUnderline: style extensions exist for Kitty, VTE,
	// mintty and iTerm2.
	BuilderStylingEffectsUnderline       BuilderStylingEffects = 1 << 3
	BuilderStylingEffectsDoubleUnderline BuilderStylingEffects = 1 << 4
	BuilderStylingEffectsCurlyUnderline  BuilderStylingEffects = 1 << 5
	BuilderStylingEffectsDottedUnderline BuilderStylingEffects = 1 << 6
	BuilderStylingEffectsDashedUnderline BuilderStylingEffects = 1 << 7
	BuilderStylingEffectsBlink           BuilderStylingEffects = 1 << 8
	// BuilderStylingEffectsInvert swaps foreground and background colors;
	// inconsistent emulation.
	BuilderStylingEffectsInvert BuilderStylingEffects = 1 << 9
	BuilderStylingEffectsHidden BuilderStylingEffects = 1 << 10
	// BuilderStylingEffectsStrikethrough: characters legible but marked as
	// if for deletion. Not supported in Terminal.app.
	BuilderStylingEffectsStrikethrough BuilderStylingEffects = 1 << 11
)

var anstyleEffectMetadata = [12]struct {
	name   string
	escape string
}{
	{"BOLD", "\x1B[1m"},
	{"DIMMED", "\x1B[2m"},
	{"ITALIC", "\x1B[3m"},
	{"UNDERLINE", "\x1B[4m"},
	{"DOUBLE_UNDERLINE", "\x1B[21m"},
	{"CURLY_UNDERLINE", "\x1B[4:3m"},
	{"DOTTED_UNDERLINE", "\x1B[4:4m"},
	{"DASHED_UNDERLINE", "\x1B[4:5m"},
	{"BLINK", "\x1B[5m"},
	{"INVERT", "\x1B[7m"},
	{"HIDDEN", "\x1B[8m"},
	{"STRIKETHROUGH", "\x1B[9m"},
}

// BuilderStylingEffectsNew is Effects::new: no effects enabled.
func BuilderStylingEffectsNew() BuilderStylingEffects { return 0 }

// IsPlain is Effects::is_plain: check if no effects are enabled.
func (e BuilderStylingEffects) IsPlain() bool { return e == 0 }

// Contains is Effects::contains: returns true if all of the effects in other
// are contained within self.
func (e BuilderStylingEffects) Contains(other BuilderStylingEffects) bool {
	return other&e == other
}

// Insert is Effects::insert: inserts the specified effects in-place.
func (e BuilderStylingEffects) Insert(other BuilderStylingEffects) BuilderStylingEffects {
	return e | other
}

// Remove is Effects::remove: removes the specified effects in-place.
func (e BuilderStylingEffects) Remove(other BuilderStylingEffects) BuilderStylingEffects {
	return e &^ other
}

// Clear is Effects::clear: reset all effects in-place.
func (e BuilderStylingEffects) Clear() BuilderStylingEffects { return 0 }

// Set is Effects::set: enable or disable the specified effects depending on
// the passed value.
func (e BuilderStylingEffects) Set(other BuilderStylingEffects, enable bool) BuilderStylingEffects {
	if enable {
		return e.Insert(other)
	}
	return e.Remove(other)
}

// Iter is Effects::iter: iterate over enabled effects.
func (e BuilderStylingEffects) Iter() []BuilderStylingEffects {
	var effects []BuilderStylingEffects
	for index := range anstyleEffectMetadata {
		effect := BuilderStylingEffects(1) << index
		if e.Contains(effect) {
			effects = append(effects, effect)
		}
	}
	return effects
}

// Render is Effects::render: render the ANSI code.
func (e BuilderStylingEffects) Render() string {
	var out strings.Builder
	for index := range anstyleEffectMetadata {
		if e.Contains(BuilderStylingEffects(1) << index) {
			out.WriteString(anstyleEffectMetadata[index].escape)
		}
	}
	return out.String()
}

// debugString is the Debug form of Effects: `Effects(BOLD | UNDERLINE)`.
func (e BuilderStylingEffects) debugString() string {
	var out strings.Builder
	out.WriteString("Effects(")
	first := true
	for index := range anstyleEffectMetadata {
		if e.Contains(BuilderStylingEffects(1) << index) {
			if !first {
				out.WriteString(" | ")
			}
			first = false
			out.WriteString(anstyleEffectMetadata[index].name)
		}
	}
	out.WriteString(")")
	return out.String()
}

// FmtDebug is `impl Debug for Effects`.
func (e BuilderStylingEffects) FmtDebug(f *rs.Formatter) error { return f.WriteStr(e.debugString()) }

// BuilderStylingAnsiColor is anstyle::AnsiColor: available 4-bit ANSI color
// palette codes.
//
// The user's terminal defines the meaning of each palette code.
type BuilderStylingAnsiColor uint8

const (
	// BuilderStylingAnsiColor_Black is Black: #0 (foreground code 30,
	// background code 40).
	BuilderStylingAnsiColor_Black BuilderStylingAnsiColor = iota
	// BuilderStylingAnsiColor_Red is Red: #1 (foreground code 31, background
	// code 41).
	BuilderStylingAnsiColor_Red
	BuilderStylingAnsiColor_Green
	BuilderStylingAnsiColor_Yellow
	BuilderStylingAnsiColor_Blue
	BuilderStylingAnsiColor_Magenta
	BuilderStylingAnsiColor_Cyan
	BuilderStylingAnsiColor_White
	BuilderStylingAnsiColor_BrightBlack
	BuilderStylingAnsiColor_BrightRed
	BuilderStylingAnsiColor_BrightGreen
	BuilderStylingAnsiColor_BrightYellow
	BuilderStylingAnsiColor_BrightBlue
	BuilderStylingAnsiColor_BrightMagenta
	BuilderStylingAnsiColor_BrightCyan
	BuilderStylingAnsiColor_BrightWhite
)

var _ = rs.RegisterEnum[BuilderStylingAnsiColor]("AnsiColor",
	rs.Const(BuilderStylingAnsiColor_Black, "Black"),
	rs.Const(BuilderStylingAnsiColor_Red, "Red"),
	rs.Const(BuilderStylingAnsiColor_Green, "Green"),
	rs.Const(BuilderStylingAnsiColor_Yellow, "Yellow"),
	rs.Const(BuilderStylingAnsiColor_Blue, "Blue"),
	rs.Const(BuilderStylingAnsiColor_Magenta, "Magenta"),
	rs.Const(BuilderStylingAnsiColor_Cyan, "Cyan"),
	rs.Const(BuilderStylingAnsiColor_White, "White"),
	rs.Const(BuilderStylingAnsiColor_BrightBlack, "BrightBlack"),
	rs.Const(BuilderStylingAnsiColor_BrightRed, "BrightRed"),
	rs.Const(BuilderStylingAnsiColor_BrightGreen, "BrightGreen"),
	rs.Const(BuilderStylingAnsiColor_BrightYellow, "BrightYellow"),
	rs.Const(BuilderStylingAnsiColor_BrightBlue, "BrightBlue"),
	rs.Const(BuilderStylingAnsiColor_BrightMagenta, "BrightMagenta"),
	rs.Const(BuilderStylingAnsiColor_BrightCyan, "BrightCyan"),
	rs.Const(BuilderStylingAnsiColor_BrightWhite, "BrightWhite"),
)

// On is AnsiColor::on: create a Style with this as the foreground.
func (c BuilderStylingAnsiColor) On(background BuilderStylingColor) BuilderStylingStyle {
	return BuilderStylingStyleNew().
		FgColor(rs.Some[BuilderStylingColor](&BuilderStylingColor_Ansi{F0: c})).
		BgColor(rs.Some(background))
}

// OnDefault is AnsiColor::on_default: create a Style with this as the
// foreground.
func (c BuilderStylingAnsiColor) OnDefault() BuilderStylingStyle {
	return BuilderStylingStyleNew().FgColor(rs.Some[BuilderStylingColor](&BuilderStylingColor_Ansi{F0: c}))
}

// RenderFg is AnsiColor::render_fg: render the ANSI code for a foreground
// color.
func (c BuilderStylingAnsiColor) RenderFg() string {
	if c.IsBright() {
		return "\x1B[9" + strconv.Itoa(int(c-BuilderStylingAnsiColor_BrightBlack)) + "m"
	}
	return "\x1B[3" + strconv.Itoa(int(c)) + "m"
}

// RenderBg is AnsiColor::render_bg: render the ANSI code for a background
// color.
func (c BuilderStylingAnsiColor) RenderBg() string {
	if c.IsBright() {
		return "\x1B[10" + strconv.Itoa(int(c-BuilderStylingAnsiColor_BrightBlack)) + "m"
	}
	return "\x1B[4" + strconv.Itoa(int(c)) + "m"
}

func (c BuilderStylingAnsiColor) renderUnderline() string {
	// No per-color codes; must delegate to `Ansi256Color`
	return BuilderStylingAnsi256ColorFromAnsi(c).renderUnderline()
}

// Bright is AnsiColor::bright: change the color to/from bright.
func (c BuilderStylingAnsiColor) Bright(yes bool) BuilderStylingAnsiColor {
	if yes {
		if c.IsBright() {
			return c
		}
		return c + BuilderStylingAnsiColor_BrightBlack
	}
	if c.IsBright() {
		return c - BuilderStylingAnsiColor_BrightBlack
	}
	return c
}

// IsBright is AnsiColor::is_bright: report whether the color is bright.
func (c BuilderStylingAnsiColor) IsBright() bool {
	return c >= BuilderStylingAnsiColor_BrightBlack
}

// BuilderStylingAnsi256Color is anstyle::Ansi256Color: 256 (8-bit) color
// support.
//
//   - 0..16 are AnsiColor palette codes
//   - 0..232 map to RgbColor color values
//   - 232.. map to RgbColor gray-scale values
type BuilderStylingAnsi256Color uint8

// On is Ansi256Color::on: create a Style with this as the foreground.
func (c BuilderStylingAnsi256Color) On(background BuilderStylingColor) BuilderStylingStyle {
	return BuilderStylingStyleNew().
		FgColor(rs.Some[BuilderStylingColor](&BuilderStylingColor_Ansi256{F0: c})).
		BgColor(rs.Some(background))
}

// OnDefault is Ansi256Color::on_default.
func (c BuilderStylingAnsi256Color) OnDefault() BuilderStylingStyle {
	return BuilderStylingStyleNew().FgColor(rs.Some[BuilderStylingColor](&BuilderStylingColor_Ansi256{F0: c}))
}

// Index is Ansi256Color::index: get the raw value.
func (c BuilderStylingAnsi256Color) Index() uint8 { return uint8(c) }

// IntoAnsi is Ansi256Color::into_ansi: convert to AnsiColor when there is
// no loss of information.
func (c BuilderStylingAnsi256Color) IntoAnsi() rs.Option[BuilderStylingAnsiColor] {
	if c <= 15 {
		return rs.Some(BuilderStylingAnsiColor(c))
	}
	return rs.None[BuilderStylingAnsiColor]()
}

// BuilderStylingAnsi256ColorFromAnsi is Ansi256Color::from_ansi: losslessly
// convert from AnsiColor.
func BuilderStylingAnsi256ColorFromAnsi(color BuilderStylingAnsiColor) BuilderStylingAnsi256Color {
	return BuilderStylingAnsi256Color(color)
}

// RenderFg is Ansi256Color::render_fg.
func (c BuilderStylingAnsi256Color) RenderFg() string {
	return "\x1B[38;5;" + strconv.Itoa(int(c)) + "m"
}

// RenderBg is Ansi256Color::render_bg.
func (c BuilderStylingAnsi256Color) RenderBg() string {
	return "\x1B[48;5;" + strconv.Itoa(int(c)) + "m"
}

func (c BuilderStylingAnsi256Color) renderUnderline() string {
	return "\x1B[58;5;" + strconv.Itoa(int(c)) + "m"
}

// BuilderStylingRgbColor is anstyle::RgbColor: 24-bit ANSI RGB color codes.
type BuilderStylingRgbColor struct {
	F0 uint8
	F1 uint8
	F2 uint8
}

var _ = rs.RegisterTupleStruct[BuilderStylingRgbColor]("RgbColor")

// On is RgbColor::on: create a Style with this as the foreground.
func (c BuilderStylingRgbColor) On(background BuilderStylingColor) BuilderStylingStyle {
	return BuilderStylingStyleNew().
		FgColor(rs.Some[BuilderStylingColor](&BuilderStylingColor_Rgb{F0: c})).
		BgColor(rs.Some(background))
}

// OnDefault is RgbColor::on_default.
func (c BuilderStylingRgbColor) OnDefault() BuilderStylingStyle {
	return BuilderStylingStyleNew().FgColor(rs.Some[BuilderStylingColor](&BuilderStylingColor_Rgb{F0: c}))
}

// R is RgbColor::r: red.
func (c BuilderStylingRgbColor) R() uint8 { return c.F0 }

// G is RgbColor::g: green.
func (c BuilderStylingRgbColor) G() uint8 { return c.F1 }

// B is RgbColor::b: blue.
func (c BuilderStylingRgbColor) B() uint8 { return c.F2 }

func (c BuilderStylingRgbColor) codes() string {
	return strconv.Itoa(int(c.F0)) + ";" + strconv.Itoa(int(c.F1)) + ";" + strconv.Itoa(int(c.F2)) + "m"
}

// RenderFg is RgbColor::render_fg.
func (c BuilderStylingRgbColor) RenderFg() string { return "\x1B[38;2;" + c.codes() }

// RenderBg is RgbColor::render_bg.
func (c BuilderStylingRgbColor) RenderBg() string { return "\x1B[48;2;" + c.codes() }

func (c BuilderStylingRgbColor) renderUnderline() string { return "\x1B[58;2;" + c.codes() }

// BuilderStylingColor is anstyle::Color: any ANSI color code scheme.
type BuilderStylingColor interface {
	isBuilderStylingColor()
	// RenderFg is Color::render_fg: render the ANSI code for a foreground
	// color.
	RenderFg() string
	// RenderBg is Color::render_bg: render the ANSI code for a background
	// color.
	RenderBg() string
	renderUnderline() string
}

// BuilderStylingColor_Ansi is Color::Ansi: available 4-bit ANSI color
// palette codes.
type BuilderStylingColor_Ansi struct{ F0 BuilderStylingAnsiColor }

// BuilderStylingColor_Ansi256 is Color::Ansi256: 256 (8-bit) color support.
type BuilderStylingColor_Ansi256 struct{ F0 BuilderStylingAnsi256Color }

// BuilderStylingColor_Rgb is Color::Rgb: 24-bit ANSI RGB color codes.
type BuilderStylingColor_Rgb struct{ F0 BuilderStylingRgbColor }

func (*BuilderStylingColor_Ansi) isBuilderStylingColor()    {}
func (*BuilderStylingColor_Ansi256) isBuilderStylingColor() {}
func (*BuilderStylingColor_Rgb) isBuilderStylingColor()     {}

func (c *BuilderStylingColor_Ansi) RenderFg() string           { return c.F0.RenderFg() }
func (c *BuilderStylingColor_Ansi) RenderBg() string           { return c.F0.RenderBg() }
func (c *BuilderStylingColor_Ansi) renderUnderline() string    { return c.F0.renderUnderline() }
func (c *BuilderStylingColor_Ansi256) RenderFg() string        { return c.F0.RenderFg() }
func (c *BuilderStylingColor_Ansi256) RenderBg() string        { return c.F0.RenderBg() }
func (c *BuilderStylingColor_Ansi256) renderUnderline() string { return c.F0.renderUnderline() }
func (c *BuilderStylingColor_Rgb) RenderFg() string            { return c.F0.RenderFg() }
func (c *BuilderStylingColor_Rgb) RenderBg() string            { return c.F0.RenderBg() }
func (c *BuilderStylingColor_Rgb) renderUnderline() string     { return c.F0.renderUnderline() }

var _ = rs.RegisterEnum[BuilderStylingColor]("Color",
	rs.Variant[BuilderStylingColor_Ansi]("Ansi", rs.ShapeTuple),
	rs.Variant[BuilderStylingColor_Ansi256]("Ansi256", rs.ShapeTuple),
	rs.Variant[BuilderStylingColor_Rgb]("Rgb", rs.ShapeTuple),
)

// BuilderStylingColorOn is Color::on: create a Style with this as the
// foreground.
func BuilderStylingColorOn(foreground, background BuilderStylingColor) BuilderStylingStyle {
	return BuilderStylingStyleNew().FgColor(rs.Some(foreground)).BgColor(rs.Some(background))
}

// BuilderStylingColorOnDefault is Color::on_default: create a Style with
// this as the foreground.
func BuilderStylingColorOnDefault(foreground BuilderStylingColor) BuilderStylingStyle {
	return BuilderStylingStyleNew().FgColor(rs.Some(foreground))
}

func anstyleColorEq(a, b rs.Option[BuilderStylingColor]) bool {
	x, xok := a.Get()
	y, yok := b.Get()
	if xok != yok {
		return false
	}
	if !xok {
		return true
	}
	switch x := x.(type) {
	case *BuilderStylingColor_Ansi:
		y, ok := y.(*BuilderStylingColor_Ansi)
		return ok && x.F0 == y.F0
	case *BuilderStylingColor_Ansi256:
		y, ok := y.(*BuilderStylingColor_Ansi256)
		return ok && x.F0 == y.F0
	case *BuilderStylingColor_Rgb:
		y, ok := y.(*BuilderStylingColor_Rgb)
		return ok && x.F0 == y.F0
	default:
		panic(rs.Unreachable())
	}
}

// BuilderStylingStyle is anstyle::Style: ANSI text styling.
//
// You can print a Style to render the corresponding ANSI code. Using the
// alternate flag `#` will render the ANSI reset code, if needed. Together,
// this makes it convenient to render styles using inline format arguments;
// in Go those two forms are Render and RenderReset.
type BuilderStylingStyle struct {
	fg        rs.Option[BuilderStylingColor]
	bg        rs.Option[BuilderStylingColor]
	underline rs.Option[BuilderStylingColor]
	effects   BuilderStylingEffects
}

// BuilderStylingStyleNew is Style::new: no effects enabled.
func BuilderStylingStyleNew() BuilderStylingStyle {
	return BuilderStylingStyle{}
}

// FgColor is Style::fg_color: set foreground color.
func (s BuilderStylingStyle) FgColor(fg rs.Option[BuilderStylingColor]) BuilderStylingStyle {
	s.fg = fg
	return s
}

// BgColor is Style::bg_color: set background color.
func (s BuilderStylingStyle) BgColor(bg rs.Option[BuilderStylingColor]) BuilderStylingStyle {
	s.bg = bg
	return s
}

// UnderlineColor is Style::underline_color: set underline color.
func (s BuilderStylingStyle) UnderlineColor(underline rs.Option[BuilderStylingColor]) BuilderStylingStyle {
	s.underline = underline
	return s
}

// Effects is Style::effects: set text effects.
func (s BuilderStylingStyle) Effects(effects BuilderStylingEffects) BuilderStylingStyle {
	s.effects = effects
	return s
}

// Render is Style::render: render the ANSI code.
//
// Style also implements Display directly, so calling this method is
// optional.
func (s BuilderStylingStyle) Render() string {
	var out strings.Builder
	out.WriteString(s.effects.Render())
	if fg, ok := s.fg.Get(); ok {
		out.WriteString(fg.RenderFg())
	}
	if bg, ok := s.bg.Get(); ok {
		out.WriteString(bg.RenderBg())
	}
	if underline, ok := s.underline.Get(); ok {
		out.WriteString(underline.renderUnderline())
	}
	return out.String()
}

// RenderReset is Style::render_reset: renders the relevant Reset code.
//
// Unlike Reset::render, this will elide the code if there is nothing to
// reset.
func (s BuilderStylingStyle) RenderReset() string {
	if !s.IsPlain() {
		return anstyleReset
	}
	return ""
}

// String is the Display form without the alternate flag: the ANSI code.
func (s BuilderStylingStyle) String() string { return s.Render() }

// FmtDisplay is `impl Display for Style`.
func (s BuilderStylingStyle) FmtDisplay(f *rs.Formatter) error {
	if f.Alternate() {
		return f.WriteStr(s.RenderReset())
	}
	return f.WriteStr(s.Render())
}

// Bold is Style::bold: apply bold effect.
func (s BuilderStylingStyle) Bold() BuilderStylingStyle {
	s.effects = s.effects.Insert(BuilderStylingEffectsBold)
	return s
}

// Dimmed is Style::dimmed: apply dimmed effect.
func (s BuilderStylingStyle) Dimmed() BuilderStylingStyle {
	s.effects = s.effects.Insert(BuilderStylingEffectsDimmed)
	return s
}

// Italic is Style::italic: apply italic effect.
func (s BuilderStylingStyle) Italic() BuilderStylingStyle {
	s.effects = s.effects.Insert(BuilderStylingEffectsItalic)
	return s
}

// Underline is Style::underline: apply underline effect.
func (s BuilderStylingStyle) Underline() BuilderStylingStyle {
	s.effects = s.effects.Insert(BuilderStylingEffectsUnderline)
	return s
}

// Blink is Style::blink: apply blink effect.
func (s BuilderStylingStyle) Blink() BuilderStylingStyle {
	s.effects = s.effects.Insert(BuilderStylingEffectsBlink)
	return s
}

// Invert is Style::invert: apply invert effect.
func (s BuilderStylingStyle) Invert() BuilderStylingStyle {
	s.effects = s.effects.Insert(BuilderStylingEffectsInvert)
	return s
}

// Hidden is Style::hidden: apply hidden effect.
func (s BuilderStylingStyle) Hidden() BuilderStylingStyle {
	s.effects = s.effects.Insert(BuilderStylingEffectsHidden)
	return s
}

// Strikethrough is Style::strikethrough: apply strikethrough effect.
func (s BuilderStylingStyle) Strikethrough() BuilderStylingStyle {
	s.effects = s.effects.Insert(BuilderStylingEffectsStrikethrough)
	return s
}

// GetFgColor is Style::get_fg_color: get the foreground color.
func (s BuilderStylingStyle) GetFgColor() rs.Option[BuilderStylingColor] { return s.fg }

// GetBgColor is Style::get_bg_color: get the background color.
func (s BuilderStylingStyle) GetBgColor() rs.Option[BuilderStylingColor] { return s.bg }

// GetUnderlineColor is Style::get_underline_color.
func (s BuilderStylingStyle) GetUnderlineColor() rs.Option[BuilderStylingColor] { return s.underline }

// GetEffects is Style::get_effects.
func (s BuilderStylingStyle) GetEffects() BuilderStylingEffects { return s.effects }

// IsPlain is Style::is_plain: check if no styling is enabled.
func (s BuilderStylingStyle) IsPlain() bool {
	return s.fg.IsNone() && s.bg.IsNone() && s.underline.IsNone() && s.effects.IsPlain()
}

// Equal is `impl PartialEq for Style`.
func (s BuilderStylingStyle) Equal(other BuilderStylingStyle) bool {
	return anstyleColorEq(s.fg, other.fg) && anstyleColorEq(s.bg, other.bg) &&
		anstyleColorEq(s.underline, other.underline) && s.effects == other.effects
}

// BuilderStylingStyleFromEffects is `impl From<Effects> for Style`.
func BuilderStylingStyleFromEffects(effects BuilderStylingEffects) BuilderStylingStyle {
	return BuilderStylingStyleNew().Effects(effects)
}
