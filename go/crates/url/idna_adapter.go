// Port of idna_adapter 1.2.2 src/lib.rs

package url

import (
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs/strs"
	"golang.org/x/net/idna"
	"golang.org/x/text/unicode/bidi"
	"golang.org/x/text/unicode/norm"
)

// This file is the Unicode back end of the UTS 46 processing in idna.go. In
// Rust it is ICU4X 2.2.0 (icu_normalizer and icu_properties, Unicode 17.0).
// Here it is:
//
//   - the UTS 46 mapping table of golang.org/x/net/idna, asked one character
//     at a time through a profile that only maps and one that only validates;
//   - NFC and the canonical combining class of golang.org/x/text/unicode/norm;
//   - Bidi_Class of golang.org/x/text/unicode/bidi;
//   - General_Category of the standard library's unicode package;
//   - Joining_Type from idna_joining_table.go, which was read out of the Rust
//     implementation because no Go library exposes that property.
//
// All four Go tables are Unicode 17.0.0 with Go 1.27 and the versions of
// x/net and x/text in go.mod. A later Go or x/text release moves them to a
// later Unicode version while the Rust crate stays on its pinned ICU4X data;
// TestDiffAgainstRust_host_every_scalar_value then fails for the characters
// that changed.

// idnaMapProfile maps one character as UTS 46 section 4 step 1 does (with
// non-transitional processing and UseSTD3ASCIIRules=false) and checks
// nothing: a disallowed character comes back as it is.
var idnaMapProfile = idna.New(
	idna.MapForLookup(),
	idna.ValidateLabels(false),
	idna.StrictDomainName(false),
	idna.Transitional(false),
)

// idnaValidProfile accepts a character whose UTS 46 status is valid or
// deviation, and checks nothing about the label it is in.
var idnaValidProfile = idna.New(
	idna.ValidateLabels(true),
	idna.CheckHyphens(false),
	idna.CheckJoiners(false),
	idna.StrictDomainName(false),
	idna.Transitional(false),
)

// idnaAdapterMapChar is the UTS 46 mapping of one character: the characters
// it maps to (itself when its status is valid, deviation or disallowed), or
// the empty string when it is ignored.
func idnaAdapterMapChar(c rune) string {
	if c < 0x80 {
		if 'A' <= c && c <= 'Z' {
			return string(c + ('a' - 'A'))
		}
		return string(c)
	}
	mapped, _ := idnaMapProfile.ToUnicode(string(c))
	return mapped
}

// idnaAdapterIsValid reports whether the UTS 46 status of c is valid or
// deviation: the character may appear in a label as it is.
func idnaAdapterIsValid(c rune) bool {
	if c < 0x80 {
		return !('A' <= c && c <= 'Z')
	}
	if c == utf8.RuneError {
		return false
	}
	s := string(c)
	checked, err := idnaValidProfile.ToUnicode(s)
	return err == nil && checked == s
}

// idnaAdapterMapNormalize is Adapter::map_normalize over the characters of
// the UTF-8 bytes non_ascii, collected: mapped, with ignored characters
// removed, disallowed ones (and bytes that are not UTF-8) turned to U+FFFD,
// and the result normalized to NFC.
func idnaAdapterMapNormalize(nonAscii []byte) []rune {
	input := strs.FromUtf8Lossy(nonAscii)
	mapped := make([]byte, 0, len(input))
	for _, c := range input {
		for _, m := range idnaAdapterMapChar(c) {
			if !idnaAdapterIsValid(m) {
				m = '�'
			}
			mapped = utf8.AppendRune(mapped, m)
		}
	}
	return []rune(string(norm.NFC.Bytes(mapped)))
}

// idnaAdapterNormalizeValidate is Adapter::normalize_validate, collected:
// like map_normalize, but an ignored character becomes U+FFFD instead of
// disappearing.
func idnaAdapterNormalizeValidate(label []rune) []rune {
	mapped := make([]byte, 0, len(label))
	for _, c := range label {
		m := idnaAdapterMapChar(c)
		if m == "" {
			mapped = utf8.AppendRune(mapped, '�')
			continue
		}
		for _, r := range m {
			if !idnaAdapterIsValid(r) {
				r = '�'
			}
			mapped = utf8.AppendRune(mapped, r)
		}
	}
	return []rune(string(norm.NFC.Bytes(mapped)))
}

// idnaAdapterIsVirama is Adapter::is_virama: the canonical combining class
// of c is Virama (9).
func idnaAdapterIsVirama(c rune) bool {
	if c < 0x80 {
		return false
	}
	var buf [utf8.UTFMax]byte
	n := utf8.EncodeRune(buf[:], c)
	return norm.NFC.Properties(buf[:n]).CCC() == 9
}

// idnaAdapterIsMark is Adapter::is_mark: the General_Category of c is one
// of the Mark categories.
func idnaAdapterIsMark(c rune) bool {
	return unicode.Is(unicode.M, c)
}

// idnaBidiClassMask is idna_adapter::BidiClassMask.
type idnaBidiClassMask uint32

func idnaBidiClassToMask(classes ...bidi.Class) idnaBidiClassMask {
	var mask idnaBidiClassMask
	for _, class := range classes {
		mask |= 1 << class
	}
	return mask
}

func (m idnaBidiClassMask) intersects(bc bidi.Class) bool {
	return bc < 32 && m&(1<<bc) != 0
}

const (
	idnaBidiClass_LeftToRight    = bidi.L
	idnaBidiClass_NonspacingMark = bidi.NSM
	idnaBidiClass_EuropeanNumber = bidi.EN
	idnaBidiClass_ArabicNumber   = bidi.AN
)

// Mask for checking for both left and dual joining.
var idnaRtlMask = idnaBidiClassToMask(bidi.R, bidi.AL, bidi.AN)

// Mask for allowable bidi classes in the first character of a label
// (either LTR or RTL) in a bidi domain.
var idnaFirstBcMask = idnaBidiClassToMask(bidi.L, bidi.R, bidi.AL)

// Mask for allowable bidi classes of the last (non-Non-Spacing Mark)
// character in an LTR label in a bidi domain.
var idnaLastLtrMask = idnaBidiClassToMask(bidi.L, bidi.EN)

// Mask for allowable bidi classes of the last (non-Non-Spacing Mark)
// character in an RTL label in a bidi domain.
var idnaLastRtlMask = idnaBidiClassToMask(bidi.R, bidi.AL, bidi.EN, bidi.AN)

// Mask for allowable bidi classes of the middle characters in an LTR label in a bidi domain.
var idnaMiddleLtrMask = idnaBidiClassToMask(bidi.L, bidi.EN, bidi.ES, bidi.CS, bidi.ET, bidi.ON, bidi.BN, bidi.NSM)

// Mask for allowable bidi classes of the middle characters in an RTL label in a bidi domain.
var idnaMiddleRtlMask = idnaBidiClassToMask(bidi.R, bidi.AL, bidi.AN, bidi.EN, bidi.ES, bidi.CS, bidi.ET, bidi.ON, bidi.BN, bidi.NSM)

// idnaAdapterBidiClass is Adapter::bidi_class.
func idnaAdapterBidiClass(c rune) bidi.Class {
	properties, _ := bidi.LookupRune(c)
	return properties.Class()
}

// idnaJoiningType is idna_adapter::JoiningType, as far as ContextJ reads it.
type idnaJoiningType uint8

const (
	// idnaJoiningType_Other is every Joining_Type the checks do not tell
	// apart: Non_Joining and Join_Causing.
	idnaJoiningType_Other idnaJoiningType = iota
	idnaJoiningType_LeftJoining
	idnaJoiningType_RightJoining
	idnaJoiningType_DualJoining
	idnaJoiningType_Transparent
)

// idnaJoiningTypeMask is idna_adapter::JoiningTypeMask.
type idnaJoiningTypeMask uint8

func (m idnaJoiningTypeMask) intersects(jt idnaJoiningType) bool {
	return m&(1<<jt) != 0
}

// Mask for checking for both left and dual joining.
const idnaJoiningLeftOrDual idnaJoiningTypeMask = 1<<idnaJoiningType_LeftJoining | 1<<idnaJoiningType_DualJoining

// Mask for checking for both left and dual joining.
const idnaJoiningRightOrDual idnaJoiningTypeMask = 1<<idnaJoiningType_RightJoining | 1<<idnaJoiningType_DualJoining

type idnaJoiningRange struct {
	lo uint32
	hi uint32
	jt idnaJoiningType
}

// idnaAdapterJoiningType is Adapter::joining_type, for a character whose
// UTS 46 status is valid or deviation. Those are the only characters the
// ContextJ check sees.
func idnaAdapterJoiningType(c rune) idnaJoiningType {
	code := uint32(c)
	lo, hi := 0, len(idnaJoiningTable)
	for lo < hi {
		mid := (lo + hi) / 2
		switch entry := idnaJoiningTable[mid]; {
		case code < entry.lo:
			hi = mid
		case code > entry.hi:
			lo = mid + 1
		default:
			return entry.jt
		}
	}
	return idnaJoiningType_Other
}
