// Port of anstream 1.0.0 src/auto.rs and src/strip.rs (StripStream),
// colorchoice 1.0.5 src/lib.rs, and anstyle-query 1.1.5 src/lib.rs

package clap

import (
	"os"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// anstreamColorChoice is anstream::ColorChoice (colorchoice::ColorChoice).
type anstreamColorChoice uint8

const (
	// Use colors if the output device appears to support them
	anstreamColorChoice_Auto anstreamColorChoice = iota
	// Like `Always`, except it never tries to use anything other than
	// emitting ANSI color codes.
	anstreamColorChoice_AlwaysAnsi
	// Try very hard to emit colors.
	//
	// This includes emitting ANSI colors on Windows if the console API is
	// unavailable.
	anstreamColorChoice_Always
	// Never emit colors.
	anstreamColorChoice_Never
)

// anstreamColorChoiceGlobal is ColorChoice::global: the process-wide
// override, which clap never sets, so it stays at its default.
func anstreamColorChoiceGlobal() anstreamColorChoice {
	return anstreamColorChoice_Auto
}

// anstreamRawStream is anstream's RawStream: a stream that is written to
// and can say whether it is a terminal. rsio's standard output and error
// locks are ones.
type anstreamRawStream interface {
	Write(buf []byte) (int, error)
	WriteAll(buf []byte) error
	IsTerminal() bool
}

type anstreamStreamInner uint8

const (
	anstreamStreamInner_PassThrough anstreamStreamInner = iota
	anstreamStreamInner_Strip
)

// anstreamAutoStream is anstream::AutoStream: automatically pass through,
// strip, or render ANSI escape codes.
type anstreamAutoStream struct {
	inner anstreamStreamInner
	raw   anstreamRawStream
	// state is StripStream's: used by the Strip variant.
	state stripBytes
}

// anstreamAutoStreamNew is AutoStream::new: runtime control over styling
// behavior.
func anstreamAutoStreamNew(raw anstreamRawStream, choice anstreamColorChoice) *anstreamAutoStream {
	switch choice {
	case anstreamColorChoice_Auto:
		return anstreamAutoStreamAuto(raw)
	case anstreamColorChoice_AlwaysAnsi:
		return anstreamAutoStreamAlwaysAnsi(raw)
	case anstreamColorChoice_Always:
		return anstreamAutoStreamAlways(raw)
	case anstreamColorChoice_Never:
		return anstreamAutoStreamNever(raw)
	default:
		panic(rs.Unreachable(choice))
	}
}

// anstreamAutoStreamAuto is AutoStream::auto: auto-adapt for the stream's
// capabilities.
func anstreamAutoStreamAuto(raw anstreamRawStream) *anstreamAutoStream {
	choice := anstreamChoice(raw)
	if rs.DebugAssertions && choice == anstreamColorChoice_Auto {
		rs.PanicAt(1, "assertion `left != right` failed\n  left: Auto\n right: Auto")
	}
	return anstreamAutoStreamNew(raw, choice)
}

// anstreamAutoStreamAlwaysAnsi is AutoStream::always_ansi: force ANSI
// escape codes to be passed through as-is, no matter what the inner
// `Write` supports.
func anstreamAutoStreamAlwaysAnsi(raw anstreamRawStream) *anstreamAutoStream {
	if raw.IsTerminal() {
		_ = anstyleQueryEnableAnsiColors()
	}
	return anstreamAutoStreamAlwaysAnsi_(raw)
}

func anstreamAutoStreamAlwaysAnsi_(raw anstreamRawStream) *anstreamAutoStream {
	return &anstreamAutoStream{inner: anstreamStreamInner_PassThrough, raw: raw, state: stripBytesNew()}
}

// anstreamAutoStreamAlways is AutoStream::always: force color, no matter
// what the inner `Write` supports.
func anstreamAutoStreamAlways(raw anstreamRawStream) *anstreamAutoStream {
	if rs.CfgWindows {
		useWincon := raw.IsTerminal() &&
			!anstyleQueryEnableAnsiColors().UnwrapOr(true) &&
			!anstyleQueryTermSupportsAnsiColor()
		if useWincon {
			return anstreamAutoStreamWincon(raw)
		}
		return anstreamAutoStreamAlwaysAnsi_(raw)
	}
	return anstreamAutoStreamAlwaysAnsi(raw)
}

// anstreamAutoStreamNever is AutoStream::never: only pass printable data
// to the inner `Write`.
func anstreamAutoStreamNever(raw anstreamRawStream) *anstreamAutoStream {
	return &anstreamAutoStream{inner: anstreamStreamInner_Strip, raw: raw, state: stripBytesNew()}
}

// anstreamAutoStreamWincon is AutoStream::wincon with the `wincon` feature
// on Windows: a legacy console without virtual terminal processing, where
// anstream turns the escape codes into console API calls.
func anstreamAutoStreamWincon(raw anstreamRawStream) *anstreamAutoStream {
	panic(rs.Todo("anstream::AutoStream::wincon: styled output to a legacy Windows console without virtual terminal processing"))
}

// anstreamChoice is anstream's `choice`: what Auto resolves to for raw.
func anstreamChoice(raw anstreamRawStream) anstreamColorChoice {
	choice := anstreamColorChoiceGlobal()
	switch choice {
	case anstreamColorChoice_Auto:
		clicolor := anstyleQueryClicolor()
		clicolorEnabled := clicolor.UnwrapOr(false)
		clicolorDisabled := !clicolor.UnwrapOr(true)
		if anstyleQueryNoColor() {
			return anstreamColorChoice_Never
		} else if anstyleQueryClicolorForce() {
			return anstreamColorChoice_Always
		} else if clicolorDisabled {
			return anstreamColorChoice_Never
		} else if raw.IsTerminal() &&
			(anstyleQueryTermSupportsColor() ||
				clicolorEnabled ||
				anstyleQueryIsCi()) {
			return anstreamColorChoice_Always
		}
		return anstreamColorChoice_Never
	case anstreamColorChoice_AlwaysAnsi, anstreamColorChoice_Always, anstreamColorChoice_Never:
		return choice
	default:
		panic(rs.Unreachable(choice))
	}
}

// writeAll is `Write::write_all` of AutoStream.
func (s *anstreamAutoStream) writeAll(buf []byte) error {
	switch s.inner {
	case anstreamStreamInner_PassThrough:
		return s.raw.WriteAll(buf)
	case anstreamStreamInner_Strip:
		// StripStream's write_all
		for _, printable := range s.state.stripNext(buf) {
			if err := s.raw.WriteAll(printable); err != nil {
				return err
			}
		}
		return nil
	default:
		panic(rs.Unreachable(s.inner))
	}
}

// anstyleQueryClicolor is anstyle_query::clicolor: check
// [CLICOLOR] status.
//
//   - When `true`, ANSI colors are supported and should be used when the
//     program isn't piped, similar to `term_supports_color`
//   - When `false`, don’t output ANSI color escape codes, similar to
//     `no_color`
//
// See also:
//   - terminfo crate
//   - term_supports_color
func anstyleQueryClicolor() rs.Option[bool] {
	value, ok := os.LookupEnv("CLICOLOR")
	if !ok {
		return rs.None[bool]()
	}
	return rs.Some(value != "0")
}

// anstyleQueryClicolorForce is anstyle_query::clicolor_force: check
// [CLICOLOR_FORCE] status.
//
// ANSI colors should be enabled no matter what.
func anstyleQueryClicolorForce() bool {
	value, _ := os.LookupEnv("CLICOLOR_FORCE")
	return anstyleQueryNonEmpty(value)
}

// anstyleQueryNoColor is anstyle_query::no_color: check [NO_COLOR]
// status.
//
// When `true`, should prevent the addition of ANSI color.
//
// User-level configuration files and per-instance command-line arguments
// should override [NO_COLOR]. A user should be able to export
// `$NO_COLOR` in their shell configuration file as a default, but
// configure a specific program in its configuration file to specifically
// enable color.
func anstyleQueryNoColor() bool {
	value, _ := os.LookupEnv("NO_COLOR")
	return anstyleQueryNonEmpty(value)
}

// anstyleQueryTruecolor is anstyle_query::truecolor: check `COLORTERM`
// for truecolor support.
func anstyleQueryTruecolor() bool {
	value, _ := os.LookupEnv("COLORTERM")
	return value == "truecolor" || value == "24bit"
}

// anstyleQueryIsCi is anstyle_query::is_ci: report whether this is running
// in CI.
//
// CI is a common environment where, despite being piped, ansi color codes
// are supported.
//
// This is not as exhaustive as you'd find in a crate like `is_ci` but it
// should work in enough cases.
func anstyleQueryIsCi() bool {
	// Assuming its CI based on presence because who would be setting `CI=false`?
	//
	// This makes it easier to all of the potential values when considering our known values:
	// - Gitlab and Github set it to `true`
	// - Woodpecker sets it to `woodpecker`
	_, ok := os.LookupEnv("CI")
	return ok
}

func anstyleQueryNonEmpty(value string) bool {
	return value != ""
}
