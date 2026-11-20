// Port of serde_json 1.0.150 src/number.rs (without arbitrary_precision)

package serdejson

import (
	"math"
	"strconv"

	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// Number is `serde_json::Number`: a JSON number. It holds the value as the
// jsonvalue representation does: an int64 for an integer that fits i64
// (Rust's PosInt or NegInt by its sign), a uint64 for a larger one (PosInt),
// or a finite float64 (Float). Number.Value returns that representation,
// which is what a Value holds for `Value::Number(n)`.
type Number struct {
	n any
}

// NumberFromI64 is `Number::from(i64)`.
func NumberFromI64(i int64) Number { return Number{n: i} }

// NumberFromU64 is `Number::from(u64)`.
func NumberFromU64(u uint64) Number {
	if u <= math.MaxInt64 {
		return Number{n: int64(u)}
	}
	return Number{n: u}
}

// NumberFromF64 is `Number::from_f64`: a Number from an f64, None for NaN
// and the infinities, which JSON cannot represent.
func NumberFromF64(f float64) rs.Option[Number] {
	if math.IsNaN(f) || math.IsInf(f, 0) {
		return rs.None[Number]()
	}
	return rs.Some(Number{n: f})
}

// numberFromParser is `impl From<ParserNumber> for Number`.
func numberFromParser(p parserNumber) Number {
	switch p.kind {
	case parserF64:
		return Number{n: p.f}
	case parserU64:
		return NumberFromU64(p.u)
	}
	return Number{n: p.i}
}

// numberOf returns the Number a Value holds, and whether it holds one.
func numberOf(v Value) (Number, bool) {
	switch x := v.(type) {
	case int64, uint64, float64:
		return Number{n: x}, true
	case int:
		return Number{n: int64(x)}, true
	case int8:
		return Number{n: int64(x)}, true
	case int16:
		return Number{n: int64(x)}, true
	case int32:
		return Number{n: int64(x)}, true
	case uint:
		return NumberFromU64(uint64(x)), true
	case uint8:
		return NumberFromU64(uint64(x)), true
	case uint16:
		return NumberFromU64(uint64(x)), true
	case uint32:
		return NumberFromU64(uint64(x)), true
	case float32:
		return Number{n: float64(x)}, true
	case Number:
		return x, true
	}
	return Number{}, false
}

// Value returns the number as a Value holds it.
func (n Number) Value() Value {
	if n.n == nil {
		return int64(0)
	}
	return n.n
}

// IsI64 is `Number::is_i64`: whether the number is an integer between
// i64::MIN and i64::MAX.
func (n Number) IsI64() bool {
	_, ok := n.n.(int64)
	return ok || n.n == nil
}

// IsU64 is `Number::is_u64`: whether the number is a non-negative integer
// at most u64::MAX.
func (n Number) IsU64() bool {
	switch x := n.n.(type) {
	case int64:
		return x >= 0
	case uint64:
		return true
	}
	return n.n == nil
}

// IsF64 is `Number::is_f64`: whether the number is a float.
func (n Number) IsF64() bool {
	_, ok := n.n.(float64)
	return ok
}

// AsI64 is `Number::as_i64`.
func (n Number) AsI64() rs.Option[int64] {
	switch x := n.n.(type) {
	case int64:
		return rs.Some(x)
	case nil:
		return rs.Some(int64(0))
	}
	return rs.None[int64]()
}

// AsU64 is `Number::as_u64`.
func (n Number) AsU64() rs.Option[uint64] {
	switch x := n.n.(type) {
	case int64:
		if x >= 0 {
			return rs.Some(uint64(x))
		}
	case uint64:
		return rs.Some(x)
	case nil:
		return rs.Some(uint64(0))
	}
	return rs.None[uint64]()
}

// AsF64 is `Number::as_f64`: the number as an f64, which may lose
// precision for a large integer.
func (n Number) AsF64() rs.Option[float64] {
	switch x := n.n.(type) {
	case int64:
		return rs.Some(float64(x))
	case uint64:
		return rs.Some(float64(x))
	case float64:
		return rs.Some(x)
	}
	return rs.Some(0.0)
}

// String is `impl Display for Number`.
func (n Number) String() string {
	switch x := n.n.(type) {
	case int64:
		return strconv.FormatInt(x, 10)
	case uint64:
		return strconv.FormatUint(x, 10)
	case float64:
		return formatF64(x)
	}
	return "0"
}

// FmtDisplay is `impl Display for Number`.
func (n Number) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(n.String()) }

// unexpected is `Number::unexpected`.
func (n Number) unexpected() serde.DeUnexpected {
	switch x := n.n.(type) {
	case int64:
		if x >= 0 {
			return &serde.DeUnexpectedUnsigned{F0: uint64(x)}
		}
		return &serde.DeUnexpectedSigned{F0: x}
	case uint64:
		return &serde.DeUnexpectedUnsigned{F0: x}
	case float64:
		return &serde.DeUnexpectedFloat{F0: x}
	}
	return &serde.DeUnexpectedUnsigned{F0: 0}
}

// SerializeSerde is `impl Serialize for Number`.
func (n Number) SerializeSerde(serializer serde.Serializer) (any, error) {
	switch x := n.n.(type) {
	case int64:
		if x >= 0 {
			return serializer.SerializeU64(uint64(x))
		}
		return serializer.SerializeI64(x)
	case uint64:
		return serializer.SerializeU64(x)
	case float64:
		return serializer.SerializeF64(x)
	}
	return serializer.SerializeU64(0)
}

// visit is `Number::deserialize_any`.
func (n Number) visit(visitor serde.DeVisitor) (any, error) {
	switch x := n.n.(type) {
	case int64:
		if x >= 0 {
			return serde.DeVisitorVisitU64(visitor, uint64(x))
		}
		return serde.DeVisitorVisitI64(visitor, x)
	case uint64:
		return serde.DeVisitorVisitU64(visitor, x)
	case float64:
		return serde.DeVisitorVisitF64(visitor, x)
	}
	return serde.DeVisitorVisitU64(visitor, 0)
}

// DeserializeSerde is `impl Deserialize for Number`.
func (n *Number) DeserializeSerde(deserializer serde.Deserializer) error {
	_, err := deserializer.DeserializeAny(&numberVisitor{out: n})
	return err
}

type numberVisitor struct{ out *Number }

func (v *numberVisitor) Expecting() string { return "a JSON number" }

func (v *numberVisitor) VisitI64(value int64) (any, error) {
	*v.out = NumberFromI64(value)
	return nil, nil
}

func (v *numberVisitor) VisitU64(value uint64) (any, error) {
	*v.out = NumberFromU64(value)
	return nil, nil
}

func (v *numberVisitor) VisitF64(value float64) (any, error) {
	n := NumberFromF64(value)
	if n.IsNone() {
		return nil, serde.DeErrorCustom("not a JSON number")
	}
	*v.out = n.Unwrap()
	return nil, nil
}
