// Port of idna 1.1.0 src/lib.rs and src/uts46.rs

package url

import (
	"bytes"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// This file is the UTS 46 processing of the idna crate, which url uses for
// the host of a special URL. Mjolnir does not use the idna crate on its own,
// so its items stay unexported and carry the prefix idna.
//
// The algorithm is the crate's, line for line. The Unicode data it reads (the
// UTS 46 mapping table, normalization, General_Category, Bidi_Class,
// Joining_Type) comes from ICU4X in Rust and from idna_adapter.go here.

// idnaDomainToAsciiFromCow is idna::domain_to_ascii_from_cow: the
// domain to ASCII (https://url.spec.whatwg.org/#concept-domain-to-ascii)
// algorithm; version returning a `Cow`.
//
// Most applications should be using this function rather than the sibling
// functions, and most applications should pass `AsciiDenyList::URL` as the
// second argument. Passing `AsciiDenyList::URL` as the second argument makes
// this function also perform the forbidden domain code point
// (https://url.spec.whatwg.org/#forbidden-domain-code-point) check in
// addition to the domain to ASCII algorithm.
//
// Returns the ASCII representation a domain name, normalizing characters
// (upper-case to lower-case and other kinds of equivalence) and using
// Punycode as necessary. The second result is false for the crate's Errors.
//
// This process may fail.
//
// If you have a `&str` instead of `&[u8]`, just call `.to_bytes()` on it
// before passing it to this function. It's still preferable to pass `&[u8]`
// in case the input is not valid UTF-8.
func idnaDomainToAsciiFromCow(domain []byte, asciiDenyList idnaAsciiDenyList) (string, bool) {
	return idnaUts46ToAsciiFromCow(
		domain,
		asciiDenyList,
		idnaHyphens_Allow,
		idnaDnsLength_Ignore,
	)
}

// idnaDomainToUnicode is idna::domain_to_unicode: the domain to Unicode
// (https://url.spec.whatwg.org/#concept-domain-to-unicode) algorithm; legacy
// version normalizing the output to lower case, with the ASCII deny list
// empty.
//
// Return the unicode representation of a domain name, normalizing characters
// (upper-case to lower-case and other kinds of equivalence) and decoding
// Punycode as necessary.
//
// If the second item of the tuple indicates an error, the first item of the
// tuple denotes errors using the REPLACEMENT CHARACTERs in order to be able
// to illustrate errors to the user. When the second item of the return tuple
// signals an error, the first item of the tuple must not be used in a
// network protocol.
func idnaDomainToUnicode(domain string) (string, bool) {
	return idnaUts46ToUnicode(
		[]byte(domain),
		idnaAsciiDenyListEmpty,
		idnaHyphens_Allow,
	)
}

// ICU4C (as of 76.1) has two layers of length limits:
//  1. Punycode-specific limit of 2000
//  2. UTS 46 limit of 1024 (cf. https://unicode-org.atlassian.net/browse/ICU-13727 )
//
// The values here are the crate's.
const idnaPunycodeDecodeMaxInputLength = 2000

const idnaPunycodeEncodeMaxInputLength = 1000

type idnaRtlNumeralState uint8

const (
	idnaRtlNumeralState_Undecided idnaRtlNumeralState = iota
	idnaRtlNumeralState_European
	idnaRtlNumeralState_Arabic
)

// idnaMask is the u128 the crate keeps ASCII sets in: one bit for each ASCII
// character.
type idnaMask struct {
	lo uint64
	hi uint64
}

func (m idnaMask) or(other idnaMask) idnaMask {
	return idnaMask{lo: m.lo | other.lo, hi: m.hi | other.hi}
}

func (m idnaMask) with(b byte) idnaMask {
	if b < 64 {
		m.lo |= 1 << b
	} else {
		m.hi |= 1 << (b - 64)
	}
	return m
}

// has reports whether the bit of the ASCII character b is set. b is below
// 128.
func (m idnaMask) has(b byte) bool {
	if b < 64 {
		return m.lo&(1<<b) != 0
	}
	return m.hi&(1<<(b-64)) != 0
}

func idnaUpperCaseMaskNew() idnaMask {
	var accu idnaMask
	for b := byte(0); b < 128; b++ {
		if b >= 'A' && b <= 'Z' {
			accu = accu.with(b)
		}
	}
	return accu
}

var idnaUpperCaseMask = idnaUpperCaseMaskNew()

func idnaGlyphlessMaskNew() idnaMask {
	var accu idnaMask
	for b := byte(0); b < 128; b++ {
		if b <= ' ' || b == 0x7F {
			accu = accu.with(b)
		}
	}
	return accu
}

var idnaGlyphlessMask = idnaGlyphlessMaskNew()

var idnaDotMask = idnaMask{}.with('.')

func idnaLdhMask() idnaMask {
	var accu idnaMask
	for b := byte(0); b < 128; b++ {
		if !((b >= 'a' && b <= 'z') || (b >= '0' && b <= '9') || b == '-' || b == '.') {
			accu = accu.with(b)
		}
	}
	return accu
}

func idnaWritePunycodeLabel(label []rune, sink []byte) []byte {
	sink = append(sink, "xn--"...)
	sink, ok := idnaPunycodeEncodeInto(label, sink)
	if !ok {
		panic(rs.Unreachable(
			"Punycode overflows should not be possible due to PUNYCODE_ENCODE_MAX_INPUT_LENGTH",
		))
	}
	return sink
}

func idnaHasPunycodePrefix(slice []byte) bool {
	if len(slice) < 4 {
		return false
	}
	return (slice[0] == 'x' || slice[0] == 'X') &&
		(slice[1] == 'n' || slice[1] == 'N') &&
		slice[2] == '-' &&
		slice[3] == '-'
}

func idnaInInclusiveRange8(u byte, start byte, end byte) bool {
	return u-start <= end-start
}

func idnaInInclusiveRangeChar(c rune, start rune, end rune) bool {
	return uint32(c)-uint32(start) <= uint32(end)-uint32(start)
}

func idnaIsPassthroughAsciiLabel(label []byte) bool {
	// XXX if we aren't performing _CheckHyphens_, this could
	// check for "xn--" and pass through YouTube CDN node names.
	if len(label) >= 4 && label[2] == '-' && label[3] == '-' {
		return false
	}
	if len(label) > 0 {
		first, tail := label[0], label[1:]
		// We need to check the first and last character
		// more strictly in case this turns out to be a
		// label in a bidi domain name. This has the side
		// effect that this function only accepts labels
		// that also conform to the STD3 rules.
		//
		// XXX: If we are in the fail-fast mode (i.e. we don't need
		// to be able to overwrite anything with U+FFFD), we could
		// merely record that we've seen a digit here and error out
		// if we later discover that the domain name is a bidi
		// domain name.
		if !idnaInInclusiveRange8(first, 'a', 'z') {
			return false
		}
		for _, b := range tail {
			// If we used LDH_MASK, we'd have to check
			// the bytes for the ASCII range anyhow.
			if idnaInInclusiveRange8(b, 'a', 'z') {
				continue
			}
			if idnaInInclusiveRange8(b, '0', '9') {
				continue
			}
			if b == '-' {
				continue
			}
			return false
		}
		return label[len(label)-1] != '-'
	} else {
		// empty
		return true
	}
}

func idnaSplitAsciiFastPathPrefix(label []byte) ([]byte, []byte) {
	pos := -1
	for i, b := range label {
		if b >= 0x80 {
			pos = i
			break
		}
	}
	if pos >= 0 {
		if pos == 0 {
			// First is non-ASCII
			return nil, label
		} else {
			// Leave one ASCII character in the suffix
			// in case it's a letter that a combining
			// character combines with.
			return label[:pos-1], label[pos-1:]
		}
	} else {
		// All ASCII
		return label, nil
	}
}

// Input known to be lower-case, but may contain non-ASCII.
func idnaApplyAsciiDenyListToLowerCasedUnicode(c rune, denyList idnaMask) rune {
	if c >= 0 && c < 128 {
		if !denyList.has(byte(c)) {
			return c
		} else {
			return '�'
		}
	} else {
		return c
	}
}

// Input known to be ASCII, but may contain upper case ASCII.
func idnaApplyAsciiDenyListToPotentiallyUpperCaseAscii(b byte, denyList idnaMask) rune {
	if !denyList.has(b) {
		return rune(b)
	}
	if idnaInInclusiveRange8(b, 'A', 'Z') {
		return rune(b + 0x20)
	}
	return '�'
}

func idnaIsAscii(label []rune) bool {
	for _, c := range label {
		if c >= 0x80 {
			return false
		}
	}
	return true
}

type idnaPunycodeClassification uint8

const (
	idnaPunycodeClassification_Ascii idnaPunycodeClassification = iota
	idnaPunycodeClassification_Unicode
	idnaPunycodeClassification_Error
)

func idnaClassifyForPunycode(label []rune) idnaPunycodeClassification {
	for i, c := range label {
		if c < 0x80 {
			continue
		}
		if c == '�' {
			return idnaPunycodeClassification_Error
		}
		for _, c := range label[i+1:] {
			if c == '�' {
				return idnaPunycodeClassification_Error
			}
		}
		return idnaPunycodeClassification_Unicode
	}
	return idnaPunycodeClassification_Ascii
}

// idnaAsciiDenyList is uts46::AsciiDenyList: the ASCII deny list to be
// applied.
type idnaAsciiDenyList struct {
	bits idnaMask
}

// idnaAsciiDenyListNew is AsciiDenyList::new: computes (preferably at
// compile time) an ASCII deny list.
//
// Setting `deny_glyphless` to `true` denies U+0020 SPACE and below as well
// as U+007F DELETE for convenience without having to list these characters
// in the `deny_list` string.
//
// `deny_list` is the list of ASCII characters to deny. This list must not
// contain any of:
//   - Letters
//   - Digits
//   - Hyphen
//   - Dot (period / full-stop)
//   - Non-ASCII
//
// # Panics
//
// If the deny list contains characters listed as prohibited above.
func idnaAsciiDenyListNew(denyGlyphless bool, denyList string) idnaAsciiDenyList {
	bits := idnaUpperCaseMask
	if denyGlyphless {
		bits = bits.or(idnaGlyphlessMask)
	}
	for i := 0; i < len(denyList); i++ {
		b := denyList[i]
		rsAssert(b < 0x80, "ASCII deny list must be ASCII.")
		// assert_ne not yet available in const context.
		rsAssert(b != '.', "ASCII deny list must not contain the dot.")
		rsAssert(b != '-', "ASCII deny list must not contain the hyphen.")
		rsAssert(
			!((b >= '0') && (b <= '9')),
			"ASCII deny list must not contain digits.",
		)
		rsAssert(
			!((b >= 'a') && (b <= 'z')),
			"ASCII deny list must not contain letters.",
		)
		rsAssert(
			!((b >= 'A') && (b <= 'Z')),
			"ASCII deny list must not contain letters.",
		)
		bits = bits.with(b)
	}
	return idnaAsciiDenyList{bits: bits}
}

// idnaAsciiDenyListEmpty is AsciiDenyList::EMPTY: no ASCII deny list. This
// corresponds to _UseSTD3ASCIIRules=false_.
//
// Equivalent to `AsciiDenyList::new(false, "")`.
//
// Note: Not denying the space and control characters can result in strange
// behavior. Without a deny list provided to the UTS 46 operation, the caller
// is expected perform filtering afterwards, but it's more efficient to use
// `AsciiDenyList` than post-processing, because the internals of this crate
// can optimize away checks in certain cases.
var idnaAsciiDenyListEmpty = idnaAsciiDenyListNew(false, "")

// idnaAsciiDenyListStd3 is AsciiDenyList::STD3: the STD 3 deny list. This
// corresponds to _UseSTD3ASCIIRules=true_.
//
// Note that this deny list rejects the underscore, which occurs in pseudo-hosts
// used by various TXT record-based protocols, and also characters that may
// occurs in non-DNS naming, such as NetBIOS.
var idnaAsciiDenyListStd3 = idnaAsciiDenyList{bits: idnaLdhMask()}

// idnaAsciiDenyListUrl is AsciiDenyList::URL: forbidden domain code point
// (https://url.spec.whatwg.org/#forbidden-domain-code-point) from the WHATWG
// URL Standard.
//
// Equivalent to `AsciiDenyList::new(true, "%#/:<>?@[\\]^|")`.
//
// Note that this deny list rejects IPv6 addresses, so (as in URL parsing)
// you need to check for IPv6 addresses first and not put them through UTS 46
// processing.
var idnaAsciiDenyListUrl = idnaAsciiDenyListNew(true, "%#/:<>?@[\\]^|")

// idnaHyphens is uts46::Hyphens: the _CheckHyphens_ mode.
type idnaHyphens uint8

const (
	// _CheckHyphens=false_: Do not place positional restrictions on hyphens.
	//
	// This mode is used by the WHATWG URL Standard for normal User Agent
	// processing (i.e. not conformance checking).
	idnaHyphens_Allow idnaHyphens = iota

	// Prohibit hyphens in the first and last position in the label but
	// allow in the third and fourth position.
	//
	// Note that this mode rejects real-world names, including some GitHub
	// user pages.
	idnaHyphens_CheckFirstLast

	// _CheckHyphens=true_: Prohibit hyphens in the first, third, fourth,
	// and last position in the label.
	//
	// Note that this mode rejects real-world names, including YouTube CDN
	// nodes and some GitHub user pages.
	idnaHyphens_Check
)

// idnaDnsLength is uts46::DnsLength: the UTS 46 _VerifyDNSLength_ flag.
type idnaDnsLength uint8

const (
	// _VerifyDNSLength=false_. (Possibly relevant for allowing non-DNS
	// naming systems.)
	idnaDnsLength_Ignore idnaDnsLength = iota
	// _VerifyDNSLength=true_ with the exception that the trailing root
	// label dot is allowed.
	idnaDnsLength_VerifyAllowRootDot
	// _VerifyDNSLength=true_. (The trailing root label dot is not allowed.)
	idnaDnsLength_Verify
)

// idnaErrorPolicy is uts46::ErrorPolicy: policy for customizing behavior in
// case of an error.
type idnaErrorPolicy uint8

const (
	// Return as early as possible without producing output in case of error.
	idnaErrorPolicy_FailFast idnaErrorPolicy = iota
	// In case of error, mark errors with the REPLACEMENT CHARACTER. (The
	// output containing REPLACEMENT CHARACTERs may be show to the user to
	// illustrate what was wrong but must not be used for naming in a network
	// protocol.)
	idnaErrorPolicy_MarkErrors
)

// idnaProcessingSuccess is uts46::ProcessingSuccess: the success outcome of
// `Uts46::process`.
type idnaProcessingSuccess uint8

const (
	// There were no errors. The caller must consider the input to be the
	// output.
	//
	// This asserts that the input can be safely passed to
	// `core::str::from_utf8_unchecked`.
	//
	// (Distinct from `WroteToSink` in order to allow `Cow` behavior to be
	// implemented on top of `Uts46::process`.)
	idnaProcessingSuccess_Passthrough idnaProcessingSuccess = iota

	// There were no errors. The caller must consider what was written to
	// the sink to be the output.
	//
	// (Distinct from `Passthrough` in order to allow `Cow` behavior to be
	// implemented on top of `Uts46::process`.)
	idnaProcessingSuccess_WroteToSink
)

type idnaAlreadyAsciiLabelKind uint8

const (
	idnaAlreadyAsciiLabel_MixedCaseAscii idnaAlreadyAsciiLabelKind = iota
	idnaAlreadyAsciiLabel_MixedCasePunycode
	idnaAlreadyAsciiLabel_Other
)

type idnaAlreadyAsciiLabel struct {
	kind idnaAlreadyAsciiLabelKind
	// label is the slice the MixedCaseAscii and MixedCasePunycode variants
	// carry.
	label []byte
}

// idnaVerifyDnsLength is uts46::verify_dns_length: performs the
// _VerifyDNSLength_ check on the output of the _ToASCII_ operation.
//
// If the second argument is `false`, the trailing root label dot is allowed.
//
// # Panics
//
// Panics in debug mode if the argument isn't ASCII.
func idnaVerifyDnsLength(domainName string, allowTrailingDot bool) bool {
	domainNameWithoutTrailingDot := []byte(domainName)
	if without, ok := bytes.CutSuffix(domainNameWithoutTrailingDot, []byte(".")); ok {
		if !allowTrailingDot {
			return false
		}
		domainNameWithoutTrailingDot = without
	}
	if len(domainNameWithoutTrailingDot) > 253 {
		return false
	}
	for _, label := range bytes.Split(domainNameWithoutTrailingDot, []byte(".")) {
		if len(label) == 0 {
			return false
		}
		if len(label) > 63 {
			return false
		}
	}
	return true
}

// idnaUts46ToAsciiFromCow is Uts46::to_ascii_from_cow: performs the ToASCII
// (https://www.unicode.org/reports/tr46/#ToASCII) operation from UTS #46
// with the options indicated.
//
// # Arguments
//
//   - `domain_name` - The input domain name as UTF-8 bytes. (The UTF-8ness
//     is checked by this method and input that is not well-formed UTF-8 is
//     treated as an error. If you already have a `&str`, call `.as_bytes()`
//     on it.)
//   - `ascii_deny_list` - What ASCII deny list, if any, to apply. The UTS 46
//     _UseSTD3ASCIIRules_ flag or the WHATWG URL Standard forbidden domain
//     code point processing is handled via this argument. Most callers are
//     probably the best off by using `AsciiDenyList::URL` here.
//   - `hyphens` - The UTS 46 _CheckHyphens_ flag. Most callers are probably
//     the best off by using `Hyphens::Allow` here.
//   - `dns_length` - The UTS 46 _VerifyDNSLength_ flag.
//
// The second result is false for the crate's Errors.
func idnaUts46ToAsciiFromCow(
	domainName []byte,
	asciiDenyList idnaAsciiDenyList,
	hyphens idnaHyphens,
	dnsLength idnaDnsLength,
) (string, bool) {
	success, s, ok := idnaUts46Process(
		domainName,
		asciiDenyList,
		hyphens,
		idnaErrorPolicy_FailFast,
		func([]rune, []rune, bool) bool { return false },
	)
	if !ok {
		return "", false
	}
	var cow string
	switch success {
	case idnaProcessingSuccess_Passthrough:
		// SAFETY: `ProcessingSuccess::Passthrough` asserts that `domain_name` is ASCII.
		cow = string(domainName)
	case idnaProcessingSuccess_WroteToSink:
		cow = s
	}
	if dnsLength != idnaDnsLength_Ignore &&
		!idnaVerifyDnsLength(cow, dnsLength == idnaDnsLength_VerifyAllowRootDot) {
		return "", false
	}
	return cow, true
}

// idnaUts46ToUnicode is Uts46::to_unicode: performs the ToUnicode
// (https://www.unicode.org/reports/tr46/#ToUnicode) operation from UTS #46
// according to the options given. When there is an error, there is still
// output, which may be rendered user, even through the output must not be
// used in networking protocols. Errors are denoted by U+FFFD REPLACEMENT
// CHARACTERs in the output. (That is, if the second item of the return tuple
// is false, the first item is guaranteed to contain at least one U+FFFD.)
//
// Most applications probably shouldn't use this method and should be using
// `Uts46::to_user_interface` instead.
func idnaUts46ToUnicode(
	domainName []byte,
	asciiDenyList idnaAsciiDenyList,
	hyphens idnaHyphens,
) (string, bool) {
	return idnaUts46ToUserInterface(domainName, asciiDenyList, hyphens, func([]rune, []rune, bool) bool { return true })
}

// idnaUts46ToUserInterface is Uts46::to_user_interface: performs the
// ToUnicode (https://www.unicode.org/reports/tr46/#ToUnicode) operation from
// UTS #46 according to options given with some error-free Unicode labels
// output according to ToASCII
// (https://www.unicode.org/reports/tr46/#ToASCII) instead as decided by
// application logic passed as the `output_as_unicode` closure. The purpose
// is to convert user-visible domains to the Unicode form in general but to
// render potentially misleading labels as Punycode.
func idnaUts46ToUserInterface(
	domainName []byte,
	asciiDenyList idnaAsciiDenyList,
	hyphens idnaHyphens,
	outputAsUnicode func(label []rune, tld []rune, isBidi bool) bool,
) (string, bool) {
	success, s, ok := idnaUts46Process(
		domainName,
		asciiDenyList,
		hyphens,
		idnaErrorPolicy_MarkErrors,
		outputAsUnicode,
	)
	if !ok {
		return s, false
	}
	switch success {
	// SAFETY: `ProcessingSuccess::Passthrough` asserts that `domain_name` is ASCII.
	case idnaProcessingSuccess_Passthrough:
		return string(domainName), true
	default:
		return s, true
	}
}

// idnaUts46Process is Uts46::process: the lower-level function that
// `to_ascii`, `to_unicode`, and `to_user_interface` are built on to allow
// support for output types other than `Cow<'a, str>` (e.g. string types in a
// non-Rust programming language).
//
// # Arguments
//
//   - `domain_name` - The input domain name as UTF-8 bytes. (The UTF-8ness
//     is checked by this method and input that is not well-formed UTF-8 is
//     treated as an error. If you already have a `&str`, call `.as_bytes()`
//     on it.)
//   - `ascii_deny_list` - What ASCII deny list, if any, to apply. The UTS 46
//     _UseSTD3ASCIIRules_ flag or the WHATWG URL Standard forbidden domain
//     code point processing is handled via this argument. Most callers are
//     probably the best off by using `AsciiDenyList::URL` here.
//   - `hyphens` - The UTS 46 _CheckHyphens_ flag. Most callers are probably
//     the best off by using `Hyphens::Allow` here.
//   - `error_policy` - Whether to fail fast or to produce output that may be
//     rendered for the user to examine in case of errors.
//   - `output_as_unicode` - A closure for deciding if a label should be
//     output as Unicode (as opposed to Punycode). The first argument is the
//     label for which a decision is needed (always non-empty slice). The
//     second argument is the TLD (potentially empty). The third argument is
//     `true` iff the domain name as a whole is a bidi domain name. Only
//     non-erroneous labels that contain at least one non-ASCII character are
//     passed to the closure as the first argument. The second and third
//     argument values are guaranteed to remain the same during a single call
//     to `process`, and the closure may cache computations derived from the
//     second and third argument (hence the `FnMut` type). If your
//     application does not have reasons to render some labels in Punycode,
//     you can simply use `|_, _, _| true`.
//
// The sink of the Rust function is the string this returns; the `ascii_sink`
// argument, which every caller in the crate passes as None, is left out. The
// last result is false for `Err(ProcessingError::ValidityError)`, and the
// string then holds what was written before the error was returned.
//
// # Safety-usable invariant
//
// If the return value is `Ok(ProcessingSuccess::Passthrough)`, `domain_name`
// is ASCII.
func idnaUts46Process(
	domainName []byte,
	asciiDenyList idnaAsciiDenyList,
	hyphens idnaHyphens,
	errorPolicy idnaErrorPolicy,
	outputAsUnicode func(label []rune, tld []rune, isBidi bool) bool,
) (idnaProcessingSuccess, string, bool) {
	var sink []byte
	failFast := errorPolicy == idnaErrorPolicy_FailFast
	var domainBuffer []rune
	var alreadyPunycode []idnaAlreadyAsciiLabel
	// `process_inner` could be pasted inline here, but it's out of line in order
	// to avoid duplicating that code when monomorphizing over `W` and `OutputUnicode`.
	passthroughUpTo, isBidi, hadErrors := idnaUts46ProcessInner(
		domainName,
		asciiDenyList,
		hyphens,
		failFast,
		&domainBuffer,
		&alreadyPunycode,
	)
	if passthroughUpTo == len(domainName) {
		debugAssert(!hadErrors, "!had_errors")
		return idnaProcessingSuccess_Passthrough, "", true
	}
	// Checked only after passthrough as a micro optimization.
	if failFast && hadErrors {
		return 0, "", false
	}
	withoutDot := domainBuffer
	if len(domainBuffer) > 0 && domainBuffer[len(domainBuffer)-1] == '.' {
		withoutDot = domainBuffer[:len(domainBuffer)-1]
	}
	// unwrap is OK, because we always have at least one label
	tld := withoutDot
	for i := len(withoutDot) - 1; i >= 0; i-- {
		if withoutDot[i] == '.' {
			tld = withoutDot[i+1:]
			break
		}
	}
	hadUnicodeOutput := false
	seenLabel := false
	alreadyPunycodeIndex := 0
	passthroughUpToExtended := passthroughUpTo
	flushedPrefix := false
	// writeMixedCase is the block the Rust code repeats for a label that was
	// ASCII in the input (MixedCaseAscii) and for one that stays Punycode
	// (MixedCasePunycode). It reports whether the whole input turned out to
	// pass through.
	writeMixedCase := func(mixedCase []byte) bool {
		firstUpperCase := -1
		for i, c := range mixedCase {
			if 'A' <= c && c <= 'Z' {
				firstUpperCase = i
				break
			}
		}
		if firstUpperCase >= 0 {
			head, tail := mixedCase[:firstUpperCase], mixedCase[firstUpperCase:]
			var sliceToWrite []byte
			if flushedPrefix {
				sliceToWrite = head
			} else {
				flushedPrefix = true
				passthroughUpToExtended += len(head)
				sliceToWrite = domainName[:passthroughUpToExtended]
			}
			// SAFETY: `mixed_case` and `domain_name` up to `passthrough_up_to_extended` are known to be ASCII.
			sink = append(sink, sliceToWrite...)
			for _, c := range tail {
				if 'A' <= c && c <= 'Z' {
					c += 'a' - 'A'
				}
				sink = append(sink, c)
			}
		} else if flushedPrefix {
			// SAFETY: `mixed_case` is known to be ASCII.
			sink = append(sink, mixedCase...)
		} else {
			passthroughUpToExtended += len(mixedCase)
			if passthroughUpToExtended == len(domainName) {
				debugAssert(!hadErrors, "!had_errors")
				return true
			}
		}
		return false
	}
	for _, label := range splitRunes(domainBuffer, '.') {
		// Unwrap is OK, because there are supposed to be as many items in
		// `already_punycode` as there are labels.
		if alreadyPunycodeIndex >= len(alreadyPunycode) {
			rs.PanicAt(0, "called `Option::unwrap()` on a `None` value")
		}
		inputPunycode := alreadyPunycode[alreadyPunycodeIndex]
		alreadyPunycodeIndex++
		if seenLabel {
			if flushedPrefix {
				sink = append(sink, '.')
			} else {
				passthroughUpToExtended++
				if passthroughUpToExtended == len(domainName) {
					debugAssert(!hadErrors, "!had_errors")
					return idnaProcessingSuccess_Passthrough, string(sink), true
				}
			}
		}
		seenLabel = true

		if inputPunycode.kind == idnaAlreadyAsciiLabel_MixedCaseAscii {
			if writeMixedCase(inputPunycode.label) {
				return idnaProcessingSuccess_Passthrough, string(sink), true
			}
			continue
		}

		var potentiallyPunycode bool
		if failFast {
			potentiallyPunycode = !idnaIsAscii(label)
		} else {
			potentiallyPunycode = idnaClassifyForPunycode(label) == idnaPunycodeClassification_Unicode
		}
		var passthrough bool
		if potentiallyPunycode {
			unicode := outputAsUnicode(label, tld, isBidi)
			hadUnicodeOutput = hadUnicodeOutput || unicode
			passthrough = unicode
		} else {
			passthrough = true
		}
		if passthrough {
			if !flushedPrefix {
				flushedPrefix = true
				// SAFETY: `domain_name` up to `passthrough_up_to_extended` is known to be ASCII.
				sink = append(sink, domainName[:passthroughUpToExtended]...)
			}
			sink = append(sink, string(label)...)
		} else if inputPunycode.kind == idnaAlreadyAsciiLabel_MixedCasePunycode {
			if writeMixedCase(inputPunycode.label) {
				return idnaProcessingSuccess_Passthrough, string(sink), true
			}
		} else {
			if !flushedPrefix {
				flushedPrefix = true
				// SAFETY: `domain_name` up to `passthrough_up_to_extended` is known to be ASCII.
				sink = append(sink, domainName[:passthroughUpToExtended]...)
			}
			sink = idnaWritePunycodeLabel(label, sink)
		}
	}

	if hadErrors {
		return 0, string(sink), false
	}

	// The Rust function writes an all-ASCII copy to `ascii_sink` here when
	// there was Unicode output; no caller in the crate passes that sink.
	_ = hadUnicodeOutput
	return idnaProcessingSuccess_WroteToSink, string(sink), true
}

// splitRunes is `slice.split(|c| *c == separator)`.
func splitRunes(s []rune, separator rune) [][]rune {
	var parts [][]rune
	start := 0
	for i, c := range s {
		if c == separator {
			parts = append(parts, s[start:i])
			start = i + 1
		}
	}
	return append(parts, s[start:])
}

// The part of `process` that doesn't need to be generic over the sink.
func idnaUts46ProcessInner(
	domainName []byte,
	asciiDenyList idnaAsciiDenyList,
	hyphens idnaHyphens,
	failFast bool,
	domainBuffer *[]rune,
	alreadyPunycode *[]idnaAlreadyAsciiLabel,
) (int, bool, bool) {
	// Sadly, this even faster-path ASCII tier is needed to avoid regressing
	// performance.
	mostRecentLabelStart := 0
	for i, b := range domainName {
		if idnaInInclusiveRange8(b, 'a', 'z') {
			continue
		}
		if b == '.' {
			mostRecentLabelStart = i + 1
			continue
		}
		return idnaUts46ProcessInnermost(
			domainName,
			asciiDenyList,
			hyphens,
			failFast,
			domainBuffer,
			alreadyPunycode,
			domainName[mostRecentLabelStart:],
		)
	}
	// Success! The whole input passes through on the fastest path!
	return len(domainName), false, false
}

// The part of `process` that doesn't need to be generic over the sink and
// can avoid monomorphizing in the interest of code size.
// Separating this into a different stack frame compared to `process_inner`
// improves performance in the ICU4X case.
func idnaUts46ProcessInnermost(
	domainName []byte,
	asciiDenyList idnaAsciiDenyList,
	hyphens idnaHyphens,
	failFast bool,
	domainBufferPtr *[]rune,
	alreadyPunycodePtr *[]idnaAlreadyAsciiLabel,
	tail []byte,
) (int, bool, bool) {
	domainBuffer := *domainBufferPtr
	alreadyPunycode := *alreadyPunycodePtr
	defer func() {
		*domainBufferPtr = domainBuffer
		*alreadyPunycodePtr = alreadyPunycode
	}()
	denyList := asciiDenyList.bits
	denyListDenyDot := denyList.or(idnaDotMask)

	hadErrors := false

	passthroughUpTo := len(domainName) - len(tail) // Index into `domain_name`
	// 253 ASCII characters is the max length for a valid domain name
	// (excluding the root dot).
	var currentLabelStart int // Index into `domain_buffer`
	seenLabel := false
	inPrefix := true
	for _, label := range bytes.Split(tail, []byte(".")) {
		// We check for passthrough only for the prefix. That is, if we
		// haven't moved on and started filling `domain_buffer`. Keeping
		// this stuff in one loop where the first items keep being skipped
		// once they have been skipped at least once instead of working
		// this into a fancier loop structure in order to make sure that
		// no item from the iterator is lost or processed twice.
		// Furthermore, after the passthrough fails, restarting the
		// normalization process after each pre-existing ASCII dot also
		// provides an opportunity for the processing to get back onto
		// an ASCII fast path that bypasses the normalizer for ASCII
		// after a pre-existing ASCII dot (pre-existing in the sense
		// of not coming from e.g. normalizing an ideographic dot).
		if inPrefix && idnaIsPassthroughAsciiLabel(label) {
			if seenLabel {
				passthroughUpTo++
			}
			seenLabel = true

			passthroughUpTo += len(label)
			continue
		}
		if seenLabel {
			if inPrefix {
				passthroughUpTo++
			} else {
				domainBuffer = append(domainBuffer, '.')
			}
		}
		seenLabel = true
		inPrefix = false
		currentLabelStart = len(domainBuffer)
		if len(label) != 0 {
			ascii, nonAscii := idnaSplitAsciiFastPathPrefix(label)
			var nonPunycodeAsciiLabel bool
			if len(nonAscii) == 0 {
				if idnaHasPunycodePrefix(ascii) {
					if ascii[len(ascii)-1] != '-' &&
						len(ascii)-4 <= idnaPunycodeDecodeMaxInputLength {
						if decode, ok := idnaPunycodeDecode(bytesToRunes(ascii[4:]), true); ok {
							// 63 ASCII characters is the max length for a valid DNS label and xn-- takes 4
							// characters.
							labelBuffer := decode

							if idnaUts46AfterPunycodeDecode(
								&domainBuffer,
								currentLabelStart,
								labelBuffer,
								denyListDenyDot,
								failFast,
								&hadErrors,
							) {
								return 0, false, true
							}

							if idnaUts46CheckLabel(
								hyphens,
								domainBuffer[currentLabelStart:],
								failFast,
								&hadErrors,
								true,
								true,
							) {
								return 0, false, true
							}
						} else {
							// Punycode failed
							if failFast {
								return 0, false, true
							}
							hadErrors = true
							domainBuffer = append(domainBuffer, '�')
							// Discard the first character that we replaced.
							for _, c := range ascii[1:] {
								// Can't have dot here, so `deny_list` vs `deny_list_deny_dot` does
								// not matter.
								domainBuffer = append(domainBuffer, idnaApplyAsciiDenyListToPotentiallyUpperCaseAscii(
									c, denyList,
								))
							}
						}
						// If there were errors, we won't be trying to use this
						// anyway later, so it's fine to put it here unconditionally.
						alreadyPunycode = append(alreadyPunycode, idnaAlreadyAsciiLabel{kind: idnaAlreadyAsciiLabel_MixedCasePunycode, label: label})
						continue
					} else if failFast {
						return 0, false, true
					}
					// Else fall through to the complex path and rediscover error
					// there.
					nonPunycodeAsciiLabel = false
				} else {
					nonPunycodeAsciiLabel = true
				}
			} else {
				nonPunycodeAsciiLabel = false
			}
			for _, b := range ascii {
				// Can't have dot here, so `deny_list` vs `deny_list_deny_dot` does
				// not matter.
				c := idnaApplyAsciiDenyListToPotentiallyUpperCaseAscii(b, denyList)
				if c == '�' {
					if failFast {
						return 0, false, true
					}
					hadErrors = true
				}
				domainBuffer = append(domainBuffer, c)
			}
			if nonPunycodeAsciiLabel {
				if hyphens != idnaHyphens_Allow &&
					idnaCheckHyphens(
						domainBuffer[currentLabelStart:],
						hyphens == idnaHyphens_CheckFirstLast,
						failFast,
						&hadErrors,
					) {
					return 0, false, true
				}
				if hadErrors {
					alreadyPunycode = append(alreadyPunycode, idnaAlreadyAsciiLabel{kind: idnaAlreadyAsciiLabel_Other})
				} else {
					alreadyPunycode = append(alreadyPunycode, idnaAlreadyAsciiLabel{kind: idnaAlreadyAsciiLabel_MixedCaseAscii, label: label})
				}
				continue
			}
			alreadyPunycode = append(alreadyPunycode, idnaAlreadyAsciiLabel{kind: idnaAlreadyAsciiLabel_Other})
			firstNeedsCombiningMarkCheck := len(ascii) == 0
			needsContextjCheck := len(nonAscii) != 0
			mapping := idnaAdapterMapNormalize(nonAscii)
			mappingIndex := 0
			for {
				var n rune
				hasN := mappingIndex < len(mapping)
				if hasN {
					n = idnaApplyAsciiDenyListToLowerCasedUnicode(mapping[mappingIndex], denyList)
					mappingIndex++
				}
				if !hasN || n == '.' {
					if hasRunePrefix(domainBuffer[currentLabelStart:], "xn--") {
						punycodePreconditionFailed := false
						for i := currentLabelStart + 4; i < len(domainBuffer); i++ {
							if domainBuffer[i] >= 0x80 {
								if failFast {
									return 0, false, true
								}
								hadErrors = true
								domainBuffer[i] = '�'
								punycodePreconditionFailed = true
							}
						}

						if len(domainBuffer) > 0 {
							last := &domainBuffer[len(domainBuffer)-1]
							if *last == '-' {
								// Either there's nothing after the "xn--" prefix
								// and we got the last hyphen of "xn--", or there
								// are no Punycode digits after the last delimiter
								// which would result in Punycode decode outputting
								// ASCII only.
								if failFast {
									return 0, false, true
								}
								hadErrors = true
								*last = '�'
								punycodePreconditionFailed = true
							}
						} else {
							panic(rs.Unreachable())
						}

						// Reject excessively long input
						// https://github.com/whatwg/url/issues/824
						// https://unicode-org.atlassian.net/browse/ICU-13727
						if len(domainBuffer)-currentLabelStart-4 >
							idnaPunycodeDecodeMaxInputLength {
							if failFast {
								return 0, false, true
							}
							hadErrors = true
							domainBuffer[currentLabelStart+
								4+
								idnaPunycodeDecodeMaxInputLength] = '�'
							punycodePreconditionFailed = true
						}

						if !punycodePreconditionFailed {
							if decode, ok := idnaPunycodeDecode(
								domainBuffer[currentLabelStart+4:],
								false,
							); ok {
								firstNeedsCombiningMarkCheck = true
								needsContextjCheck = true
								// 63 ASCII characters is the max length for a valid DNS label and xn-- takes 4
								// characters.
								labelBuffer := decode

								domainBuffer = domainBuffer[:currentLabelStart]
								if idnaUts46AfterPunycodeDecode(
									&domainBuffer,
									currentLabelStart,
									labelBuffer,
									denyListDenyDot,
									failFast,
									&hadErrors,
								) {
									return 0, false, true
								}
							} else {
								// Punycode failed
								if failFast {
									return 0, false, true
								}
								hadErrors = true
								domainBuffer[currentLabelStart] = '�'
								needsContextjCheck = false // ASCII label
								firstNeedsCombiningMarkCheck = false
							}
						} else {
							firstNeedsCombiningMarkCheck = false
							needsContextjCheck = false // Non-ASCII already turned to U+FFFD.
						}
					}
					if idnaUts46CheckLabel(
						hyphens,
						domainBuffer[currentLabelStart:],
						failFast,
						&hadErrors,
						firstNeedsCombiningMarkCheck,
						needsContextjCheck,
					) {
						return 0, false, true
					}

					if !hasN {
						break
					}
					domainBuffer = append(domainBuffer, '.')
					currentLabelStart = len(domainBuffer)
					firstNeedsCombiningMarkCheck = true
					needsContextjCheck = true
					alreadyPunycode = append(alreadyPunycode, idnaAlreadyAsciiLabel{kind: idnaAlreadyAsciiLabel_Other})
				} else {
					if n == '�' {
						if failFast {
							return 0, false, true
						}
						hadErrors = true
					}
					domainBuffer = append(domainBuffer, n)
				}
			}
		} else {
			// Empty label
			alreadyPunycode = append(alreadyPunycode, idnaAlreadyAsciiLabel{kind: idnaAlreadyAsciiLabel_MixedCaseAscii, label: label})
		}
	}

	isBidi := idnaUts46IsBidi(domainBuffer)
	if isBidi {
		for _, label := range splitRunes(domainBuffer, '.') {
			if len(label) == 0 {
				continue
			}
			first, tail := &label[0], label[1:]
			firstBc := idnaAdapterBidiClass(*first)
			if !idnaFirstBcMask.intersects(firstBc) {
				// Neither RTL label nor LTR label
				if failFast {
					return 0, false, true
				}
				hadErrors = true
				*first = '�'
				continue
			}
			isLtr := firstBc == idnaBidiClass_LeftToRight
			// Trim NSM
			middle := tail
			for {
				if len(middle) > 0 {
					last, prior := &middle[len(middle)-1], middle[:len(middle)-1]
					lastBc := idnaAdapterBidiClass(*last)
					if lastBc == idnaBidiClass_NonspacingMark {
						middle = prior
						continue
					}
					lastMask := idnaLastRtlMask
					if isLtr {
						lastMask = idnaLastLtrMask
					}
					if !lastMask.intersects(lastBc) {
						if failFast {
							return 0, false, true
						}
						hadErrors = true
						*last = '�'
					}
					if isLtr {
						for i := range prior {
							c := &prior[i]
							bc := idnaAdapterBidiClass(*c)
							if !idnaMiddleLtrMask.intersects(bc) {
								if failFast {
									return 0, false, true
								}
								hadErrors = true
								*c = '�'
							}
						}
					} else {
						numeralState := idnaRtlNumeralState_Undecided
						for i := range prior {
							c := &prior[i]
							bc := idnaAdapterBidiClass(*c)
							if !idnaMiddleRtlMask.intersects(bc) {
								if failFast {
									return 0, false, true
								}
								hadErrors = true
								*c = '�'
							} else {
								switch numeralState {
								case idnaRtlNumeralState_Undecided:
									if bc == idnaBidiClass_EuropeanNumber {
										numeralState = idnaRtlNumeralState_European
									} else if bc == idnaBidiClass_ArabicNumber {
										numeralState = idnaRtlNumeralState_Arabic
									}
								case idnaRtlNumeralState_European:
									if bc == idnaBidiClass_ArabicNumber {
										if failFast {
											return 0, false, true
										}
										hadErrors = true
										*c = '�'
									}
								case idnaRtlNumeralState_Arabic:
									if bc == idnaBidiClass_EuropeanNumber {
										if failFast {
											return 0, false, true
										}
										hadErrors = true
										*c = '�'
									}
								}
							}
						}
						if (numeralState == idnaRtlNumeralState_European &&
							lastBc == idnaBidiClass_ArabicNumber) ||
							(numeralState == idnaRtlNumeralState_Arabic &&
								lastBc == idnaBidiClass_EuropeanNumber) {
							if failFast {
								return 0, false, true
							}
							hadErrors = true
							*last = '�'
						}
					}
					break
				} else {
					// One-character label or label where
					// everything after the first character
					// is just non-spacing marks.
					break
				}
			}
		}
	}

	return passthroughUpTo, isBidi, hadErrors
}

func bytesToRunes(b []byte) []rune {
	out := make([]rune, len(b))
	for i, c := range b {
		out[i] = rune(c)
	}
	return out
}

func hasRunePrefix(s []rune, prefix string) bool {
	if len(s) < len(prefix) {
		return false
	}
	for i := 0; i < len(prefix); i++ {
		if s[i] != rune(prefix[i]) {
			return false
		}
	}
	return true
}

func idnaUts46AfterPunycodeDecode(
	domainBuffer *[]rune,
	currentLabelStart int,
	labelBuffer []rune,
	denyListDenyDot idnaMask,
	failFast bool,
	hadErrors *bool,
) bool {
	for _, c := range idnaAdapterNormalizeValidate(labelBuffer) {
		c = idnaApplyAsciiDenyListToLowerCasedUnicode(c, denyListDenyDot)
		if c == '�' {
			if failFast {
				return true
			}
			*hadErrors = true
		}
		*domainBuffer = append(*domainBuffer, c)
	}
	normalized := (*domainBuffer)[currentLabelStart:]
	for i := 0; i < len(normalized) && i < len(labelBuffer); i++ {
		if normalized[i] != labelBuffer[i] {
			// Mark the first difference
			normalized[i] = '�'
			if failFast {
				return true
			}
			*hadErrors = true
			break
		}
	}
	return false
}

func idnaUts46CheckLabel(
	hyphens idnaHyphens,
	mutLabel []rune,
	failFast bool,
	hadErrors *bool,
	firstNeedsCombiningMarkCheck bool,
	needsContextjCheck bool,
) bool {
	if hyphens != idnaHyphens_Allow &&
		idnaCheckHyphens(
			mutLabel,
			hyphens == idnaHyphens_CheckFirstLast,
			failFast,
			hadErrors,
		) {
		return true
	}
	if firstNeedsCombiningMarkCheck {
		if len(mutLabel) > 0 {
			first := &mutLabel[0]
			if idnaAdapterIsMark(*first) {
				if failFast {
					return true
				}
				*hadErrors = true
				*first = '�'
			}
		}
	}
	if needsContextjCheck {
		// ContextJ
		for i := 0; i < len(mutLabel); i++ {
			c := mutLabel[i]
			if !idnaInInclusiveRangeChar(c, '‌', '‍') {
				continue
			}
			head, joinerAndTail := mutLabel[:i], mutLabel[i:]

			joiner, tail := &joinerAndTail[0], joinerAndTail[1:]
			if len(head) > 0 {
				previous := head[len(head)-1]
				if idnaAdapterIsVirama(previous) {
					continue
				}
			} else {
				// No preceding character
				if failFast {
					return true
				}
				*hadErrors = true
				*joiner = '�'
				continue
			}
			if c == '‍' {
				// ZWJ only has the virama rule
				if failFast {
					return true
				}
				*hadErrors = true
				*joiner = '�'
				continue
			}
			if !idnaUts46HasAppropriatelyJoiningCharReversed(
				head,
				idnaJoiningLeftOrDual,
			) || !idnaUts46HasAppropriatelyJoiningChar(
				tail,
				idnaJoiningRightOrDual,
			) {
				if failFast {
					return true
				}
				*hadErrors = true
				*joiner = '�'
			}
		}
	}

	if !idnaIsAscii(mutLabel) && len(mutLabel) > idnaPunycodeEncodeMaxInputLength {
		// Limit quadratic behavior
		// https://github.com/whatwg/url/issues/824
		// https://unicode-org.atlassian.net/browse/ICU-13727
		if failFast {
			return true
		}
		*hadErrors = true
		mutLabel[idnaPunycodeEncodeMaxInputLength] = '�'
	}
	return false
}

// idnaUts46HasAppropriatelyJoiningChar is has_appropriately_joining_char
// over the characters of chars in order; the Reversed form is the same over
// `iter().rev()`.
func idnaUts46HasAppropriatelyJoiningChar(chars []rune, required idnaJoiningTypeMask) bool {
	for _, c := range chars {
		jt := idnaAdapterJoiningType(c)
		if required.intersects(jt) {
			return true
		}
		if jt == idnaJoiningType_Transparent {
			continue
		}
		return false
	}
	return false
}

func idnaUts46HasAppropriatelyJoiningCharReversed(chars []rune, required idnaJoiningTypeMask) bool {
	for i := len(chars) - 1; i >= 0; i-- {
		jt := idnaAdapterJoiningType(chars[i])
		if required.intersects(jt) {
			return true
		}
		if jt == idnaJoiningType_Transparent {
			continue
		}
		return false
	}
	return false
}

func idnaUts46IsBidi(buffer []rune) bool {
	for _, c := range buffer {
		if c < '֐' {
			// Below Hebrew
			continue
		}
		if idnaInInclusiveRangeChar(c, 'ऀ', '﬜') {
			continue
		}
		if idnaInInclusiveRangeChar(c, 0x1F000, 0x3FFFF) {
			continue
		}
		if idnaInInclusiveRangeChar(c, '＀', 0x107FF) {
			continue
		}
		if idnaInInclusiveRangeChar(c, 0x11000, 0x1E7FF) {
			continue
		}
		if idnaRtlMask.intersects(idnaAdapterBidiClass(c)) {
			return true
		}
	}
	return false
}

func idnaCheckHyphens(
	mutLabel []rune,
	allowThirdFourth bool,
	failFast bool,
	hadErrors *bool,
) bool {
	if len(mutLabel) > 0 {
		first := &mutLabel[0]
		if *first == '-' {
			if failFast {
				return true
			}
			*hadErrors = true
			*first = '�'
		}
	}
	if len(mutLabel) > 0 {
		last := &mutLabel[len(mutLabel)-1]
		if *last == '-' {
			if failFast {
				return true
			}
			*hadErrors = true
			*last = '�'
		}
	}
	if allowThirdFourth {
		return false
	}
	if len(mutLabel) >= 4 && mutLabel[2] == '-' && mutLabel[3] == '-' {
		if failFast {
			return true
		}
		*hadErrors = true
		mutLabel[2] = '�'
		mutLabel[3] = '�'
	}
	return false
}
