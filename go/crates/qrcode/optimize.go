// Port of qrcode 0.14.1 src/optimize.rs

package qrcode

import "github.com/BrokkAi/mjolnir/go/rs"

// Find the optimal data mode sequence to encode a piece of data.

//------------------------------------------------------------------------------
//{{{ Segment

// OptimizeSegment is qrcode::optimize::Segment: a segment of data committed
// to an encoding mode.
type OptimizeSegment struct {
	// The encoding mode of the segment of data.
	Mode TypesMode `rs:"mode"`

	// The start index of the segment.
	Begin int `rs:"begin"`

	// The end index (exclusive) of the segment.
	End int `rs:"end"`
}

var _ = rs.RegisterStruct[OptimizeSegment]("Segment")

// EncodedLen is Segment::encoded_len: compute the number of bits (including
// the size of the mode indicator and length bits) when this segment is
// encoded.
func (s OptimizeSegment) EncodedLen(version Version) int {
	byteSize := s.End - s.Begin
	charsCount := byteSize
	if s.Mode == TypesMode_Kanji {
		charsCount = byteSize / 2
	}

	modeBitsCount := version.ModeBitsCount()
	lengthBitsCount := s.Mode.LengthBitsCount(version)
	dataBitsCount := s.Mode.DataBitsCount(charsCount)

	return modeBitsCount + lengthBitsCount + dataBitsCount
}

// OptimizeSegmentIter is `I: Iterator<Item = Segment>`, what Optimizer reads
// and Bits.PushSegments writes. OptimizeParser and OptimizeOptimizer are
// such iterators.
type OptimizeSegmentIter interface {
	Next() rs.Option[OptimizeSegment]
}

// optimizeSliceIter is `segments.iter().copied()` over a slice.
type optimizeSliceIter struct {
	segments []OptimizeSegment
}

func (it *optimizeSliceIter) Next() rs.Option[OptimizeSegment] {
	if len(it.segments) == 0 {
		return rs.None[OptimizeSegment]()
	}
	segment := it.segments[0]
	it.segments = it.segments[1:]
	return rs.Some(segment)
}

// optimizeCollect is `.collect::<Vec<Segment>>()`.
func optimizeCollect(it OptimizeSegmentIter) []OptimizeSegment {
	segments := []OptimizeSegment{}
	for {
		segment, ok := it.Next().Get()
		if !ok {
			return segments
		}
		segments = append(segments, segment)
	}
}

//}}}
//------------------------------------------------------------------------------
//{{{ Parser

// optimizeEcsIter is basically equivalent to
//
//	data.map(|c| ExclCharSet::from_u8(*c))
//	    .chain(Some(ExclCharSet::End).move_iter())
//	    .enumerate()
//
// But the type is too hard to write, thus the new type.
type optimizeEcsIter struct {
	base  []byte
	index int
	ended bool
}

func (it *optimizeEcsIter) next() (int, optimizeExclCharSet, bool) {
	if it.ended {
		return 0, 0, false
	}

	if len(it.base) == 0 {
		it.ended = true
		return it.index, optimizeExclCharSet_End, true
	}
	c := it.base[0]
	it.base = it.base[1:]
	oldIndex := it.index
	it.index++
	return oldIndex, optimizeExclCharSetFromU8(c), true
}

// OptimizeParser is qrcode::optimize::Parser: QR code data parser to
// classify the input into distinct segments.
type OptimizeParser struct {
	ecsIter           optimizeEcsIter
	state             optimizeState
	begin             int
	pendingSingleByte bool
}

// OptimizeParserNew is Parser::new: creates a new iterator which parse the
// data into segments that only contains their exclusive subsets. No
// optimization is done at this point.
//
//	OptimizeParserNew([]byte("ABC123abcd")).Collect() ==
//	    []OptimizeSegment{{Alphanumeric, 0, 3}, {Numeric, 3, 6}, {Byte, 6, 10}}
func OptimizeParserNew(data []byte) *OptimizeParser {
	return &OptimizeParser{
		ecsIter:           optimizeEcsIter{base: data, index: 0, ended: false},
		state:             optimizeState_Init,
		begin:             0,
		pendingSingleByte: false,
	}
}

// Next is Iterator::next.
func (p *OptimizeParser) Next() rs.Option[OptimizeSegment] {
	if p.pendingSingleByte {
		p.pendingSingleByte = false
		p.begin++
		return rs.Some(OptimizeSegment{Mode: TypesMode_Byte, Begin: p.begin - 1, End: p.begin})
	}

	for {
		i, ecs, ok := p.ecsIter.next()
		if !ok {
			return rs.None[OptimizeSegment]()
		}
		transition := optimizeStateTransition[int(p.state)+int(ecs)]
		p.state = transition.state

		oldBegin := p.begin
		var pushMode TypesMode
		switch transition.action {
		case optimizeAction_Idle:
			continue
		case optimizeAction_Numeric:
			pushMode = TypesMode_Numeric
		case optimizeAction_Alpha:
			pushMode = TypesMode_Alphanumeric
		case optimizeAction_Byte:
			pushMode = TypesMode_Byte
		case optimizeAction_Kanji:
			pushMode = TypesMode_Kanji
		case optimizeAction_KanjiAndSingleByte:
			nextBegin := i - 1
			if p.begin == nextBegin {
				pushMode = TypesMode_Byte
			} else {
				p.pendingSingleByte = true
				p.begin = nextBegin
				return rs.Some(OptimizeSegment{Mode: TypesMode_Kanji, Begin: oldBegin, End: nextBegin})
			}
		default:
			panic(rs.Unreachable(transition.action))
		}

		p.begin = i
		return rs.Some(OptimizeSegment{Mode: pushMode, Begin: oldBegin, End: i})
	}
}

// Collect is `.collect::<Vec<Segment>>()`.
func (p *OptimizeParser) Collect() []OptimizeSegment {
	return optimizeCollect(p)
}

//}}}
//------------------------------------------------------------------------------
//{{{ Optimizer

// OptimizeOptimizer is qrcode::optimize::Optimizer.
type OptimizeOptimizer struct {
	parser          OptimizeSegmentIter
	lastSegment     OptimizeSegment
	lastSegmentSize int
	version         Version
	ended           bool
}

// OptimizeOptimizerNew is Optimizer::new: optimize the segments by combining
// adjacent segments when possible.
//
// Currently this method uses a greedy algorithm by combining segments from
// left to right until the new segment is longer than before. This method
// does *not* use Annex J from the ISO standard.
func OptimizeOptimizerNew(segments OptimizeSegmentIter, version Version) *OptimizeOptimizer {
	segment, ok := segments.Next().Get()
	if !ok {
		return &OptimizeOptimizer{
			parser:          segments,
			lastSegment:     OptimizeSegment{Mode: TypesMode_Numeric, Begin: 0, End: 0},
			lastSegmentSize: 0,
			version:         version,
			ended:           true,
		}
	}
	return &OptimizeOptimizer{
		parser:          segments,
		lastSegment:     segment,
		lastSegmentSize: segment.EncodedLen(version),
		version:         version,
		ended:           false,
	}
}

// Optimize is Parser::optimize.
func (p *OptimizeParser) Optimize(version Version) *OptimizeOptimizer {
	return OptimizeOptimizerNew(p, version)
}

// Next is Iterator::next.
func (o *OptimizeOptimizer) Next() rs.Option[OptimizeSegment] {
	if o.ended {
		return rs.None[OptimizeSegment]()
	}

	for {
		segment, ok := o.parser.Next().Get()
		if !ok {
			o.ended = true
			return rs.Some(o.lastSegment)
		}
		segSize := segment.EncodedLen(o.version)

		newSegment := OptimizeSegment{
			Mode:  o.lastSegment.Mode.Max(segment.Mode),
			Begin: o.lastSegment.Begin,
			End:   segment.End,
		}
		newSize := newSegment.EncodedLen(o.version)

		if o.lastSegmentSize+segSize >= newSize {
			o.lastSegment = newSegment
			o.lastSegmentSize = newSize
		} else {
			oldSegment := o.lastSegment
			o.lastSegment = segment
			o.lastSegmentSize = segSize
			return rs.Some(oldSegment)
		}
	}
}

// Collect is `.collect::<Vec<Segment>>()`.
func (o *OptimizeOptimizer) Collect() []OptimizeSegment {
	return optimizeCollect(o)
}

// OptimizeTotalEncodedLen is optimize::total_encoded_len: computes the total
// encoded length of all segments.
func OptimizeTotalEncodedLen(segments []OptimizeSegment, version Version) int {
	total := 0
	for _, seg := range segments {
		total += seg.EncodedLen(version)
	}
	return total
}

//}}}
//------------------------------------------------------------------------------
//{{{ Internal types and data for parsing

// optimizeExclCharSet is ExclCharSet: all values of `u8` can be split into 9
// different character sets when determining which encoding to use. This enum
// represents these groupings for parsing purpose.
type optimizeExclCharSet uint8

const (
	// The end of string.
	optimizeExclCharSet_End optimizeExclCharSet = 0

	// All symbols supported by the Alphanumeric encoding, i.e. space, `$`, `%`,
	// `*`, `+`, `-`, `.`, `/` and `:`.
	optimizeExclCharSet_Symbol optimizeExclCharSet = 1

	// All numbers (0–9).
	optimizeExclCharSet_Numeric optimizeExclCharSet = 2

	// All uppercase letters (A–Z). These characters may also appear in the
	// second byte of a Shift JIS 2-byte encoding.
	optimizeExclCharSet_Alpha optimizeExclCharSet = 3

	// The first byte of a Shift JIS 2-byte encoding, in the range 0x81–0x9f.
	optimizeExclCharSet_KanjiHi1 optimizeExclCharSet = 4

	// The first byte of a Shift JIS 2-byte encoding, in the range 0xe0–0xea.
	optimizeExclCharSet_KanjiHi2 optimizeExclCharSet = 5

	// The first byte of a Shift JIS 2-byte encoding, of value 0xeb. This is
	// different from the other two range that the second byte has a smaller
	// range.
	optimizeExclCharSet_KanjiHi3 optimizeExclCharSet = 6

	// The second byte of a Shift JIS 2-byte encoding, in the range 0x40–0xbf,
	// excluding letters (covered by `Alpha`), 0x81–0x9f (covered by `KanjiHi1`),
	// and the invalid byte 0x7f.
	optimizeExclCharSet_KanjiLo1 optimizeExclCharSet = 7

	// The second byte of a Shift JIS 2-byte encoding, in the range 0xc0–0xfc,
	// excluding the range 0xe0–0xeb (covered by `KanjiHi2` and `KanjiHi3`).
	// This half of byte-pair cannot appear as the second byte leaded by
	// `KanjiHi3`.
	optimizeExclCharSet_KanjiLo2 optimizeExclCharSet = 8

	// Any other values not covered by the above character sets.
	optimizeExclCharSet_Byte optimizeExclCharSet = 9
)

// optimizeExclCharSetFromU8 is ExclCharSet::from_u8: determines which
// character set a byte is in.
func optimizeExclCharSetFromU8(c byte) optimizeExclCharSet {
	switch {
	case c == 0x20 || c == 0x24 || c == 0x25 || c == 0x2a || c == 0x2b || (0x2d <= c && c <= 0x2f) || c == 0x3a:
		return optimizeExclCharSet_Symbol
	case 0x30 <= c && c <= 0x39:
		return optimizeExclCharSet_Numeric
	case 0x41 <= c && c <= 0x5a:
		return optimizeExclCharSet_Alpha
	case 0x81 <= c && c <= 0x9f:
		return optimizeExclCharSet_KanjiHi1
	case 0xe0 <= c && c <= 0xea:
		return optimizeExclCharSet_KanjiHi2
	case c == 0xeb:
		return optimizeExclCharSet_KanjiHi3
	case c == 0x40 || (0x5b <= c && c <= 0x7e) || c == 0x80 || (0xa0 <= c && c <= 0xbf):
		return optimizeExclCharSet_KanjiLo1
	case (0xc0 <= c && c <= 0xdf) || (0xec <= c && c <= 0xfc):
		return optimizeExclCharSet_KanjiLo2
	default:
		return optimizeExclCharSet_Byte
	}
}

// optimizeState is State: the current parsing state.
type optimizeState uint8

const (
	// Just initialized.
	optimizeState_Init optimizeState = 0

	// Inside a string that can be exclusively encoded as Numeric.
	optimizeState_Numeric optimizeState = 10

	// Inside a string that can be exclusively encoded as Alphanumeric.
	optimizeState_Alpha optimizeState = 20

	// Inside a string that can be exclusively encoded as 8-Bit Byte.
	optimizeState_Byte optimizeState = 30

	// Just encountered the first byte of a Shift JIS 2-byte sequence of the
	// set `KanjiHi1` or `KanjiHi2`.
	optimizeState_KanjiHi12 optimizeState = 40

	// Just encountered the first byte of a Shift JIS 2-byte sequence of the
	// set `KanjiHi3`.
	optimizeState_KanjiHi3 optimizeState = 50

	// Inside a string that can be exclusively encoded as Kanji.
	optimizeState_Kanji optimizeState = 60
)

// optimizeAction is Action: what should the parser do after a state
// transition.
type optimizeAction uint8

const (
	// The parser should do nothing.
	optimizeAction_Idle optimizeAction = iota

	// Push the current segment as a Numeric string, and reset the marks.
	optimizeAction_Numeric

	// Push the current segment as an Alphanumeric string, and reset the marks.
	optimizeAction_Alpha

	// Push the current segment as a 8-Bit Byte string, and reset the marks.
	optimizeAction_Byte

	// Push the current segment as a Kanji string, and reset the marks.
	optimizeAction_Kanji

	// Push the current segment excluding the last byte as a Kanji string, then
	// push the remaining single byte as a Byte string, and reset the marks.
	optimizeAction_KanjiAndSingleByte
)

type optimizeTransition struct {
	state  optimizeState
	action optimizeAction
}

var optimizeStateTransition = [70]optimizeTransition{
	// STATE_TRANSITION[current_state + next_character] == (next_state, what_to_do)

	// Init state:
	{optimizeState_Init, optimizeAction_Idle},      // End
	{optimizeState_Alpha, optimizeAction_Idle},     // Symbol
	{optimizeState_Numeric, optimizeAction_Idle},   // Numeric
	{optimizeState_Alpha, optimizeAction_Idle},     // Alpha
	{optimizeState_KanjiHi12, optimizeAction_Idle}, // KanjiHi1
	{optimizeState_KanjiHi12, optimizeAction_Idle}, // KanjiHi2
	{optimizeState_KanjiHi3, optimizeAction_Idle},  // KanjiHi3
	{optimizeState_Byte, optimizeAction_Idle},      // KanjiLo1
	{optimizeState_Byte, optimizeAction_Idle},      // KanjiLo2
	{optimizeState_Byte, optimizeAction_Idle},      // Byte
	// Numeric state:
	{optimizeState_Init, optimizeAction_Numeric},      // End
	{optimizeState_Alpha, optimizeAction_Numeric},     // Symbol
	{optimizeState_Numeric, optimizeAction_Idle},      // Numeric
	{optimizeState_Alpha, optimizeAction_Numeric},     // Alpha
	{optimizeState_KanjiHi12, optimizeAction_Numeric}, // KanjiHi1
	{optimizeState_KanjiHi12, optimizeAction_Numeric}, // KanjiHi2
	{optimizeState_KanjiHi3, optimizeAction_Numeric},  // KanjiHi3
	{optimizeState_Byte, optimizeAction_Numeric},      // KanjiLo1
	{optimizeState_Byte, optimizeAction_Numeric},      // KanjiLo2
	{optimizeState_Byte, optimizeAction_Numeric},      // Byte
	// Alpha state:
	{optimizeState_Init, optimizeAction_Alpha},      // End
	{optimizeState_Alpha, optimizeAction_Idle},      // Symbol
	{optimizeState_Numeric, optimizeAction_Alpha},   // Numeric
	{optimizeState_Alpha, optimizeAction_Idle},      // Alpha
	{optimizeState_KanjiHi12, optimizeAction_Alpha}, // KanjiHi1
	{optimizeState_KanjiHi12, optimizeAction_Alpha}, // KanjiHi2
	{optimizeState_KanjiHi3, optimizeAction_Alpha},  // KanjiHi3
	{optimizeState_Byte, optimizeAction_Alpha},      // KanjiLo1
	{optimizeState_Byte, optimizeAction_Alpha},      // KanjiLo2
	{optimizeState_Byte, optimizeAction_Alpha},      // Byte
	// Byte state:
	{optimizeState_Init, optimizeAction_Byte},      // End
	{optimizeState_Alpha, optimizeAction_Byte},     // Symbol
	{optimizeState_Numeric, optimizeAction_Byte},   // Numeric
	{optimizeState_Alpha, optimizeAction_Byte},     // Alpha
	{optimizeState_KanjiHi12, optimizeAction_Byte}, // KanjiHi1
	{optimizeState_KanjiHi12, optimizeAction_Byte}, // KanjiHi2
	{optimizeState_KanjiHi3, optimizeAction_Byte},  // KanjiHi3
	{optimizeState_Byte, optimizeAction_Idle},      // KanjiLo1
	{optimizeState_Byte, optimizeAction_Idle},      // KanjiLo2
	{optimizeState_Byte, optimizeAction_Idle},      // Byte
	// KanjiHi12 state:
	{optimizeState_Init, optimizeAction_KanjiAndSingleByte},    // End
	{optimizeState_Alpha, optimizeAction_KanjiAndSingleByte},   // Symbol
	{optimizeState_Numeric, optimizeAction_KanjiAndSingleByte}, // Numeric
	{optimizeState_Kanji, optimizeAction_Idle},                 // Alpha
	{optimizeState_Kanji, optimizeAction_Idle},                 // KanjiHi1
	{optimizeState_Kanji, optimizeAction_Idle},                 // KanjiHi2
	{optimizeState_Kanji, optimizeAction_Idle},                 // KanjiHi3
	{optimizeState_Kanji, optimizeAction_Idle},                 // KanjiLo1
	{optimizeState_Kanji, optimizeAction_Idle},                 // KanjiLo2
	{optimizeState_Byte, optimizeAction_KanjiAndSingleByte},    // Byte
	// KanjiHi3 state:
	{optimizeState_Init, optimizeAction_KanjiAndSingleByte},      // End
	{optimizeState_Alpha, optimizeAction_KanjiAndSingleByte},     // Symbol
	{optimizeState_Numeric, optimizeAction_KanjiAndSingleByte},   // Numeric
	{optimizeState_Kanji, optimizeAction_Idle},                   // Alpha
	{optimizeState_Kanji, optimizeAction_Idle},                   // KanjiHi1
	{optimizeState_KanjiHi12, optimizeAction_KanjiAndSingleByte}, // KanjiHi2
	{optimizeState_KanjiHi3, optimizeAction_KanjiAndSingleByte},  // KanjiHi3
	{optimizeState_Kanji, optimizeAction_Idle},                   // KanjiLo1
	{optimizeState_Byte, optimizeAction_KanjiAndSingleByte},      // KanjiLo2
	{optimizeState_Byte, optimizeAction_KanjiAndSingleByte},      // Byte
	// Kanji state:
	{optimizeState_Init, optimizeAction_Kanji},     // End
	{optimizeState_Alpha, optimizeAction_Kanji},    // Symbol
	{optimizeState_Numeric, optimizeAction_Kanji},  // Numeric
	{optimizeState_Alpha, optimizeAction_Kanji},    // Alpha
	{optimizeState_KanjiHi12, optimizeAction_Idle}, // KanjiHi1
	{optimizeState_KanjiHi12, optimizeAction_Idle}, // KanjiHi2
	{optimizeState_KanjiHi3, optimizeAction_Idle},  // KanjiHi3
	{optimizeState_Byte, optimizeAction_Kanji},     // KanjiLo1
	{optimizeState_Byte, optimizeAction_Kanji},     // KanjiLo2
	{optimizeState_Byte, optimizeAction_Kanji},     // Byte
}

//}}}
