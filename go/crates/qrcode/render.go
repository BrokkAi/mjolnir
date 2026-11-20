// Port of qrcode 0.14.1 src/render/mod.rs

package qrcode

import "github.com/BrokkAi/mjolnir/go/rs"

// Render a QR code into image.
//
// Only the string pixels (render/string.rs) are ported: the `image`, `svg`
// and `pic` features are off in Mjolnir's build, and nothing uses the
// Dense1x2 pixel of render/unicode.rs.

//------------------------------------------------------------------------------
//{{{ Renderer

// RenderRenderer is qrcode::render::Renderer for a string pixel P: a QR code
// renderer. This is a builder type which converts a bool-vector into an
// image.
type RenderRenderer[P RenderStringElement] struct {
	content      []Color
	modulesCount uint32 // <- we call it `modules_count` here to avoid ambiguity of `width`.
	quietZone    uint32
	moduleSize   [2]uint32

	darkColor    P
	lightColor   P
	hasQuietZone bool
}

// RenderRendererNew is Renderer::new: creates a new renderer.
//
// # Panics
//
// Panics if the length of `content` is not exactly `modulesCount *
// modulesCount`.
func RenderRendererNew[P RenderStringElement](content []Color, modulesCount int, quietZone uint32) *RenderRenderer[P] {
	if modulesCount*modulesCount != len(content) {
		rs.PanicAt(1, "assertion failed: modules_count * modules_count == content.len()")
	}
	unitWidth, unitHeight := renderStringDefaultUnitSize()
	return &RenderRenderer[P]{
		content:      content,
		modulesCount: asU32Usize(modulesCount),
		quietZone:    quietZone,
		moduleSize:   [2]uint32{unitWidth, unitHeight},
		darkColor:    renderStringDefaultColor[P](Color_Dark),
		lightColor:   renderStringDefaultColor[P](Color_Light),
		hasQuietZone: true,
	}
}

// DarkColor is Renderer::dark_color: sets color of a dark module. Default is
// opaque black.
func (r *RenderRenderer[P]) DarkColor(color P) *RenderRenderer[P] {
	r.darkColor = color
	return r
}

// LightColor is Renderer::light_color: sets color of a light module. Default
// is opaque white.
func (r *RenderRenderer[P]) LightColor(color P) *RenderRenderer[P] {
	r.lightColor = color
	return r
}

// QuietZone is Renderer::quiet_zone: whether to include the quiet zone in
// the generated image.
func (r *RenderRenderer[P]) QuietZone(hasQuietZone bool) *RenderRenderer[P] {
	r.hasQuietZone = hasQuietZone
	return r
}

// ModuleSize is Renderer::module_size: sets the size of each module in
// pixels. Default is 8px.
//
// Deprecated: since qrcode 0.4.0, use `.ModuleDimensions(width, width)`.
func (r *RenderRenderer[P]) ModuleSize(width uint32) *RenderRenderer[P] {
	return r.ModuleDimensions(width, width)
}

// ModuleDimensions is Renderer::module_dimensions: sets the size of each
// module in pixels. Default is 8×8.
func (r *RenderRenderer[P]) ModuleDimensions(width uint32, height uint32) *RenderRenderer[P] {
	r.moduleSize = [2]uint32{max(width, 1), max(height, 1)}
	return r
}

// MinWidth is Renderer::min_width.
//
// Deprecated: since qrcode 0.4.0, use `.MinDimensions(width, width)`.
func (r *RenderRenderer[P]) MinWidth(width uint32) *RenderRenderer[P] {
	return r.MinDimensions(width, width)
}

// MinDimensions is Renderer::min_dimensions: sets the minimum total image
// size in pixels, including the quiet zone if applicable. The renderer will
// try to find the dimension as small as possible, such that each module in
// the QR code has uniform size (no distortion).
//
// For instance, a version 1 QR code has 19 modules across including the
// quiet zone. If we request an image of size ≥200×200, we get that each
// module's size should be 11×11, so the actual image size will be 209×209.
func (r *RenderRenderer[P]) MinDimensions(width uint32, height uint32) *RenderRenderer[P] {
	quietZone := uint32(0)
	if r.hasQuietZone {
		quietZone = 2 * r.quietZone
	}
	widthInModules := r.modulesCount + quietZone
	unitWidth := (width + widthInModules - 1) / widthInModules
	unitHeight := (height + widthInModules - 1) / widthInModules
	return r.ModuleDimensions(unitWidth, unitHeight)
}

// MaxDimensions is Renderer::max_dimensions: sets the maximum total image
// size in pixels, including the quiet zone if applicable. The renderer will
// try to find the dimension as large as possible, such that each module in
// the QR code has uniform size (no distortion).
//
// For instance, a version 1 QR code has 19 modules across including the
// quiet zone. If we request an image of size ≤200×200, we get that each
// module's size should be 10×10, so the actual image size will be 190×190.
//
// The module size is at least 1×1, so if the restriction is too small, the
// final image *can* be larger than the input.
func (r *RenderRenderer[P]) MaxDimensions(width uint32, height uint32) *RenderRenderer[P] {
	quietZone := uint32(0)
	if r.hasQuietZone {
		quietZone = 2 * r.quietZone
	}
	widthInModules := r.modulesCount + quietZone
	unitWidth := width / widthInModules
	unitHeight := height / widthInModules
	return r.ModuleDimensions(unitWidth, unitHeight)
}

// ToImage is Renderer::to_image: renders the QR code into an image.
//
// Deprecated: since qrcode 0.4.0, renamed to Build to de-emphasize the
// image connection.
func (r *RenderRenderer[P]) ToImage() string {
	return r.Build()
}

// Build is Renderer::build: renders the QR code into an image, here a
// string.
func (r *RenderRenderer[P]) Build() string {
	w := r.modulesCount
	qz := uint32(0)
	if r.hasQuietZone {
		qz = r.quietZone
	}
	width := w + 2*qz

	mw, mh := r.moduleSize[0], r.moduleSize[1]
	realWidth := width * mw
	realHeight := width * mh

	canvas := renderStringCanvasNew(realWidth, realHeight, r.darkColor, r.lightColor)
	i := 0
	for y := uint32(0); y < width; y++ {
		for x := uint32(0); x < width; x++ {
			if qz <= x && x < w+qz && qz <= y && y < w+qz {
				if r.content[i] != Color_Light {
					canvas.drawDarkRect(x*mw, y*mh, mw, mh)
				}
				i++
			}
		}
	}

	return canvas.intoImage()
}

//}}}
