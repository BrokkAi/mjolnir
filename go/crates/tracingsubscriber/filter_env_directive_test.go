// Port of tracing-subscriber 0.3.23 src/filter/env/directive.rs

package tracingsubscriber

import (
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/tracing"
	"github.com/BrokkAi/mjolnir/go/rs"
)

func parseDirectives(dirs string) []*FilterDirective {
	var parsed []*FilterDirective
	for _, s := range strings.Split(dirs, ",") {
		if d, err := FilterDirectiveFromStr(s); err == nil {
			parsed = append(parsed, d)
		}
	}
	return parsed
}

func expectParse(t *testing.T, dirs string) []*FilterDirective {
	t.Helper()
	var parsed []*FilterDirective
	for _, s := range strings.Split(dirs, ",") {
		d, err := FilterDirectiveFromStr(s)
		if err != nil {
			t.Fatalf("directive '%q' should parse: %v", s, err)
		}
		parsed = append(parsed, d)
	}
	return parsed
}

// sortUnstable is `dirs.sort_unstable()`: the order of equal directives is
// not looked at by these tests.
func sortUnstable(dirs []*FilterDirective) {
	slices.SortFunc(dirs, (*FilterDirective).compare)
}

func targetsOf(dirs []*FilterDirective) []string {
	var sorted []string
	for _, d := range dirs {
		sorted = append(sorted, d.target.Unwrap())
	}
	return sorted
}

func TestFilterEnvDirective_directive_ordering_by_target_len(t *testing.T) {
	// TODO(eliza): it would be nice to have a property-based test for this
	// instead.
	dirs := expectParse(t, "foo::bar=debug,foo::bar::baz=trace,foo=info,a_really_long_name_with_no_colons=warn")
	sortUnstable(dirs)

	expected := []string{
		"a_really_long_name_with_no_colons",
		"foo::bar::baz",
		"foo::bar",
		"foo",
	}
	assertEqual(t, expected, targetsOf(dirs))
}

func TestFilterEnvDirective_directive_ordering_by_span(t *testing.T) {
	// TODO(eliza): it would be nice to have a property-based test for this
	// instead.
	dirs := expectParse(t, "bar[span]=trace,foo=debug,baz::quux=info,a[span]=warn")
	sortUnstable(dirs)

	expected := []string{"baz::quux", "bar", "foo", "a"}
	assertEqual(t, expected, targetsOf(dirs))
}

func TestFilterEnvDirective_directive_ordering_uses_lexicographic_when_equal(t *testing.T) {
	// TODO(eliza): it would be nice to have a property-based test for this
	// instead.
	dirs := expectParse(t, "span[b]=debug,b=debug,a=trace,c=info,span[a]=info")
	sortUnstable(dirs)

	expected := []rs.Tuple2[string, rs.Option[string]]{
		rs.T2("span", rs.Some("b")),
		rs.T2("span", rs.Some("a")),
		rs.T2("c", rs.None[string]()),
		rs.T2("b", rs.None[string]()),
		rs.T2("a", rs.None[string]()),
	}
	var sorted []rs.Tuple2[string, rs.Option[string]]
	for _, d := range dirs {
		sorted = append(sorted, rs.T2(d.target.Unwrap(), d.inSpan))
	}
	assertEqual(t, expected, sorted)
}

// TODO: this test requires the parser to support directives with multiple
// fields, which it currently can't handle. We should enable this test when
// that's implemented.
func TestFilterEnvDirective_directive_ordering_by_field_num(t *testing.T) {
	ignored(t, "the parser cannot read directives with several fields yet")
	// TODO(eliza): it would be nice to have a property-based test for this
	// instead.
	dirs := expectParse(t,
		"b[{foo,bar}]=info,c[{baz,quuux,quuux}]=debug,a[{foo}]=warn,bar[{field}]=trace,foo=debug,baz::quux=info")
	sortUnstable(dirs)

	expected := []string{"baz::quux", "bar", "foo", "c", "b", "a"}
	assertEqual(t, expected, targetsOf(dirs))
}

type expectedDirective struct {
	target rs.Option[string]
	level  tracing.LevelFilter
	inSpan rs.Option[string]
}

func assertDirectives(t *testing.T, dirs []*FilterDirective, expected []expectedDirective) {
	t.Helper()
	if len(dirs) != len(expected) {
		t.Fatalf("assertion `left == right` failed: \nparsed: %d directives, expected %d", len(dirs), len(expected))
	}
	for i, want := range expected {
		assertEqual(t, dirs[i].target, want.target)
		assertEqual(t, dirs[i].level, want.level)
		assertEqual(t, dirs[i].inSpan, want.inSpan)
	}
}

func target(name string) rs.Option[string] { return rs.Some(name) }

var noSpan = rs.None[string]()

func TestFilterEnvDirective_parse_directives_ralith(t *testing.T) {
	dirs := parseDirectives("common=trace,server=trace")
	assertDirectives(t, dirs, []expectedDirective{
		{target("common"), tracing.LevelFilterTrace, noSpan},
		{target("server"), tracing.LevelFilterTrace, noSpan},
	})
}

func TestFilterEnvDirective_parse_directives_ralith_uc(t *testing.T) {
	dirs := parseDirectives("common=INFO,server=DEBUG")
	assertDirectives(t, dirs, []expectedDirective{
		{target("common"), tracing.LevelFilterInfo, noSpan},
		{target("server"), tracing.LevelFilterDebug, noSpan},
	})
}

func TestFilterEnvDirective_parse_directives_ralith_mixed(t *testing.T) {
	dirs := parseDirectives("common=iNfo,server=dEbUg")
	assertDirectives(t, dirs, []expectedDirective{
		{target("common"), tracing.LevelFilterInfo, noSpan},
		{target("server"), tracing.LevelFilterDebug, noSpan},
	})
}

func TestFilterEnvDirective_parse_directives_valid(t *testing.T) {
	dirs := parseDirectives("crate1::mod1=error,crate1::mod2,crate2=debug,crate3=off")
	assertDirectives(t, dirs, []expectedDirective{
		{target("crate1::mod1"), tracing.LevelFilterError, noSpan},
		{target("crate1::mod2"), tracing.LevelFilterTrace, noSpan},
		{target("crate2"), tracing.LevelFilterDebug, noSpan},
		{target("crate3"), tracing.LevelFilterOff, noSpan},
	})
}

var sixDirectives = []expectedDirective{
	{target("crate1::mod1"), tracing.LevelFilterError, noSpan},
	{target("crate1::mod2"), tracing.LevelFilterWarn, noSpan},
	{target("crate1::mod2::mod3"), tracing.LevelFilterInfo, noSpan},
	{target("crate2"), tracing.LevelFilterDebug, noSpan},
	{target("crate3"), tracing.LevelFilterTrace, noSpan},
	{target("crate3::mod2::mod1"), tracing.LevelFilterOff, noSpan},
}

func TestFilterEnvDirective_parse_level_directives(t *testing.T) {
	dirs := parseDirectives("crate1::mod1=error,crate1::mod2=warn,crate1::mod2::mod3=info," +
		"crate2=debug,crate3=trace,crate3::mod2::mod1=off")
	assertDirectives(t, dirs, sixDirectives)
}

func TestFilterEnvDirective_parse_uppercase_level_directives(t *testing.T) {
	dirs := parseDirectives("crate1::mod1=ERROR,crate1::mod2=WARN,crate1::mod2::mod3=INFO," +
		"crate2=DEBUG,crate3=TRACE,crate3::mod2::mod1=OFF")
	assertDirectives(t, dirs, sixDirectives)
}

func TestFilterEnvDirective_parse_numeric_level_directives(t *testing.T) {
	dirs := parseDirectives("crate1::mod1=1,crate1::mod2=2,crate1::mod2::mod3=3,crate2=4," +
		"crate3=5,crate3::mod2::mod1=0")
	assertDirectives(t, dirs, sixDirectives)
}

func TestFilterEnvDirective_parse_directives_invalid_crate(t *testing.T) {
	// test parse_directives with multiple = in specification
	dirs := parseDirectives("crate1::mod1=warn=info,crate2=debug")
	assertDirectives(t, dirs, []expectedDirective{{target("crate2"), tracing.LevelFilterDebug, noSpan}})
}

func TestFilterEnvDirective_parse_directives_invalid_level(t *testing.T) {
	// test parse_directives with 'noNumber' as log level
	dirs := parseDirectives("crate1::mod1=noNumber,crate2=debug")
	assertDirectives(t, dirs, []expectedDirective{{target("crate2"), tracing.LevelFilterDebug, noSpan}})
}

func TestFilterEnvDirective_parse_directives_string_level(t *testing.T) {
	// test parse_directives with 'warn' as log level
	dirs := parseDirectives("crate1::mod1=wrong,crate2=warn")
	assertDirectives(t, dirs, []expectedDirective{{target("crate2"), tracing.LevelFilterWarn, noSpan}})
}

func TestFilterEnvDirective_parse_directives_empty_level(t *testing.T) {
	// test parse_directives with '' as log level
	dirs := parseDirectives("crate1::mod1=wrong,crate2=")
	assertDirectives(t, dirs, []expectedDirective{{target("crate2"), tracing.LevelFilterTrace, noSpan}})
}

func TestFilterEnvDirective_parse_directives_global(t *testing.T) {
	// test parse_directives with no crate
	dirs := parseDirectives("warn,crate2=debug")
	assertDirectives(t, dirs, []expectedDirective{
		{rs.None[string](), tracing.LevelFilterWarn, noSpan},
		{target("crate2"), tracing.LevelFilterDebug, noSpan},
	})
}

// helper function for tests below
func testParseBareLevel(t *testing.T, directiveToTest string, levelExpected tracing.LevelFilter) {
	t.Helper()
	dirs := parseDirectives(directiveToTest)
	assertDirectives(t, dirs, []expectedDirective{{rs.None[string](), levelExpected, noSpan}})
}

func TestFilterEnvDirective_parse_directives_global_bare_warn_lc(t *testing.T) {
	// test parse_directives with no crate, in isolation, all lowercase
	testParseBareLevel(t, "warn", tracing.LevelFilterWarn)
}

func TestFilterEnvDirective_parse_directives_global_bare_warn_uc(t *testing.T) {
	// test parse_directives with no crate, in isolation, all uppercase
	testParseBareLevel(t, "WARN", tracing.LevelFilterWarn)
}

func TestFilterEnvDirective_parse_directives_global_bare_warn_mixed(t *testing.T) {
	// test parse_directives with no crate, in isolation, mixed case
	testParseBareLevel(t, "wArN", tracing.LevelFilterWarn)
}

func TestFilterEnvDirective_parse_directives_valid_with_spans(t *testing.T) {
	dirs := parseDirectives("crate1::mod1[foo]=error,crate1::mod2[bar],crate2[baz]=debug")
	assertDirectives(t, dirs, []expectedDirective{
		{target("crate1::mod1"), tracing.LevelFilterError, rs.Some("foo")},
		{target("crate1::mod2"), tracing.LevelFilterTrace, rs.Some("bar")},
		{target("crate2"), tracing.LevelFilterDebug, rs.Some("baz")},
	})
}

func TestFilterEnvDirective_parse_directives_with_dash_in_target_name(t *testing.T) {
	dirs := parseDirectives("target-name=info")
	assertDirectives(t, dirs, []expectedDirective{{target("target-name"), tracing.LevelFilterInfo, noSpan}})
}

func TestFilterEnvDirective_parse_directives_with_dash_in_span_name(t *testing.T) {
	// Reproduces https://github.com/tokio-rs/tracing/issues/1367

	dirs := parseDirectives("target[span-name]=info")
	assertDirectives(t, dirs, []expectedDirective{{target("target"), tracing.LevelFilterInfo, rs.Some("span-name")}})
}

func TestFilterEnvDirective_parse_directives_with_special_characters_in_span_name(t *testing.T) {
	spanName := "!\"#$%&'()*+-./:;<=>?@^_`|~[}"

	dirs := parseDirectives("target[" + spanName + "]=info")
	assertDirectives(t, dirs, []expectedDirective{{target("target"), tracing.LevelFilterInfo, rs.Some(spanName)}})
}

func TestFilterEnvDirective_parse_directives_with_invalid_span_chars(t *testing.T) {
	invalidSpanName := "]{"

	dirs := parseDirectives("target[" + invalidSpanName + "]=info")
	assertDirectives(t, dirs, nil)
}
