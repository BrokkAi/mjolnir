// Port of core 1.99.1 src/str/lossy.rs

package strs

// Utf8Chunk is str::Utf8Chunk: a run of valid UTF-8 and the invalid bytes
// that follow it.
type Utf8Chunk struct {
	valid   string
	invalid []byte
}

// Valid is Utf8Chunk::valid: the next validated UTF-8 substring. It is empty
// only when the chunk starts with an invalid sequence.
func (c Utf8Chunk) Valid() string { return c.valid }

// Invalid is Utf8Chunk::invalid: the invalid sequence that ended the valid
// run, at most three bytes long. It is empty only in the last chunk of input
// that ends in valid UTF-8. A lossy conversion replaces it with one U+FFFD.
func (c Utf8Chunk) Invalid() []byte { return c.invalid }

// Utf8Chunks is `<[u8]>::utf8_chunks`: v cut into runs of valid UTF-8, each
// followed by the maximal invalid sequence that ended it. Rust's is an
// iterator; this returns every chunk. The invalid parts alias v.
func Utf8Chunks(v []byte) []Utf8Chunk {
	var chunks []Utf8Chunk
	source := v
	for len(source) > 0 {
		chunk, rest := nextUtf8Chunk(source)
		chunks = append(chunks, chunk)
		source = rest
	}
	return chunks
}

// nextUtf8Chunk is `<Utf8Chunks as Iterator>::next` for a source that is not
// empty: the chunk and what remains after it.
func nextUtf8Chunk(source []byte) (Utf8Chunk, []byte) {
	const tagContU8 = 128
	safeGet := func(xs []byte, i int) byte {
		if i < len(xs) {
			return xs[i]
		}
		return 0
	}

	i := 0
	validUpTo := 0
scan:
	for i < len(source) {
		b := source[i]
		i++

		if b < 128 {
			// This could be a `1 => ...` case in the match below, but for
			// the common case of all-ASCII inputs, we bypass loading the
			// sizeable UTF8_CHAR_WIDTH table into cache.
		} else {
			w := utf8CharWidth(b)

			switch w {
			case 2:
				if safeGet(source, i)&192 != tagContU8 {
					break scan
				}
				i++
			case 3:
				second := safeGet(source, i)
				switch {
				case b == 0xE0 && second >= 0xA0 && second <= 0xBF:
				case b >= 0xE1 && b <= 0xEC && second >= 0x80 && second <= 0xBF:
				case b == 0xED && second >= 0x80 && second <= 0x9F:
				case b >= 0xEE && b <= 0xEF && second >= 0x80 && second <= 0xBF:
				default:
					break scan
				}
				i++
				if safeGet(source, i)&192 != tagContU8 {
					break scan
				}
				i++
			case 4:
				second := safeGet(source, i)
				switch {
				case b == 0xF0 && second >= 0x90 && second <= 0xBF:
				case b >= 0xF1 && b <= 0xF3 && second >= 0x80 && second <= 0xBF:
				case b == 0xF4 && second >= 0x80 && second <= 0x8F:
				default:
					break scan
				}
				i++
				if safeGet(source, i)&192 != tagContU8 {
					break scan
				}
				i++
				if safeGet(source, i)&192 != tagContU8 {
					break scan
				}
				i++
			default:
				break scan
			}
		}

		validUpTo = i
	}

	inspected, remaining := source[:i], source[i:]
	return Utf8Chunk{valid: string(inspected[:validUpTo]), invalid: inspected[validUpTo:]}, remaining
}
