// Port of qrcode 0.14.1 src/bits.rs

package qrcode

import (
	"slices"
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The `bits` module encodes binary data into raw bits used in a QR code.

//------------------------------------------------------------------------------
//{{{ Bits

// BitsBits is qrcode::bits::Bits: the `Bits` structure stores the encoded
// data for a QR code.
type BitsBits struct {
	data      []byte
	bitOffset int
	version   Version
}

// BitsBitsNew is Bits::new: constructs a new, empty bits structure.
func BitsBitsNew(version Version) *BitsBits {
	return &BitsBits{data: []byte{}, bitOffset: 0, version: version}
}

// pushNumber pushes an N-bit big-endian integer to the end of the bits.
//
// Note: It is up to the developer to ensure that `number` really only is
// `n` bit in size. Otherwise the excess bits may stomp on the existing
// ones.
func (b *BitsBits) pushNumber(n int, number uint16) {
	if debugAssertions && !(n == 16 || n < 16 && int(number) < (1<<n)) {
		rs.PanicAt(1, strconv.Itoa(int(number))+" is too big as a "+strconv.Itoa(n)+"-bit number")
	}

	end := b.bitOffset + n
	lastIndex := len(b.data) - 1
	switch {
	case b.bitOffset == 0 && end <= 8:
		b.data = append(b.data, truncateAsU8(number<<(8-end)))
	case b.bitOffset == 0:
		b.data = append(b.data, truncateAsU8(number>>(end-8)))
		b.data = append(b.data, truncateAsU8(number<<(16-end)))
	case end <= 8:
		b.data[bitsIndex(b.data, lastIndex)] |= truncateAsU8(number << (8 - end))
	case end <= 16:
		b.data[bitsIndex(b.data, lastIndex)] |= truncateAsU8(number >> (end - 8))
		b.data = append(b.data, truncateAsU8(number<<(16-end)))
	default:
		b.data[bitsIndex(b.data, lastIndex)] |= truncateAsU8(number >> (end - 8))
		b.data = append(b.data, truncateAsU8(number>>(end-16)))
		b.data = append(b.data, truncateAsU8(number<<(24-end)))
	}
	b.bitOffset = end & 7
}

// bitsIndex is `data[index]` with Rust's panic message for an index out of
// bounds (`data.len().wrapping_sub(1)` of an empty vector is usize::MAX).
func bitsIndex(data []byte, index int) int {
	if index < 0 || index >= len(data) {
		shown := strconv.Itoa(index)
		if index < 0 {
			shown = "18446744073709551615"
		}
		rs.PanicAt(2, "index out of bounds: the len is "+strconv.Itoa(len(data))+" but the index is "+shown)
	}
	return index
}

// pushNumberChecked pushes an N-bit big-endian integer to the end of the
// bits, and check that the number does not overflow the bits.
//
// Returns `Err(QrError::DataTooLong)` on overflow.
func (b *BitsBits) pushNumberChecked(n int, number int) error {
	if n > 16 || number >= (1<<n) {
		return TypesQrError_DataTooLong
	}
	b.pushNumber(n, asU16Usize(number))
	return nil
}

// reserve reserves `n` extra bits of space for pushing.
func (b *BitsBits) reserve(n int) {
	extraBytes := (n + (8-b.bitOffset)%8) / 8
	b.data = slices.Grow(b.data, extraBytes)
}

// IntoBytes is Bits::into_bytes: convert the bits into a bytes vector.
func (b *BitsBits) IntoBytes() []byte {
	return b.data
}

// Len is Bits::len: total number of bits currently pushed.
func (b *BitsBits) Len() int {
	if b.bitOffset == 0 {
		return len(b.data) * 8
	}
	return (len(b.data)-1)*8 + b.bitOffset
}

// IsEmpty is Bits::is_empty: whether there are any bits pushed.
func (b *BitsBits) IsEmpty() bool {
	return len(b.data) == 0
}

// MaxLen is Bits::max_len: the maximum number of bits allowed by the provided
// QR code version and error correction level.
//
// # Errors
//
// Returns `Err(QrError::InvalidVersion)` if it is not valid to use the
// `ecLevel` for the given version (e.g. `Version::Micro(1)` with
// `EcLevel::H`).
func (b *BitsBits) MaxLen(ecLevel EcLevel) (int, error) {
	return VersionFetch(b.version, ecLevel, bitsDataLengths[:])
}

// Version is Bits::version: version of the QR code.
func (b *BitsBits) Version() Version {
	return b.version
}

//}}}
//------------------------------------------------------------------------------
//{{{ Mode indicator

// BitsExtendedMode is qrcode::bits::ExtendedMode: an "extended" mode
// indicator, includes all indicators supported by QR code beyond those
// bearing data. A value is a pointer to one of the variant structs.
type BitsExtendedMode interface{ isBitsExtendedMode() }

// BitsExtendedMode_Eci is ExtendedMode::Eci: ECI mode indicator, to
// introduce an ECI designator.
type BitsExtendedMode_Eci struct{}

// BitsExtendedMode_Data is ExtendedMode::Data: the normal mode to introduce
// data.
type BitsExtendedMode_Data struct{ F0 TypesMode }

// BitsExtendedMode_Fnc1First is ExtendedMode::Fnc1First: FNC-1 mode in the
// first position.
type BitsExtendedMode_Fnc1First struct{}

// BitsExtendedMode_Fnc1Second is ExtendedMode::Fnc1Second: FNC-1 mode in the
// second position.
type BitsExtendedMode_Fnc1Second struct{}

// BitsExtendedMode_StructuredAppend is ExtendedMode::StructuredAppend:
// structured append.
type BitsExtendedMode_StructuredAppend struct{}

var _ = rs.RegisterEnum[BitsExtendedMode]("ExtendedMode",
	rs.Variant[BitsExtendedMode_Eci]("Eci", rs.ShapeUnit),
	rs.Variant[BitsExtendedMode_Data]("Data", rs.ShapeTuple),
	rs.Variant[BitsExtendedMode_Fnc1First]("Fnc1First", rs.ShapeUnit),
	rs.Variant[BitsExtendedMode_Fnc1Second]("Fnc1Second", rs.ShapeUnit),
	rs.Variant[BitsExtendedMode_StructuredAppend]("StructuredAppend", rs.ShapeUnit),
)

func (*BitsExtendedMode_Eci) isBitsExtendedMode()              {}
func (*BitsExtendedMode_Data) isBitsExtendedMode()             {}
func (*BitsExtendedMode_Fnc1First) isBitsExtendedMode()        {}
func (*BitsExtendedMode_Fnc1Second) isBitsExtendedMode()       {}
func (*BitsExtendedMode_StructuredAppend) isBitsExtendedMode() {}

// PushModeIndicator is Bits::push_mode_indicator: push the mode indicator to
// the end of the bits.
//
// # Errors
//
// If the mode is not supported in the provided version, this method returns
// `Err(QrError::UnsupportedCharacterSet)`.
func (b *BitsBits) PushModeIndicator(mode BitsExtendedMode) error {
	var number int
	data, isData := mode.(*BitsExtendedMode_Data)
	if micro, isMicro := b.version.(*Version_Micro); isMicro {
		switch {
		case micro.F0 == 1 && isData && data.F0 == TypesMode_Numeric:
			return nil
		case isData && data.F0 == TypesMode_Numeric:
			number = 0
		case isData && data.F0 == TypesMode_Alphanumeric:
			number = 1
		case isData && data.F0 == TypesMode_Byte:
			number = 0b10
		case isData && data.F0 == TypesMode_Kanji:
			number = 0b11
		default:
			return TypesQrError_UnsupportedCharacterSet
		}
	} else {
		switch m := mode.(type) {
		case *BitsExtendedMode_Data:
			switch m.F0 {
			case TypesMode_Numeric:
				number = 0b0001
			case TypesMode_Alphanumeric:
				number = 0b0010
			case TypesMode_Byte:
				number = 0b0100
			case TypesMode_Kanji:
				number = 0b1000
			default:
				panic(rs.Unreachable(m.F0))
			}
		case *BitsExtendedMode_Eci:
			number = 0b0111
		case *BitsExtendedMode_Fnc1First:
			number = 0b0101
		case *BitsExtendedMode_Fnc1Second:
			number = 0b1001
		case *BitsExtendedMode_StructuredAppend:
			number = 0b0011
		default:
			panic(rs.Unreachable(mode))
		}
	}
	bits := b.version.ModeBitsCount()
	if b.pushNumberChecked(bits, number) != nil {
		return TypesQrError_UnsupportedCharacterSet
	}
	return nil
}

//}}}
//------------------------------------------------------------------------------
//{{{ ECI

// PushEciDesignator is Bits::push_eci_designator: push an ECI (Extended
// Channel Interpretation) designator to the bits.
//
// An ECI designator is a 6-digit number to specify the character set of the
// following binary data. After calling this method, one could call
// `.PushByteData()` or similar methods to insert the actual data, e.g.
//
//	bits := BitsBitsNew(&Version_Normal{F0: 1})
//	bits.PushEciDesignator(9) // 9 = ISO-8859-7 (Greek).
//	bits.PushByteData([]byte("\xa1\xa2\xa3\xa4\xa5")) // ΑΒΓΔΕ
//
// The full list of ECI designator values can be found from
// <http://strokescribe.com/en/ECI.html>. Some example values are:
//
//	ECI # | Character set
//	------|-------------------------------------
//	3     | ISO-8859-1 (Western European)
//	20    | Shift JIS (Japanese)
//	23    | Windows 1252 (Latin 1) (Western European)
//	25    | UTF-16 Big Endian
//	26    | UTF-8
//	28    | Big 5 (Traditional Chinese)
//	29    | GB-18030 (Simplified Chinese)
//	30    | EUC-KR (Korean)
//
// # Errors
//
// If the QR code version does not support ECI, this method will return
// `Err(QrError::UnsupportedCharacterSet)`.
//
// If the designator is outside of the expected range, this method will
// return `Err(QrError::InvalidECIDesignator)`.
func (b *BitsBits) PushEciDesignator(eciDesignator uint32) error {
	b.reserve(12) // assume the common case that eci_designator <= 127.
	if err := b.PushModeIndicator(&BitsExtendedMode_Eci{}); err != nil {
		return err
	}
	switch {
	case eciDesignator <= 127:
		b.pushNumber(8, asU16U32(eciDesignator))
	case eciDesignator <= 16383:
		b.pushNumber(2, 0b10)
		b.pushNumber(14, asU16U32(eciDesignator))
	case eciDesignator <= 999_999:
		b.pushNumber(3, 0b110)
		b.pushNumber(5, asU16U32(eciDesignator>>16))
		b.pushNumber(16, asU16U32(eciDesignator&0xffff))
	default:
		return TypesQrError_InvalidEciDesignator
	}
	return nil
}

//}}}
//------------------------------------------------------------------------------
//{{{ Mode::Numeric mode

func (b *BitsBits) pushHeader(mode TypesMode, rawDataLen int) error {
	lengthBits := mode.LengthBitsCount(b.version)
	b.reserve(lengthBits + 4 + mode.DataBitsCount(rawDataLen))
	if err := b.PushModeIndicator(&BitsExtendedMode_Data{F0: mode}); err != nil {
		return err
	}
	return b.pushNumberChecked(lengthBits, rawDataLen)
}

// bitsChunks is `data.chunks(size)`.
func bitsChunks(data []byte, size int) [][]byte {
	if size == 0 {
		rs.PanicAt(2, "chunk size must be non-zero")
	}
	var chunks [][]byte
	for len(data) > 0 {
		n := min(size, len(data))
		chunks = append(chunks, data[:n])
		data = data[n:]
	}
	return chunks
}

// PushNumericData is Bits::push_numeric_data: encodes a numeric string to the
// bits.
//
// The data should only contain the characters 0 to 9.
//
// # Errors
//
// Returns `Err(QrError::DataTooLong)` on overflow.
func (b *BitsBits) PushNumericData(data []byte) error {
	if err := b.pushHeader(TypesMode_Numeric, len(data)); err != nil {
		return err
	}
	for _, chunk := range bitsChunks(data, 3) {
		number := uint16(0)
		for _, c := range chunk {
			number = number*10 + uint16(subU8(c, '0'))
		}
		length := len(chunk)*3 + 1
		b.pushNumber(length, number)
	}
	return nil
}

//}}}
//------------------------------------------------------------------------------
//{{{ Mode::Alphanumeric mode

// bitsAlphanumericDigit is alphanumeric_digit: in QR code
// `Mode::Alphanumeric` mode, a pair of alphanumeric characters will be
// encoded as a base-45 integer. `alphanumeric_digit` converts each character
// into its corresponding base-45 digit.
//
// The conversion is specified in ISO/IEC 18004:2006, §8.4.3, Table 5.
func bitsAlphanumericDigit(character byte) uint16 {
	switch {
	case '0' <= character && character <= '9':
		return uint16(character - '0')
	case 'A' <= character && character <= 'Z':
		return uint16(character-'A') + 10
	case character == ' ':
		return 36
	case character == '$':
		return 37
	case character == '%':
		return 38
	case character == '*':
		return 39
	case character == '+':
		return 40
	case character == '-':
		return 41
	case character == '.':
		return 42
	case character == '/':
		return 43
	case character == ':':
		return 44
	default:
		return 0
	}
}

// PushAlphanumericData is Bits::push_alphanumeric_data: encodes an
// alphanumeric string to the bits.
//
// The data should only contain the charaters A to Z (excluding lowercase),
// 0 to 9, space, `$`, `%`, `*`, `+`, `-`, `.`, `/` or `:`.
//
// # Errors
//
// Returns `Err(QrError::DataTooLong)` on overflow.
func (b *BitsBits) PushAlphanumericData(data []byte) error {
	if err := b.pushHeader(TypesMode_Alphanumeric, len(data)); err != nil {
		return err
	}
	for _, chunk := range bitsChunks(data, 2) {
		number := uint16(0)
		for _, c := range chunk {
			number = number*45 + bitsAlphanumericDigit(c)
		}
		length := len(chunk)*5 + 1
		b.pushNumber(length, number)
	}
	return nil
}

//}}}
//------------------------------------------------------------------------------
//{{{ Mode::Byte mode

// PushByteData is Bits::push_byte_data: encodes 8-bit byte data to the bits.
//
// # Errors
//
// Returns `Err(QrError::DataTooLong)` on overflow.
func (b *BitsBits) PushByteData(data []byte) error {
	if err := b.pushHeader(TypesMode_Byte, len(data)); err != nil {
		return err
	}
	for _, c := range data {
		b.pushNumber(8, uint16(c))
	}
	return nil
}

//}}}
//------------------------------------------------------------------------------
//{{{ Mode::Kanji mode

// PushKanjiData is Bits::push_kanji_data: encodes Shift JIS double-byte data
// to the bits.
//
// # Errors
//
// Returns `Err(QrError::DataTooLong)` on overflow.
//
// Returns `Err(QrError::InvalidCharacter)` if the data is not Shift JIS
// double-byte data (e.g. if the length of data is not an even number).
func (b *BitsBits) PushKanjiData(data []byte) error {
	if err := b.pushHeader(TypesMode_Kanji, len(data)/2); err != nil {
		return err
	}
	for _, kanji := range bitsChunks(data, 2) {
		if len(kanji) != 2 {
			return TypesQrError_InvalidCharacter
		}
		cp := uint16(kanji[0])*256 + uint16(kanji[1])
		var bytes uint16
		if cp < 0xe040 {
			bytes = subU16(cp, 0x8140)
		} else {
			bytes = subU16(cp, 0xc140)
		}
		number := (bytes>>8)*0xc0 + (bytes & 0xff)
		b.pushNumber(13, number)
	}
	return nil
}

//}}}
//------------------------------------------------------------------------------
//{{{ FNC1 mode

// PushFnc1FirstPosition is Bits::push_fnc1_first_position: encodes an
// indicator that the following data are formatted according to the UCC/EAN
// Application Identifiers standard.
//
//	bits := BitsBitsNew(&Version_Normal{F0: 1})
//	bits.PushFnc1FirstPosition()
//	bits.PushNumericData([]byte("01049123451234591597033130128"))
//	bits.PushAlphanumericData([]byte("%10ABC123"))
//
// In QR code, the character `%` is used as the data field separator (0x1D).
//
// # Errors
//
// If the mode is not supported in the provided version, this method returns
// `Err(QrError::UnsupportedCharacterSet)`.
func (b *BitsBits) PushFnc1FirstPosition() error {
	return b.PushModeIndicator(&BitsExtendedMode_Fnc1First{})
}

// PushFnc1SecondPosition is Bits::push_fnc1_second_position: encodes an
// indicator that the following data are formatted in accordance with
// specific industry or application specifications previously agreed with
// AIM International.
//
//	bits := BitsBitsNew(&Version_Normal{F0: 1})
//	bits.PushFnc1SecondPosition(37)
//	bits.PushAlphanumericData([]byte("AA1234BBB112"))
//	bits.PushByteData([]byte("text text text text\r"))
//
// If the application indicator is a single Latin alphabet (a–z / A–Z),
// please pass in its ASCII value + 100:
//
//	bits.PushFnc1SecondPosition('A' + 100)
//
// # Errors
//
// If the mode is not supported in the provided version, this method returns
// `Err(QrError::UnsupportedCharacterSet)`.
func (b *BitsBits) PushFnc1SecondPosition(applicationIndicator uint8) error {
	if err := b.PushModeIndicator(&BitsExtendedMode_Fnc1Second{}); err != nil {
		return err
	}
	b.pushNumber(8, uint16(applicationIndicator))
	return nil
}

//}}}
//------------------------------------------------------------------------------
//{{{ Finish

// This table is copied from ISO/IEC 18004:2006 §6.4.10, Table 7.
var bitsDataLengths = [44][4]int{
	// Normal versions
	{152, 128, 104, 72},
	{272, 224, 176, 128},
	{440, 352, 272, 208},
	{640, 512, 384, 288},
	{864, 688, 496, 368},
	{1088, 864, 608, 480},
	{1248, 992, 704, 528},
	{1552, 1232, 880, 688},
	{1856, 1456, 1056, 800},
	{2192, 1728, 1232, 976},
	{2592, 2032, 1440, 1120},
	{2960, 2320, 1648, 1264},
	{3424, 2672, 1952, 1440},
	{3688, 2920, 2088, 1576},
	{4184, 3320, 2360, 1784},
	{4712, 3624, 2600, 2024},
	{5176, 4056, 2936, 2264},
	{5768, 4504, 3176, 2504},
	{6360, 5016, 3560, 2728},
	{6888, 5352, 3880, 3080},
	{7456, 5712, 4096, 3248},
	{8048, 6256, 4544, 3536},
	{8752, 6880, 4912, 3712},
	{9392, 7312, 5312, 4112},
	{10208, 8000, 5744, 4304},
	{10960, 8496, 6032, 4768},
	{11744, 9024, 6464, 5024},
	{12248, 9544, 6968, 5288},
	{13048, 10136, 7288, 5608},
	{13880, 10984, 7880, 5960},
	{14744, 11640, 8264, 6344},
	{15640, 12328, 8920, 6760},
	{16568, 13048, 9368, 7208},
	{17528, 13800, 9848, 7688},
	{18448, 14496, 10288, 7888},
	{19472, 15312, 10832, 8432},
	{20528, 15936, 11408, 8768},
	{21616, 16816, 12016, 9136},
	{22496, 17728, 12656, 9776},
	{23648, 18672, 13328, 10208},
	// Micro versions
	{20, 0, 0, 0},
	{40, 32, 0, 0},
	{84, 68, 0, 0},
	{128, 112, 80, 0},
}

// bitsPaddingBytes is the PADDING_BYTES of push_terminator.
var bitsPaddingBytes = [2]byte{0b1110_1100, 0b0001_0001}

// PushTerminator is Bits::push_terminator: pushes the ending bits to
// indicate no more data.
//
// # Errors
//
// Returns `Err(QrError::DataTooLong)` on overflow.
//
// Returns `Err(QrError::InvalidVersion)` if it is not valid to use the
// `ecLevel` for the given version (e.g. `Version::Micro(1)` with
// `EcLevel::H`).
func (b *BitsBits) PushTerminator(ecLevel EcLevel) error {
	terminatorSize := 4
	if micro, ok := b.version.(*Version_Micro); ok {
		terminatorSize = asUsizeI16(micro.F0)*2 + 1
	}

	curLength := b.Len()
	dataLength, err := b.MaxLen(ecLevel)
	if err != nil {
		return err
	}
	if curLength > dataLength {
		return TypesQrError_DataTooLong
	}

	terminatorSize = min(terminatorSize, dataLength-curLength)
	if terminatorSize > 0 {
		b.pushNumber(terminatorSize, 0)
	}

	if b.Len() < dataLength {
		b.bitOffset = 0
		dataBytesLength := dataLength / 8
		paddingBytesCount := dataBytesLength - len(b.data)
		if paddingBytesCount < 0 {
			// The usize subtraction went below zero. A Micro QR code whose
			// data length is not a whole number of bytes gets here when the
			// terminator leaves the last byte partly filled (Micro(3) with
			// EcLevel::L and 74 to 76 bits of data, for one). Rust panics on
			// the subtraction with debug assertions; without them the
			// padding is `take(usize::MAX)` of a cycle, and
			// `Vec::extend` panics reserving room for it.
			if debugAssertions {
				rs.PanicAt(1, panicSubOverflow)
			}
			rs.PanicAt(1, "capacity overflow")
		}
		for i := range paddingBytesCount {
			b.data = append(b.data, bitsPaddingBytes[i%2])
		}
	}

	if b.Len() < dataLength {
		b.data = append(b.data, 0)
	}

	return nil
}

//}}}
//------------------------------------------------------------------------------
//{{{ Front end.

// PushSegments is Bits::push_segments: push a segmented data to the bits,
// and then terminate it.
//
// # Errors
//
// Returns `Err(QrError::DataTooLong)` on overflow.
//
// Returns `Err(QrError::InvalidData)` if the segment refers to incorrectly
// encoded byte sequence.
func (b *BitsBits) PushSegments(data []byte, segmentsIter OptimizeSegmentIter) error {
	for {
		segment, ok := segmentsIter.Next().Get()
		if !ok {
			return nil
		}
		slice := bitsSlice(data, segment.Begin, segment.End)
		var err error
		switch segment.Mode {
		case TypesMode_Numeric:
			err = b.PushNumericData(slice)
		case TypesMode_Alphanumeric:
			err = b.PushAlphanumericData(slice)
		case TypesMode_Byte:
			err = b.PushByteData(slice)
		case TypesMode_Kanji:
			err = b.PushKanjiData(slice)
		default:
			panic(rs.Unreachable(segment.Mode))
		}
		if err != nil {
			return err
		}
	}
}

// bitsSlice is `&data[begin..end]` with Rust's panic messages.
func bitsSlice(data []byte, begin int, end int) []byte {
	switch {
	case begin > end:
		rs.PanicAt(2, "slice index starts at "+strconv.Itoa(begin)+" but ends at "+strconv.Itoa(end))
	case end > len(data):
		rs.PanicAt(2, "range end index "+strconv.Itoa(end)+" out of range for slice of length "+strconv.Itoa(len(data)))
	}
	return data[begin:end]
}

// PushOptimalData is Bits::push_optimal_data: pushes the data the bits,
// using the optimal encoding.
//
// # Errors
//
// Returns `Err(QrError::DataTooLong)` on overflow.
func (b *BitsBits) PushOptimalData(data []byte) error {
	segments := OptimizeParserNew(data).Optimize(b.version)
	return b.PushSegments(data, segments)
}

//}}}
//------------------------------------------------------------------------------
//{{{ Auto version minimization

// BitsEncodeAuto is bits::encode_auto: automatically determines the minimum
// version to store the data, and encode the result.
//
// This method will not consider any Micro QR code versions.
//
// # Errors
//
// Returns `Err(QrError::DataTooLong)` if the data is too long to fit even
// the highest QR code version.
func BitsEncodeAuto(data []byte, ecLevel EcLevel) (*BitsBits, error) {
	segments := OptimizeParserNew(data).Collect()
	for _, version := range []Version{&Version_Normal{F0: 9}, &Version_Normal{F0: 26}, &Version_Normal{F0: 40}} {
		optSegments := OptimizeOptimizerNew(&optimizeSliceIter{segments: segments}, version).Collect()
		totalLen := OptimizeTotalEncodedLen(optSegments, version)
		dataCapacity, err := VersionFetch(version, ecLevel, bitsDataLengths[:])
		if err != nil {
			rs.PanicAt(1, "invalid DATA_LENGTHS: "+rs.DebugString(err))
		}
		if totalLen <= dataCapacity {
			minVersion := bitsFindMinVersion(totalLen, ecLevel)
			bits := BitsBitsNew(minVersion)
			bits.reserve(totalLen)
			if err := bits.PushSegments(data, &optimizeSliceIter{segments: optSegments}); err != nil {
				return nil, err
			}
			if err := bits.PushTerminator(ecLevel); err != nil {
				return nil, err
			}
			return bits, nil
		}
	}
	return nil, TypesQrError_DataTooLong
}

// bitsFindMinVersion is find_min_version: finds the smallest version (QR
// code only) that can store N bits of data in the given error correction
// level.
func bitsFindMinVersion(length int, ecLevel EcLevel) Version {
	base := 0
	size := 39
	for size > 1 {
		half := size / 2
		mid := base + half
		// mid is always in [0, size).
		// mid >= 0: by definition
		// mid < size: mid = size / 2 + size / 4 + size / 8 ...
		if bitsDataLengths[mid][ecLevel] <= length {
			base = mid
		}
		size -= half
	}
	// base is always in [0, mid) because base <= mid.
	if bitsDataLengths[base][ecLevel] < length {
		base++
	}
	return &Version_Normal{F0: asI16Usize(base + 1)}
}

//}}}
