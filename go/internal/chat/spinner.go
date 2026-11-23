// Port of mj-chat/src/spinner.rs.

// Prompt-activity spinner styles.
//
// A spinner style is a purely client-side visual preference: it is persisted
// as `spinner` in `config.toml` and changeable from the command palette.
//
// Every style renders to frames of exactly SpinnerWidth display columns
// (including its idle frame) so the activity row never reflows when a turn
// starts, ends, or the style changes. Frames are generated once on first use.
//
// Frames carry color as SpinnerInk slots rather than concrete colors, so the
// palette can adapt gradients to the active terminal capabilities.

package chat

import (
	"image/color"
	"sync"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type SpinnerStyle = core.SpinnerStyle

// SpinnerWidth is the display width (terminal columns) of every spinner
// frame, for every style.
const SpinnerWidth = 12

// ScanRedLevels is the number of intensity levels in the scan-light gradient.
const ScanRedLevels = SpinnerWidth

const scanRedMax uint8 = ScanRedLevels - 1

// idleGlyph is the resting ornament shown when no turn is in flight.
const idleGlyph = '─'

// SpinnerFrameIntervalMs is the wall-clock dwell per animation frame. Kept
// deliberately calmer than streaming redraws so progress reads as steady
// activity without making queued prompt typing feel visually noisy.
const SpinnerFrameIntervalMs uint64 = 250

// SpinnerRedrawIntervalMs is the fastest frame interval any spinner uses. The
// UI redraw timer follows this value while individual styles retain their
// own wall-clock cadence.
const SpinnerRedrawIntervalMs uint64 = scanFrameIntervalMs

// scanFrameIntervalMs is the dwell between adjacent scan-light positions.
// Across the full-width rail, this produces an edge-to-edge sweep of roughly
// one second.
const scanFrameIntervalMs uint64 = 90

// SpinnerInkKind names a SpinnerInk slot.
type SpinnerInkKind uint8

const (
	// SpinnerInkKindFaint is the resting rail and the coldest cells; recedes
	// toward the border.
	SpinnerInkKindFaint SpinnerInkKind = iota
	// SpinnerInkKindCool is the body of the motion.
	SpinnerInkKindCool
	// SpinnerInkKindBright is the leading edge.
	SpinnerInkKindBright
	// SpinnerInkKindVivid is the single hottest cell of a frame.
	SpinnerInkKindVivid
	// SpinnerInkKindCalm is the low end of a metered ramp.
	SpinnerInkKindCalm
	// SpinnerInkKindWarm is the middle of a metered ramp.
	SpinnerInkKindWarm
	// SpinnerInkKindHot is the peak of a metered ramp.
	SpinnerInkKindHot
	// SpinnerInkKindRed is a red gradient level, from the dim rail at zero to
	// the bright head.
	SpinnerInkKindRed
)

// SpinnerInk is the color slot for one spinner cell, resolved against the
// shared terminal palette by the rendering helpers.
//
// The first four are a cold-to-hot energy ramp used by the motion styles.
// Calm, Warm, and Hot form the metered ramp used by Bars; Red carries one
// level of the scan-light gradient in Level. Level is zero for every other
// kind, so inks compare with ==.
type SpinnerInk struct {
	Kind  SpinnerInkKind
	Level uint8
}

var (
	SpinnerInkFaint  = SpinnerInk{Kind: SpinnerInkKindFaint}
	SpinnerInkCool   = SpinnerInk{Kind: SpinnerInkKindCool}
	SpinnerInkBright = SpinnerInk{Kind: SpinnerInkKindBright}
	SpinnerInkVivid  = SpinnerInk{Kind: SpinnerInkKindVivid}
	SpinnerInkCalm   = SpinnerInk{Kind: SpinnerInkKindCalm}
	SpinnerInkWarm   = SpinnerInk{Kind: SpinnerInkKindWarm}
	SpinnerInkHot    = SpinnerInk{Kind: SpinnerInkKindHot}
)

// SpinnerInkRed is Rust's `SpinnerInk::Red(level)`.
func SpinnerInkRed(level uint8) SpinnerInk {
	return SpinnerInk{Kind: SpinnerInkKindRed, Level: level}
}

// SpinnerRun is one maximal same-ink run of a frame.
type SpinnerRun struct {
	Text string
	Ink  SpinnerInk
}

// SpinnerFrame is one rendered frame: the glyph row together with the ink
// each glyph takes. Built as a unit from a single (glyph, ink) sequence, so a
// style cannot ship colors that disagree with the glyphs they are meant to
// shade.
//
// Both representations are computed once, at construction: like the frames
// themselves they never change, and they are read on every redraw.
type SpinnerFrame struct {
	text string
	runs []SpinnerRun
}

// Text is the frame's glyphs, without color. Used where a plain string is
// all the surface can carry (plain-text previews and width assertions).
func (f *SpinnerFrame) Text() string {
	return f.text
}

// Runs is the frame split into maximal same-ink runs, one styled span each.
// Merging keeps a twelve-cell strip down to a handful of spans.
func (f *SpinnerFrame) Runs() []SpinnerRun {
	return f.runs
}

// spinnerStyleIndex is the SpinnerFrames trait's `index`, derived from
// SpinnerStyleAll so it cannot drift from frameSets (also All-ordered).
func spinnerStyleIndex(style SpinnerStyle) int {
	for index, candidate := range core.SpinnerStyleAll {
		if candidate == style {
			return index
		}
	}
	return 0
}

// spinnerStyleFrames is the animated frames for this style. Always
// non-empty; index with the wall-clock tick and this style's frame interval.
func spinnerStyleFrames(style SpinnerStyle) []SpinnerFrame {
	return frameSets()[spinnerStyleIndex(style)].animated
}

// spinnerStyleIdleFrame is the resting frame shown when no turn is in
// flight.
func spinnerStyleIdleFrame(style SpinnerStyle) *SpinnerFrame {
	return &frameSets()[spinnerStyleIndex(style)].idle
}

// spinnerStyleFrameIntervalMs is the wall-clock dwell for this style's
// full-width animation frames.
//
// Remote viewers receive the same frames as the TUI and use this value to
// keep the animation cadence in sync as well.
func spinnerStyleFrameIntervalMs(style SpinnerStyle) uint64 {
	if style == core.SpinnerStyleScan {
		return scanFrameIntervalMs
	}
	return SpinnerFrameIntervalMs
}

func spinnerStyleCompactFrames(style SpinnerStyle) []string {
	panic("unported: mj-chat/src/spinner.rs spinnerStyleCompactFrames")
}

var spinnerStarted = sync.OnceValue(time.Now)

// ElapsedMs is milliseconds on a monotonic animation clock, independent of
// input redraws.
func ElapsedMs() uint64 {
	return uint64(time.Since(spinnerStarted()).Milliseconds())
}

func frameIndex(elapsedMs uint64, count int, intervalMs uint64) int {
	return int((elapsedMs / intervalMs) % uint64(count))
}

// asciiCompactFrames are the frames of every style when only ASCII can be
// drawn.
var asciiCompactFrames = [4]string{"|", "/", "-", "\\"}

// ActivityLine is the colored twelve-column activity ornament. Idle always
// rests on a quiet rule.
func ActivityLine(style SpinnerStyle, elapsedMs uint64, active bool) Line {
	panic("unported: mj-chat/src/spinner.rs ActivityLine")
}

// CompactFrame is the single-column activity frame for session lists and
// loading states.
func CompactFrame(style SpinnerStyle, elapsedMs uint64) string {
	panic("unported: mj-chat/src/spinner.rs CompactFrame")
}

// CompactSpan is the compact loading indicator with the same palette as the
// full ornament.
func CompactSpan(style SpinnerStyle, elapsedMs uint64) Span {
	panic("unported: mj-chat/src/spinner.rs CompactSpan")
}

func inkColor(ink SpinnerInk) color.Color {
	panic("unported: mj-chat/src/spinner.rs inkColor")
}

type frameSet struct {
	animated []SpinnerFrame
	idle     SpinnerFrame
}

func frameSetFor(style SpinnerStyle) frameSet {
	switch style {
	case core.SpinnerStylePulse:
		return buildPulse()
	case core.SpinnerStyleWave:
		return buildWave()
	case core.SpinnerStyleBars:
		return buildBars()
	case core.SpinnerStyleShimmer:
		return buildShimmer()
	case core.SpinnerStyleGlobe:
		return buildGlobe()
	default:
		return buildScan()
	}
}

// frameSets is all styles' frames, generated once and kept for the process
// lifetime. Built by mapping over SpinnerStyleAll, so
// frameSets()[spinnerStyleIndex(style)] is always style's frames.
var frameSets = sync.OnceValue(func() *[len(core.SpinnerStyleAll)]frameSet {
	var sets [len(core.SpinnerStyleAll)]frameSet
	for index, style := range core.SpinnerStyleAll {
		sets[index] = frameSetFor(style)
	}
	return &sets
})

// spinnerCell is one (glyph, ink) cell of a frame being built.
type spinnerCell struct {
	glyph rune
	ink   SpinnerInk
}

// row assembles one frame from its (glyph, ink) cells, checking the width
// contract at the single point where every style's frames are born.
func row(cells []spinnerCell) SpinnerFrame {
	panic("unported: mj-chat/src/spinner.rs row")
}

// idleRow is the rule every style rests on. Faint on purpose: the resting
// ornament should recede into the border so an active turn's color reads as
// a change.
func idleRow() SpinnerFrame {
	cells := make([]spinnerCell, SpinnerWidth)
	for index := range cells {
		cells[index] = spinnerCell{glyph: idleGlyph, ink: SpinnerInkFaint}
	}
	return row(cells)
}

// buildPulse: a bright dot glides left-to-right and wraps, with a symmetric
// brightness falloff on either side so it reads as a soft pulse rather than a
// hard pip. Color follows the same falloff, giving the dot a hot core and a
// cold tail.
func buildPulse() frameSet {
	panic("unported: mj-chat/src/spinner.rs buildPulse")
}

// buildWave: an undulating braille ribbon that scrolls one full wavelength
// per loop. Height also drives color, so crests catch the light and troughs
// sink into the rail.
func buildWave() frameSet {
	panic("unported: mj-chat/src/spinner.rs buildWave")
}

// buildBars: vertical eighth-block bars whose heights ripple like an
// equalizer, shaded like one too: green through the low range, amber as it
// climbs, and red only on the single tallest step.
func buildBars() frameSet {
	panic("unported: mj-chat/src/spinner.rs buildBars")
}

// buildShimmer: the whole row brightens and dims together — a calm,
// confident "working…". Size and color breathe on the same ramp.
func buildShimmer() frameSet {
	panic("unported: mj-chat/src/spinner.rs buildShimmer")
}

// globePhases is one rotation of a lit sphere, ordered so the terminator
// sweeps steadily across the disc: fully dark, lit growing in from the right
// edge, fully lit, then shrinking off the left edge.
var globePhases = [8]rune{'🌑', '🌒', '🌓', '🌔', '🌕', '🌖', '🌗', '🌘'}

const globeClearance = 2

// buildGlobe: a single sphere spins at the centre of the idle rule with a
// small gap around it. The sphere is inked Bright for terminals that render
// the moons as monochrome text; those that use emoji presentation supply
// their own color and ignore it.
func buildGlobe() frameSet {
	panic("unported: mj-chat/src/spinner.rs buildGlobe")
}

// scanHeads is the head positions for two complete one-way sweeps. Each
// movement advances one adjacent cell, and both journeys include their
// destination wall.
func scanHeads() []int64 {
	width := int64(SpinnerWidth)
	heads := make([]int64, 0, 2*width-1)
	for head := int64(0); head < width; head++ {
		heads = append(heads, head)
	}
	for head := width - 2; head >= 0; head-- {
		heads = append(heads, head)
	}
	return heads
}

func scanCell(distance int64) spinnerCell {
	if distance < 0 || distance >= SpinnerWidth {
		return spinnerCell{glyph: '·', ink: SpinnerInkRed(0)}
	}
	level := scanRedMax - uint8(distance)
	glyph := '·'
	switch {
	case distance == 0:
		glyph = '●'
	case distance <= 4:
		glyph = '•'
	case distance <= 8:
		glyph = '∙'
	}
	return spinnerCell{glyph: glyph, ink: SpinnerInkRed(level)}
}

// buildScan: a lit head sweeps to one wall, reverses, and sweeps back,
// dragging a short fading tail. The tail always trails the direction of
// travel, which is what makes the light read as bouncing between the walls
// rather than wrapping around like Pulse. Every active cell keeps a low red
// glow so motion comes from the brighter peak and afterglow rather than cells
// switching fully off.
func buildScan() frameSet {
	panic("unported: mj-chat/src/spinner.rs buildScan")
}
