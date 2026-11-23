// Port of mj-tui/src/splash/lightning.rs.
//
// Lightning answering the hammer: branched bolts from the clouds into its
// crown, and short arcs crawling over the head. Each bolt holds its shape
// for one flicker interval and is redrawn from a new seed in the next, so
// the sky crackles.
package tui

// lightningCore and lightningGlow are Rust's CORE and GLOW: a package-level
// `core` would collide with the core package import.
var (
	lightningCore = Rgb{0.95, 0.97, 1.0}
	lightningGlow = Rgb{0.45, 0.52, 1.0}
)

const (
	thunderFlicker float32 = 0.075
	heldFlicker    float32 = 0.11
)

// Strike is one flicker interval in which lightning may strike.
type Strike struct {
	seed uint64
	// brightness is how bright the strike is now; it also lights the
	// clouds.
	brightness float32
}

// strikeAt is the strike at scene time t, if one is lit. The first one
// lands with the hammer; later ones come less often while the splash waits.
func strikeAt(t float32, holding bool) (Strike, bool) {
	panic("unported: mj-tui/src/splash/lightning.rs Strike::at")
}

// bolt is midpoint displacement: split every segment, nudge the midpoint
// sideways, halve the nudge, repeat.
func bolt(rng *Rng, from, to [2]float32, depth uint32, roughness float32) [][2]float32 {
	panic("unported: mj-tui/src/splash/lightning.rs bolt")
}

func stroke(field *Field, view *View, points [][2]float32, core, intensity float32) {
	glow := max(view.scale*0.05, 1.6)
	for index := 0; index+1 < len(points); index++ {
		field.segment(view.pixel(points[index]), view.pixel(points[index+1]), core, glow, intensity)
	}
}

// lightningDraw is Rust's lightning::draw.
func lightningDraw(canvas *Canvas, view *View, strike *Strike, pose Pose) {
	panic("unported: mj-tui/src/splash/lightning.rs draw")
}
