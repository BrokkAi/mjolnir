// Port of toml_edit 0.20.2 src/parser/value.rs

package tomledit

import (
	"github.com/BrokkAi/mjolnir/go/rs"
)

// val = string / boolean / array / inline-table / date-time / float / integer
func value(in *input, check recursionCheck) (Value, parseErr) {
	// dispatch!{peek(any); ...}
	//     .with_span()
	//     .try_map(|(value, span)| apply_raw(value, span))
	start := in.pos
	val, e := func() (Value, parseErr) {
		b, ok := in.peek()
		if !ok {
			return nil, backtrack()
		}
		// stringContext is what the keyword branches and the fallback carry:
		// a value that is none of the others is reported as a missing string.
		stringContext := []strContext{label("string"), expectedChar('"'), expectedChar('\'')}
		switch {
		case b == quotationMark || b == apostrophe:
			s, e := parseString(in)
			if e.failed() {
				return nil, e
			}
			return &Value_String{F0: FormattedNew(s)}, parseErr{}
		case b == arrayOpen:
			a, e := array(in, check)
			if e.failed() {
				return nil, e
			}
			return &Value_Array{F0: a}, parseErr{}
		case b == inlineTableOpen:
			t, e := inlineTable(in, check)
			if e.failed() {
				return nil, e
			}
			return &Value_InlineTable{F0: t}, parseErr{}
		// Date/number starts
		case b == '+' || b == '-' || (b >= '0' && b <= '9'):
			// Uncommon enough not to be worth optimizing at this time
			// alt((
			//     date_time.map(v::Value::from),
			//     float.map(v::Value::from),
			//     integer.map(v::Value::from),
			// ))
			alternativesStart := in.pos
			d, e := dateTime(in)
			if e.mode != errBacktrack {
				if e.failed() {
					return nil, e
				}
				return ValueFromDatetime(d), parseErr{}
			}
			in.pos = alternativesStart
			f, e := float(in)
			if e.mode != errBacktrack {
				if e.failed() {
					return nil, e
				}
				return ValueFromFloat64(f), parseErr{}
			}
			in.pos = alternativesStart
			i, e := integer(in)
			if e.failed() {
				return nil, e
			}
			return ValueFromInt64(i), parseErr{}
		// Report as if they were numbers because its most likely a typo
		case b == '_':
			i, e := integer(in)
			if e.failed() {
				return nil, e.add(expectedDescription("leading digit"))
			}
			return ValueFromInt64(i), parseErr{}
		// Report as if they were numbers because its most likely a typo
		case b == '.':
			f, e := float(in)
			if e.failed() {
				return nil, e.add(expectedDescription("leading digit"))
			}
			return ValueFromFloat64(f), parseErr{}
		case b == 't':
			v, e := parseTrue(in)
			if e.failed() {
				return nil, e.add(stringContext...)
			}
			return ValueFromBool(v), parseErr{}
		case b == 'f':
			v, e := parseFalse(in)
			if e.failed() {
				return nil, e.add(stringContext...)
			}
			return ValueFromBool(v), parseErr{}
		case b == 'i':
			v, e := parseInf(in)
			if e.failed() {
				return nil, e.add(stringContext...)
			}
			return ValueFromFloat64(v), parseErr{}
		case b == 'n':
			v, e := parseNan(in)
			if e.failed() {
				return nil, e.add(stringContext...)
			}
			return ValueFromFloat64(v), parseErr{}
		default:
			return nil, backtrack().add(stringContext...)
		}
	}()
	if e.failed() {
		return nil, e
	}
	applyRaw(val, Range{Start: start, End: in.pos})
	return val, parseErr{}
}

func applyRaw(val Value, span Range) {
	switch v := val.(type) {
	case *Value_String:
		raw := rawStringWithSpan(span)
		v.F0.setReprUnchecked(reprNewUnchecked(raw))
	case *Value_Integer:
		raw := rawStringWithSpan(span)
		v.F0.setReprUnchecked(reprNewUnchecked(raw))
	case *Value_Float:
		raw := rawStringWithSpan(span)
		v.F0.setReprUnchecked(reprNewUnchecked(raw))
	case *Value_Boolean:
		raw := rawStringWithSpan(span)
		v.F0.setReprUnchecked(reprNewUnchecked(raw))
	case *Value_Datetime:
		raw := rawStringWithSpan(span)
		v.F0.setReprUnchecked(reprNewUnchecked(raw))
	case *Value_Array:
		v.F0.span = rs.Some(span)
	case *Value_InlineTable:
		v.F0.span = rs.Some(span)
	}
	val.decorate(RawStringFromString(""), RawStringFromString(""))
}
