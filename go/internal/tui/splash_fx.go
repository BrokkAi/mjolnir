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
	glowCentre, glowStrength, glowing := stormGlow(t)
	for py := range canvas.height {
		for px := range canvas.width {
			point := view.scene(px, py)
			x, y := point[0], point[1]
			if y > groundY {
				continue
			}
			color := mix(
				Rgb{0.010, 0.008, 0.030},
				Rgb{0.045, 0.030, 0.090},
				smoothstep(-1.05, groundY, y),
			)
			reach := smoothstep(0.15, -0.6, y)
			var density float32
			if reach > 0.0 {
				density = smoothstep(0.42, 0.85, clouds(x*1.3+t*0.12, y*2.6-t*0.03)) * reach
			}
			light := 0.18 + flash*0.9
			if glowing {
				gx, gy := x-glowCentre[0], y-glowCentre[1]
				distance := gx*gx + gy*gy*2.0
				light += glowStrength * 2.2 * splashExp(-distance/0.18)
			}
			cloud := Rgb{0.55, 0.42, 1.0}
			for channel := range 3 {
				color[channel] += cloud[channel] * density * light
			}
			if density < 0.15 && y < 0.25 {
				seed := hash2(int32(px), int32(py))
				if seed > 0.994 {
					twinkle := 0.55 + 0.45*splashSin(t*7.0+seed*1000.0)
					star := twinkle * (1.0 - density*6.0) * 0.9
					for channel, tint := range [3]float32{0.75, 0.8, 1.0} {
						color[channel] += tint * star
					}
				}
			}
			canvas.set(px, py, color)
		}
	}
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
	warmth := heat(t)
	for py := range canvas.height {
		for px := range canvas.width {
			point := view.scene(px, py)
			x, y := point[0], point[1]
			if y <= groundY {
				continue
			}
			depth := y - groundY
			grain := valueNoise(x*9.0, depth*26.0) * 0.02
			color := Rgb{0.030 + grain, 0.026 + grain, 0.042 + grain}
			rim := splashExp(-depth*55.0) * (0.25 + flash)
			for channel, tint := range [3]float32{0.16, 0.14, 0.34} {
				color[channel] += tint * rim
			}
			deep := depth * 3.0
			pool := splashExp(-(x*x+deep*deep)*5.0) * warmth
			for channel, tint := range [3]float32{1.0, 0.42, 0.12} {
				color[channel] += tint * pool * 0.9
			}
			canvas.set(px, py, color)
		}
	}
}

// cracks races cracks outward from the impact, glowing white-hot and
// cooling to a dull red.
func cracks(canvas *Canvas, view *View, t float32) {
	since := t - impact
	if since < 0.0 {
		return
	}
	grown := smoothstep(0.0, 0.2, since)
	field := newField(canvas.width, canvas.height)
	for crack := range crackCount {
		rng := newRng(0xC4AC ^ crack*0x9E37)
		spread := (float32(crack) + rng.between(0.2, 0.8)) / float32(crackCount)
		heading := -0.12*splashPi + spread*1.24*splashPi
		point := [2]float32{splashCos(heading) * 0.3, groundY + 0.006}
		const segments = 6
		visible := grown * float32(segments)
		for segment := range segments {
			heading += rng.between(-0.45, 0.45)
			length := 0.14 * rng.between(0.7, 1.3)
			next := [2]float32{
				point[0] + splashCos(heading)*length,
				max(point[1]+splashSin(heading)*length*0.45, groundY+0.004),
			}
			shown := splashClamp(visible-float32(segment), 0.0, 1.0)
			if shown > 0.0 {
				end := [2]float32{
					point[0] + (next[0]-point[0])*shown,
					point[1] + (next[1]-point[1])*shown,
				}
				taper := 1.0 - float32(segment)/float32(segments)*0.6
				field.segment(view.pixel(point), view.pixel(end), 0.35, 1.4, taper)
			}
			point = next
		}
	}
	cooling := splashExp(-since * 2.8)
	color := mix(Rgb{0.55, 0.08, 0.02}, Rgb{1.0, 0.85, 0.5}, cooling)
	strength := 0.5 + 1.2*cooling + 0.1*splashSin(t*11.0)
	for y := range canvas.height {
		for x := range canvas.width {
			value := field.get(x, y)
			if value > 0.004 {
				canvas.add(x, y, scaled(color, value*strength))
			}
		}
	}
}

// shockwave draws two shock rings with split color fringes: one along the
// ground in perspective, one through the air around the head.
func shockwave(canvas *Canvas, view *View, t float32) {
	since := t - impact
	if !(since >= 0.0 && since < 0.9) {
		return
	}
	groundRadius := since * 2.8
	groundFade := splashExp(-since*3.5) * 1.2
	airRadius := since * 3.6
	airFade := splashExp(-since*5.0) * 0.6
	fringe := [3]float32{1.05, 1.0, 0.95}
	for py := range canvas.height {
		for px := range canvas.width {
			point := view.scene(px, py)
			x, y := point[0], point[1]
			var light [3]float32
			for channel := range 3 {
				radius := groundRadius * fringe[channel]
				squash := (y - groundY) / 0.28
				ring := splashAbs(splashHypot(x, squash) - radius)
				spread := ring / 0.045
				light[channel] += splashExp(-(spread * spread)) * groundFade
				radius = airRadius * fringe[channel]
				ring = splashAbs(splashHypot(x, y-0.2) - radius)
				spread = ring / 0.025
				light[channel] += splashExp(-(spread * spread)) * airFade
			}
			canvas.add(px, py, Rgb{light[0], light[1] * 0.9, light[2] * 1.1})
		}
	}
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
	since := t - impact
	if !(since >= 0.0 && since < 1.1) {
		return
	}
	for spark := range sparkCount {
		rng := newRng(0x5BA4 ^ spark*0x2545_F491)
		heading := -math.Pi/2 + rng.between(-1.25, 1.25)
		speed := rng.between(1.0, 3.4)
		life := rng.between(0.35, 1.05)
		start := rng.between(-0.38, 0.38)
		if since > life {
			continue
		}
		warmth := 1.0 - since/life
		vx, vy := splashCos(heading)*speed, splashSin(heading)*speed
		for _, trail := range [3]struct{ step, weight float32 }{{0.0, 1.0}, {0.012, 0.6}, {0.024, 0.3}} {
			age := since - trail.step
			if age < 0.0 {
				break
			}
			position := [2]float32{
				start + vx*age,
				groundY - 0.02 + vy*age + 0.5*gravity*age*age,
			}
			canvas.splat(view.pixel(position), scaled(ember(warmth), 2.2*trail.weight))
		}
	}
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
