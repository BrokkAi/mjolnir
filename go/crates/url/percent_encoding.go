// Port of percent-encoding 2.3.2 src/lib.rs and src/ascii_set.rs

package url

// The percent-encoding crate is a dependency of url and is not used by
// Mjolnir on its own, so its items stay unexported here. They carry the
// prefix percentEncoding, the crate's name.

// percentEncodingAsciiSet is percent_encoding::AsciiSet: a set of ASCII
// bytes (U+0000 to U+007F) that should be percent-encoded. Bytes outside the
// ASCII range are always encoded.
type percentEncodingAsciiSet struct {
	mask [percentEncodingAsciiRangeLen / percentEncodingBitsPerChunk]uint32
}

const percentEncodingAsciiRangeLen = 0x80

const percentEncodingBitsPerChunk = 32

// percentEncodingAsciiSetEmpty is AsciiSet::EMPTY.
var percentEncodingAsciiSetEmpty = percentEncodingAsciiSet{mask: [4]uint32{0, 0, 0, 0}}

// contains is AsciiSet::contains. Like the Rust method it indexes out of
// range for a byte above 0x7F; callers ask shouldPercentEncode.
func (set *percentEncodingAsciiSet) contains(b byte) bool {
	chunk := set.mask[int(b)/percentEncodingBitsPerChunk]
	mask := uint32(1) << (uint(b) % percentEncodingBitsPerChunk)
	return chunk&mask != 0
}

func (set *percentEncodingAsciiSet) shouldPercentEncode(b byte) bool {
	return b >= 0x80 || set.contains(b)
}

// add is AsciiSet::add.
func (set *percentEncodingAsciiSet) add(b byte) *percentEncodingAsciiSet {
	mask := set.mask
	mask[int(b)/percentEncodingBitsPerChunk] |= uint32(1) << (uint(b) % percentEncodingBitsPerChunk)
	return &percentEncodingAsciiSet{mask: mask}
}

// remove is AsciiSet::remove.
func (set *percentEncodingAsciiSet) remove(b byte) *percentEncodingAsciiSet {
	mask := set.mask
	mask[int(b)/percentEncodingBitsPerChunk] &^= uint32(1) << (uint(b) % percentEncodingBitsPerChunk)
	return &percentEncodingAsciiSet{mask: mask}
}

// union is AsciiSet::union, and the `+` operator.
func (set *percentEncodingAsciiSet) union(other *percentEncodingAsciiSet) *percentEncodingAsciiSet {
	return &percentEncodingAsciiSet{mask: [4]uint32{
		set.mask[0] | other.mask[0],
		set.mask[1] | other.mask[1],
		set.mask[2] | other.mask[2],
		set.mask[3] | other.mask[3],
	}}
}

// complement is AsciiSet::complement, and the `!` operator.
func (set *percentEncodingAsciiSet) complement() *percentEncodingAsciiSet {
	return &percentEncodingAsciiSet{mask: [4]uint32{^set.mask[0], ^set.mask[1], ^set.mask[2], ^set.mask[3]}}
}

func (set *percentEncodingAsciiSet) addAll(bytes string) *percentEncodingAsciiSet {
	out := set
	for i := 0; i < len(bytes); i++ {
		out = out.add(bytes[i])
	}
	return out
}

// percentEncodingControls is percent_encoding::CONTROLS: the set of 0x00 to
// 0x1F (C0 controls), and 0x7F (DEL).
var percentEncodingControls = &percentEncodingAsciiSet{mask: [4]uint32{
	^uint32(0), // C0: 0x00 to 0x1F (32 bits set)
	0,
	0,
	1 << (0x7F % 32), // DEL: 0x7F (one bit set)
}}

// percentEncodingNonAlphanumeric is percent_encoding::NON_ALPHANUMERIC:
// everything that is not an ASCII letter or digit.
var percentEncodingNonAlphanumeric = percentEncodingControls.addAll(" !\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~")

const percentEncodingEncTable = "" +
	"%00%01%02%03%04%05%06%07%08%09%0A%0B%0C%0D%0E%0F" +
	"%10%11%12%13%14%15%16%17%18%19%1A%1B%1C%1D%1E%1F" +
	"%20%21%22%23%24%25%26%27%28%29%2A%2B%2C%2D%2E%2F" +
	"%30%31%32%33%34%35%36%37%38%39%3A%3B%3C%3D%3E%3F" +
	"%40%41%42%43%44%45%46%47%48%49%4A%4B%4C%4D%4E%4F" +
	"%50%51%52%53%54%55%56%57%58%59%5A%5B%5C%5D%5E%5F" +
	"%60%61%62%63%64%65%66%67%68%69%6A%6B%6C%6D%6E%6F" +
	"%70%71%72%73%74%75%76%77%78%79%7A%7B%7C%7D%7E%7F" +
	"%80%81%82%83%84%85%86%87%88%89%8A%8B%8C%8D%8E%8F" +
	"%90%91%92%93%94%95%96%97%98%99%9A%9B%9C%9D%9E%9F" +
	"%A0%A1%A2%A3%A4%A5%A6%A7%A8%A9%AA%AB%AC%AD%AE%AF" +
	"%B0%B1%B2%B3%B4%B5%B6%B7%B8%B9%BA%BB%BC%BD%BE%BF" +
	"%C0%C1%C2%C3%C4%C5%C6%C7%C8%C9%CA%CB%CC%CD%CE%CF" +
	"%D0%D1%D2%D3%D4%D5%D6%D7%D8%D9%DA%DB%DC%DD%DE%DF" +
	"%E0%E1%E2%E3%E4%E5%E6%E7%E8%E9%EA%EB%EC%ED%EE%EF" +
	"%F0%F1%F2%F3%F4%F5%F6%F7%F8%F9%FA%FB%FC%FD%FE%FF"

// percentEncodingPercentEncodeByte is percent_encoding::percent_encode_byte:
// the percent-encoding of the given byte, unconditionally.
func percentEncodingPercentEncodeByte(b byte) string {
	index := int(b) * 3
	return percentEncodingEncTable[index : index+3]
}

// percentEncodingPercentEncode is the PercentEncode iterator that
// percent_encoding::percent_encode returns. Next yields the same string
// slices the Rust iterator yields.
type percentEncodingPercentEncode struct {
	bytes    []byte
	asciiSet *percentEncodingAsciiSet
}

func percentEncodingPercentEncodeNew(input []byte, asciiSet *percentEncodingAsciiSet) percentEncodingPercentEncode {
	return percentEncodingPercentEncode{bytes: input, asciiSet: asciiSet}
}

// percentEncodingUtf8PercentEncode is percent_encoding::utf8_percent_encode.
func percentEncodingUtf8PercentEncode(input string, asciiSet *percentEncodingAsciiSet) percentEncodingPercentEncode {
	return percentEncodingPercentEncodeNew([]byte(input), asciiSet)
}

func (p *percentEncodingPercentEncode) next() (string, bool) {
	if len(p.bytes) == 0 {
		return "", false
	}
	firstByte := p.bytes[0]
	remaining := p.bytes[1:]
	if p.asciiSet.shouldPercentEncode(firstByte) {
		p.bytes = remaining
		return percentEncodingPercentEncodeByte(firstByte), true
	}
	for i, b := range remaining {
		if p.asciiSet.shouldPercentEncode(b) {
			// 1 for first_byte + i for previous iterations of this loop
			unchanged := p.bytes[:1+i]
			p.bytes = p.bytes[1+i:]
			return string(unchanged), true
		}
	}
	unchanged := p.bytes
	p.bytes = nil
	return string(unchanged), true
}

// String is the Display impl: every piece, concatenated. It is also what
// `.collect::<String>()` and `Cow::from` give.
func (p percentEncodingPercentEncode) String() string {
	return string(p.appendTo(nil))
}

// appendTo is `string.extend(percent_encode(..))`.
func (p percentEncodingPercentEncode) appendTo(out []byte) []byte {
	for _, b := range p.bytes {
		if p.asciiSet.shouldPercentEncode(b) {
			index := int(b) * 3
			out = append(out, percentEncodingEncTable[index:index+3]...)
		} else {
			out = append(out, b)
		}
	}
	return out
}

func appendUtf8PercentEncode(out []byte, input string, asciiSet *percentEncodingAsciiSet) []byte {
	for i := 0; i < len(input); i++ {
		b := input[i]
		if asciiSet.shouldPercentEncode(b) {
			index := int(b) * 3
			out = append(out, percentEncodingEncTable[index:index+3]...)
		} else {
			out = append(out, b)
		}
	}
	return out
}

// percentEncodingPercentDecode is the PercentDecode iterator that
// percent_encoding::percent_decode returns.
type percentEncodingPercentDecode struct {
	bytes []byte
}

func percentEncodingPercentDecodeNew(input []byte) percentEncodingPercentDecode {
	return percentEncodingPercentDecode{bytes: input}
}

// percentEncodingPercentDecodeStr is percent_encoding::percent_decode_str.
func percentEncodingPercentDecodeStr(input string) percentEncodingPercentDecode {
	return percentEncodingPercentDecodeNew([]byte(input))
}

func hexDigitValue(b byte) (byte, bool) {
	switch {
	case '0' <= b && b <= '9':
		return b - '0', true
	case 'a' <= b && b <= 'f':
		return b - 'a' + 10, true
	case 'A' <= b && b <= 'F':
		return b - 'A' + 10, true
	}
	return 0, false
}

// percentEncodingAfterPercentSign decodes the two hexadecimal digits at the
// start of rest, if they are there.
func percentEncodingAfterPercentSign(rest []byte) (byte, bool) {
	if len(rest) < 2 {
		return 0, false
	}
	h, ok := hexDigitValue(rest[0])
	if !ok {
		return 0, false
	}
	l, ok := hexDigitValue(rest[1])
	if !ok {
		return 0, false
	}
	return h*0x10 + l, true
}

func (p *percentEncodingPercentDecode) next() (byte, bool) {
	if len(p.bytes) == 0 {
		return 0, false
	}
	b := p.bytes[0]
	p.bytes = p.bytes[1:]
	if b == '%' {
		if decoded, ok := percentEncodingAfterPercentSign(p.bytes); ok {
			p.bytes = p.bytes[2:]
			return decoded, true
		}
	}
	return b, true
}

// ifAny is PercentDecode::if_any: the decoded bytes when at least one
// percent-encoded sequence is decoded, and false when the input decodes to
// itself.
func (p percentEncodingPercentDecode) ifAny() ([]byte, bool) {
	for i := 0; i < len(p.bytes); i++ {
		if p.bytes[i] != '%' {
			continue
		}
		if decodedByte, ok := percentEncodingAfterPercentSign(p.bytes[i+1:]); ok {
			decoded := make([]byte, 0, len(p.bytes))
			decoded = append(decoded, p.bytes[:i]...)
			decoded = append(decoded, decodedByte)
			rest := percentEncodingPercentDecode{bytes: p.bytes[i+3:]}
			for {
				b, ok := rest.next()
				if !ok {
					break
				}
				decoded = append(decoded, b)
			}
			return decoded, true
		}
	}
	// Nothing to decode
	return nil, false
}

// collect is `Cow::from(iter)` and `.collect::<Vec<u8>>()`: the decoded
// bytes. The second result is true when anything was decoded (Cow::Owned).
func (p percentEncodingPercentDecode) collect() ([]byte, bool) {
	if decoded, ok := p.ifAny(); ok {
		return decoded, true
	}
	return p.bytes, false
}

// decodeUtf8 is PercentDecode::decode_utf8: the decoded bytes as a string,
// or the error std::str::from_utf8 gives.
func (p percentEncodingPercentDecode) decodeUtf8() (string, error) {
	bytes, _ := p.collect()
	if err := runUtf8Validation(bytes); err != nil {
		return "", err
	}
	return string(bytes), nil
}

// decodeUtf8Lossy is PercentDecode::decode_utf8_lossy.
func (p percentEncodingPercentDecode) decodeUtf8Lossy() string {
	bytes, _ := p.collect()
	return stringFromUtf8Lossy(bytes)
}
