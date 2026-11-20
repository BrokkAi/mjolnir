// Port of clap_builder 4.6.0 src/output/fmt.rs

package clap

import (
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rsio"
)

type outputFmtStream uint8

const (
	outputFmtStream_Stdout outputFmtStream = iota
	outputFmtStream_Stderr
)

var _ = rs.RegisterEnum[outputFmtStream]("Stream",
	rs.Const(outputFmtStream_Stdout, "Stdout"),
	rs.Const(outputFmtStream_Stderr, "Stderr"),
)

type outputFmtColorizer struct {
	stream    outputFmtStream
	colorWhen ColorChoice
	content   BuilderStyledStr
}

func outputFmtColorizerNew(stream outputFmtStream, colorWhen ColorChoice) outputFmtColorizer {
	return outputFmtColorizer{
		stream:    stream,
		colorWhen: colorWhen,
		content:   BuilderStyledStrNew(),
	}
}

func (c outputFmtColorizer) withContent(content BuilderStyledStr) outputFmtColorizer {
	c.content = content
	return c
}

// print writes the content through anstream's AutoStream, which keeps or
// strips the styling as the color choice and the stream decide.
func (c outputFmtColorizer) print() error {
	var colorWhen anstreamColorChoice
	switch c.colorWhen {
	case ColorChoice_Always:
		colorWhen = anstreamColorChoice_Always
	case ColorChoice_Auto:
		colorWhen = anstreamColorChoice_Auto
	case ColorChoice_Never:
		colorWhen = anstreamColorChoice_Never
	default:
		panic(rs.Unreachable(c.colorWhen))
	}

	switch c.stream {
	case outputFmtStream_Stderr:
		lock := rsio.Stderr().Lock()
		defer lock.Drop()
		stderr := anstreamAutoStreamNew(lock, colorWhen)
		return c.content.writeTo(stderr)
	case outputFmtStream_Stdout:
		lock := rsio.Stdout().Lock()
		defer lock.Drop()
		stdout := anstreamAutoStreamNew(lock, colorWhen)
		return c.content.writeTo(stdout)
	default:
		panic(rs.Unreachable(c.stream))
	}
}

// String is `impl Display for Colorizer`.
func (c outputFmtColorizer) String() string {
	return c.content.String()
}

// writeTo is StyledStr::write_to.
func (s BuilderStyledStr) writeTo(buffer *anstreamAutoStream) error {
	return buffer.writeAll([]byte(s))
}
