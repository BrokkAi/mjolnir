// Port of mj-tui/src/splash/hammer.rs.
//
// Mjölnir as a signed distance field: a flared Uru head with a knotwork
// band, a short leather-wrapped haft, a pommel and its lanyard loop.
package tui

import "math"

// The hammer's constants carry the module's name in the flat package
// (Rust's REST_Y is hammerRestY, and so on).
const (
	// hammerRestY is where the head's centre rests once it has struck.
	hammerRestY float32 = 0.20
	// hammerDropFrom is how far above the scene the hammer starts falling.
	hammerDropFrom float32 = -2.4
	// hammerTurns is how many times it spins on the way down.
	hammerTurns float32 = 2.25
	// hammerReach bounds everything drawn: all of it lies within this
	// distance of the pivot.
	hammerReach float32 = 1.05
)

// hammerPivot is the point the hammer spins around: its centre of mass,
// inside the head.
var hammerPivot = [2]float32{0.0, -0.15}

// hammerBlur holds the earlier instants averaged into each falling frame,
// for motion blur.
var hammerBlur = [3]float32{0.0, 0.012, 0.024}

// hammerTowardLight says light comes from the upper left.
var hammerTowardLight = [2]float32{-0.55, -0.835}

var hammerRim = Rgb{0.35, 0.65, 1.25}

// hammerCrown is the local point that lightning strikes: the top of the
// lanyard loop.
var hammerCrown = [2]float32{0.0, -1.045}

// hammerHead is the head's half extents, for arcs that crawl over it.
var hammerHead = [2]float32{0.42, 0.2}

// Pose is the hammer's placement in the scene.
type Pose struct {
	origin [2]float32
	angle  float32
}

// poseAt is the hammer's placement at scene time t, once it has appeared.
// It falls under gravity while its spin unwinds, and lands upright.
func poseAt(t float32) (Pose, bool) {
	if t < descentStart {
		return Pose{}, false
	}
	progress := min((t-descentStart)/(impact-descentStart), 1.0)
	var y float32
	if progress < 1.0 {
		y = hammerDropFrom + (hammerRestY-hammerDropFrom)*progress*progress
	} else {
		since := t - impact
		y = hammerRestY - 0.04*splashAbs(splashSin(since*30.0))*splashExp(-since*10.0)
	}
	return Pose{
		origin: [2]float32{0.0, y},
		angle:  hammerTurns * splashTau * splashPow(1.0-progress, 1.7),
	}, true
}

func (p Pose) toWorld(local [2]float32) [2]float32 {
	rotated := rotate([2]float32{local[0] - hammerPivot[0], local[1] - hammerPivot[1]}, p.angle)
	return [2]float32{p.origin[0] + hammerPivot[0] + rotated[0], p.origin[1] + hammerPivot[1] + rotated[1]}
}

func (p Pose) toLocal(world [2]float32) [2]float32 {
	rotated := rotate([2]float32{
		world[0] - p.origin[0] - hammerPivot[0],
		world[1] - p.origin[1] - hammerPivot[1],
	}, -p.angle)
	return [2]float32{rotated[0] + hammerPivot[0], rotated[1] + hammerPivot[1]}
}

// Part is which part of the hammer a point is nearest.
type Part int

const (
	PartHead Part = iota
	PartHaft
	PartMetal
)

func splashHypot(x, y float32) float32 {
	return float32(math.Hypot(float64(x), float64(y)))
}

func roundBox(point [2]float32, half [2]float32, radius float32) float32 {
	dx, dy := float32(math.Abs(float64(point[0])))-half[0]+radius, float32(math.Abs(float64(point[1])))-half[1]+radius
	return splashHypot(max(dx, 0.0), max(dy, 0.0)) + min(max(dx, dy), 0.0) - radius
}

func sdf(point [2]float32) (float32, Part) {
	x, y := point[0], point[1]
	body := roundBox([2]float32{x, y}, [2]float32{0.34, 0.185}, 0.03)
	faces := roundBox([2]float32{float32(math.Abs(float64(x))) - 0.375, y}, [2]float32{0.05, 0.205}, 0.02)
	head := min(body, faces)
	haft := roundBox([2]float32{x, y + 0.51}, [2]float32{0.048, 0.31}, 0.02)
	pommel := splashHypot(x, y+0.86) - 0.068
	lanyard := float32(math.Abs(float64(splashHypot(x, y+0.975)-0.055))) - 0.014
	metal := min(pommel, lanyard)
	switch {
	case head <= haft && head <= metal:
		return head, PartHead
	case haft <= metal:
		return haft, PartHaft
	default:
		return metal, PartMetal
	}
}

func normal(point [2]float32) [2]float32 {
	const epsilon float32 = 0.004
	dx := sdfDistance(sdf([2]float32{point[0] + epsilon, point[1]})) - sdfDistance(sdf([2]float32{point[0] - epsilon, point[1]}))
	dy := sdfDistance(sdf([2]float32{point[0], point[1] + epsilon})) - sdfDistance(sdf([2]float32{point[0], point[1] - epsilon}))
	length := max(splashHypot(dx, dy), 1e-6)
	return [2]float32{dx / length, dy / length}
}

// sdfDistance is the distance half of an sdf result.
func sdfDistance(distance float32, _ Part) float32 {
	return distance
}

// uru is brushed Uru metal: cool steel, lighter toward the top, with fine
// streaks.
func uru(point [2]float32) Rgb {
	y := point[1]
	streak := hash2(int32(math.Floor(float64(y*90.0))), 7) * 0.12
	tone := 0.30 + 0.28*min(max(0.5-y*0.9, 0.0), 1.0) + streak
	return Rgb{tone * 0.92, tone * 0.98, tone * 1.08}
}

// knotwork is the knotwork band round the middle of the head: a diagonal
// lattice of grooves framed by two rings.
func knotwork(point [2]float32) float32 {
	x, y := point[0], point[1]
	absX := float32(math.Abs(float64(x)))
	if absX > 0.165 || float32(math.Abs(float64(y))) > 0.17 {
		return 1.0
	}
	frame := float32(math.Abs(float64(absX - 0.15)))
	lattice := func(value float32) float32 {
		return float32(math.Abs(float64(value - float32(math.Floor(float64(value))) - 0.5)))
	}
	groove := min(lattice((x+y)*9.0), lattice((x-y)*9.0), frame*4.0)
	return 0.5 + 0.5*smoothstep(0.02, 0.09, groove)
}

func leather(point [2]float32) Rgb {
	x, y := point[0], point[1]
	wrap := y*16.0 + x*5.0
	stripe := smoothstep(0.42, 0.58, wrap-float32(math.Floor(float64(wrap))))
	base := mix(Rgb{0.42, 0.24, 0.12}, Rgb{0.20, 0.11, 0.05}, stripe)
	across := min(max(x/0.05, -1.0), 1.0)
	roundness := float32(math.Cos(float64(across * (math.Pi / 2))))
	return scaled(base, 0.45+0.75*roundness-0.15*across)
}

func shade(local [2]float32, distance float32, part Part, pose Pose, charge float32) Rgb {
	depth := -distance
	turned := rotate(normal(local), pose.angle)
	lit := turned[0]*hammerTowardLight[0] + turned[1]*hammerTowardLight[1]
	var base Rgb
	switch part {
	case PartHead:
		base = scaled(uru(local), knotwork(local))
	case PartHaft:
		base = leather(local)
	case PartMetal:
		base = uru(local)
	}
	bevel := smoothstep(0.045, 0.0, depth)
	litColor := scaled(base, 1.0+bevel*lit*0.9)
	rim := smoothstep(0.03, 0.0, depth) * (0.25 + charge)
	return mix(litColor, hammerRim, min(rim*0.6, 1.0))
}

// hammerDraw is Rust's hammer::draw: it paints the hammer over the scene,
// with motion blur while it falls and a blue halo whose strength follows
// charge.
func hammerDraw(canvas *Canvas, view *View, t, charge float32) {
	blur := hammerBlur[:]
	if t >= impact {
		blur = hammerBlur[:1]
	}
	var poses []Pose
	for _, delay := range blur {
		if pose, ok := poseAt(t - delay); ok {
			poses = append(poses, pose)
		}
	}
	if len(poses) == 0 {
		return
	}
	left := float32(math.MaxFloat32)
	top := float32(math.MaxFloat32)
	right := float32(-math.MaxFloat32)
	bottom := float32(-math.MaxFloat32)
	for _, pose := range poses {
		centre := view.pixel(pose.toWorld(hammerPivot))
		reach := hammerReach*view.scale + 2.0
		left = min(left, centre[0]-reach)
		top = min(top, centre[1]-reach)
		right = max(right, centre[0]+reach)
		bottom = max(bottom, centre[1]+reach)
	}
	columnStart, columnEnd := asUsize(max(left, 0.0)), min(asUsize(max(right, 0.0)), canvas.width)
	rowStart, rowEnd := asUsize(max(top, 0.0)), min(asUsize(max(bottom, 0.0)), canvas.height)
	samples := float32(len(poses))
	for y := rowStart; y < rowEnd; y++ {
		for x := columnStart; x < columnEnd; x++ {
			point := view.scene(x, y)
			var color Rgb
			var coverage, halo float32
			for index, pose := range poses {
				local := pose.toLocal(point)
				distance, part := sdf(local)
				if index == 0 && distance > 0.0 {
					halo = splashExp(-distance * 16.0)
				}
				cover := splashClamp(0.5-distance*view.scale, 0.0, 1.0)
				if cover > 0.0 {
					shaded := shade(local, distance, part, pose, charge)
					for channel := range 3 {
						color[channel] += shaded[channel] * cover
					}
					coverage += cover
				}
			}
			alpha := coverage / samples
			under := canvas.get(x, y)
			glow := halo * charge * 0.5 * (1.0 - alpha)
			var out Rgb
			for channel := range 3 {
				out[channel] = under[channel]*(1.0-alpha) + color[channel]/samples + hammerRim[channel]*glow
			}
			canvas.set(x, y, out)
		}
	}
}
