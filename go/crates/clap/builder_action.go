// Port of clap_builder 4.6.0 src/builder/action.rs

package clap

import (
	"reflect"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// ArgAction is clap::ArgAction: behavior of arguments when they are
// encountered while parsing.
type ArgAction uint8

const (
	// ArgAction_Set: when encountered, store the associated value(s) in
	// ArgMatches.
	//
	// NOTE: If the argument has previously been seen, it will result in a
	// ArgumentConflict unless Command::args_override_self(true) is set.
	ArgAction_Set ArgAction = iota
	// ArgAction_Append: when encountered, store the associated value(s) in
	// ArgMatches.
	ArgAction_Append
	// ArgAction_SetTrue: when encountered, act as if `"true"` was
	// encountered on the command-line.
	//
	// If no default_value is set, it will be `false`.
	//
	// No value is allowed. To optionally accept a value, see
	// Arg::default_missing_value.
	//
	// NOTE: If the argument has previously been seen, it will result in a
	// ArgumentConflict unless Command::args_override_self(true) is set.
	ArgAction_SetTrue
	// ArgAction_SetFalse: when encountered, act as if `"false"` was
	// encountered on the command-line.
	//
	// If no default_value is set, it will be `true`.
	//
	// No value is allowed. To optionally accept a value, see
	// Arg::default_missing_value.
	//
	// NOTE: If the argument has previously been seen, it will result in a
	// ArgumentConflict unless Command::args_override_self(true) is set.
	ArgAction_SetFalse
	// ArgAction_Count: when encountered, increment a `u8` counter starting
	// from `0`.
	//
	// If no default_value is set, it will be `0`.
	//
	// No value is allowed. To optionally accept a value, see
	// Arg::default_missing_value.
	ArgAction_Count
	// ArgAction_Help: when encountered, display Command::print_help.
	//
	// Depending on the flag, Command::print_long_help may be shown.
	ArgAction_Help
	// ArgAction_HelpShort: when encountered, display Command::print_help.
	ArgAction_HelpShort
	// ArgAction_HelpLong: when encountered, display Command::print_long_help.
	ArgAction_HelpLong
	// ArgAction_Version: when encountered, display Command::version.
	//
	// Depending on the flag, Command::long_version may be shown.
	ArgAction_Version
)

var _ = rs.RegisterEnum[ArgAction]("ArgAction",
	rs.Const(ArgAction_Set, "Set"),
	rs.Const(ArgAction_Append, "Append"),
	rs.Const(ArgAction_SetTrue, "SetTrue"),
	rs.Const(ArgAction_SetFalse, "SetFalse"),
	rs.Const(ArgAction_Count, "Count"),
	rs.Const(ArgAction_Help, "Help"),
	rs.Const(ArgAction_HelpShort, "HelpShort"),
	rs.Const(ArgAction_HelpLong, "HelpLong"),
	rs.Const(ArgAction_Version, "Version"),
)

var argActionNames = [...]string{"Set", "Append", "SetTrue", "SetFalse", "Count", "Help", "HelpShort", "HelpLong", "Version"}

// debugString is the derived Debug form of ArgAction.
func (a ArgAction) debugString() string {
	if int(a) < len(argActionNames) {
		return argActionNames[a]
	}
	panic(rs.Unreachable())
}

// TakesValues is ArgAction::takes_values: returns whether this action
// accepts values on the command-line.
//
// default_values and env may still be processed.
func (a ArgAction) TakesValues() bool {
	switch a {
	case ArgAction_Set:
		return true
	case ArgAction_Append:
		return true
	case ArgAction_SetTrue:
		return false
	case ArgAction_SetFalse:
		return false
	case ArgAction_Count:
		return false
	case ArgAction_Help:
		return false
	case ArgAction_HelpShort:
		return false
	case ArgAction_HelpLong:
		return false
	case ArgAction_Version:
		return false
	default:
		panic(rs.Unreachable())
	}
}

func (a ArgAction) maxNumArgs() BuilderValueRange {
	switch a {
	case ArgAction_Set:
		return builderValueRangeFull
	case ArgAction_Append:
		return builderValueRangeFull
	case ArgAction_SetTrue:
		return builderValueRangeOptional
	case ArgAction_SetFalse:
		return builderValueRangeOptional
	case ArgAction_Count:
		return BuilderValueRangeEmpty
	case ArgAction_Help:
		return BuilderValueRangeEmpty
	case ArgAction_HelpShort:
		return BuilderValueRangeEmpty
	case ArgAction_HelpLong:
		return BuilderValueRangeEmpty
	case ArgAction_Version:
		return BuilderValueRangeEmpty
	default:
		panic(rs.Unreachable())
	}
}

func (a ArgAction) defaultNumArgs() BuilderValueRange {
	switch a {
	case ArgAction_Set:
		return BuilderValueRangeSingle
	case ArgAction_Append:
		return BuilderValueRangeSingle
	case ArgAction_SetTrue:
		return BuilderValueRangeEmpty
	case ArgAction_SetFalse:
		return BuilderValueRangeEmpty
	case ArgAction_Count:
		return BuilderValueRangeEmpty
	case ArgAction_Help:
		return BuilderValueRangeEmpty
	case ArgAction_HelpShort:
		return BuilderValueRangeEmpty
	case ArgAction_HelpLong:
		return BuilderValueRangeEmpty
	case ArgAction_Version:
		return BuilderValueRangeEmpty
	default:
		panic(rs.Unreachable())
	}
}

func (a ArgAction) defaultValue() rs.Option[string] {
	switch a {
	case ArgAction_Set:
		return rs.None[string]()
	case ArgAction_Append:
		return rs.None[string]()
	case ArgAction_SetTrue:
		return rs.Some("false")
	case ArgAction_SetFalse:
		return rs.Some("true")
	case ArgAction_Count:
		return rs.Some("0")
	case ArgAction_Help:
		return rs.None[string]()
	case ArgAction_HelpShort:
		return rs.None[string]()
	case ArgAction_HelpLong:
		return rs.None[string]()
	case ArgAction_Version:
		return rs.None[string]()
	default:
		panic(rs.Unreachable())
	}
}

func (a ArgAction) defaultMissingValue() rs.Option[string] {
	switch a {
	case ArgAction_Set:
		return rs.None[string]()
	case ArgAction_Append:
		return rs.None[string]()
	case ArgAction_SetTrue:
		return rs.Some("true")
	case ArgAction_SetFalse:
		return rs.Some("false")
	case ArgAction_Count:
		return rs.None[string]()
	case ArgAction_Help:
		return rs.None[string]()
	case ArgAction_HelpShort:
		return rs.None[string]()
	case ArgAction_HelpLong:
		return rs.None[string]()
	case ArgAction_Version:
		return rs.None[string]()
	default:
		panic(rs.Unreachable())
	}
}

func (a ArgAction) defaultValueParser() rs.Option[BuilderValueParser] {
	switch a {
	case ArgAction_Set:
		return rs.None[BuilderValueParser]()
	case ArgAction_Append:
		return rs.None[BuilderValueParser]()
	case ArgAction_SetTrue:
		return rs.Some(BuilderValueParserBool())
	case ArgAction_SetFalse:
		return rs.Some(BuilderValueParserBool())
	case ArgAction_Count:
		return rs.Some(ValueParserU8().IntoValueParser())
	case ArgAction_Help:
		return rs.None[BuilderValueParser]()
	case ArgAction_HelpShort:
		return rs.None[BuilderValueParser]()
	case ArgAction_HelpLong:
		return rs.None[BuilderValueParser]()
	case ArgAction_Version:
		return rs.None[BuilderValueParser]()
	default:
		panic(rs.Unreachable())
	}
}

func (a ArgAction) valueTypeId() rs.Option[utilAnyValueId] {
	switch a {
	case ArgAction_Set:
		return rs.None[utilAnyValueId]()
	case ArgAction_Append:
		return rs.None[utilAnyValueId]()
	case ArgAction_SetTrue:
		return rs.None[utilAnyValueId]()
	case ArgAction_SetFalse:
		return rs.None[utilAnyValueId]()
	case ArgAction_Count:
		return rs.Some(utilAnyValueIdOf[builderCountType]())
	case ArgAction_Help:
		return rs.None[utilAnyValueId]()
	case ArgAction_HelpShort:
		return rs.None[utilAnyValueId]()
	case ArgAction_HelpLong:
		return rs.None[utilAnyValueId]()
	case ArgAction_Version:
		return rs.None[utilAnyValueId]()
	default:
		panic(rs.Unreachable())
	}
}

// builderCountType is CountType: the type of ArgAction::Count's value.
type builderCountType = uint8

// utilAnyValueId is clap's AnyValueId: the type of a parsed value. In Go it
// is the value's reflect.Type and, for the Rust types that share a Go type
// (String, OsString and PathBuf are all string), the Rust type's name.
type utilAnyValueId struct {
	typeId   reflect.Type
	typeName string
}

func utilAnyValueIdOf[A any]() utilAnyValueId {
	t := reflect.TypeFor[A]()
	return utilAnyValueId{typeId: t, typeName: t.String()}
}

func utilAnyValueIdNamed[A any](rustName string) utilAnyValueId {
	return utilAnyValueId{typeId: reflect.TypeFor[A](), typeName: rustName}
}

// equal is `impl PartialEq for AnyValueId`: by type only.
func (id utilAnyValueId) equal(other utilAnyValueId) bool {
	return id.typeId == other.typeId
}

// debugString is `impl Debug for AnyValueId` in a debug build: the type's
// name. The Go name stands where Rust prints `std::any::type_name`.
func (id utilAnyValueId) debugString() string {
	return id.typeName
}
