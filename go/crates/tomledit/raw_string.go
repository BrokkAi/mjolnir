// Port of toml_edit 0.20.2 src/raw_string.rs

package tomledit

import (
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Range is Rust's std::ops::Range<usize> as toml_edit uses it: a byte span
// within the parsed document, Start inclusive and End exclusive.
type Range struct {
	Start int `rs:"start"`
	End   int `rs:"end"`
}

type rawStringKind uint8

const (
	rawStringEmpty rawStringKind = iota
	rawStringExplicit
	rawStringSpanned
)

// RawString is toml_edit::RawString: opaque string storage for raw TOML. It
// is a small immutable value; copying one copies it.
type RawString struct {
	kind     rawStringKind
	explicit string
	spanned  Range
}

func rawStringWithSpan(span Range) RawString {
	if span.Start == span.End {
		return RawString{kind: rawStringEmpty}
	}
	return RawString{kind: rawStringSpanned, spanned: span}
}

// AsStr is RawString::as_str: the underlying string, or None while the string
// is still a span into a document that is being parsed.
func (r RawString) AsStr() rs.Option[string] {
	switch r.kind {
	case rawStringEmpty:
		return rs.Some("")
	case rawStringExplicit:
		return rs.Some(r.explicit)
	default:
		return rs.None[string]()
	}
}

func spanSlice(input string, span Range) string {
	// str::get: None unless both ends are in range and on char boundaries.
	if span.Start > span.End || span.End > len(input) || !isCharBoundary(input, span.Start) || !isCharBoundary(input, span.End) {
		rs.PanicAt(2, "span "+strconv.Itoa(span.Start)+".."+strconv.Itoa(span.End)+" should be in input:\n```\n"+input+"\n```")
	}
	return input[span.Start:span.End]
}

func isCharBoundary(s string, index int) bool {
	if index == 0 || index == len(s) {
		return true
	}
	if index > len(s) {
		return false
	}
	// A continuation byte is 0b10xxxxxx.
	return int8(s[index]) >= -0x40
}

func (r RawString) toStr(input string) string {
	switch r.kind {
	case rawStringEmpty:
		return ""
	case rawStringExplicit:
		return r.explicit
	default:
		return spanSlice(input, r.spanned)
	}
}

func (r RawString) toStrWithDefault(input rs.Option[string], fallback string) string {
	switch r.kind {
	case rawStringEmpty:
		return ""
	case rawStringExplicit:
		return r.explicit
	default:
		if text, ok := input.Get(); ok {
			return spanSlice(text, r.spanned)
		}
		return fallback
	}
}

// span is RawString::span: the underlying span.
func (r RawString) span() rs.Option[Range] {
	if r.kind == rawStringSpanned {
		return rs.Some(r.spanned)
	}
	return rs.None[Range]()
}

func (r *RawString) despan(input string) {
	if r.kind == rawStringSpanned {
		*r = RawStringFromString(spanSlice(input, r.spanned))
	}
}

func (r RawString) encode(buf *strings.Builder, input string) {
	raw := r.toStr(input)
	// for part in raw.split('\r') { write!(buf, "{}", part)?; }
	writeWithoutCarriageReturns(buf, raw)
}

func (r RawString) encodeWithDefault(buf *strings.Builder, input rs.Option[string], fallback string) {
	raw := r.toStrWithDefault(input, fallback)
	writeWithoutCarriageReturns(buf, raw)
}

func writeWithoutCarriageReturns(buf *strings.Builder, raw string) {
	for {
		cr := strings.IndexByte(raw, '\r')
		if cr < 0 {
			buf.WriteString(raw)
			return
		}
		buf.WriteString(raw[:cr])
		raw = raw[cr+1:]
	}
}

// RawStringDefault is RawString::default: the empty string.
func RawStringDefault() RawString { return RawString{} }

// RawStringFromString is `RawString::from(s)` for &str, String, &String,
// Box<str> and InternalString.
func RawStringFromString(s string) RawString {
	if s == "" {
		return RawString{kind: rawStringEmpty}
	}
	return RawString{kind: rawStringExplicit, explicit: s}
}

// rawStringFromInternalString is `From<InternalString> for RawString`, which,
// unlike the conversions from other strings, keeps an empty string explicit.
func rawStringFromInternalString(s string) RawString {
	return RawString{kind: rawStringExplicit, explicit: s}
}

// intoRawString is `impl Into<RawString>`: a string or a RawString.
func intoRawString(value any) RawString {
	switch v := value.(type) {
	case RawString:
		return v
	case *RawString:
		return *v
	case string:
		return RawStringFromString(v)
	default:
		panic(rs.Unreachable("toml_edit: Into<RawString> is implemented for strings and RawString only"))
	}
}

// Clone is `impl Clone for RawString`.
func (r RawString) Clone() RawString { return r }

// Equal is `impl PartialEq for RawString`.
func (r RawString) Equal(other RawString) bool { return r == other }

// FmtDebug is `impl Debug for RawString`.
func (r RawString) FmtDebug(f *rs.Formatter) error {
	switch r.kind {
	case rawStringEmpty:
		return f.WriteStr("empty")
	case rawStringExplicit:
		return rs.DebugFmt(r.explicit, f)
	default:
		return f.WriteStr(strconv.Itoa(r.spanned.Start) + ".." + strconv.Itoa(r.spanned.End))
	}
}
