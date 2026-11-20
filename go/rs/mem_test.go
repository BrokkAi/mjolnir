package rs

import "testing"

func TestMem_test_swap(t *testing.T) {
	// Port of coretests mem.rs test_swap.
	x := 31337
	y := 42
	Swap(&x, &y)
	if x != 42 || y != 31337 {
		t.Fatalf("x = %d, y = %d", x, y)
	}
	a, b := []string{"a"}, []string{"b", "c"}
	Swap(&a, &b)
	if len(a) != 2 || len(b) != 1 {
		t.Fatal("swap of slices")
	}
}

func TestMem_test_replace(t *testing.T) {
	// Port of coretests mem.rs test_replace.
	x := Some("test")
	y := Replace(&x, None[string]())
	if !x.IsNone() || !y.IsSome() {
		t.Fatal("replace")
	}
	// Port of the example of mem::replace.
	v := []int32{1, 2}
	oldV := Replace(&v, []int32{3, 4, 5})
	if !Eq(oldV, []int32{1, 2}) || !Eq(v, []int32{3, 4, 5}) {
		t.Fatal("replace of a Vec")
	}
}

func TestMem_test_take(t *testing.T) {
	// Port of the example of mem::take.
	v := []int32{1, 2}
	oldV := Take(&v)
	if !Eq(oldV, []int32{1, 2}) || len(v) != 0 {
		t.Fatal("take of a Vec")
	}

	text := "taken"
	if Take(&text) != "taken" || text != "" {
		t.Fatal("take of a String")
	}
	option := Some(3)
	if Take(&option) != Some(3) || option.IsSome() {
		t.Fatal("take of an Option leaves None")
	}

	// The Default that is left is the registered one.
	mode := probeMode_Always
	if Take(&mode) != probeMode_Always || mode != probeMode_Push {
		t.Fatalf("take left %d", mode)
	}
	settings := probeSettingsLeaf{Mode: probeMode_Off, Name: "n"}
	taken := Take(&settings)
	if taken.Name != "n" || taken.Mode != probeMode_Off || settings.Name != "" || settings.Mode != probeMode_Push {
		t.Fatalf("take of a struct left %+v", settings)
	}
	var layout probeLayout = &probeLayout_Split{Ratio: 0.5}
	if _, ok := Take(&layout).(*probeLayout_Split); !ok {
		t.Fatal("take returns the value")
	}
	if _, ok := layout.(*probeLayout_Stacked); !ok {
		t.Fatal("take of an enum leaves its #[default] variant")
	}
}

// probeGuard is a type that must be dropped and knows how to be forgotten.
type probeGuard struct {
	forgotten *bool
}

func (g probeGuard) Forget() { *g.forgotten = true }

type probePointerGuard struct {
	forgotten bool
}

func (g *probePointerGuard) Forget() { g.forgotten = true }

func TestMemForget(t *testing.T) {
	Forget(5)
	Forget("text")
	Forget([]int{1})

	forgotten := false
	Forget(probeGuard{forgotten: &forgotten})
	if !forgotten {
		t.Fatal("a Forgetter must be told")
	}
	guard := &probePointerGuard{}
	Forget(guard)
	if !guard.forgotten {
		t.Fatal("a Forgetter held by pointer must be told")
	}
}

// probeFooABC is Foo of the example of mem::discriminant.
type probeFooABC interface{ isProbeFooABC() }

type probeFooABC_A struct{ F0 string }
type probeFooABC_B struct{ F0 int32 }
type probeFooABC_C struct{ F0 int32 }

func (*probeFooABC_A) isProbeFooABC() {}
func (*probeFooABC_B) isProbeFooABC() {}
func (*probeFooABC_C) isProbeFooABC() {}

var _ = RegisterEnum[probeFooABC]("Foo",
	Variant[probeFooABC_A]("A", ShapeTuple),
	Variant[probeFooABC_B]("B", ShapeTuple),
	Variant[probeFooABC_C]("C", ShapeTuple),
)

func TestMemDiscriminant(t *testing.T) {
	// Port of the example of mem::discriminant.
	of := func(value probeFooABC) MemDiscriminant[probeFooABC] { return Discriminant(value) }
	if of(&probeFooABC_A{"bar"}) != of(&probeFooABC_A{"baz"}) {
		t.Fatal(`discriminant(&Foo::A("bar")) == discriminant(&Foo::A("baz"))`)
	}
	if of(&probeFooABC_B{1}) != of(&probeFooABC_B{2}) {
		t.Fatal("discriminant(&Foo::B(1)) == discriminant(&Foo::B(2))")
	}
	if of(&probeFooABC_B{3}) == of(&probeFooABC_C{3}) {
		t.Fatal("discriminant(&Foo::B(3)) != discriminant(&Foo::C(3))")
	}
	if !Eq(of(&probeFooABC_B{1}), of(&probeFooABC_B{2})) || Eq(of(&probeFooABC_A{}), of(&probeFooABC_B{})) {
		t.Fatal("Discriminant is PartialEq")
	}
	if of(&probeFooABC_C{}).index != 2 || of(&probeFooABC_A{}).index != 0 {
		t.Fatal("the discriminant of a registered variant is its position")
	}

	// It works as a field and as a map key, as the dashboard uses it.
	type pressed struct {
		mode MemDiscriminant[probeFooABC]
		row  int
	}
	if !Eq(pressed{of(&probeFooABC_B{1}), 2}, pressed{of(&probeFooABC_B{9}), 2}) ||
		Eq(pressed{of(&probeFooABC_B{1}), 2}, pressed{of(&probeFooABC_C{1}), 2}) {
		t.Fatal("a struct holding a Discriminant compares by it")
	}
	seen := map[MemDiscriminant[probeFooABC]]int{}
	seen[of(&probeFooABC_A{"x"})]++
	seen[of(&probeFooABC_A{"y"})]++
	seen[of(&probeFooABC_C{1})]++
	if len(seen) != 2 || seen[of(&probeFooABC_A{})] != 2 {
		t.Fatalf("seen = %v", seen)
	}

	// Data-free enums, options and results.
	if Discriminant(probeColor_Blue) != Discriminant(probeColor_Blue) || Discriminant(probeColor_Blue) == Discriminant(probeColor_Red) {
		t.Fatal("a data-free enum")
	}
	if Discriminant(probeColor_Blue).index != 2 {
		t.Fatal("the discriminant of a data-free enum is its value")
	}
	if Discriminant(Some(1)) != Discriminant(Some(2)) || Discriminant(Some(1)) == Discriminant(None[int]()) {
		t.Fatal("an Option")
	}
	if Discriminant(Ok(1)) != Discriminant(Ok(2)) || Discriminant(Ok(1)) == Discriminant(Err[int](&probeFault_Refused{})) {
		t.Fatal("a Result")
	}

	// An enum nobody registered still tells its variants apart.
	if Discriminant[probeToken](&probeToken_Word{}) == Discriminant[probeToken](&probeToken_Pair{}) ||
		Discriminant[probeToken](&probeToken_Word{text: []byte("a")}) != Discriminant[probeToken](&probeToken_Word{text: []byte("b")}) {
		t.Fatal("an unregistered enum")
	}

	if formatterReady() {
		if got := DebugString(of(&probeFooABC_B{1})); got != "Discriminant(1)" {
			t.Fatalf("{:?} = %s", got)
		}
	}
}
