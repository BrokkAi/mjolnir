// Port of qrcode 0.14.1 src/render/string.rs (tests)

package qrcode

import "testing"

func TestRenderString_test_render_to_string(t *testing.T) {
	colors := []Color{Color_Dark, Color_Light, Color_Light, Color_Dark}
	image := RenderRendererNew[rune](colors, 2, 1).Build()
	qrAssertEq(t, image, "    \n █  \n  █ \n    ")

	image2 := RenderRendererNew[string](colors, 2, 1).LightColor("A").DarkColor("!B!").ModuleDimensions(2, 2).Build()

	qrAssertEq(t,
		image2,
		"AAAAAAAA\n"+
			"AAAAAAAA\n"+
			"AA!B!!B!AAAA\n"+
			"AA!B!!B!AAAA\n"+
			"AAAA!B!!B!AA\n"+
			"AAAA!B!!B!AA\n"+
			"AAAAAAAA\n"+
			"AAAAAAAA",
	)
}
