// Port of toml_datetime 0.6.3 src/datetime.rs
//
// toml_edit re-exports the whole crate (`pub use toml_datetime::*`), and toml
// re-exports its types from `toml::value`, so the types live here and
// go/crates/toml aliases them.

package tomledit

import (
	"cmp"
	"strconv"
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// Datetime is toml_datetime::Datetime: a parsed TOML datetime value.
//
// This structure is intended to represent the datetime primitive type that can
// be encoded into TOML documents. This type is a parsed version that contains
// all metadata internally.
//
// Currently this type is intentionally conservative and only supports
// `to_string` as an accessor. Over time though it's intended that it'll grow
// more support!
//
// Note that if you're using `Deserialize` to deserialize a TOML document, you
// can use this as a placeholder for where you're expecting a datetime to be
// specified.
//
// Also note though that while this type implements `Serialize` and
// `Deserialize` it's only recommended to use this type with the TOML format,
// otherwise encoded in other formats it may look a little odd.
type Datetime struct {
	// Optional date.
	// Required for: *Offset Date-Time*, *Local Date-Time*, *Local Date*.
	Date rs.Option[Date] `rs:"date"`

	// Optional time.
	// Required for: *Offset Date-Time*, *Local Date-Time*, *Local Time*.
	Time rs.Option[Time] `rs:"time"`

	// Optional offset.
	// Required for: *Offset Date-Time*.
	Offset rs.Option[Offset] `rs:"offset"`
}

var _ = rs.RegisterStruct[Datetime]("Datetime")

// DatetimeParseError is toml_datetime::DatetimeParseError: the error
// returned from parsing a `Datetime` in the `FromStr` implementation.
type DatetimeParseError struct{}

var _ = rs.RegisterStruct[DatetimeParseError]("DatetimeParseError")

// DatetimeField and DatetimeName are toml_datetime::__unstable::FIELD and
// NAME.
//
// Currently serde itself doesn't have a datetime type, so we map our `Datetime`
// to a special value in the serde data model. Namely one with these special
// fields/struct names.
//
// In general the TOML encoder/decoder will catch this and not literally emit
// these strings but rather emit datetimes as they're intended.
const (
	DatetimeField = "$__toml_private_datetime"
	DatetimeName  = "$__toml_private_Datetime"
)

// Date is toml_datetime::Date: a parsed TOML date value.
//
// May be part of a `Datetime`. Alone, `Date` corresponds to a Local Date.
// From the TOML v1.0.0 spec:
//
// > If you include only the date portion of an RFC 3339 formatted date-time,
// > it will represent that entire day without any relation to an offset or
// > timezone.
type Date struct {
	// Year: four digits
	Year uint16 `rs:"year"`
	// Month: 1 to 12
	Month uint8 `rs:"month"`
	// Day: 1 to {28, 29, 30, 31} (based on month/year)
	Day uint8 `rs:"day"`
}

var _ = rs.RegisterStruct[Date]("Date")

// Time is toml_datetime::Time: a parsed TOML time value.
//
// May be part of a `Datetime`. Alone, `Time` corresponds to a Local Time.
// From the TOML v1.0.0 spec:
//
// > If you include only the time portion of an RFC 3339 formatted date-time,
// > it will represent that time of day without any relation to a specific
// > day or any offset or timezone.
type Time struct {
	// Hour: 0 to 23
	Hour uint8 `rs:"hour"`
	// Minute: 0 to 59
	Minute uint8 `rs:"minute"`
	// Second: 0 to {58, 59, 60} (based on leap second rules)
	Second uint8 `rs:"second"`
	// Nanosecond: 0 to 999_999_999
	Nanosecond uint32 `rs:"nanosecond"`
}

var _ = rs.RegisterStruct[Time]("Time")

// Offset is toml_datetime::Offset: a parsed TOML time offset. Its variants
// are *Offset_Z and *Offset_Custom; a variant is never changed in place.
type Offset interface {
	isOffset()
	// String is `impl Display for Offset`.
	String() string
	// Equal is the derived `PartialEq for Offset`.
	Equal(other Offset) bool
	// Compare is the derived `Ord for Offset`.
	Compare(other Offset) int
}

// Offset_Z is Offset::Z.
//
// > A suffix which, when applied to a time, denotes a UTC offset of 00:00;
// > often spoken "Zulu" from the ICAO phonetic alphabet representation of
// > the letter "Z". --- [RFC 3339 section 2]
type Offset_Z struct{}

// Offset_Custom is Offset::Custom: an offset between local time and UTC.
type Offset_Custom struct {
	// Minutes: -1_440..1_440
	Minutes int16 `rs:"minutes"`
}

func (*Offset_Z) isOffset()      {}
func (*Offset_Custom) isOffset() {}

var _ = rs.RegisterEnum[Offset]("Offset",
	rs.Variant[Offset_Z]("Z", rs.ShapeUnit),
	rs.Variant[Offset_Custom]("Custom", rs.ShapeStruct),
)

// DatetimeFromDate is `impl From<Date> for Datetime`.
func DatetimeFromDate(other Date) Datetime {
	return Datetime{Date: rs.Some(other)}
}

// DatetimeFromTime is `impl From<Time> for Datetime`.
func DatetimeFromTime(other Time) Datetime {
	return Datetime{Time: rs.Some(other)}
}

// String is `impl Display for Datetime`.
func (d Datetime) String() string {
	var out strings.Builder
	if date, ok := d.Date.Get(); ok {
		out.WriteString(date.String())
	}
	if time, ok := d.Time.Get(); ok {
		if d.Date.IsSome() {
			out.WriteString("T")
		}
		out.WriteString(time.String())
	}
	if offset, ok := d.Offset.Get(); ok {
		out.WriteString(offset.String())
	}
	return out.String()
}

// FmtDisplay is `impl Display for Datetime`.
func (d Datetime) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(d.String()) }

func zeroPad(value uint64, width int) string {
	text := strconv.FormatUint(value, 10)
	for len(text) < width {
		text = "0" + text
	}
	return text
}

// String is `impl Display for Date`.
func (d Date) String() string {
	return zeroPad(uint64(d.Year), 4) + "-" + zeroPad(uint64(d.Month), 2) + "-" + zeroPad(uint64(d.Day), 2)
}

// FmtDisplay is `impl Display for Date`.
func (d Date) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(d.String()) }

// String is `impl Display for Time`.
func (t Time) String() string {
	out := zeroPad(uint64(t.Hour), 2) + ":" + zeroPad(uint64(t.Minute), 2) + ":" + zeroPad(uint64(t.Second), 2)
	if t.Nanosecond != 0 {
		s := zeroPad(uint64(t.Nanosecond), 9)
		out += "." + strings.TrimRight(s, "0")
	}
	return out
}

// FmtDisplay is `impl Display for Time`.
func (t Time) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(t.String()) }

// String is `impl Display for Offset`.
func (*Offset_Z) String() string { return "Z" }

// String is `impl Display for Offset`.
func (o *Offset_Custom) String() string {
	// The arithmetic is i16 in Rust; -32768 cannot be negated there, and no
	// offset the parser or FromStr produces comes near it.
	minutes := int(o.Minutes)
	sign := "+"
	if minutes < 0 {
		minutes *= -1
		sign = "-"
	}
	hours := minutes / 60
	minutes = minutes % 60
	return sign + zeroPad(uint64(hours), 2) + ":" + zeroPad(uint64(minutes), 2)
}

// FmtDisplay is `impl Display for Offset`.
func (o *Offset_Z) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(o.String()) }

// FmtDisplay is `impl Display for Offset`.
func (o *Offset_Custom) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(o.String()) }

// Equal is the derived `PartialEq for Offset`.
func (*Offset_Z) Equal(other Offset) bool {
	_, ok := other.(*Offset_Z)
	return ok
}

// Equal is the derived `PartialEq for Offset`.
func (o *Offset_Custom) Equal(other Offset) bool {
	custom, ok := other.(*Offset_Custom)
	return ok && custom.Minutes == o.Minutes
}

// Compare is the derived `Ord for Offset`: Z sorts before Custom.
func (*Offset_Z) Compare(other Offset) int {
	if _, ok := other.(*Offset_Z); ok {
		return 0
	}
	return -1
}

// Compare is the derived `Ord for Offset`: Z sorts before Custom.
func (o *Offset_Custom) Compare(other Offset) int {
	custom, ok := other.(*Offset_Custom)
	if !ok {
		return 1
	}
	return cmp.Compare(o.Minutes, custom.Minutes)
}

func optionEqual[T any](a, b rs.Option[T], equal func(T, T) bool) bool {
	x, okX := a.Get()
	y, okY := b.Get()
	if okX != okY {
		return false
	}
	return !okX || equal(x, y)
}

// optionCompare orders two options as Rust does: None before Some.
func optionCompare[T any](a, b rs.Option[T], compare func(T, T) int) int {
	x, okX := a.Get()
	y, okY := b.Get()
	switch {
	case !okX && !okY:
		return 0
	case !okX:
		return -1
	case !okY:
		return 1
	default:
		return compare(x, y)
	}
}

// Equal is the derived `PartialEq for Datetime`.
func (d Datetime) Equal(other Datetime) bool {
	return d.Date == other.Date && d.Time == other.Time &&
		optionEqual(d.Offset, other.Offset, func(a, b Offset) bool { return a.Equal(b) })
}

// Compare is the derived `Ord for Datetime`.
func (d Datetime) Compare(other Datetime) int {
	if order := optionCompare(d.Date, other.Date, Date.Compare); order != 0 {
		return order
	}
	if order := optionCompare(d.Time, other.Time, Time.Compare); order != 0 {
		return order
	}
	return optionCompare(d.Offset, other.Offset, func(a, b Offset) int { return a.Compare(b) })
}

// Compare is the derived `Ord for Date`.
func (d Date) Compare(other Date) int {
	return cmp.Or(cmp.Compare(d.Year, other.Year), cmp.Compare(d.Month, other.Month), cmp.Compare(d.Day, other.Day))
}

// Compare is the derived `Ord for Time`.
func (t Time) Compare(other Time) int {
	return cmp.Or(cmp.Compare(t.Hour, other.Hour), cmp.Compare(t.Minute, other.Minute),
		cmp.Compare(t.Second, other.Second), cmp.Compare(t.Nanosecond, other.Nanosecond))
}

// DatetimeFromStr is `impl FromStr for Datetime`.
func DatetimeFromStr(date string) (Datetime, error) {
	fail := func() (Datetime, error) { return Datetime{}, &DatetimeParseError{} }

	// Accepted formats:
	//
	// 0000-00-00T00:00:00.00Z
	// 0000-00-00T00:00:00.00
	// 0000-00-00
	// 00:00:00.00
	if len(date) < 3 {
		return fail()
	}
	offsetAllowed := true
	chars := &charsIter{rest: date}

	// First up, parse the full date if we can
	fullDate := rs.None[Date]()
	if third, ok := chars.nth(2); ok && third == ':' {
		offsetAllowed = false
	} else {
		var digits [8]uint16
		read := func(index int) bool {
			d, ok := chars.digit()
			digits[index] = uint16(d)
			return ok
		}
		if !read(0) || !read(1) || !read(2) || !read(3) {
			return fail()
		}
		if c, ok := chars.next(); !ok || c != '-' {
			return fail()
		}
		if !read(4) || !read(5) {
			return fail()
		}
		if c, ok := chars.next(); !ok || c != '-' {
			return fail()
		}
		if !read(6) || !read(7) {
			return fail()
		}
		parsed := Date{
			Year:  digits[0]*1000 + digits[1]*100 + digits[2]*10 + digits[3],
			Month: uint8(digits[4]*10 + digits[5]),
			Day:   uint8(digits[6]*10 + digits[7]),
		}

		if parsed.Month < 1 || parsed.Month > 12 {
			return fail()
		}
		if parsed.Day < 1 || parsed.Day > 31 {
			return fail()
		}

		fullDate = rs.Some(parsed)
	}

	// Next parse the "partial-time" if available
	next, hasNext := chars.peek()
	var partialTime bool
	if fullDate.IsSome() && hasNext && (next == 'T' || next == 't' || next == ' ') {
		chars.next()
		partialTime = true
	} else {
		partialTime = fullDate.IsNone()
	}

	time := rs.None[Time]()
	if partialTime {
		var digits [6]uint8
		read := func(index int) bool {
			d, ok := chars.digit()
			digits[index] = d
			return ok
		}
		if !read(0) || !read(1) {
			return fail()
		}
		if c, ok := chars.next(); !ok || c != ':' {
			return fail()
		}
		if !read(2) || !read(3) {
			return fail()
		}
		if c, ok := chars.next(); !ok || c != ':' {
			return fail()
		}
		if !read(4) || !read(5) {
			return fail()
		}

		var nanosecond uint32
		if c, ok := chars.peek(); ok && c == '.' {
			chars.next()
			whole := chars.rest

			end := len(whole)
			for i := 0; i < len(whole); i++ {
				b := whole[i]
				if b >= '0' && b <= '9' {
					if i < 9 {
						p := uint32(1)
						for range 8 - i {
							p *= 10
						}
						nanosecond += p * uint32(b-'0')
					}
				} else {
					end = i
					break
				}
			}
			if end == 0 {
				return fail()
			}
			chars.rest = whole[end:]
		}

		parsed := Time{
			Hour:       digits[0]*10 + digits[1],
			Minute:     digits[2]*10 + digits[3],
			Second:     digits[4]*10 + digits[5],
			Nanosecond: nanosecond,
		}

		if parsed.Hour > 24 {
			return fail()
		}
		if parsed.Minute > 59 {
			return fail()
		}
		if parsed.Second > 59 {
			return fail()
		}
		if parsed.Nanosecond > 999_999_999 {
			return fail()
		}

		time = rs.Some(parsed)
	} else {
		offsetAllowed = false
	}

	// And finally, parse the offset
	offset := rs.None[Offset]()
	if offsetAllowed {
		next, hasNext := chars.peek()
		if hasNext && (next == 'Z' || next == 'z') {
			chars.next()
			offset = rs.Some[Offset](&Offset_Z{})
		} else if !hasNext {
			// None
		} else {
			var sign int16
			switch next {
			case '+':
				sign = 1
			case '-':
				sign = -1
			default:
				return fail()
			}
			chars.next()
			var digits [4]int16
			read := func(index int) bool {
				d, ok := chars.digit()
				digits[index] = int16(d)
				return ok
			}
			if !read(0) || !read(1) {
				return fail()
			}
			if c, ok := chars.next(); !ok || c != ':' {
				return fail()
			}
			if !read(2) || !read(3) {
				return fail()
			}

			hours := digits[0]*10 + digits[1]
			minutes := digits[2]*10 + digits[3]

			totalMinutes := sign * (hours*60 + minutes)

			if totalMinutes < -24*60 || totalMinutes > 24*60 {
				return fail()
			}

			offset = rs.Some[Offset](&Offset_Custom{Minutes: totalMinutes})
		}
	}

	// Return an error if we didn't hit eof, otherwise return our parsed
	// date
	if _, more := chars.next(); more {
		return fail()
	}

	return Datetime{Date: fullDate, Time: time, Offset: offset}, nil
}

// charsIter is the `str::Chars` the parser above walks.
type charsIter struct {
	rest string
}

func (c *charsIter) peek() (rune, bool) {
	if c.rest == "" {
		return 0, false
	}
	r, _ := utf8.DecodeRuneInString(c.rest)
	return r, true
}

func (c *charsIter) next() (rune, bool) {
	if c.rest == "" {
		return 0, false
	}
	r, size := utf8.DecodeRuneInString(c.rest)
	c.rest = c.rest[size:]
	return r, true
}

// nth is `chars.clone().nth(n)`: the n-th char, without advancing.
func (c *charsIter) nth(n int) (rune, bool) {
	index := 0
	for _, r := range c.rest {
		if index == n {
			return r, true
		}
		index++
	}
	return 0, false
}

func (c *charsIter) digit() (uint8, bool) {
	r, ok := c.next()
	if ok && r >= '0' && r <= '9' {
		return uint8(r - '0'), true
	}
	return 0, false
}

// SerializeSerde is `impl Serialize for Datetime`: a struct of the private
// name with the one private field holding the text, which the TOML
// serializer recognises.
func (d *Datetime) SerializeSerde(serializer serde.Serializer) (any, error) {
	s, err := serializer.SerializeStruct(DatetimeName, 1)
	if err != nil {
		return nil, err
	}
	if err := s.SerializeField(DatetimeField, d.String()); err != nil {
		return nil, err
	}
	return s.End()
}

// DeserializeSerde is `impl Deserialize for Datetime`.
func (d *Datetime) DeserializeSerde(deserializer serde.Deserializer) error {
	fields := []string{DatetimeField}
	value, err := deserializer.DeserializeStruct(DatetimeName, fields, datetimeVisitor{})
	if err != nil {
		return err
	}
	*d = value.(Datetime)
	return nil
}

// datetimeVisitor is the DatetimeVisitor of `impl Deserialize for Datetime`.
type datetimeVisitor struct{}

func (datetimeVisitor) Expecting() string { return "a TOML datetime" }

func (datetimeVisitor) VisitMap(visitor serde.DeMapAccess) (any, error) {
	value, err := serde.DeMapAccessNextKey[datetimeKey](visitor)
	if err != nil {
		return nil, err
	}
	if value.IsNone() {
		return nil, serde.DeErrorCustom("datetime key not found")
	}
	v, err := serde.DeMapAccessNextValue[DatetimeFromString](visitor)
	if err != nil {
		return nil, err
	}
	return v.Value, nil
}

// datetimeKey is DatetimeKey: the private field name, and nothing else.
type datetimeKey struct{}

// DeserializeSerde is `impl Deserialize for DatetimeKey`.
func (*datetimeKey) DeserializeSerde(deserializer serde.Deserializer) error {
	_, err := deserializer.DeserializeIdentifier(datetimeFieldVisitor{})
	return err
}

// datetimeFieldVisitor is the FieldVisitor of `impl Deserialize for
// DatetimeKey`.
type datetimeFieldVisitor struct{}

func (datetimeFieldVisitor) Expecting() string { return "a valid datetime field" }

func (datetimeFieldVisitor) VisitStr(s string) (any, error) {
	if s == DatetimeField {
		return rs.Unit{}, nil
	}
	return nil, serde.DeErrorCustom("expected field with custom name")
}

// DatetimeFromString is toml_datetime::__unstable::DatetimeFromString: a
// datetime read from its text.
type DatetimeFromString struct {
	Value Datetime `rs:"value"`
}

// DeserializeSerde is `impl Deserialize for DatetimeFromString`.
func (d *DatetimeFromString) DeserializeSerde(deserializer serde.Deserializer) error {
	value, err := deserializer.DeserializeStr(datetimeFromStringVisitor{})
	if err != nil {
		return err
	}
	*d = value.(DatetimeFromString)
	return nil
}

// datetimeFromStringVisitor is the Visitor of `impl Deserialize for
// DatetimeFromString`.
type datetimeFromStringVisitor struct{}

func (datetimeFromStringVisitor) Expecting() string { return "string containing a datetime" }

func (datetimeFromStringVisitor) VisitStr(s string) (any, error) {
	date, err := DatetimeFromStr(s)
	if err != nil {
		return nil, serde.DeErrorCustom(err)
	}
	return DatetimeFromString{Value: date}, nil
}

// Error is `impl Display for DatetimeParseError`.
func (*DatetimeParseError) Error() string { return "failed to parse datetime" }

// FmtDisplay is `impl Display for DatetimeParseError`.
func (e *DatetimeParseError) FmtDisplay(f *rs.Formatter) error { return f.Pad(e.Error()) }
