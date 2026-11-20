// Port of toml_edit 0.20.2 src/repr.rs

package tomledit

import (
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Formatted is toml_edit::Formatted<T>: a value together with its `to_string`
// representation, including surrounding it whitespaces and comments. T is
// string, int64, float64, bool or Datetime.
type Formatted[T any] struct {
	value T
	repr  rs.Option[Repr]
	decor Decor
}

// FormattedNew is Formatted::new: a default-formatted value.
func FormattedNew[T any](value T) Formatted[T] {
	return Formatted[T]{value: value}
}

func (f *Formatted[T]) setReprUnchecked(repr Repr) {
	f.repr = rs.Some(repr)
}

// Value is Formatted::value: the wrapped value.
func (f *Formatted[T]) Value() T { return f.value }

// IntoValue is Formatted::into_value: the wrapped value.
func (f *Formatted[T]) IntoValue() T { return f.value }

// AsRepr is Formatted::as_repr: the raw representation, if available.
func (f *Formatted[T]) AsRepr() rs.Option[*Repr] {
	if f.repr.IsSome() {
		return rs.Some(f.repr.AsRef())
	}
	return rs.None[*Repr]()
}

// DefaultRepr is Formatted::default_repr: the default raw representation.
func (f *Formatted[T]) DefaultRepr() Repr {
	return valueToRepr(f.value)
}

// DisplayRepr is Formatted::display_repr: a raw representation.
func (f *Formatted[T]) DisplayRepr() string {
	if repr, ok := f.repr.Get(); ok {
		if text, ok := repr.rawValue.AsStr().Get(); ok {
			return text
		}
	}
	return f.DefaultRepr().rawValue.AsStr().Unwrap()
}

// span is Formatted::span: the location within the original document.
func (f *Formatted[T]) span() rs.Option[Range] {
	if repr, ok := f.repr.Get(); ok {
		return repr.span()
	}
	return rs.None[Range]()
}

func (f *Formatted[T]) despan(input string) {
	f.decor.despan(input)
	if repr := f.repr.AsRef(); repr != nil {
		repr.despan(input)
	}
}

// DecorMut is Formatted::decor_mut: the surrounding whitespace.
func (f *Formatted[T]) DecorMut() *Decor { return &f.decor }

// Decor is Formatted::decor: the surrounding whitespace.
func (f *Formatted[T]) Decor() *Decor { return &f.decor }

// Fmt is Formatted::fmt: auto formats the value.
func (f *Formatted[T]) Fmt() {
	f.repr = rs.Some(valueToRepr(f.value))
}

// Clone is `impl Clone for Formatted<T>`.
func (f *Formatted[T]) Clone() Formatted[T] { return *f }

// Equal is the derived `PartialEq for Formatted<T>`.
func (f *Formatted[T]) Equal(other *Formatted[T]) bool {
	return formattedValueEqual(f.value, other.value) && optionReprEqual(f.repr, other.repr) && f.decor.Equal(&other.decor)
}

// formattedValueEqual is `==` on the types a Formatted holds.
func formattedValueEqual(a, b any) bool {
	switch x := a.(type) {
	case string:
		return x == b.(string)
	case int64:
		return x == b.(int64)
	case float64:
		return x == b.(float64)
	case bool:
		return x == b.(bool)
	case Datetime:
		return x.Equal(b.(Datetime))
	default:
		panic(rs.Unreachable("toml_edit: ValueRepr is implemented for String, i64, f64, bool and Datetime only"))
	}
}

func optionReprEqual(a, b rs.Option[Repr]) bool {
	x, okX := a.Get()
	y, okY := b.Get()
	return okX == okY && (!okX || x.Equal(&y))
}

// FmtDebug is `impl Debug for Formatted<T>`.
func (f *Formatted[T]) FmtDebug(formatter *rs.Formatter) error {
	d := formatter.DebugStruct("Formatted")
	d.Field("value", f.value)
	if repr, ok := f.repr.Get(); ok {
		d.Field("repr", repr)
	} else {
		d.Field("repr", "default")
	}
	d.Field("decor", f.decor)
	return d.Finish()
}

// String is `impl Display for Formatted<T>`.
func (f *Formatted[T]) String() string {
	var buf strings.Builder
	encodeFormatted(f, &buf, rs.None[string](), "", "")
	return buf.String()
}

// FmtDisplay is `impl Display for Formatted<T>`.
func (f *Formatted[T]) FmtDisplay(formatter *rs.Formatter) error {
	return formatter.WriteStr(f.String())
}

// valueToRepr is ValueRepr::to_repr, the TOML representation of the value,
// for each of the types the trait is implemented for.
func valueToRepr(value any) Repr {
	switch v := value.(type) {
	case string:
		return toStringRepr(v, rs.None[stringStyle](), rs.None[bool]())
	case int64:
		return int64ToRepr(v)
	case float64:
		return toF64Repr(v)
	case bool:
		return boolToRepr(v)
	case Datetime:
		return datetimeToRepr(v)
	default:
		panic(rs.Unreachable("toml_edit: ValueRepr is implemented for String, i64, f64, bool and Datetime only"))
	}
}

// Repr is toml_edit::Repr: a TOML-encoded value.
type Repr struct {
	rawValue RawString
}

func reprNewUnchecked(raw RawString) Repr {
	return Repr{rawValue: raw}
}

// AsRaw is Repr::as_raw: the underlying value.
func (r *Repr) AsRaw() *RawString { return &r.rawValue }

// span is Repr::span: the location within the original document.
func (r *Repr) span() rs.Option[Range] { return r.rawValue.span() }

func (r *Repr) despan(input string) { r.rawValue.despan(input) }

func (r *Repr) encode(buf *strings.Builder, input string) { r.rawValue.encode(buf, input) }

// Clone is `impl Clone for Repr`.
func (r *Repr) Clone() Repr { return *r }

// Equal is the derived `PartialEq for Repr`.
func (r *Repr) Equal(other *Repr) bool { return r.rawValue == other.rawValue }

// FmtDebug is `impl Debug for Repr`.
func (r Repr) FmtDebug(f *rs.Formatter) error { return r.rawValue.FmtDebug(f) }

// Decor is toml_edit::Decor: a prefix and suffix, including comments,
// whitespaces and newlines. Its zero value is the default decor.
type Decor struct {
	prefix rs.Option[RawString]
	suffix rs.Option[RawString]
}

// DecorNew is Decor::new: a decor from the given prefix and suffix, each a
// string or a RawString (`impl Into<RawString>`).
func DecorNew(prefix any, suffix any) Decor {
	return Decor{prefix: rs.Some(intoRawString(prefix)), suffix: rs.Some(intoRawString(suffix))}
}

// decorWithSpans is `Decor::new(RawString::with_span(prefix),
// RawString::with_span(suffix))`.
func decorWithSpans(prefix Range, suffix Range) Decor {
	return Decor{prefix: rs.Some(rawStringWithSpan(prefix)), suffix: rs.Some(rawStringWithSpan(suffix))}
}

// DecorDefault is Decor::default.
func DecorDefault() Decor { return Decor{} }

// Clear is Decor::clear: go back to default decor.
func (d *Decor) Clear() {
	d.prefix = rs.None[RawString]()
	d.suffix = rs.None[RawString]()
}

// Prefix is Decor::prefix.
func (d *Decor) Prefix() rs.Option[*RawString] {
	if d.prefix.IsSome() {
		return rs.Some(d.prefix.AsRef())
	}
	return rs.None[*RawString]()
}

func (d *Decor) prefixEncode(buf *strings.Builder, input rs.Option[string], fallback string) {
	if prefix, ok := d.prefix.Get(); ok {
		prefix.encodeWithDefault(buf, input, fallback)
	} else {
		buf.WriteString(fallback)
	}
}

// SetPrefix is Decor::set_prefix; prefix is a string or a RawString.
func (d *Decor) SetPrefix(prefix any) {
	d.prefix = rs.Some(intoRawString(prefix))
}

// Suffix is Decor::suffix.
func (d *Decor) Suffix() rs.Option[*RawString] {
	if d.suffix.IsSome() {
		return rs.Some(d.suffix.AsRef())
	}
	return rs.None[*RawString]()
}

func (d *Decor) suffixEncode(buf *strings.Builder, input rs.Option[string], fallback string) {
	if suffix, ok := d.suffix.Get(); ok {
		suffix.encodeWithDefault(buf, input, fallback)
	} else {
		buf.WriteString(fallback)
	}
}

// SetSuffix is Decor::set_suffix; suffix is a string or a RawString.
func (d *Decor) SetSuffix(suffix any) {
	d.suffix = rs.Some(intoRawString(suffix))
}

func (d *Decor) despan(input string) {
	if prefix := d.prefix.AsRef(); prefix != nil {
		prefix.despan(input)
	}
	if suffix := d.suffix.AsRef(); suffix != nil {
		suffix.despan(input)
	}
}

// Clone is `impl Clone for Decor`.
func (d *Decor) Clone() Decor { return *d }

// Equal is the derived `PartialEq for Decor`.
func (d *Decor) Equal(other *Decor) bool {
	return *d == *other
}

// FmtDebug is `impl Debug for Decor`.
func (d Decor) FmtDebug(f *rs.Formatter) error {
	s := f.DebugStruct("Decor")
	if prefix, ok := d.prefix.Get(); ok {
		s.Field("prefix", prefix)
	} else {
		s.Field("prefix", "default")
	}
	if suffix, ok := d.suffix.Get(); ok {
		s.Field("suffix", suffix)
	} else {
		s.Field("suffix", "default")
	}
	return s.Finish()
}
