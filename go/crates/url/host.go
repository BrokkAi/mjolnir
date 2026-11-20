// Port of url 2.5.8 src/host.rs

package url

import (
	"net/netip"
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The addresses in Host are std::net::Ipv4Addr and std::net::Ipv6Addr in
// Rust. go/rs/rsnet, which is to hold those types, did not exist when this
// package was written, so they are net/netip.Addr here: Host_Ipv4 always
// holds a 4-byte address and Host_Ipv6 a 16-byte one.

type hostHostInternal interface{ isHostHostInternal() }

type hostHostInternal_None struct{}
type hostHostInternal_Domain struct{}
type hostHostInternal_Ipv4 struct{ F0 netip.Addr }
type hostHostInternal_Ipv6 struct{ F0 netip.Addr }

func (*hostHostInternal_None) isHostHostInternal()   {}
func (*hostHostInternal_Domain) isHostHostInternal() {}
func (*hostHostInternal_Ipv4) isHostHostInternal()   {}
func (*hostHostInternal_Ipv6) isHostHostInternal()   {}

// hostHostInternalFromHost is `impl From<Host<Cow<'_, str>>> for
// HostInternal`.
func hostHostInternalFromHost(host Host) hostHostInternal {
	switch h := host.(type) {
	case *Host_Domain:
		if h.F0 == "" {
			return &hostHostInternal_None{}
		}
		return &hostHostInternal_Domain{}
	case *Host_Ipv4:
		return &hostHostInternal_Ipv4{F0: h.F0}
	case *Host_Ipv6:
		return &hostHostInternal_Ipv6{F0: h.F0}
	default:
		panic(rs.Unreachable(host))
	}
}

// hostHostInternalEq is the derived PartialEq of HostInternal.
func hostHostInternalEq(a hostHostInternal, b hostHostInternal) bool {
	switch x := a.(type) {
	case *hostHostInternal_None:
		return rs.Is[*hostHostInternal_None](b)
	case *hostHostInternal_Domain:
		return rs.Is[*hostHostInternal_Domain](b)
	case *hostHostInternal_Ipv4:
		y, ok := b.(*hostHostInternal_Ipv4)
		return ok && x.F0 == y.F0
	case *hostHostInternal_Ipv6:
		y, ok := b.(*hostHostInternal_Ipv6)
		return ok && x.F0 == y.F0
	default:
		panic(rs.Unreachable(a))
	}
}

// Host is url::Host: the host name of an URL. A value is a pointer to one of
// the variant structs: *Host_Domain, *Host_Ipv4 or *Host_Ipv6.
//
// Rust has Host<String>, Host<&str> and Host<Cow<str>>; all three are this
// one type.
type Host interface {
	isHost()
	// String is the Display text: the domain, the IPv4 address as four
	// decimal integers, or the IPv6 address between square brackets.
	String() string
	// FmtDisplay is `impl Display for Host`.
	FmtDisplay(f *rs.Formatter) error
	// ToOwned is Host<&str>::to_owned: return a copy of `self` that owns an
	// allocated `String` but does not borrow an `&Url`.
	ToOwned() Host
	// Equal is `impl PartialEq<Host<T>> for Host<S>`.
	Equal(other Host) bool
	// Compare is the derived Ord: by variant, then by value.
	Compare(other Host) int
	// HashKey is the derived Hash: two hosts have the same key exactly
	// when they are equal.
	HashKey() string
}

// Host_Domain is Host::Domain: a DNS domain name, as '.' dot-separated
// labels. Non-ASCII labels are encoded in punycode per IDNA if this is the
// host of a special URL, or percent encoded for non-special URLs. Hosts for
// non-special URLs are also called opaque hosts.
type Host_Domain struct{ F0 string }

// Host_Ipv4 is Host::Ipv4: an IPv4 address. `Url::host_str` returns the
// serialization of this address, as four decimal integers separated by `.`
// dots.
type Host_Ipv4 struct{ F0 netip.Addr }

// Host_Ipv6 is Host::Ipv6: an IPv6 address. `Url::host_str` returns the
// serialization of that address between `[` and `]` brackets, in the format
// per RFC 5952 *A Recommendation for IPv6 Address Text Representation*
// (https://tools.ietf.org/html/rfc5952): lowercase hexadecimal with maximal
// `::` compression.
type Host_Ipv6 struct{ F0 netip.Addr }

var _ = rs.RegisterEnum[Host]("Host",
	rs.Variant[Host_Domain]("Domain", rs.ShapeTuple),
	rs.Variant[Host_Ipv4]("Ipv4", rs.ShapeTuple),
	rs.Variant[Host_Ipv6]("Ipv6", rs.ShapeTuple),
)

func (*Host_Domain) isHost() {}
func (*Host_Ipv4) isHost()   {}
func (*Host_Ipv6) isHost()   {}

func (h *Host_Domain) HashKey() string { return "Domain:" + h.F0 }
func (h *Host_Ipv4) HashKey() string   { return "Ipv4:" + h.F0.String() }
func (h *Host_Ipv6) HashKey() string   { return "Ipv6:" + h.F0.String() }

func (h *Host_Domain) ToOwned() Host { return &Host_Domain{F0: h.F0} }
func (h *Host_Ipv4) ToOwned() Host   { return &Host_Ipv4{F0: h.F0} }
func (h *Host_Ipv6) ToOwned() Host   { return &Host_Ipv6{F0: h.F0} }

// HostParse is Host::parse: parse a host: either an IPv6 address in []
// square brackets, or a domain.
//
// <https://url.spec.whatwg.org/#host-parsing>
func HostParse(input string) (Host, error) {
	return hostParseCow(input)
}

// HostParseOpaque is Host::parse_opaque.
//
// <https://url.spec.whatwg.org/#concept-opaque-host-parser>
func HostParseOpaque(input string) (Host, error) {
	return hostParseOpaqueCow(input)
}

func hostParseCow(input string) (Host, error) {
	if strings.HasPrefix(input, "[") {
		if !strings.HasSuffix(input, "]") {
			return nil, ParseError_InvalidIpv6Address
		}
		address, err := hostParseIpv6addr(input[1 : len(input)-1])
		if err != nil {
			return nil, err
		}
		return &Host_Ipv6{F0: address}, nil
	}
	domainBytes, _ := percentEncodingPercentDecodeNew([]byte(input)).collect()

	domain, ok := idnaDomainToAsciiFromCow(domainBytes, idnaAsciiDenyListUrl)
	if !ok {
		return nil, ParseError_IdnaError
	}

	if domain == "" {
		return nil, ParseError_EmptyHost
	}

	if hostEndsInANumber(domain) {
		address, err := hostParseIpv4addr(domain)
		if err != nil {
			return nil, err
		}
		return &Host_Ipv4{F0: address}, nil
	} else {
		return &Host_Domain{F0: domain}, nil
	}
}

func hostParseOpaqueCow(input string) (Host, error) {
	if strings.HasPrefix(input, "[") {
		if !strings.HasSuffix(input, "]") {
			return nil, ParseError_InvalidIpv6Address
		}
		address, err := hostParseIpv6addr(input[1 : len(input)-1])
		if err != nil {
			return nil, err
		}
		return &Host_Ipv6{F0: address}, nil
	}

	isInvalidHostChar := func(c byte) bool {
		switch c {
		case 0, '\t', '\n', '\r', ' ', '#', '/', ':', '<', '>', '?', '@', '[', '\\', ']', '^', '|':
			return true
		}
		return false
	}

	for i := 0; i < len(input); i++ {
		if isInvalidHostChar(input[i]) {
			return nil, ParseError_InvalidDomainCharacter
		}
	}

	// Call utf8_percent_encode and use the result.
	return &Host_Domain{F0: string(appendUtf8PercentEncode(nil, input, percentEncodingControls))}, nil
}

func (h *Host_Domain) String() string { return h.F0 }

func (h *Host_Ipv4) String() string { return hostIpv4String(h.F0) }

func (h *Host_Ipv6) String() string {
	return "[" + hostWriteIpv6(h.F0) + "]"
}

// FmtDisplay passes the formatter's flags on to the domain or the IPv4
// address, as the Rust impl does, and writes an IPv6 address without them.
func (h *Host_Domain) FmtDisplay(f *rs.Formatter) error { return f.Pad(h.F0) }

func (h *Host_Ipv4) FmtDisplay(f *rs.Formatter) error { return f.Pad(hostIpv4String(h.F0)) }

func (h *Host_Ipv6) FmtDisplay(f *rs.Formatter) error {
	if err := f.WriteStr("["); err != nil {
		return err
	}
	if err := f.WriteStr(hostWriteIpv6(h.F0)); err != nil {
		return err
	}
	return f.WriteStr("]")
}

// FmtDebug is the derived Debug of Host::Ipv4: `Ipv4(127.0.0.1)`. It is
// written by hand because the Go field is a netip.Addr, whose reflected form
// is not Rust's: the address prints as `impl Debug for Ipv4Addr` does, which
// is its Display through `Formatter::pad`.
func (h *Host_Ipv4) FmtDebug(f *rs.Formatter) error {
	address := h.F0
	return f.DebugTuple("Ipv4").Field(rs.FmtFromFnOf(func(f *rs.Formatter) error {
		return f.Pad(hostIpv4String(address))
	})).Finish()
}

// FmtDebug is the derived Debug of Host::Ipv6: `Ipv6(::1)`, with the
// address as the standard library writes it (stdIpv6AddrString), not as the
// URL Standard serializes it.
func (h *Host_Ipv6) FmtDebug(f *rs.Formatter) error {
	address := h.F0
	return f.DebugTuple("Ipv6").Field(rs.FmtFromFnOf(func(f *rs.Formatter) error {
		return f.Pad(stdIpv6AddrString(address))
	})).Finish()
}

func (h *Host_Domain) Equal(other Host) bool {
	o, ok := other.(*Host_Domain)
	return ok && h.F0 == o.F0
}

func (h *Host_Ipv4) Equal(other Host) bool {
	o, ok := other.(*Host_Ipv4)
	return ok && h.F0 == o.F0
}

func (h *Host_Ipv6) Equal(other Host) bool {
	o, ok := other.(*Host_Ipv6)
	return ok && h.F0 == o.F0
}

func hostVariantIndex(h Host) int {
	switch h.(type) {
	case *Host_Domain:
		return 0
	case *Host_Ipv4:
		return 1
	case *Host_Ipv6:
		return 2
	default:
		panic(rs.Unreachable(h))
	}
}

func hostCompare(a Host, b Host) int {
	if ai, bi := hostVariantIndex(a), hostVariantIndex(b); ai != bi {
		if ai < bi {
			return -1
		}
		return 1
	}
	switch x := a.(type) {
	case *Host_Domain:
		return strings.Compare(x.F0, b.(*Host_Domain).F0)
	case *Host_Ipv4:
		return x.F0.Compare(b.(*Host_Ipv4).F0)
	case *Host_Ipv6:
		return x.F0.Compare(b.(*Host_Ipv6).F0)
	default:
		panic(rs.Unreachable(a))
	}
}

func (h *Host_Domain) Compare(other Host) int { return hostCompare(h, other) }
func (h *Host_Ipv4) Compare(other Host) int   { return hostCompare(h, other) }
func (h *Host_Ipv6) Compare(other Host) int   { return hostCompare(h, other) }

// hostIpv4String is `impl Display for Ipv4Addr`.
func hostIpv4String(addr netip.Addr) string {
	octets := addr.As4()
	return strconv.Itoa(int(octets[0])) + "." + strconv.Itoa(int(octets[1])) + "." +
		strconv.Itoa(int(octets[2])) + "." + strconv.Itoa(int(octets[3]))
}

// hostIpv6Segments is Ipv6Addr::segments.
func hostIpv6Segments(addr netip.Addr) [8]uint16 {
	octets := addr.As16()
	var segments [8]uint16
	for i := range segments {
		segments[i] = uint16(octets[2*i])<<8 | uint16(octets[2*i+1])
	}
	return segments
}

func hostWriteIpv6(addr netip.Addr) string {
	segments := hostIpv6Segments(addr)
	compressStart, compressEnd := hostLongestZeroSequence(&segments)
	var f strings.Builder
	i := 0
	for i < 8 {
		if i == compressStart {
			f.WriteString(":")
			if i == 0 {
				f.WriteString(":")
			}
			if compressEnd < 8 {
				i = compressEnd
			} else {
				break
			}
		}
		f.WriteString(strconv.FormatUint(uint64(segments[i]), 16))
		if i < 7 {
			f.WriteString(":")
		}
		i++
	}
	return f.String()
}

// https://url.spec.whatwg.org/#concept-ipv6-serializer step 2 and 3
func hostLongestZeroSequence(pieces *[8]uint16) (int, int) {
	longest := -1
	longestLength := -1
	start := -1
	finishSequence := func(end int) {
		if start >= 0 {
			length := end - start
			if length > longestLength {
				longest = start
				longestLength = length
			}
		}
	}
	for i := 0; i < 8; i++ {
		if pieces[i] == 0 {
			if start < 0 {
				start = i
			}
		} else {
			finishSequence(i)
			start = -1
		}
	}
	finishSequence(8)
	// https://url.spec.whatwg.org/#concept-ipv6-serializer
	// step 3: ignore lone zeroes
	if longestLength < 2 {
		return -1, -2
	} else {
		return longest, longest + longestLength
	}
}

func isAllAsciiDigits(s string) bool {
	for i := 0; i < len(s); i++ {
		if s[i] < '0' || s[i] > '9' {
			return false
		}
	}
	return true
}

// <https://url.spec.whatwg.org/#ends-in-a-number-checker>
func hostEndsInANumber(input string) bool {
	parts := strings.Split(input, ".")
	last := parts[len(parts)-1]
	if last == "" {
		if len(parts) >= 2 {
			last = parts[len(parts)-2]
		} else {
			return false
		}
	}
	if last != "" && isAllAsciiDigits(last) {
		return true
	}

	_, _, ok := hostParseIpv4number(last)
	return ok
}

// <https://url.spec.whatwg.org/#ipv4-number-parser>
// The second result is false when the input is a valid number, but it
// overflows a `u32` (Ok(None) in Rust); the third is false when the input is
// not a number (Err(())).
func hostParseIpv4number(input string) (uint32, bool, bool) {
	if input == "" {
		return 0, false, false
	}

	r := 10
	if strings.HasPrefix(input, "0x") || strings.HasPrefix(input, "0X") {
		input = input[2:]
		r = 16
	} else if len(input) >= 2 && strings.HasPrefix(input, "0") {
		input = input[1:]
		r = 8
	}

	if input == "" {
		return 0, true, true
	}

	validNumber := true
	for i := 0; i < len(input); i++ {
		c := input[i]
		switch r {
		case 8:
			validNumber = '0' <= c && c <= '7'
		case 10:
			validNumber = '0' <= c && c <= '9'
		case 16:
			validNumber = isAsciiHexdigit(rune(c))
		}
		if !validNumber {
			break
		}
	}
	if !validNumber {
		return 0, false, false
	}

	num, err := strconv.ParseUint(input, r, 32)
	if err != nil {
		// The only possible error kind here is an integer overflow.
		// The validity of the chars in the input is checked above.
		return 0, false, true
	}
	return uint32(num), true, true
}

// <https://url.spec.whatwg.org/#concept-ipv4-parser>
func hostParseIpv4addr(input string) (netip.Addr, error) {
	parts := strings.Split(input, ".")
	if parts[len(parts)-1] == "" {
		parts = parts[:len(parts)-1]
	}
	if len(parts) > 4 {
		return netip.Addr{}, ParseError_InvalidIpv4Address
	}
	var numbers []uint32
	for _, part := range parts {
		n, fits, ok := hostParseIpv4number(part)
		if !ok {
			return netip.Addr{}, ParseError_InvalidIpv4Address
		}
		if !fits {
			return netip.Addr{}, ParseError_InvalidIpv4Address // u32 overflow
		}
		numbers = append(numbers, n)
	}
	if len(numbers) == 0 {
		rs.PanicAt(0, "a non-empty list of numbers")
	}
	ipv4 := numbers[len(numbers)-1]
	numbers = numbers[:len(numbers)-1]
	// Equivalent to: ipv4 >= 256 ** (4 − numbers.len())
	if ipv4 > uint32(0xFFFFFFFF)>>(8*uint32(len(numbers))) {
		return netip.Addr{}, ParseError_InvalidIpv4Address
	}
	for _, x := range numbers {
		if x > 255 {
			return netip.Addr{}, ParseError_InvalidIpv4Address
		}
	}
	for counter, n := range numbers {
		ipv4 += n << (8 * (3 - uint32(counter)))
	}
	return netip.AddrFrom4([4]byte{byte(ipv4 >> 24), byte(ipv4 >> 16), byte(ipv4 >> 8), byte(ipv4)}), nil
}

// <https://url.spec.whatwg.org/#concept-ipv6-parser>
func hostParseIpv6addr(inputStr string) (netip.Addr, error) {
	input := []byte(inputStr)
	length := len(input)
	isIpV4 := false
	pieces := [8]uint16{0, 0, 0, 0, 0, 0, 0, 0}
	piecePointer := 0
	compressPointer := -1
	i := 0

	fail := func() (netip.Addr, error) { return netip.Addr{}, ParseError_InvalidIpv6Address }

	if length < 2 {
		return fail()
	}

	if input[0] == ':' {
		if input[1] != ':' {
			return fail()
		}
		i = 2
		piecePointer = 1
		compressPointer = 1
	}

	for i < length {
		if piecePointer == 8 {
			return fail()
		}
		if input[i] == ':' {
			if compressPointer >= 0 {
				return fail()
			}
			i++
			piecePointer++
			compressPointer = piecePointer
			continue
		}
		start := i
		end := min(length, start+4)
		value := uint16(0)
		for i < end {
			digit, ok := hexDigitValue(input[i])
			if !ok {
				break
			}
			value = value*0x10 + uint16(digit)
			i++
		}
		if i < length {
			switch input[i] {
			case '.':
				if i == start {
					return fail()
				}
				i = start
				if piecePointer > 6 {
					return fail()
				}
				isIpV4 = true
			case ':':
				i++
				if i == length {
					return fail()
				}
			default:
				return fail()
			}
		}
		if isIpV4 {
			break
		}
		pieces[piecePointer] = value
		piecePointer++
	}

	if isIpV4 {
		if piecePointer > 6 {
			return fail()
		}
		numbersSeen := 0
		for i < length {
			if numbersSeen > 0 {
				if numbersSeen < 4 && (i < length && input[i] == '.') {
					i++
				} else {
					return fail()
				}
			}

			ipv4Piece := -1
			for i < length {
				c := input[i]
				if c < '0' || c > '9' {
					break
				}
				digit := int(c - '0')
				switch {
				case ipv4Piece < 0:
					ipv4Piece = digit
				case ipv4Piece == 0:
					return fail() // No leading zero
				default:
					ipv4Piece = ipv4Piece*10 + digit
					if ipv4Piece > 255 {
						return fail()
					}
				}
				i++
			}

			if ipv4Piece >= 0 {
				pieces[piecePointer] = pieces[piecePointer]*0x100 + uint16(ipv4Piece)
			} else {
				return fail()
			}
			numbersSeen++

			if numbersSeen == 2 || numbersSeen == 4 {
				piecePointer++
			}
		}

		if numbersSeen != 4 {
			return fail()
		}
	}

	if i < length {
		return fail()
	}

	if compressPointer >= 0 {
		swaps := piecePointer - compressPointer
		piecePointer = 7
		for swaps > 0 {
			pieces[piecePointer], pieces[compressPointer+swaps-1] = pieces[compressPointer+swaps-1], pieces[piecePointer]
			swaps--
			piecePointer--
		}
	} else {
		if piecePointer != 8 {
			return fail()
		}
	}
	var octets [16]byte
	for index, piece := range pieces {
		octets[2*index] = byte(piece >> 8)
		octets[2*index+1] = byte(piece)
	}
	return netip.AddrFrom16(octets), nil
}
