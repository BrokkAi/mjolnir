package semver

import (
	"encoding/json"
	"math/rand/v2"
	"os"
	"slices"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// The differential tests ask the real crate (go/port/oracle, module
// `semver`) to parse, compare, sort and print versions, pre-releases and
// build metadata, and hold this package to the same answers. The corpora are
// recorded in testdata/oracle:
//
//   - semver: parsing (every field, or the error's Display and Debug text),
//     the identifier constructors, every comparison and sorting. The Go side
//     answers with the String and debugString methods, without rs.Format.
//   - fmt: what goes through rs.Formatter: Display with width, fill,
//     alignment and precision, Debug and pretty Debug alone and as a field of
//     a derived Debug, and the parse answers again, built with rs.ToString
//     and rs.DebugString.
//
// MJ_ORACLE_SCALE multiplies the number of generated random cases. It is for
// a longer run against the live oracle (`MJ_ORACLE=live MJ_ORACLE_SCALE=50
// ...`); the recorded corpus is made with the default of 1.
func oracleScale() int {
	scale, err := strconv.Atoi(os.Getenv("MJ_ORACLE_SCALE"))
	if err != nil || scale < 1 {
		return 1
	}
	return scale
}

type oracleRequest struct {
	Op       string   `json:"op"`
	Text     string   `json:"text"`
	A        string   `json:"a"`
	B        string   `json:"b"`
	Versions []string `json:"versions"`
	Spec     string   `json:"spec"`
	What     string   `json:"what"`
}

// oracleOrdering is the oracle's `ordering`: -1, 0 or 1.
func oracleOrdering(order int) int {
	return int(rs.OrderingOf(order))
}

// oracleErrorReply is the oracle's `error`. Without the formatter the texts
// come from Error and debugString; with it, from rs.ToString and
// rs.DebugString.
func oracleErrorReply(err error, formatter bool) any {
	semverError, ok := err.(*Error)
	if !ok {
		panic(rs.Panicf("error {:?} is not a *semver.Error", err.Error()))
	}
	if formatter {
		return map[string]any{"err": rs.ToString(semverError), "err_debug": rs.DebugString(semverError)}
	}
	return map[string]any{"err": semverError.Error(), "err_debug": semverError.debugString()}
}

func oracleParse(text string, formatter bool) any {
	version, err := VersionParse(text)
	if err != nil {
		return oracleErrorReply(err, formatter)
	}
	again := rs.Expect(rs.ResultOf(VersionFromStr(text)), "FromStr agrees with parse")
	if !rs.Eq(again, version) {
		panic(rs.Panicf("assertion `left == right` failed"))
	}
	rebuilt := Version{
		Major: version.Major,
		Minor: version.Minor,
		Patch: version.Patch,
		Pre:   rs.Expect(rs.ResultOf(PrereleaseNew(version.Pre.AsStr())), "a parsed pre-release parses"),
		Build: rs.Expect(rs.ResultOf(BuildMetadataNew(version.Build.AsStr())), "parsed build metadata parses"),
	}
	reply := map[string]any{
		"major":       version.Major,
		"minor":       version.Minor,
		"patch":       version.Patch,
		"pre":         version.Pre.AsStr(),
		"build":       version.Build.AsStr(),
		"pre_empty":   version.Pre.IsEmpty(),
		"build_empty": version.Build.IsEmpty(),
		"pre_len":     len(version.Pre.Deref()),
		"build_len":   len(version.Build.Deref()),
		"rebuilt_eq":  rs.Eq(rebuilt, version),
		"new_eq":      rs.Eq(VersionNew(version.Major, version.Minor, version.Patch), version),
	}
	if formatter {
		reply["display"] = rs.ToString(&version)
		reply["pre_display"] = rs.ToString(&version.Pre)
		reply["build_display"] = rs.ToString(&version.Build)
		reply["pre_debug"] = rs.DebugString(&version.Pre)
		reply["build_debug"] = rs.DebugString(&version.Build)
	} else {
		reply["display"] = version.String()
		reply["pre_display"] = version.Pre.String()
		reply["build_display"] = version.Build.String()
		reply["pre_debug"] = version.Pre.debugString()
		reply["build_debug"] = version.Build.debugString()
	}
	return map[string]any{"ok": reply}
}

func oraclePrerelease(text string) any {
	pre, err := PrereleaseNew(text)
	if err != nil {
		return oracleErrorReply(err, false)
	}
	return map[string]any{"ok": map[string]any{
		"as_str":         pre.AsStr(),
		"is_empty":       pre.IsEmpty(),
		"len":            len(pre.Deref()),
		"display":        pre.String(),
		"debug":          pre.debugString(),
		"is_default":     rs.Eq(pre, rs.Default[Prerelease]()),
		"is_const_empty": rs.Eq(pre, PrereleaseEmpty),
	}}
}

func oracleBuild(text string) any {
	build, err := BuildMetadataNew(text)
	if err != nil {
		return oracleErrorReply(err, false)
	}
	return map[string]any{"ok": map[string]any{
		"as_str":         build.AsStr(),
		"is_empty":       build.IsEmpty(),
		"len":            len(build.Deref()),
		"display":        build.String(),
		"debug":          build.debugString(),
		"is_default":     rs.Eq(build, rs.Default[BuildMetadata]()),
		"is_const_empty": rs.Eq(build, BuildMetadataEmpty),
	}}
}

func oracleCmp(textA, textB string) any {
	a, errA := VersionParse(textA)
	b, errB := VersionParse(textB)
	if errA != nil || errB != nil {
		return map[string]any{"invalid": true}
	}
	order := rs.Cmp(a, b)
	partial := rs.PartialCmp(a, b).Unwrap()
	maximum := rs.Max(rs.Clone(a), rs.Clone(b))
	minimum := rs.Min(rs.Clone(a), rs.Clone(b))
	return map[string]any{
		"cmp":         oracleOrdering(order),
		"partial_cmp": int(partial),
		"precedence":  int(a.CmpPrecedence(&b)),
		"eq":          rs.Eq(a, b),
		"lt":          order < 0,
		"le":          order <= 0,
		"gt":          order > 0,
		"ge":          order >= 0,
		"pre":         oracleOrdering(rs.Cmp(a.Pre, b.Pre)),
		"build":       oracleOrdering(rs.Cmp(a.Build, b.Build)),
		"pre_eq":      rs.Eq(a.Pre, b.Pre),
		"build_eq":    rs.Eq(a.Build, b.Build),
		"max":         maximum.String(),
		"min":         minimum.String(),
	}
}

func oracleCmpIdentifier[T any](a T, errA error, b T, errB error) any {
	if errA != nil || errB != nil {
		return map[string]any{"invalid": true}
	}
	return map[string]any{
		"cmp":         oracleOrdering(rs.Cmp(a, b)),
		"partial_cmp": int(rs.PartialCmp(a, b).Unwrap()),
		"eq":          rs.Eq(a, b),
		"lt":          rs.Cmp(a, b) < 0,
		"self":        oracleOrdering(rs.Cmp(a, rs.Clone(a))),
	}
}

func oracleSort(texts []string) any {
	versions := []Version{}
	for _, text := range texts {
		version, err := VersionParse(text)
		if err != nil {
			return map[string]any{"invalid": true}
		}
		versions = append(versions, version)
	}
	byPrecedence := rs.Clone(versions)
	// A stable sort: versions that differ only in build metadata keep their order.
	slices.SortStableFunc(byPrecedence, func(a, b Version) int { return int(a.CmpPrecedence(&b)) })
	total := versions
	slices.SortStableFunc(total, rs.Cmp[Version])
	displayed := func(versions []Version) []string {
		texts := []string{}
		for i := range versions {
			texts = append(texts, versions[i].String())
		}
		return texts
	}
	return map[string]any{"by_precedence": displayed(byPrecedence), "total": displayed(total)}
}

// oracleWrapper is the oracle's Wrapper: a struct whose Debug is derived,
// holding a version as Mjolnir's BuildIdentity does.
type oracleWrapper struct {
	Version  *Version          `rs:"version"`
	Revision rs.Option[string] `rs:"revision"`
}

var _ = rs.RegisterStruct[oracleWrapper]("Wrapper")

func oracleDebug(text string) any {
	version, err := VersionParse(text)
	if err != nil {
		return oracleErrorReply(err, true)
	}
	wrapper := oracleWrapper{Version: &version, Revision: rs.Some("abc")}
	return map[string]any{
		"debug":         rs.Format("{:?}", &version),
		"pretty":        rs.Format("{:#?}", &version),
		"option":        rs.Format("{:?}", rs.Some(&version)),
		"list":          rs.Format("{:?}", [2]*Version{&version, &version}),
		"nested":        rs.Format("{:?}", wrapper),
		"nested_pretty": rs.Format("{:#?}", wrapper),
	}
}

// oracleSpecs are the format specs the oracle's `format` knows.
var oracleSpecs = []string{
	"{}", "{:20}", "{:<20}", "{:>20}", "{:^20}", "{:*^20}", "{:*^21}", "{:-<9}", "{:0>12}", "{:012}", "{:5}", "{:.3}",
	"{:10.3}", "{:é^11}", "{:🦀<12}", "{:>1}", "{:^9}", "{:+}", "{:#}", "{:+>14}", "{:#?}", "{:?}", "{:8?}", "{:>40?}",
}

func oracleFormat(request oracleRequest) any {
	if !slices.Contains(oracleSpecs, request.Spec) {
		panic(rs.Panicf("the oracle does not know the spec {}", request.Spec))
	}
	switch request.What {
	case "", "version":
		version, err := VersionParse(request.Text)
		if err != nil {
			return oracleErrorReply(err, true)
		}
		return map[string]any{"text": rs.Format(request.Spec, &version)}
	case "pre":
		pre, err := PrereleaseNew(request.Text)
		if err != nil {
			return oracleErrorReply(err, true)
		}
		return map[string]any{"text": rs.Format(request.Spec, &pre)}
	case "build":
		build, err := BuildMetadataNew(request.Text)
		if err != nil {
			return oracleErrorReply(err, true)
		}
		return map[string]any{"text": rs.Format(request.Spec, &build)}
	case "error":
		_, err := VersionParse(request.Text)
		if err == nil {
			return map[string]any{"invalid": true}
		}
		return map[string]any{"text": rs.Format(request.Spec, err)}
	default:
		panic(rs.Panicf("unknown value kind {}", request.What))
	}
}

func oracleDecode(t *testing.T, raw json.RawMessage) oracleRequest {
	var request oracleRequest
	if err := json.Unmarshal(raw, &request); err != nil {
		t.Fatalf("decode %s: %v", raw, err)
	}
	return request
}

func oracleAnswer(t *testing.T) func(json.RawMessage) any {
	return func(raw json.RawMessage) any {
		request := oracleDecode(t, raw)
		return oracle.Catch(func() any {
			switch request.Op {
			case "parse":
				return oracleParse(request.Text, false)
			case "prerelease":
				return oraclePrerelease(request.Text)
			case "build":
				return oracleBuild(request.Text)
			case "cmp":
				return oracleCmp(request.A, request.B)
			case "cmp_pre":
				a, errA := PrereleaseNew(request.A)
				b, errB := PrereleaseNew(request.B)
				return oracleCmpIdentifier(a, errA, b, errB)
			case "cmp_build":
				a, errA := BuildMetadataNew(request.A)
				b, errB := BuildMetadataNew(request.B)
				return oracleCmpIdentifier(a, errA, b, errB)
			case "sort":
				return oracleSort(request.Versions)
			default:
				t.Fatalf("op %q is not in this corpus", request.Op)
				return nil
			}
		})
	}
}

func oracleAnswerFmt(t *testing.T) func(json.RawMessage) any {
	return func(raw json.RawMessage) any {
		request := oracleDecode(t, raw)
		return oracle.Catch(func() any {
			switch request.Op {
			case "parse":
				return oracleParse(request.Text, true)
			case "debug":
				return oracleDebug(request.Text)
			case "format":
				return oracleFormat(request)
			default:
				t.Fatalf("op %q is not in this corpus", request.Op)
				return nil
			}
		})
	}
}

// oracleFixed are versions and near-versions written out: the crate's own
// test inputs, the shapes Mjolnir parses (release tags with and without a
// `v`, build metadata carrying a Git revision and two times), and the
// boundaries of each component.
var oracleFixed = []string{
	"", "  ", "1", "1.2", "1.2.3-", "a.b.c", "1.2.3 abc", "1.2.3-01", "1.2.3++", "07", "111111111111111111111.0.0", "8\x00",
	"1.2.3", "1.2.3-alpha1", "1.2.3+build5", "1.2.3+5build", "1.2.3-alpha1+build5", "1.2.3-1.alpha1.9+build5.7.3aedf",
	"1.2.3-0a.alpha1.9+05build.7.3aedf", "0.4.0-beta.1+0851523", "1.1.0-beta-10", "1.q.r", "1.20.0+c144a98", "1.20.0",
	"1.0.0", "1.0.0-alpha", "1.20.0+bc17664", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-beta", "1.0.0-beta.2",
	"1.0.0-beta.11", "1.0.0-rc.1", "1.2.3-rc1", "0.0.0", "0.0.0-reserved", "0.11.0-shitshow", "0.7.0-alpha.1",
	"0.12.20+1.1.0", "0.1.13+deprecated", "2.0.4+20210327", "0.0.6+c7fcc0e", "0.8.1+zstd.1.5.0",
	"2.24.0+19485f1719485f1719485f1719485f1719485f17.c1790824791.m1790830000123456789",
	"2.24.0+6a2a07136a2a07136a2a07136a2a07136a2a0713.c100.m5000", "2.24.0+19485F17", "v2.4.2", "2.4.2", "V1.0.0",
	"9.9.9", "2.45.0", "2.5.0", "=1.2.3", "^1.2.3", "~1.2.3", ">=1.2.3", "*", "1.2.*", "1.2.x", "1.2.3.4", "1.2.3.",
	".1.2.3", "1..3", "1.2..3", "-1.2.3", "+1.2.3", "1.2.3-+", "1.2.3+-", "1.2.3--", "1.2.3+--", "1.2.3-a+b+c",
	"1.2.3-a-b+c-d", "1.2.3-.", "1.2.3+.", "1.2.3-a.", "1.2.3+a.", "1.2.3-.a", "1.2.3+.a", "1.2.3-a..b", "1.2.3+a..b",
	"1.2.3-0", "1.2.3-00", "1.2.3-0.0", "1.2.3-0.00", "1.2.3-00a", "1.2.3-0a", "1.2.3-01-", "1.2.3--01", "1.2.3+00",
	"1.2.3+01.001", "1.2.3-a_b", "1.2.3-a~b", "1.2.3-a b", "1.2.3+a b", "1.2.3-é", "1.2.3-aé", "1.2.3+é", "1.2.3+aé",
	" 1.2.3", "1.2.3 ", "1.2.3\n", "1.2.3\t", "\t1.2.3", "1.2.3\x00", "1.2.3-\x00", "1.2.3+\x00", "1.2.3-a\x00",
	"1.2.3+a\x00", "1.2.3\u0301", "1.2.3-alpha\u0301", "1.2.3\u200b", "1.2.3\ufeff", "1.2.3😀", "😀", "é", "1.é",
	"1.2.é", "01.2.3", "1.02.3", "1.2.03", "0.0.0-0", "0.0.0+0", "00.0.0", "0.00.0", "0.0.00", "18446744073709551615.0.0",
	"18446744073709551616.0.0", "0.18446744073709551615.0", "0.18446744073709551616.0", "0.0.18446744073709551615",
	"0.0.18446744073709551616", "18446744073709551615.18446744073709551615.18446744073709551615",
	"18446744073709551610.0.0", "18446744073709551619.0.0", "1844674407370955161.0.0", "18446744073709551620.0.0",
	"99999999999999999999.0.0", "1.2.3-18446744073709551616", "1.2.3-99999999999999999999999999999",
	"1.2.3+99999999999999999999999999999", "1.2.3-" + strings.Repeat("a", 8), "1.2.3-" + strings.Repeat("a", 9),
	"1.2.3-" + strings.Repeat("a", 127), "1.2.3-" + strings.Repeat("a", 128), "1.2.3+" + strings.Repeat("1", 300),
	"1.2.3-" + strings.Repeat("a.", 40) + "a", "1.2.3-" + strings.Repeat("1", 1000),
}

// oracleChars returns the code points put in every position of a version:
// all of ASCII and Latin-1, characters the Debug escape of a char treats
// specially (controls, white space, grapheme extenders, format and
// default-ignorable characters, private use, unassigned and noncharacters,
// the two halfwidth katakana marks it leaves alone), and seeded random
// scalar values from every plane.
func oracleChars(random *rand.Rand) []rune {
	chars := []rune{}
	for c := rune(0); c < 0x100; c++ {
		chars = append(chars, c)
	}
	chars = append(chars,
		0x300, 0x301, 0x34F, 0x378, 0x37E, 0x483, 0x488, 0x5BF, 0x600, 0x61C, 0x6DD, 0x70F, 0x8E2, 0x93A, 0x115F,
		0x1160, 0x1680, 0x17B4, 0x180B, 0x180E, 0x180F, 0x2000, 0x200A, 0x200B, 0x200C, 0x200D, 0x200E, 0x2028,
		0x2029, 0x202A, 0x202F, 0x205F, 0x2060, 0x2064, 0x2065, 0x206F, 0x2400, 0x3000, 0x302A, 0x3164, 0xD7FF,
		0xE000, 0xF8FF, 0xFDD0, 0xFE00, 0xFE0F, 0xFEFF, 0xFF9E, 0xFF9F, 0xFFA0, 0xFFF0, 0xFFF9, 0xFFFB, 0xFFFC,
		0xFFFD, 0xFFFE, 0xFFFF, 0x10000, 0x110BD, 0x1BCA0, 0x1D173, 0x1D17A, 0x1F3FB, 0x1F600, 0x1F9D1, 0x1FFFE,
		0x20000, 0x2A6DF, 0x2FFFF, 0x30000, 0x3134A, 0x323AF, 0x323B0, 0xE0000, 0xE0001, 0xE0020, 0xE007F, 0xE0080,
		0xE0100, 0xE01EF, 0xE01F0, 0xF0000, 0xFFFFD, 0x100000, 0x10FFFD, 0x10FFFF,
	)
	for range 200 {
		chars = append(chars, oracleScalar(random, 0x10FFFF))
	}
	for range 200 {
		chars = append(chars, oracleScalar(random, 0xFFFF))
	}
	return chars
}

// oracleScalar is a random Unicode scalar value up to limit (surrogates are
// not chars).
func oracleScalar(random *rand.Rand, limit int) rune {
	for {
		c := rune(random.IntN(limit + 1))
		if c < 0xD800 || c > 0xDFFF {
			return c
		}
	}
}

var oracleNumbers = []string{
	"0", "1", "9", "10", "99", "100", "00", "01", "007", "10000000000", "1844674407370955161", "18446744073709551609",
	"18446744073709551610", "18446744073709551615", "18446744073709551616", "18446744073709551619",
	"18446744073709551620", "99999999999999999999", "111111111111111111111", "", "a", "-1", "+1", " 1", "1 ", "１", "٣",
	"0x1", "1e3", "*", "x",
}

var oraclePres = []string{
	"", "alpha", "alpha.1", "alpha.beta", "beta", "beta.2", "beta.11", "rc.1", "0", "00", "01", "0a", "a0", "1a0",
	"-", "--", "-1", "1-", "0-0", "a.b.c.d.e", "x.7.z.92", "A-Z", "a.", ".a", "a..b", "a_b", "a+b", "é", "1.0",
	"1.00", "0.0.0", "000", "18446744073709551616", strings.Repeat("a", 8), strings.Repeat("a", 9),
	strings.Repeat("9", 20), "alpha.0a", "alpha.85", "alpha.90", "alpha.200", "alpha.a", "Alpha", "ALPHA", "a-",
	"pre12", "pre8", "pre.12", "pre.8", "pre.x", "1", "2", "11", "a.1", "a.b",
}

var oracleBuilds = []string{
	"", "build", "build.42", "5build", "05build.7.3aedf", "0851523", "001", "01", "1", "00", "0", "000", "2", "02",
	"002", "10", "demo", "demo.85", "demo.90", "demo.090", "demo.200", "demo.1a0", "demo.a", "memo", "-", "a-b",
	"zstd.1.5.0", "1.1.0", "deprecated", "20210327", "c7fcc0e", "a.", ".a", "a..b", "a_b", "é", "+", "1.0", "1.00",
	"19485f1719485f1719485f1719485f1719485f17.c1790824791.m1790830000123456789", strings.Repeat("0", 30) + "1",
	strings.Repeat("0", 31) + "1", "99999999999999999999", "100000000000000000000", "a", "A", "Z", "z",
}

// oracleTriples are major.minor.patch parts shared by many versions, so that
// pre-release and build metadata decide the comparisons.
var oracleTriples = []string{"1.2.3", "1.2.3", "1.2.3", "1.2.4", "1.3.0", "0.0.0", "2.0.0", "18446744073709551615.0.0", "1.10.3"}

// oracleRandomVersion builds a version from random parts: mostly valid, and
// sometimes broken in one place.
func oracleRandomVersion(random *rand.Rand) string {
	var text strings.Builder
	if random.IntN(4) == 0 {
		for i := range 3 {
			if i > 0 {
				text.WriteString(".")
			}
			text.WriteString(oracleNumbers[random.IntN(len(oracleNumbers))])
		}
	} else {
		text.WriteString(oracleTriples[random.IntN(len(oracleTriples))])
	}
	if random.IntN(3) > 0 {
		text.WriteString("-" + oraclePres[random.IntN(len(oraclePres))])
	}
	if random.IntN(2) == 0 {
		text.WriteString("+" + oracleBuilds[random.IntN(len(oracleBuilds))])
	}
	if random.IntN(12) == 0 {
		junk := []string{" ", ".", "-", "+", "x", "é", "\x00", "..", "+a", "-a", "_"}
		text.WriteString(junk[random.IntN(len(junk))])
	}
	return text.String()
}

func oracleRandom(seed uint64) *rand.Rand {
	return rand.New(rand.NewPCG(seed, 0x73656d766572))
}

func TestDiffAgainstRust(t *testing.T) {
	oracle.Check(t, "semver", "semver", func(yield func(any)) {
		random := oracleRandom(1)

		parse := func(text string) { yield(map[string]any{"op": "parse", "text": text}) }
		for _, text := range oracleFixed {
			parse(text)
		}

		// Every character in the places a parse error names it, and in
		// identifiers.
		for _, c := range oracleChars(random) {
			char := string(c)
			parse(char)
			parse("1.2.3" + char)
			if c < 0x100 {
				parse("1." + char)
				parse("1.2." + char)
				parse("1" + char)
				parse("1.2" + char)
				parse("1.2.3-a" + char)
				parse("1.2.3+b" + char)
				yield(map[string]any{"op": "prerelease", "text": "a" + char})
				yield(map[string]any{"op": "build", "text": "a" + char})
			}
		}

		// Each number in each numeric position, and in identifiers.
		for _, number := range oracleNumbers {
			parse(number + ".0.0")
			parse("0." + number + ".0")
			parse("0.0." + number)
			parse(number)
			parse(number + ".")
			parse("1.2.3-" + number)
			parse("1.2.3+" + number)
			parse("1.2.3-a." + number)
		}

		// Pre-release and build metadata on their own, with each other and
		// through Version::parse.
		for _, pre := range oraclePres {
			yield(map[string]any{"op": "prerelease", "text": pre})
			yield(map[string]any{"op": "build", "text": pre})
			parse("1.2.3-" + pre)
		}
		for _, build := range oracleBuilds {
			yield(map[string]any{"op": "prerelease", "text": build})
			yield(map[string]any{"op": "build", "text": build})
			parse("1.2.3+" + build)
			parse("1.2.3-" + oraclePres[random.IntN(len(oraclePres))] + "+" + build)
		}
		for range 800 * oracleScale() {
			parse(oracleRandomVersion(random))
		}

		// Comparisons: every pair of a pool that shares major, minor and
		// patch, so that pre-release and build metadata decide.
		pool := []string{
			"1.2.3", "1.2.3-alpha", "1.2.3-alpha.1", "1.2.3-alpha.beta", "1.2.3-beta.2", "1.2.3-beta.11",
			"1.2.3-rc.1", "1.2.3+build", "1.2.3+build.1", "1.2.3+001", "1.2.3+1", "1.2.3+01", "1.2.3-alpha+1",
			"1.2.3-alpha+01", "1.2.3-1", "1.2.3-2", "1.2.3-11", "1.2.3-a", "1.2.3-A", "1.2.3--", "1.2.3-1a",
			"1.2.4", "1.2.4-0", "1.3.0", "0.0.0", "2.0.0-0", "18446744073709551615.0.0", "1.10.3", "1.9.3",
		}
		for _, a := range pool {
			for _, b := range pool {
				yield(map[string]any{"op": "cmp", "a": a, "b": b})
			}
		}
		for range 600 * oracleScale() {
			yield(map[string]any{"op": "cmp", "a": oracleRandomVersion(random), "b": oracleRandomVersion(random)})
		}

		// Identifiers against each other.
		for _, a := range oraclePres {
			for _, b := range oraclePres {
				yield(map[string]any{"op": "cmp_pre", "a": a, "b": b})
			}
		}
		for _, a := range oracleBuilds {
			for _, b := range oracleBuilds {
				yield(map[string]any{"op": "cmp_build", "a": a, "b": b})
			}
		}

		// Sorting, where a stable sort by precedence keeps the order of
		// versions that differ only in build metadata.
		for range 300 * oracleScale() {
			versions := []string{}
			for range 1 + random.IntN(12) {
				version := oracleTriples[random.IntN(3)]
				if random.IntN(2) == 0 {
					version += "-" + []string{"alpha", "alpha.1", "beta", "1", "rc.1"}[random.IntN(5)]
				}
				if random.IntN(2) == 0 {
					version += "+" + []string{"a", "b", "1", "01", "001", "c144a98", "bc17664"}[random.IntN(7)]
				}
				versions = append(versions, version)
			}
			yield(map[string]any{"op": "sort", "versions": versions})
		}
		yield(map[string]any{"op": "sort", "versions": []string{}})
		yield(map[string]any{"op": "sort", "versions": []string{"1.2.3", "nope"}})
	}, oracleAnswer(t))
}

// oracleFormatterPorted reports whether rs.Format and rs.Formatter are
// implemented yet. Until they are, they panic with "not yet ported".
func oracleFormatterPorted() (ported bool) {
	defer func() {
		if recovered := recover(); recovered != nil {
			if strings.HasPrefix(rs.PanicInfoOf(recovered).Message, "not yet ported: core::fmt") {
				ported = false
				return
			}
			panic(recovered)
		}
	}()
	_ = rs.Format("{:#?} {:>4}", rs.Some(1), "x")
	return true
}

// TestDiffFmtAgainstRust compares what goes through rs.Formatter. Until the
// formatter exists it is skipped, except when recording.
func TestDiffFmtAgainstRust(t *testing.T) {
	if os.Getenv("MJ_ORACLE") != "record" && !oracleFormatterPorted() {
		t.Skip("waiting for rs.Format and rs.Formatter (go/rs/formatter_pending.go)")
	}
	oracle.Check(t, "semver", "fmt", func(yield func(any)) {
		random := oracleRandom(2)
		versions := []string{
			"1.2.3", "1.2.3-rc1", "1.2.3+build.5", "1.2.3-alpha.1+build.5", "0.0.0", "10.20.30-beta-10+0851523",
			"18446744073709551615.18446744073709551615.18446744073709551615", "1.2.3-" + strings.Repeat("a", 30),
			"2.24.0+19485f1719485f1719485f1719485f1719485f17.c1790824791.m1790830000123456789", "1.2",
			"1.2.3-01", "1.2.3\u0301",
		}
		for _, text := range versions {
			yield(map[string]any{"op": "debug", "text": text})
			for _, spec := range oracleSpecs {
				yield(map[string]any{"op": "format", "text": text, "spec": spec})
			}
		}
		for _, text := range []string{"rc.1", "alpha", "", "a.b.c.d.e.f.g.h.i.j.k", "01"} {
			for _, spec := range oracleSpecs {
				yield(map[string]any{"op": "format", "what": "pre", "text": text, "spec": spec})
			}
		}
		for _, text := range []string{"build.5", "001", "", "x.y.z.0000000000000000000", "a_b"} {
			for _, spec := range oracleSpecs {
				yield(map[string]any{"op": "format", "what": "build", "text": text, "spec": spec})
			}
		}
		for _, text := range []string{"", "1.2", "1.2.3-01", "1.2.3\u0301", "1.q.r", "18446744073709551616.0.0", "1.2.3"} {
			for _, spec := range oracleSpecs {
				yield(map[string]any{"op": "format", "what": "error", "text": text, "spec": spec})
			}
		}
		// The parse answers again, through the formatter.
		for _, text := range oracleFixed {
			yield(map[string]any{"op": "parse", "text": text})
			yield(map[string]any{"op": "debug", "text": text})
		}
		for _, c := range oracleChars(random) {
			yield(map[string]any{"op": "parse", "text": "1.2.3" + string(c)})
		}
		for range 200 * oracleScale() {
			yield(map[string]any{"op": "debug", "text": oracleRandomVersion(random)})
		}
	}, oracleAnswerFmt(t))
}
