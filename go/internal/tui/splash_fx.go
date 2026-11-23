// Port of mj-tui/src/splash/fx.rs.
//
// The storm, the ground, and everything the impact throws off.
package tui

import "math"

// sparkCount is Rust's SPARKS and crackCount its CRACKS, renamed because
// the sparks and cracks functions share their Go names.
const (
	sparkCount uint64  = 90
	gravity    float32 = 5.5
	crackCount uint64  = 9
)

func valueNoise(x, y float32) float32 {
	floorX, floorY := float32(math.Floor(float64(x))), float32(math.Floor(float64(y)))
	ix, iy := int32(floorX), int32(floorY)
	fx, fy := x-floorX, y-floorY
	sx, sy := fx*fx*(3.0-2.0*fx), fy*fy*(3.0-2.0*fy)
	top := hash2(ix, iy) + (hash2(ix+1, iy)-hash2(ix, iy))*sx
	bottom := hash2(ix, iy+1) + (hash2(ix+1, iy+1)-hash2(ix, iy+1))*sx
	return top + (bottom-top)*sy
}

func clouds(x, y float32) float32 {
	var total float32
	amplitude := float32(0.55)
	frequency := float32(1.0)
	for range 3 {
		total += amplitude * valueNoise(x*frequency, y*frequency)
		amplitude *= 0.5
		frequency *= 2.1
	}
	return total
}

// stormGlow is sheet lightning inside the clouds before the hammer lands: a
// soft bloom at a random place in some flicker intervals. It reports the
// bloom's centre and strength, or false when the sky is dark.
func stormGlow(t float32) ([2]float32, float32, bool) {
	if t >= impact {
		return [2]float32{}, 0, false
	}
	interval := float32(math.Floor(float64(t / 0.11)))
	rng := newRng(0x5707 ^ uint64(interval))
	if rng.unit() < 0.55 {
		return [2]float32{}, 0, false
	}
	within := t/0.11 - interval
	centre := [2]float32{rng.between(-1.4, 1.4), rng.between(-0.95, -0.45)}
	return centre, rng.between(0.4, 0.9) * (1.0 - within), true
}

// sky paints a night sky: scrolling storm clouds, stars in the gaps, and
// the clouds lit from within by flash.
func sky(canvas *Canvas, view *View, t, flash float32) {
	panic("unported: mj-tui/src/splash/fx.rs sky")
}

// heat is how hot the ground is where the hammer struck: a flash of heat
// that settles into a glowing bed of embers.
func heat(t float32) float32 {
	since := t - impact
	if since < 0.0 {
		return 0.0
	}
	return float32(math.Exp(float64(-since*2.2))) + 0.18 + 0.05*float32(math.Sin(float64(t*9.0)))
}

// ground paints dark rock below the horizon, a rim of storm light along
// it, and the glow of the impact.
func ground(canvas *Canvas, view *View, t, flash float32) {
	panic("unported: mj-tui/src/splash/fx.rs ground")
}

// cracks races cracks outward from the impact, glowing white-hot and
// cooling to a dull red.
func cracks(canvas *Canvas, view *View, t float32) {
	panic("unported: mj-tui/src/splash/fx.rs cracks")
}

// shockwave draws two shock rings with split color fringes: one along the
// ground in perspective, one through the air around the head.
func shockwave(canvas *Canvas, view *View, t float32) {
	panic("unported: mj-tui/src/splash/fx.rs shockwave")
}

func ember(heat float32) Rgb {
	switch {
	case heat > 0.7:
		return mix(Rgb{1.0, 0.8, 0.3}, Rgb{1.0, 0.97, 0.85}, (heat-0.7)/0.3)
	case heat > 0.35:
		return mix(Rgb{1.0, 0.42, 0.08}, Rgb{1.0, 0.8, 0.3}, (heat-0.35)/0.35)
	default:
		return mix(Rgb{0.0, 0.0, 0.0}, Rgb{1.0, 0.42, 0.08}, heat/0.35)
	}
}

// sparks throws sparks up from the impact, falling under gravity with
// short streaks behind them.
func sparks(canvas *Canvas, view *View, t float32) {
	panic("unported: mj-tui/src/splash/fx.rs sparks")
}

// impactFlash is the white flash of the strike, cooling to blue over three
// frames.
func impactFlash(canvas *Canvas, t float32) {
	since := t - impact
	if since < 0.0 || since >= 0.07 {
		return
	}
	fade := 1.0 - since/0.07
	strength := fade * fade * 1.6
	canvas.addAll(Rgb{strength * 0.85, strength * 0.9, strength * 1.15})
}
