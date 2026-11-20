// Port of clap_builder 4.6.0 src/parser/matches/value_source.rs

package clap

import "github.com/BrokkAi/mjolnir/go/rs"

// ParserValueSource is clap::parser::ValueSource: origin of the argument's
// value.
//
// The order is the one Rust derives: a later source outranks an earlier
// one.
type ParserValueSource uint8

const (
	// ParserValueSource_DefaultValue: value came Arg::default_value.
	ParserValueSource_DefaultValue ParserValueSource = iota
	// ParserValueSource_EnvVariable: value came Arg::env.
	ParserValueSource_EnvVariable
	// ParserValueSource_CommandLine: value was passed in on the
	// command-line.
	ParserValueSource_CommandLine
)

var _ = rs.RegisterEnum[ParserValueSource]("ValueSource",
	rs.Const(ParserValueSource_DefaultValue, "DefaultValue"),
	rs.Const(ParserValueSource_EnvVariable, "EnvVariable"),
	rs.Const(ParserValueSource_CommandLine, "CommandLine"),
)

func (s ParserValueSource) isExplicit() bool {
	return s != ParserValueSource_DefaultValue
}

// debugString is the derived Debug form: the variant's name.
func (s ParserValueSource) debugString() string {
	switch s {
	case ParserValueSource_DefaultValue:
		return "DefaultValue"
	case ParserValueSource_EnvVariable:
		return "EnvVariable"
	case ParserValueSource_CommandLine:
		return "CommandLine"
	default:
		panic(rs.Unreachable(s))
	}
}

// FmtDebug is the derived `impl Debug for ValueSource`.
func (s ParserValueSource) FmtDebug(f *rs.Formatter) error { return f.WriteStr(s.debugString()) }
