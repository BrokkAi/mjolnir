// Port of mj-tui/src/splash.rs.
//
// The startup splash: Mjölnir falls out of a storm, strikes the ground and
// calls down lightning while the dashboard loads behind it.
//
// A frame is a pure function of [SplashFrame], so any instant can be
// rendered again or tested. [SplashTimeline] alone decides the phases; the
// driver asks it when the splash is over instead of keeping its own clock.
//
// The scene is painted into a floating-point canvas at two pixels per cell
// (upper half block: foreground on top, background below), tone-mapped,
// and written to the buffer once.
//
// Rust's splash module and its private submodules (canvas, font, fx,
// hammer, lightning) share package tui here. Their f32 pairs are
// [2]float32, and their private constants with generic names carry the
// submodule's name (hammerRim, lightningCore) so they cannot collide with
// the rest of the package or with an imported package name.
package tui

import (
	"math"
	"time"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

// Scene times, in seconds. The splash opens with the hammer already
// falling above the screen; it comes into view about 0.16s later.
const (
	descentStart float32 = -0.16
	impact       float32 = 0.44
	titleStart   float32 = 0.61
)

// holdStart is when the finished title stays up for a moment before the
// splash may end.
const holdStart = 1340 * time.Millisecond

const dissolve = 250 * time.Millisecond

// groundY is the horizon, in scene units below the centre of the screen.
const groundY float32 = 0.40

// SplashTimeline says when the splash may end. Loading readiness is the only
// input; every phase decision is derived here. The zero value is Rust's
// Default: loading has not finished.
type SplashTimeline struct {
	// readyAt is when loading finished; meaningful only when ready is set
	// (Rust's `Option<Duration>`).
	readyAt time.Duration
	ready   bool
}

// MarkReady records that loading finished at at. Only the first report
// counts.
func (t *SplashTimeline) MarkReady(at time.Duration) {
	if !t.ready {
		t.readyAt = at
		t.ready = true
	}
}

// IsReady reports whether loading has finished.
func (t SplashTimeline) IsReady() bool {
	return t.ready
}

// DissolveStart is when the dissolve into the dashboard begins, once
// loading is ready.
func (t SplashTimeline) DissolveStart() (time.Duration, bool) {
	if !t.ready {
		return 0, false
	}
	return max(t.readyAt, holdStart), true
}

// Finished reports whether the last frame has been shown and the dashboard
// can draw.
func (t SplashTimeline) Finished(elapsed time.Duration) bool {
	start, ok := t.DissolveStart()
	return ok && elapsed >= start+dissolve
}

// Holding reports whether the animation has played out and is waiting for
// loading.
func (t SplashTimeline) Holding(elapsed time.Duration) bool {
	return t.sceneTime(elapsed) >= durationSecsF32(holdStart) && t.dissolveProgress(elapsed) == 0.0
}

func (t SplashTimeline) sceneTime(elapsed time.Duration) float32 {
	return durationSecsF32(elapsed)
}

func (t SplashTimeline) dissolveProgress(elapsed time.Duration) float32 {
	start, ok := t.DissolveStart()
	if !ok {
		return 0.0
	}
	since := max(elapsed-start, 0)
	return min(durationSecsF32(since)/durationSecsF32(dissolve), 1.0)
}

// durationSecsF32 is Rust's Duration::as_secs_f32: whole seconds and the
// nanosecond remainder converted to f32 separately, so scene times round
// exactly as they do in the Rust build.
func durationSecsF32(d time.Duration) float32 {
	secs := uint64(d / time.Second)
	nanos := uint32(d % time.Second)
	return float32(secs) + float32(nanos)/float32(time.Second)
}

// Fits reports whether a terminal of this width shows the splash. The
// splash asks no more of the terminal than the dashboard behind it does.
func Fits(width int) bool {
	return width >= narrowTerminalWidth
}

// SplashFrame is everything one splash frame depends on.
type SplashFrame struct {
	Elapsed  time.Duration
	Timeline SplashTimeline
	// Status is the latest startup notice, shown while the splash waits for
	// loading; nil when there is none.
	Status *string
	// Background is the dashboard's background, which the dissolve reveals.
	Background chat.Color
}

// RenderSplash draws frame into area of buf.
func RenderSplash(area uv.Rectangle, buf *uv.Buffer, frame *SplashFrame) {
	panic("unported: mj-tui/src/splash.rs render_splash")
}

func fill(area uv.Rectangle, buf *uv.Buffer, background chat.Color) {
	panic("unported: mj-tui/src/splash.rs fill")
}

// drawStatus draws the startup notice, or a quiet default, centred on the
// bottom row.
func drawStatus(area uv.Rectangle, buf *uv.Buffer, status *string, t float32) {
	panic("unported: mj-tui/src/splash.rs draw_status")
}

// View maps canvas pixels to scene units: the origin is the screen centre,
// y grows downward, and one unit is about half the screen height.
type View struct {
	width  int
	height int
	scale  float32
	shake  [2]float32
}

func newView(width, height int, t float32) View {
	panic("unported: mj-tui/src/splash.rs View::new")
}

// scene is the scene point under a pixel's centre.
func (v *View) scene(x, y int) [2]float32 {
	return [2]float32{
		(float32(x)+0.5-float32(v.width)/2.0)/v.scale - v.shake[0],
		(float32(y)+0.5-float32(v.height)/2.0)/v.scale - v.shake[1],
	}
}

// pixel is a scene point in continuous pixel coordinates.
func (v *View) pixel(point [2]float32) [2]float32 {
	return [2]float32{
		(point[0]+v.shake[0])*v.scale + float32(v.width)/2.0,
		(point[1]+v.shake[1])*v.scale + float32(v.height)/2.0,
	}
}

// extent is half the visible scene, in scene units.
func (v *View) extent() [2]float32 {
	return [2]float32{
		float32(v.width) / 2.0 / v.scale,
		float32(v.height) / 2.0 / v.scale,
	}
}

func smoothstep(edge0, edge1, x float32) float32 {
	t := min(max((x-edge0)/(edge1-edge0), 0.0), 1.0)
	return t * t * (3.0 - 2.0*t)
}

func mix(a, b Rgb, t float32) Rgb {
	return Rgb{
		a[0] + (b[0]-a[0])*t,
		a[1] + (b[1]-a[1])*t,
		a[2] + (b[2]-a[2])*t,
	}
}

func scaled(color Rgb, factor float32) Rgb {
	return Rgb{color[0] * factor, color[1] * factor, color[2] * factor}
}

func rotate(point [2]float32, angle float32) [2]float32 {
	sin, cos := math.Sincos(float64(angle))
	s, c := float32(sin), float32(cos)
	return [2]float32{point[0]*c - point[1]*s, point[0]*s + point[1]*c}
}

// Rng is SplitMix64: small, seedable, and identical on every platform.
type Rng uint64

func newRng(seed uint64) Rng {
	return Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
}

func (r *Rng) next() uint64 {
	*r += 0x9E37_79B9_7F4A_7C15
	z := uint64(*r)
	z = (z ^ (z >> 30)) * 0xBF58_476D_1CE4_E5B9
	z = (z ^ (z >> 27)) * 0x94D0_49BB_1331_11EB
	return z ^ (z >> 31)
}

func (r *Rng) unit() float32 {
	return float32(r.next()>>40) / float32(uint64(1)<<24)
}

// between is Rust's `Rng::range`, renamed because range is a Go keyword.
func (r *Rng) between(low, high float32) float32 {
	return low + (high-low)*r.unit()
}

// hash2 is a stable pseudo-random value in [0, 1) for a lattice point.
func hash2(x, y int32) float32 {
	rng := newRng(uint64(uint32(x))<<32 | uint64(uint32(y)))
	return rng.unit()
}
