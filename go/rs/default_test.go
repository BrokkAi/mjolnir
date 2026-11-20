package rs

import (
	"strings"
	"sync"
	"testing"
	"time"
)

// probeMode is a data-free enum whose `#[default]` is not its first variant.
type probeMode uint8

const (
	probeMode_Off probeMode = iota
	probeMode_Push
	probeMode_Always
)

var _ = RegisterEnum[probeMode]("Mode",
	Const(probeMode_Off, "Off"),
	Const(probeMode_Push, "Push"),
	Const(probeMode_Always, "Always"),
)

var _ = RegisterDefault(func() probeMode { return probeMode_Push })

// probeLayout is an enum held as an interface, with `#[default]` on a unit
// variant.
type probeLayout interface{ isProbeLayout() }

type probeLayout_Stacked struct{}

type probeLayout_Split struct {
	Ratio float64 `rs:"ratio"`
}

func (*probeLayout_Stacked) isProbeLayout() {}
func (*probeLayout_Split) isProbeLayout()   {}

var _ = RegisterEnum[probeLayout]("Layout",
	Variant[probeLayout_Stacked]("Stacked", ShapeUnit),
	Variant[probeLayout_Split]("Split", ShapeStruct),
)

var _ = RegisterDefault(func() probeLayout { return &probeLayout_Stacked{} })

// probeLimit has a hand-written Default.
type probeLimit struct {
	Max   int      `rs:"max"`
	Hosts []string `rs:"hosts"`
}

func probeLimitDefault() probeLimit {
	return probeLimit{Max: 10, Hosts: []string{"localhost"}}
}

var _ = RegisterStruct[probeLimit]("Limit")
var _ = RegisterDefault(probeLimitDefault)

// probeSnapshot is generic, so it says what its Default is with a method.
type probeSnapshot[T any] struct {
	inner   *T
	version int
}

func (s *probeSnapshot[T]) ReflectSetDefault() {
	*s = probeSnapshot[T]{inner: new(T), version: 1}
}

// probeSettingsLeaf is a small ported struct.
type probeSettingsLeaf struct {
	Mode probeMode `rs:"mode"`
	Name string    `rs:"name"`
}

var _ = RegisterStruct[probeSettingsLeaf]("SettingsLeaf")

// probeSettings is the port of a struct with `#[derive(Default)]`.
type probeSettings struct {
	Mode     probeMode                       `rs:"mode"`
	Layout   probeLayout                     `rs:"layout"`
	Limit    probeLimit                      `rs:"limit"`
	Boxed    *probeSettingsLeaf              `rs:"boxed"`
	Shared   *probeSettingsLeaf              `rs:"shared,arc"`
	Maybe    Option[*probeSettingsLeaf]      `rs:"maybe"`
	MaybeNot Option[probeMode]               `rs:"maybe_not"`
	Names    []string                        `rs:"names"`
	Lookup   map[string]int                  `rs:"lookup"`
	Tree     probeTreeMap[string, probeMode] `rs:"tree"`
	Pair     Tuple2[probeMode, *probeLimit]  `rs:"pair"`
	Modes    [2]probeMode                    `rs:"modes"`
	Scroll   Cell[probeMode]                 `rs:"scroll"`
	Areas    RefCell[[]probeMode]            `rs:"areas"`
	Snapshot probeSnapshot[string]           `rs:"snapshot"`
	Number   *int                            `rs:"number"`
	Extra    any                             `rs:"extra"`
	Failure  error                           `rs:"failure"`
	Callback func()                          `rs:"callback"`
	Count    uint64                          `rs:"count"`
	Title    string                          `rs:"title"`
	when     time.Time
	internal probeInternals
}

var _ = RegisterStruct[probeSettings]("Settings")

// probeInternals is not the port of a Rust struct: no registration, no tags.
// It stands for the private state of a runtime type.
type probeInternals struct {
	mode  probeMode
	node  *probeInternals
	cache map[string]int
	lock  sync.Mutex
	limit probeLimit
}

func TestDefaultOfTypesWhoseDefaultIsTheZeroValue(t *testing.T) {
	type plain struct {
		A int
		B string
		C []byte
		D Option[string]
		E [3]float64
		F Tuple2[bool, uint8]
		G probeColor
		H probeTreeMap[string, int]
	}
	for range 2 {
		got := Default[plain]()
		if got.A != 0 || got.B != "" || got.C != nil || got.D.IsSome() || got.E != [3]float64{} || got.F != T2(false, uint8(0)) ||
			got.G != probeColor_Red || got.H.ReflectLen() != 0 {
			t.Fatalf("Default = %+v", got)
		}
	}
	if Default[int]() != 0 || Default[string]() != "" || Default[bool]() || Default[[]int]() != nil ||
		Default[Option[int]]().IsSome() || Default[probeColor]() != probeColor_Red || Default[Unit]() != (Unit{}) {
		t.Fatal("defaults of plain types")
	}
	if Default[any]() != nil || Default[error]() != nil {
		t.Fatal("an interface that is not an enum has no default but nil")
	}
	when := Default[time.Time]()
	if !when.IsZero() {
		t.Fatal("a standard library value keeps its zero value")
	}
}

func TestDefaultUsesRegisteredDefaults(t *testing.T) {
	if got := Default[probeMode](); got != probeMode_Push {
		t.Fatalf("Default of an enum with #[default] on its second variant = %d", got)
	}
	if _, ok := Default[probeLayout]().(*probeLayout_Stacked); !ok {
		t.Fatal("Default of an enum held as an interface")
	}
	limit := Default[probeLimit]()
	if limit.Max != 10 || len(limit.Hosts) != 1 {
		t.Fatalf("a hand-written Default was not used: %+v", limit)
	}
	limit.Hosts[0] = "changed"
	if Default[probeLimit]().Hosts[0] != "localhost" {
		t.Fatal("two defaults share a slice")
	}

	if Default[Option[probeMode]]().IsSome() {
		t.Fatal("the Default of an Option is None, whatever it would hold")
	}
	if got := Default[[3]probeMode](); got != [3]probeMode{probeMode_Push, probeMode_Push, probeMode_Push} {
		t.Fatalf("Default of an array = %v", got)
	}
	if got := Default[Tuple3[probeMode, int, probeLimit]](); got.F0 != probeMode_Push || got.F1 != 0 || got.F2.Max != 10 {
		t.Fatalf("Default of a tuple = %+v", got)
	}
	if got := Default[[]probeMode](); got != nil {
		t.Fatalf("Default of a Vec = %v", got)
	}
	if got := Default[Reverse[probeMode]](); got.F0 != probeMode_Push {
		t.Fatalf("Default of Reverse = %+v", got)
	}
}

func TestDefaultOfAPortedStruct(t *testing.T) {
	got := Default[probeSettings]()

	if got.Mode != probeMode_Push {
		t.Errorf("mode = %d", got.Mode)
	}
	if _, ok := got.Layout.(*probeLayout_Stacked); !ok {
		t.Errorf("layout = %#v", got.Layout)
	}
	if got.Limit.Max != 10 {
		t.Errorf("limit = %+v", got.Limit)
	}
	// Box<T> and Arc<T> are never nil: Box::new(T::default()).
	if got.Boxed == nil || got.Boxed.Mode != probeMode_Push || got.Boxed.Name != "" {
		t.Errorf("boxed = %+v", got.Boxed)
	}
	if got.Shared == nil || got.Shared.Mode != probeMode_Push || got.Shared == got.Boxed {
		t.Errorf("shared = %+v", got.Shared)
	}
	if got.Number == nil || *got.Number != 0 {
		t.Errorf("number = %v", got.Number)
	}
	if got.Maybe.IsSome() || got.MaybeNot.IsSome() {
		t.Error("options are None")
	}
	if got.Names != nil {
		t.Errorf("names = %v", got.Names)
	}
	if got.Lookup == nil || len(got.Lookup) != 0 {
		t.Errorf("lookup = %v: a HashMap is empty, never nil", got.Lookup)
	}
	got.Lookup["writable"] = 1
	if got.Tree.ReflectLen() != 0 {
		t.Error("a collection is empty")
	}
	if got.Pair.F0 != probeMode_Push || got.Pair.F1 == nil || got.Pair.F1.Max != 10 {
		t.Errorf("pair = %+v", got.Pair)
	}
	if got.Modes != [2]probeMode{probeMode_Push, probeMode_Push} {
		t.Errorf("modes = %v", got.Modes)
	}
	if got.Scroll.Get() != probeMode_Push {
		t.Errorf("scroll = %v", got.Scroll.Get())
	}
	if *got.Areas.Borrow() != nil {
		t.Errorf("areas = %v", *got.Areas.Borrow())
	}
	if got.Snapshot.version != 1 || got.Snapshot.inner == nil {
		t.Errorf("snapshot = %+v: ReflectSetDefault was not called", got.Snapshot)
	}
	if got.Extra != nil || got.Failure != nil || got.Callback != nil || got.Count != 0 || got.Title != "" || !got.when.IsZero() {
		t.Error("the remaining fields keep their zero value")
	}

	// The private state of a type that is not a ported struct keeps nil
	// pointers and maps, but a value with a registered Default still gets it.
	if got.internal.node != nil || got.internal.cache != nil {
		t.Errorf("internal: node %v, cache %v: pointers and maps of a runtime type must stay nil", got.internal.node, got.internal.cache)
	}
	if got.internal.mode != probeMode_Push || got.internal.limit.Max != 10 {
		t.Errorf("internal: mode %d, limit %+v: nested registered defaults must apply", got.internal.mode, got.internal.limit)
	}

	other := Default[probeSettings]()
	if other.Boxed == got.Boxed || other.Pair.F1 == got.Pair.F1 {
		t.Error("two defaults share an allocation")
	}
}

func TestDefaultOfPointersMapsAndHooks(t *testing.T) {
	boxed := Default[*probeSettingsLeaf]()
	if boxed == nil || boxed.Mode != probeMode_Push {
		t.Fatalf("Default of a Box = %+v", boxed)
	}
	if number := Default[*int](); number == nil || *number != 0 {
		t.Fatal("Default of a Box<i64>")
	}
	lookup := Default[map[string]int]()
	if lookup == nil {
		t.Fatal("Default of a map is an empty map")
	}
	lookup["k"] = 1
	snapshot := Default[probeSnapshot[int]]()
	if snapshot.version != 1 || snapshot.inner == nil {
		t.Fatalf("ReflectSetDefault was not used: %+v", snapshot)
	}
	if nested := Default[Tuple2[probeSnapshot[int], int]](); nested.F0.version != 1 {
		t.Fatalf("ReflectSetDefault was not used for a nested value: %+v", nested)
	}
	if nested := Default[*probeSnapshot[bool]](); nested == nil || nested.version != 1 {
		t.Fatalf("ReflectSetDefault was not used behind a Box: %+v", nested)
	}
	cell := Default[Cell[probeMode]]()
	if cell.Get() != probeMode_Push {
		t.Fatal("Default of a Cell is a Cell of the Default")
	}
}

// probeUnset is an enum nobody registered a Default for.
type probeUnset interface{ isProbeUnset() }

type probeUnset_Only struct{}

func (*probeUnset_Only) isProbeUnset() {}

type probeNeedsUnset struct {
	Value probeUnset `rs:"value"`
}

type probeLoop struct {
	Next *probeLoop `rs:"next"`
}

func expectPanic(t *testing.T, want string, f func()) {
	t.Helper()
	defer func() {
		t.Helper()
		recovered := recover()
		if recovered == nil {
			t.Fatalf("no panic; wanted one containing %q", want)
		}
		message := PanicInfoOf(recovered).Message
		if !strings.Contains(message, want) {
			t.Fatalf("panic %q does not contain %q", message, want)
		}
	}()
	f()
}

func TestDefaultPanicsWhereRustWouldNotCompile(t *testing.T) {
	expectPanic(t, "has no Default registered", func() { Default[probeUnset]() })
	expectPanic(t, "probeUnset has no Default registered", func() { Default[probeNeedsUnset]() })
	expectPanic(t, "holds itself through a pointer", func() { Default[probeLoop]() })
	expectPanic(t, "already registered", func() { RegisterDefault(func() probeMode { return probeMode_Always }) })
	if Default[probeMode]() != probeMode_Push {
		t.Fatal("a refused registration must not replace the first")
	}
}

type probeLate uint8

type probeHoldsLate struct {
	Late probeLate `rs:"late"`
}

var probeLateRegistered bool

func TestDefaultSeesARegistrationMadeAfterFirstUse(t *testing.T) {
	if !probeLateRegistered {
		if got := Default[probeHoldsLate](); got.Late != 0 {
			t.Fatalf("before registration: %+v", got)
		}
		RegisterDefault(func() probeLate { return 3 })
		probeLateRegistered = true
	}
	if got := Default[probeHoldsLate](); got.Late != 3 {
		t.Fatalf("after registration: %+v", got)
	}
	if got := Default[probeLate](); got != 3 {
		t.Fatalf("after registration: %d", got)
	}
}
