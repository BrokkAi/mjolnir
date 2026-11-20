// Port of toml_edit 0.20.2 src/ser/mod.rs

package tomledit

import (
	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// Serializing Rust structures into TOML.
//
// This module contains all the Serde support for serializing Rust structures into TOML.

// SerError is toml_edit::ser::Error: errors that can occur when deserializing
// a type. A value is a pointer to one of the variant structs.
type SerError interface {
	isSerError()
	// Error is `impl Display for Error`.
	Error() string
	// String is `impl Display for Error`.
	String() string
	// FmtDisplay is `impl Display for Error`.
	FmtDisplay(f *rs.Formatter) error
}

// SerError_UnsupportedType is Error::UnsupportedType: type could not be
// serialized to TOML.
type SerError_UnsupportedType struct{ F0 rs.Option[string] }

// SerError_OutOfRange is Error::OutOfRange: value was out of range for the
// given type.
type SerError_OutOfRange struct{ F0 rs.Option[string] }

// SerError_UnsupportedNone is Error::UnsupportedNone: `None` could not be
// serialized to TOML.
type SerError_UnsupportedNone struct{}

// SerError_KeyNotString is Error::KeyNotString: key was not convertible to
// `String` for serializing to TOML.
type SerError_KeyNotString struct{}

// SerError_DateInvalid is Error::DateInvalid: a serialized date was invalid.
type SerError_DateInvalid struct{}

// SerError_Custom is Error::Custom: other serialization error.
type SerError_Custom struct{ F0 string }

func (*SerError_UnsupportedType) isSerError() {}
func (*SerError_OutOfRange) isSerError()      {}
func (*SerError_UnsupportedNone) isSerError() {}
func (*SerError_KeyNotString) isSerError()    {}
func (*SerError_DateInvalid) isSerError()     {}
func (*SerError_Custom) isSerError()          {}

var _ = rs.RegisterEnum[SerError]("Error",
	rs.Variant[SerError_UnsupportedType]("UnsupportedType", rs.ShapeTuple),
	rs.Variant[SerError_OutOfRange]("OutOfRange", rs.ShapeTuple),
	rs.Variant[SerError_UnsupportedNone]("UnsupportedNone", rs.ShapeUnit),
	rs.Variant[SerError_KeyNotString]("KeyNotString", rs.ShapeUnit),
	rs.Variant[SerError_DateInvalid]("DateInvalid", rs.ShapeUnit),
	rs.Variant[SerError_Custom]("Custom", rs.ShapeTuple),
)

// serErrorCustom is Error::custom.
func serErrorCustom(msg string) SerError {
	return &SerError_Custom{F0: msg}
}

// SerErrorCustom is `impl serde::ser::Error for Error`: `Error::custom(msg)`.
func SerErrorCustom(msg string) SerError {
	return serErrorCustom(msg)
}

// serErrorFrom is how an error that comes back from a Serialize impl is this
// format's error. In Rust the impl makes a toml_edit::ser::Error itself,
// through `serde::ser::Error::custom`: here it returns serde's own error, or
// any other, and this is where the format takes it, as Error::Custom with
// the message. An error that already is a SerError passes unchanged.
func serErrorFrom(err error) SerError {
	if own, ok := err.(SerError); ok {
		return own
	}
	return serErrorCustom(err.Error())
}

func serErrorString(e SerError) string {
	switch e := e.(type) {
	case *SerError_UnsupportedType:
		if t, ok := e.F0.Get(); ok {
			return "unsupported " + t + " type"
		}
		return "unsupported rust type"
	case *SerError_OutOfRange:
		if t, ok := e.F0.Get(); ok {
			return "out-of-range value for " + t + " type"
		}
		return "out-of-range value"
	case *SerError_UnsupportedNone:
		return "unsupported None value"
	case *SerError_KeyNotString:
		return "map key was not a string"
	case *SerError_DateInvalid:
		return "a serialized date was invalid"
	case *SerError_Custom:
		return e.F0
	default:
		panic(rs.Unreachable(e))
	}
}

func (e *SerError_UnsupportedType) Error() string { return serErrorString(e) }
func (e *SerError_OutOfRange) Error() string      { return serErrorString(e) }
func (e *SerError_UnsupportedNone) Error() string { return serErrorString(e) }
func (e *SerError_KeyNotString) Error() string    { return serErrorString(e) }
func (e *SerError_DateInvalid) Error() string     { return serErrorString(e) }
func (e *SerError_Custom) Error() string          { return serErrorString(e) }

func (e *SerError_UnsupportedType) String() string { return serErrorString(e) }
func (e *SerError_OutOfRange) String() string      { return serErrorString(e) }
func (e *SerError_UnsupportedNone) String() string { return serErrorString(e) }
func (e *SerError_KeyNotString) String() string    { return serErrorString(e) }
func (e *SerError_DateInvalid) String() string     { return serErrorString(e) }
func (e *SerError_Custom) String() string          { return serErrorString(e) }

// The Display impl writes through `write!` and `str::fmt`, so the
// formatter's width and precision apply to the plain message.
func (e *SerError_UnsupportedType) FmtDisplay(f *rs.Formatter) error { return serErrorFmt(e, f) }
func (e *SerError_OutOfRange) FmtDisplay(f *rs.Formatter) error      { return serErrorFmt(e, f) }
func (e *SerError_UnsupportedNone) FmtDisplay(f *rs.Formatter) error { return serErrorFmt(e, f) }
func (e *SerError_KeyNotString) FmtDisplay(f *rs.Formatter) error    { return serErrorFmt(e, f) }
func (e *SerError_DateInvalid) FmtDisplay(f *rs.Formatter) error     { return serErrorFmt(e, f) }
func (e *SerError_Custom) FmtDisplay(f *rs.Formatter) error          { return serErrorFmt(e, f) }

func serErrorFmt(e SerError, f *rs.Formatter) error {
	switch e.(type) {
	case *SerError_UnsupportedType, *SerError_OutOfRange:
		// `write!(formatter, "unsupported {t} type")` ignores the flags.
		return f.WriteStr(serErrorString(e))
	default:
		// `"...".fmt(formatter)` and `s.fmt(formatter)` pad.
		return f.Pad(serErrorString(e))
	}
}

// SerErrorFromTomlError is `impl From<TomlError> for Error`.
func SerErrorFromTomlError(e *TomlError) SerError {
	return serErrorCustom(e.Error())
}

// TomlErrorFromSerError is `impl From<Error> for TomlError`.
func TomlErrorFromSerError(e SerError) *TomlError {
	return tomlErrorCustom(e.Error(), rs.None[Range]())
}

// SerToVec is toml_edit::ser::to_vec: serialize the given data structure as
// a TOML byte vector.
//
// Serialization can fail if `T`'s implementation of `Serialize` decides to
// fail, if `T` contains a map with non-string keys, or if `T` attempts to
// serialize an unsupported datatype such as an enum, tuple, or tuple struct.
func SerToVec(value any) ([]byte, error) {
	text, err := SerToString(value)
	if err != nil {
		return nil, err
	}
	return []byte(text), nil
}

// SerToString is toml_edit::ser::to_string: serialize the given data
// structure as a String of TOML.
//
// Serialization can fail if `T`'s implementation of `Serialize` decides to
// fail, if `T` contains a map with non-string keys, or if `T` attempts to
// serialize an unsupported datatype such as an enum, tuple, or tuple struct.
//
//	#[derive(Serialize)]
//	struct Config {
//	    database: Database,
//	}
//
//	#[derive(Serialize)]
//	struct Database {
//	    ip: String,
//	    port: Vec<u16>,
//	    connection_max: u32,
//	    enabled: bool,
//	}
//
//	let config = Config {
//	    database: Database {
//	        ip: "192.168.1.1".to_string(),
//	        port: vec![8001, 8002, 8003],
//	        connection_max: 5000,
//	        enabled: false,
//	    },
//	};
//
//	let toml = toml_edit::ser::to_string(&config).unwrap();
//	println!("{}", toml)
func SerToString(value any) (string, error) {
	document, err := SerToDocument(value)
	if err != nil {
		return "", err
	}
	return document.String(), nil
}

// SerToStringPretty is toml_edit::ser::to_string_pretty: serialize the given
// data structure as a "pretty" String of TOML.
//
// This is identical to `to_string` except the output string has a more
// "pretty" output. See `ValueSerializer::pretty` for more details.
func SerToStringPretty(value any) (string, error) {
	document, err := SerToDocument(value)
	if err != nil {
		return "", err
	}
	var pretty serPretty
	pretty.VisitDocumentMut(&document)
	return document.String(), nil
}

// SerToDocument is toml_edit::ser::to_document: serialize the given data
// structure into a TOML document.
//
// This would allow custom formatting to be applied, mixing with format
// preserving edits, etc.
func SerToDocument(value any) (Document, error) {
	serialized, err := serializeValue(value, SerValueSerializerNew())
	if err != nil {
		return Document{}, err
	}
	var item Item = &Item_Value{F0: serialized}
	root, err := item.IntoTable()
	if err != nil {
		return Document{}, &SerError_UnsupportedType{F0: rs.None[string]()}
	}
	return DocumentFromTable(root), nil
}

// serializeValue is `value.serialize(serializer)` for a serializer whose
// `Ok` is a Value, with whatever error comes back taken as this format's.
func serializeValue(value any, serializer serde.Serializer) (Value, error) {
	serialized, err := serde.Serialize(value, serializer)
	if err != nil {
		return nil, serErrorFrom(err)
	}
	return serialized.(Value), nil
}
