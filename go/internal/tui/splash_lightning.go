// Port of mj-tui/src/splash/lightning.rs.
//
// Lightning answering the hammer: branched bolts from the clouds into its
// crown, and short arcs crawling over the head. Each bolt holds its shape
// for one flicker interval and is redrawn from a new seed in the next, so
// the sky crackles.
package tui

import "math"

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
	since := t - impact
	if since < 0.0 {
		return Strike{}, false
	}
	length, chance, salt := thunderFlicker, float32(0.8), uint64(0x7E0D)
	if holding {
		length, chance, salt = heldFlicker, 0.4, 0x4E1D
	}
	interval := splashFloor(since / length)
	within := since/length - interval
	seed := salt ^ asU64(interval)*0x2545_F491_4F6C_DD1D
	rng := newRng(seed)
	first := !holding && interval == 0.0
	if !first && rng.unit() > chance {
		return Strike{}, false
	}
	var level float32 = 1.4
	if !first {
		level = rng.between(0.6, 1.1)
	}
	// A strike flares, fades, and sometimes flares again as it re-forms.
	var restrike float32
	if rng.unit() > 0.5 {
		restrike = 0.6 * max(1.0-splashAbs((within-0.55)/0.1), 0.0)
	}
	return Strike{
		seed:       seed,
		brightness: level * (splashPow(1.0-within, 0.7) + restrike),
	}, true
}

// bolt is midpoint displacement: split every segment, nudge the midpoint
// sideways, halve the nudge, repeat.
func bolt(rng *Rng, from, to [2]float32, depth uint32, roughness float32) [][2]float32 {
	points := [][2]float32{from, to}
	amplitude := roughness * splashHypot(to[0]-from[0], to[1]-from[1])
	for range depth {
		next := make([][2]float32, 0, len(points)*2)
		for index := 0; index+1 < len(points); index++ {
			a, b := points[index], points[index+1]
			dx, dy := b[0]-a[0], b[1]-a[1]
			length := max(splashHypot(dx, dy), 1e-6)
			offset := rng.between(-1.0, 1.0) * amplitude
			next = append(next, a, [2]float32{
				(a[0]+b[0])/2.0 - dy/length*offset,
				(a[1]+b[1])/2.0 + dx/length*offset,
			})
		}
		next = append(next, to)
		points = next
		amplitude *= 0.5
	}
	return points
}

func stroke(field *Field, view *View, points [][2]float32, core, intensity float32) {
	glow := max(view.scale*0.05, 1.6)
	for index := 0; index+1 < len(points); index++ {
		field.segment(view.pixel(points[index]), view.pixel(points[index+1]), core, glow, intensity)
	}
}

// lightningDraw is Rust's lightning::draw.
func lightningDraw(canvas *Canvas, view *View, strike *Strike, pose Pose) {
	rng := newRng(strike.seed ^ 0xB017)
	field := newField(canvas.width, canvas.height)
	extent := view.extent()
	halfWidth, halfHeight := extent[0], extent[1]
	crown := pose.toWorld(hammerCrown)
	bolts := 1
	if rng.unit() > 0.6 {
		bolts = 2
	}
	for range bolts {
		from := [2]float32{
			rng.between(-halfWidth, halfWidth) * 0.8,
			-halfHeight - 0.05,
		}
		target := [2]float32{crown[0] + rng.between(-0.01, 0.01), crown[1]}
		path := bolt(&rng, from, target, 6, 0.22)
		stroke(&field, view, path, 0.7, 1.0)
		branches := 2 + asUsize(rng.unit()*3.0)
		for range branches {
			start := path[asUsize(rng.between(0.1, 0.75)*float32(len(path)))]
			heading := float32(math.Atan2(float64(target[1]-from[1]), float64(target[0]-from[0])))
			turn := rng.between(0.4, 0.95)
			if rng.unit() > 0.5 {
				heading += turn * 1.0
			} else {
				heading += turn * -1.0
			}
			length := rng.between(0.25, 0.6)
			end := [2]float32{
				start[0] + splashCos(heading)*length,
				start[1] + splashSin(heading)*length,
			}
			branch := bolt(&rng, start, end, 4, 0.25)
			stroke(&field, view, branch, 0.35, 0.55)
		}
	}
	// Arcs from edge to edge of the head.
	for range 3 {
		edge := func(rng *Rng) [2]float32 {
			along := rng.between(-1.0, 1.0)
			var side float32 = -1.0
			if rng.unit() > 0.5 {
				side = 1.0
			}
			if rng.unit() > 0.4 {
				return pose.toWorld([2]float32{along * hammerHead[0], side * hammerHead[1]})
			}
			return pose.toWorld([2]float32{side * hammerHead[0], along * hammerHead[1]})
		}
		a := edge(&rng)
		b := edge(&rng)
		arc := bolt(&rng, a, b, 3, 0.3)
		stroke(&field, view, arc, 0.25, 0.6)
	}
	brightness := min(strike.brightness, 1.4)
	for y := range canvas.height {
		for x := range canvas.width {
			value := field.get(x, y) * brightness
			if value > 0.004 {
				color := mix(lightningGlow, lightningCore, smoothstep(0.55, 1.0, value))
				canvas.add(x, y, Rgb{color[0] * value * 1.3, color[1] * value * 1.3, color[2] * value * 1.3})
			}
		}
	}
}
