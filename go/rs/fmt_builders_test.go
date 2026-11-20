// Port of coretests 1.99.1 tests/fmt/builders.rs

package rs

import (
	"reflect"
	"strings"
	"testing"
)

// The Rust tests declare a unit struct with a hand-written Debug for each
// case. Go cannot declare methods inside a function, so each is a FmtFromFn
// (fmt::from_fn) holding the body of its fmt method, and `{Foo:?}` passes it
// by name.

func fmtExpectEq(t *testing.T, want, got string) {
	t.Helper()
	if got != want {
		t.Errorf("assertion `left == right` failed\n  left: %q\n right: %q", want, got)
	}
}

func fmtTenTwenty() FmtArguments { return FormatArgs("{}/{}", 10, 20) }

func TestFmtBuildersDebugStruct_test_empty(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugStruct("Foo").Finish()
	})

	fmtExpectEq(t, "Foo", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "Foo", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugStruct_test_single(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugStruct("Foo").Field("bar", true).Finish()
	})

	fmtExpectEq(t, "Foo { bar: true }", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "Foo {\n    bar: true,\n}", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugStruct_test_multiple(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugStruct("Foo").
			Field("bar", true).
			Field("baz", fmtTenTwenty()).
			Finish()
	})

	fmtExpectEq(t, "Foo { bar: true, baz: 10/20 }", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "Foo {\n    bar: true,\n    baz: 10/20,\n}", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugStruct_test_nested(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugStruct("Foo").
			Field("bar", true).
			Field("baz", fmtTenTwenty()).
			Finish()
	})
	bar := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugStruct("Bar").Field("foo", foo).Field("hello", "world").Finish()
	})

	fmtExpectEq(t, `Bar { foo: Foo { bar: true, baz: 10/20 }, hello: "world" }`, Format("{Bar:?}", Arg("Bar", bar)))
	fmtExpectEq(t, `Bar {
    foo: Foo {
        bar: true,
        baz: 10/20,
    },
    hello: "world",
}`, Format("{Bar:#?}", Arg("Bar", bar)))
}

func TestFmtBuildersDebugStruct_test_empty_non_exhaustive(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugStruct("Foo").FinishNonExhaustive()
	})

	fmtExpectEq(t, "Foo { .. }", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "Foo { .. }", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugStruct_test_multiple_and_non_exhaustive(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugStruct("Foo").
			Field("bar", true).
			Field("baz", fmtTenTwenty()).
			FinishNonExhaustive()
	})

	fmtExpectEq(t, "Foo { bar: true, baz: 10/20, .. }", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "Foo {\n    bar: true,\n    baz: 10/20,\n    ..\n}", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugStruct_test_nested_non_exhaustive(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugStruct("Foo").
			Field("bar", true).
			Field("baz", fmtTenTwenty()).
			FinishNonExhaustive()
	})
	bar := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugStruct("Bar").
			Field("foo", foo).
			Field("hello", "world").
			FinishNonExhaustive()
	})

	fmtExpectEq(t, `Bar { foo: Foo { bar: true, baz: 10/20, .. }, hello: "world", .. }`, Format("{Bar:?}", Arg("Bar", bar)))
	fmtExpectEq(t, `Bar {
    foo: Foo {
        bar: true,
        baz: 10/20,
        ..
    },
    hello: "world",
    ..
}`, Format("{Bar:#?}", Arg("Bar", bar)))
}

func TestFmtBuildersDebugStruct_test_field_with(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugStruct("Foo").
			fieldWith("bar", func(f *Formatter) error { return f.WriteStr("true") }).
			fieldWith("baz", func(f *Formatter) error { return f.WriteStr("false") }).
			Finish()
	})

	fmtExpectEq(t, "Foo {\n    bar: true,\n    baz: false,\n}", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugTuple_test_empty(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugTuple("Foo").Finish()
	})

	fmtExpectEq(t, "Foo", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "Foo", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugTuple_test_single(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugTuple("Foo").Field(true).Finish()
	})

	fmtExpectEq(t, "Foo(true)", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "Foo(\n    true,\n)", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugTuple_test_multiple(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugTuple("Foo").Field(true).Field(fmtTenTwenty()).Finish()
	})

	fmtExpectEq(t, "Foo(true, 10/20)", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "Foo(\n    true,\n    10/20,\n)", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugTuple_test_nested(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugTuple("Foo").Field(true).Field(fmtTenTwenty()).Finish()
	})
	bar := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugTuple("Bar").Field(foo).Field("world").Finish()
	})

	fmtExpectEq(t, `Bar(Foo(true, 10/20), "world")`, Format("{Bar:?}", Arg("Bar", bar)))
	fmtExpectEq(t, `Bar(
    Foo(
        true,
        10/20,
    ),
    "world",
)`, Format("{Bar:#?}", Arg("Bar", bar)))
}

func TestFmtBuildersDebugTuple_test_empty_non_exhaustive(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugTuple("Foo").FinishNonExhaustive()
	})

	fmtExpectEq(t, "Foo(..)", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "Foo(..)", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugTuple_test_multiple_and_non_exhaustive(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugTuple("Foo").
			Field(true).
			Field(fmtTenTwenty()).
			FinishNonExhaustive()
	})

	fmtExpectEq(t, "Foo(true, 10/20, ..)", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "Foo(\n    true,\n    10/20,\n    ..\n)", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugTuple_test_nested_non_exhaustive(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugTuple("Foo").
			Field(true).
			Field(fmtTenTwenty()).
			FinishNonExhaustive()
	})
	bar := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugTuple("Bar").Field(foo).Field("world").FinishNonExhaustive()
	})

	fmtExpectEq(t, `Bar(Foo(true, 10/20, ..), "world", ..)`, Format("{Bar:?}", Arg("Bar", bar)))
	fmtExpectEq(t, `Bar(
    Foo(
        true,
        10/20,
        ..
    ),
    "world",
    ..
)`, Format("{Bar:#?}", Arg("Bar", bar)))
}

func TestFmtBuildersDebugMap_test_empty(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugMap().Finish()
	})

	fmtExpectEq(t, "{}", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "{}", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugMap_test_single(t *testing.T) {
	entry := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugMap().Entry("bar", true).Finish()
	})
	keyValue := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugMap().Key("bar").Value(true).Finish()
	})

	fmtExpectEq(t, Format("{Entry:?}", Arg("Entry", entry)), Format("{KeyValue:?}", Arg("KeyValue", keyValue)))
	fmtExpectEq(t, Format("{Entry:#?}", Arg("Entry", entry)), Format("{KeyValue:#?}", Arg("KeyValue", keyValue)))

	fmtExpectEq(t, `{"bar": true}`, Format("{Entry:?}", Arg("Entry", entry)))
	fmtExpectEq(t, "{\n    \"bar\": true,\n}", Format("{Entry:#?}", Arg("Entry", entry)))
}

func TestFmtBuildersDebugMap_test_multiple(t *testing.T) {
	entry := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugMap().
			Entry("bar", true).
			Entry(10, fmtTenTwenty()).
			Finish()
	})
	keyValue := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugMap().
			Key("bar").
			Value(true).
			Key(10).
			Value(fmtTenTwenty()).
			Finish()
	})

	fmtExpectEq(t, Format("{Entry:?}", Arg("Entry", entry)), Format("{KeyValue:?}", Arg("KeyValue", keyValue)))
	fmtExpectEq(t, Format("{Entry:#?}", Arg("Entry", entry)), Format("{KeyValue:#?}", Arg("KeyValue", keyValue)))

	fmtExpectEq(t, `{"bar": true, 10: 10/20}`, Format("{Entry:?}", Arg("Entry", entry)))
	fmtExpectEq(t, "{\n    \"bar\": true,\n    10: 10/20,\n}", Format("{Entry:#?}", Arg("Entry", entry)))
}

func TestFmtBuildersDebugMap_test_nested(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugMap().
			Entry("bar", true).
			Entry(10, fmtTenTwenty()).
			Finish()
	})
	bar := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugMap().Entry("foo", foo).Entry(foo, "world").Finish()
	})

	fmtExpectEq(t, `{"foo": {"bar": true, 10: 10/20}, {"bar": true, 10: 10/20}: "world"}`, Format("{Bar:?}", Arg("Bar", bar)))
	fmtExpectEq(t, `{
    "foo": {
        "bar": true,
        10: 10/20,
    },
    {
        "bar": true,
        10: 10/20,
    }: "world",
}`, Format("{Bar:#?}", Arg("Bar", bar)))
}

func TestFmtBuildersDebugMap_test_entry_err(t *testing.T) {
	// Ensure errors in a map entry don't trigger panics (#65231)
	errorFmt := FmtFromFnOf(func(*Formatter) error {
		return FmtError{}
	})
	keyValue := func(count int, key, value any) FmtFromFn {
		return FmtFromFnOf(func(fmt *Formatter) error {
			m := fmt.DebugMap()

			for range count {
				m.Entry(key, value)
			}

			return m.Finish()
		})
	}

	var buf strings.Builder

	if Write(&buf, "{:?}", keyValue(1, errorFmt, "bar")) == nil {
		t.Error("KeyValue(1, ErrorFmt, \"bar\") wrote without an error")
	}
	if Write(&buf, "{:?}", keyValue(1, "foo", errorFmt)) == nil {
		t.Error("KeyValue(1, \"foo\", ErrorFmt) wrote without an error")
	}

	if Write(&buf, "{:?}", keyValue(2, errorFmt, "bar")) == nil {
		t.Error("KeyValue(2, ErrorFmt, \"bar\") wrote without an error")
	}
	if Write(&buf, "{:?}", keyValue(2, "foo", errorFmt)) == nil {
		t.Error("KeyValue(2, \"foo\", ErrorFmt) wrote without an error")
	}
}

// fmtExpectPanic runs f and fails unless it panics, as #[should_panic] does.
func fmtExpectPanic(t *testing.T, f func()) {
	t.Helper()
	defer func() {
		if recover() == nil {
			t.Error("did not panic")
		}
	}()
	f()
}

func TestFmtBuildersDebugMap_test_invalid_key_when_entry_is_incomplete(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugMap().Key("bar").Key("invalid").Finish()
	})

	fmtExpectPanic(t, func() { _ = Format("{Foo:?}", Arg("Foo", foo)) })
}

func TestFmtBuildersDebugMap_test_invalid_finish_incomplete_entry(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugMap().Key("bar").Finish()
	})

	fmtExpectPanic(t, func() { _ = Format("{Foo:?}", Arg("Foo", foo)) })
}

func TestFmtBuildersDebugMap_test_invalid_value_before_key(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugMap().Value("invalid").Key("bar").Finish()
	})

	fmtExpectPanic(t, func() { _ = Format("{Foo:?}", Arg("Foo", foo)) })
}

func TestFmtBuildersDebugMap_test_empty_non_exhaustive(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugMap().FinishNonExhaustive()
	})

	fmtExpectEq(t, "{..}", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "{..}", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugMap_test_multiple_and_non_exhaustive(t *testing.T) {
	entry := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugMap().
			Entry("bar", true).
			Entry(10, fmtTenTwenty()).
			FinishNonExhaustive()
	})
	keyValue := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugMap().
			Key("bar").
			Value(true).
			Key(10).
			Value(fmtTenTwenty()).
			FinishNonExhaustive()
	})

	fmtExpectEq(t, Format("{Entry:?}", Arg("Entry", entry)), Format("{KeyValue:?}", Arg("KeyValue", keyValue)))
	fmtExpectEq(t, Format("{Entry:#?}", Arg("Entry", entry)), Format("{KeyValue:#?}", Arg("KeyValue", keyValue)))

	fmtExpectEq(t, `{"bar": true, 10: 10/20, ..}`, Format("{Entry:?}", Arg("Entry", entry)))
	fmtExpectEq(t, "{\n    \"bar\": true,\n    10: 10/20,\n    ..\n}", Format("{Entry:#?}", Arg("Entry", entry)))
}

func TestFmtBuildersDebugMap_test_nested_non_exhaustive(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugMap().
			Entry("bar", true).
			Entry(10, fmtTenTwenty()).
			FinishNonExhaustive()
	})
	bar := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugMap().Entry("foo", foo).Entry(foo, "world").FinishNonExhaustive()
	})

	fmtExpectEq(t, `{"foo": {"bar": true, 10: 10/20, ..}, {"bar": true, 10: 10/20, ..}: "world", ..}`, Format("{Bar:?}", Arg("Bar", bar)))
	fmtExpectEq(t, `{
    "foo": {
        "bar": true,
        10: 10/20,
        ..
    },
    {
        "bar": true,
        10: 10/20,
        ..
    }: "world",
    ..
}`, Format("{Bar:#?}", Arg("Bar", bar)))
}

func TestFmtBuildersDebugSet_test_empty(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugSet().Finish()
	})

	fmtExpectEq(t, "{}", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "{}", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugSet_test_single(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugSet().Entry(true).Finish()
	})

	fmtExpectEq(t, "{true}", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "{\n    true,\n}", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugSet_test_multiple(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugSet().Entry(true).Entry(fmtTenTwenty()).Finish()
	})

	fmtExpectEq(t, "{true, 10/20}", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "{\n    true,\n    10/20,\n}", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugSet_test_nested(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugSet().Entry(true).Entry(fmtTenTwenty()).Finish()
	})
	bar := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugSet().Entry(foo).Entry("world").Finish()
	})

	fmtExpectEq(t, `{{true, 10/20}, "world"}`, Format("{Bar:?}", Arg("Bar", bar)))
	fmtExpectEq(t, `{
    {
        true,
        10/20,
    },
    "world",
}`, Format("{Bar:#?}", Arg("Bar", bar)))
}

func TestFmtBuildersDebugSet_test_empty_non_exhaustive(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugSet().FinishNonExhaustive()
	})

	fmtExpectEq(t, "{..}", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "{..}", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugSet_test_multiple_and_non_exhaustive(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugSet().
			Entry(true).
			Entry(fmtTenTwenty()).
			FinishNonExhaustive()
	})

	fmtExpectEq(t, "{true, 10/20, ..}", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "{\n    true,\n    10/20,\n    ..\n}", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugSet_test_nested_non_exhaustive(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugSet().
			Entry(true).
			Entry(fmtTenTwenty()).
			FinishNonExhaustive()
	})
	bar := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugSet().Entry(foo).Entry("world").FinishNonExhaustive()
	})

	fmtExpectEq(t, `{{true, 10/20, ..}, "world", ..}`, Format("{Bar:?}", Arg("Bar", bar)))
	fmtExpectEq(t, `{
    {
        true,
        10/20,
        ..
    },
    "world",
    ..
}`, Format("{Bar:#?}", Arg("Bar", bar)))
}

func TestFmtBuildersDebugList_test_empty(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugList().Finish()
	})

	fmtExpectEq(t, "[]", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "[]", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugList_test_single(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugList().Entry(true).Finish()
	})

	fmtExpectEq(t, "[true]", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "[\n    true,\n]", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugList_test_multiple(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugList().Entry(true).Entry(fmtTenTwenty()).Finish()
	})

	fmtExpectEq(t, "[true, 10/20]", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "[\n    true,\n    10/20,\n]", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugList_test_nested(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugList().Entry(true).Entry(fmtTenTwenty()).Finish()
	})
	bar := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugList().Entry(foo).Entry("world").Finish()
	})

	fmtExpectEq(t, `[[true, 10/20], "world"]`, Format("{Bar:?}", Arg("Bar", bar)))
	fmtExpectEq(t, `[
    [
        true,
        10/20,
    ],
    "world",
]`, Format("{Bar:#?}", Arg("Bar", bar)))
}

func TestFmtBuildersDebugList_test_empty_non_exhaustive(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugList().FinishNonExhaustive()
	})

	fmtExpectEq(t, "[..]", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "[..]", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugList_test_multiple_non_exhaustive(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugList().
			Entry(true).
			Entry(fmtTenTwenty()).
			FinishNonExhaustive()
	})

	fmtExpectEq(t, "[true, 10/20, ..]", Format("{Foo:?}", Arg("Foo", foo)))
	fmtExpectEq(t, "[\n    true,\n    10/20,\n    ..\n]", Format("{Foo:#?}", Arg("Foo", foo)))
}

func TestFmtBuildersDebugList_test_nested_non_exhaustive(t *testing.T) {
	foo := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugList().
			Entry(true).
			Entry(fmtTenTwenty()).
			FinishNonExhaustive()
	})
	bar := FmtFromFnOf(func(fmt *Formatter) error {
		return fmt.DebugList().Entry(foo).Entry("world").FinishNonExhaustive()
	})

	fmtExpectEq(t, `[[true, 10/20, ..], "world", ..]`, Format("{Bar:?}", Arg("Bar", bar)))
	fmtExpectEq(t, `[
    [
        true,
        10/20,
        ..
    ],
    "world",
    ..
]`, Format("{Bar:#?}", Arg("Bar", bar)))
}

// fmtTestFoo is the struct of test_formatting_parameters_are_forwarded, whose
// Debug is derived.
type fmtTestFoo struct {
	Bar uint32 `rs:"bar"`
	Baz uint32 `rs:"baz"`
}

var _ = RegisterStruct[fmtTestFoo]("Foo")

// fmtTestBTreeMap and fmtTestBTreeSet iterate in key order, as BTreeMap and
// BTreeSet do; they describe themselves to the formatter the way the
// collections of go/rs/collections do.
type fmtTestBTreeMap struct {
	keys   []string
	values []int32
}

func (m *fmtTestBTreeMap) insert(key string, value int32) {
	at := 0
	for at < len(m.keys) && m.keys[at] < key {
		at++
	}
	m.keys = append(m.keys[:at], append([]string{key}, m.keys[at:]...)...)
	m.values = append(m.values[:at], append([]int32{value}, m.values[at:]...)...)
}

func (m *fmtTestBTreeMap) ReflectLen() int                  { return len(m.keys) }
func (m *fmtTestBTreeMap) ReflectKeyType() reflect.Type     { return reflect.TypeFor[string]() }
func (m *fmtTestBTreeMap) ReflectValueType() reflect.Type   { return reflect.TypeFor[int32]() }
func (m *fmtTestBTreeMap) ReflectInsert(key any, value any) { m.insert(key.(string), value.(int32)) }
func (m *fmtTestBTreeMap) ReflectSorted() bool              { return true }
func (m *fmtTestBTreeMap) ReflectEntries(yield func(key any, value any) bool) {
	for i := range m.keys {
		if !yield(m.keys[i], &m.values[i]) {
			return
		}
	}
}

type fmtTestBTreeSet struct {
	elements []int32
}

func (s *fmtTestBTreeSet) insert(element int32) {
	at := 0
	for at < len(s.elements) && s.elements[at] < element {
		at++
	}
	s.elements = append(s.elements[:at], append([]int32{element}, s.elements[at:]...)...)
}

func (s *fmtTestBTreeSet) ReflectLen() int               { return len(s.elements) }
func (s *fmtTestBTreeSet) ReflectElemType() reflect.Type { return reflect.TypeFor[int32]() }
func (s *fmtTestBTreeSet) ReflectPush(element any)       { s.insert(element.(int32)) }
func (s *fmtTestBTreeSet) ReflectIsSet() bool            { return true }
func (s *fmtTestBTreeSet) ReflectSorted() bool           { return true }
func (s *fmtTestBTreeSet) ReflectElements(yield func(element any) bool) {
	for i := range s.elements {
		if !yield(&s.elements[i]) {
			return
		}
	}
}

func TestFmtBuilders_test_formatting_parameters_are_forwarded(t *testing.T) {
	struct_ := fmtTestFoo{Bar: 1024, Baz: 7}
	tuple := T2(int32(1024), int32(7))
	list := [2]int32{1024, 7}
	map_ := &fmtTestBTreeMap{}
	map_.insert("bar", 1024)
	map_.insert("baz", 7)
	set := &fmtTestBTreeSet{}
	set.insert(1024)
	set.insert(7)

	fmtExpectEq(t, "Foo { bar: 1024, baz: 007 }", Format("{struct_:03?}", Arg("struct_", struct_)))
	fmtExpectEq(t, "(1024, 007)", Format("{tuple:03?}", Arg("tuple", tuple)))
	fmtExpectEq(t, "[1024, 007]", Format("{list:03?}", Arg("list", list)))
	fmtExpectEq(t, `{"bar": 1024, "baz": 007}`, Format("{map:03?}", Arg("map", map_)))
	fmtExpectEq(t, "{007, 1024}", Format("{set:03?}", Arg("set", set)))
	fmtExpectEq(t, strings.TrimSpace(`
Foo {
    bar: 1024,
    baz: 007,
}
    `), Format("{struct_:#03?}", Arg("struct_", struct_)))
	fmtExpectEq(t, strings.TrimSpace(`
(
    1024,
    007,
)
    `), Format("{tuple:#03?}", Arg("tuple", tuple)))
	fmtExpectEq(t, strings.TrimSpace(`
[
    1024,
    007,
]
    `), Format("{list:#03?}", Arg("list", list)))
	fmtExpectEq(t, strings.TrimSpace(`
{
    "bar": 1024,
    "baz": 007,
}
    `), Format("{map:#03?}", Arg("map", map_)))
	fmtExpectEq(t, strings.TrimSpace(`
{
    007,
    1024,
}
    `), Format("{set:#03?}", Arg("set", set)))
}
