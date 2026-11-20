// Port of clap_builder 4.6.0 src/util/color.rs

package clap

import "github.com/BrokkAi/mjolnir/go/rs"

// ColorChoice is clap::ColorChoice: represents the color preferences for
// program output.
type ColorChoice uint8

const (
	// ColorChoice_Auto enables colored output only when the output is going
	// to a terminal or TTY.
	//
	// NOTE: This is the default behavior of clap.
	ColorChoice_Auto ColorChoice = iota

	// ColorChoice_Always enables colored output regardless of whether or not
	// the output is going to a terminal/TTY.
	ColorChoice_Always

	// ColorChoice_Never disables colored output no matter if the output is
	// going to a terminal/TTY, or not.
	ColorChoice_Never
)

var _ = rs.RegisterEnum[ColorChoice]("ColorChoice",
	rs.Const(ColorChoice_Auto, "Auto"),
	rs.Const(ColorChoice_Always, "Always"),
	rs.Const(ColorChoice_Never, "Never"),
)

// ColorChoicePossibleValues is ColorChoice::possible_values: report all
// possible values.
func ColorChoicePossibleValues() []BuilderPossibleValue {
	var values []BuilderPossibleValue
	for _, variant := range ColorChoice(0).ValueVariants() {
		if value, ok := variant.ToPossibleValue().Get(); ok {
			values = append(values, value)
		}
	}
	return values
}

// String is `impl Display for ColorChoice`.
func (c ColorChoice) String() string {
	pv := c.ToPossibleValue().Expect("no values are skipped")
	return pv.GetName()
}

// FmtDisplay is `impl Display for ColorChoice`.
func (c ColorChoice) FmtDisplay(f *rs.Formatter) error { return f.Pad(c.String()) }

// ColorChoiceFromStr is `impl FromStr for ColorChoice`.
func ColorChoiceFromStr(s string) (ColorChoice, error) {
	for _, variant := range ColorChoice(0).ValueVariants() {
		value := variant.ToPossibleValue().Unwrap()
		if value.Matches(s, false) {
			return variant, nil
		}
	}
	return 0, stringError("invalid variant: " + s)
}

// ValueVariants is `impl ValueEnum for ColorChoice`.
func (ColorChoice) ValueVariants() []ColorChoice {
	return []ColorChoice{ColorChoice_Auto, ColorChoice_Always, ColorChoice_Never}
}

// ToPossibleValue is `impl ValueEnum for ColorChoice`.
func (c ColorChoice) ToPossibleValue() rs.Option[BuilderPossibleValue] {
	switch c {
	case ColorChoice_Auto:
		return rs.Some(BuilderPossibleValueNew("auto"))
	case ColorChoice_Always:
		return rs.Some(BuilderPossibleValueNew("always"))
	case ColorChoice_Never:
		return rs.Some(BuilderPossibleValueNew("never"))
	default:
		panic(rs.Unreachable())
	}
}

// stringError is a Rust `String` used as an error (`Err(String)`): its
// Display form is the string.
type stringError string

func (e stringError) Error() string { return string(e) }
