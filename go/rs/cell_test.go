package rs

import "testing"

// Ported from coretests cell.rs. The tests there that borrow a RefCell twice
// and expect a panic or an error (double_imm_borrow, no_mut_then_imm_borrow,
// no_imm_then_borrow_mut, no_double_borrow_mut, imm_release_borrow_mut,
// mut_release_borrow_mut, double_borrow_single_release_no_borrow_mut,
// discard_doesnt_unborrow, ref_clone_updates_flag, the ref_map tests,
// refcell_swap_borrows, refcell_replace_borrows, refcell_borrow,
// refcell_borrow_mut) have no port: rs.RefCell does not track borrows.
// UnsafeCell and SyncUnsafeCell are not provided.

func TestCell_smoketest_cell(t *testing.T) {
	x := CellNew(10)
	if !Eq(x, CellNew(10)) || x.Get() != 10 {
		t.Fatal("Cell::new(10)")
	}
	x.Set(20)
	if !Eq(x, CellNew(20)) || x.Get() != 20 {
		t.Fatal("after set(20)")
	}

	y := CellNew(T2(30, 40))
	if !Eq(y, CellNew(T2(30, 40))) || y.Get() != T2(30, 40) {
		t.Fatal("a Cell of a tuple")
	}
}

func TestCell_cell_update(t *testing.T) {
	x := CellNew(10)

	x.Update(func(x int) int { return x + 5 })
	if x.Get() != 15 {
		t.Fatalf("after + 5: %d", x.Get())
	}

	x.Update(func(x int) int { return x / 3 })
	if x.Get() != 5 {
		t.Fatalf("after / 3: %d", x.Get())
	}
}

func TestCell_cell_has_sensible_show(t *testing.T) {
	if !formatterReady() {
		t.Skip("the formatter is not ported yet")
	}
	x := CellNew("foo bar")
	if got := DebugString(x); got != `Cell { value: "foo bar" }` {
		t.Fatalf("{x:?} = %s", got)
	}
	x.Set("baz qux")
	if got := DebugString(x); got != `Cell { value: "baz qux" }` {
		t.Fatalf("{x:?} = %s", got)
	}
	refcell := RefCellNew("foo")
	if got := DebugString(refcell); got != `RefCell { value: "foo" }` {
		t.Fatalf("{refcell:?} = %s", got)
	}
}

func TestCell_as_ptr(t *testing.T) {
	c1 := CellNew(0)
	c1.Set(1)
	if *c1.AsPtr() != 1 {
		t.Fatal("1 == *c1.as_ptr()")
	}

	c2 := CellNew(0)
	*c2.AsPtr() = 1
	if c2.Get() != 1 {
		t.Fatal("1 == c2.get()")
	}

	r1 := RefCellNew(0)
	*r1.BorrowMut() = 1
	if *r1.AsPtr() != 1 {
		t.Fatal("1 == *r1.as_ptr()")
	}

	r2 := RefCellNew(0)
	*r2.AsPtr() = 1
	if *r2.Borrow() != 1 {
		t.Fatal("1 == *r2.borrow()")
	}
}

func TestCell_cell_default(t *testing.T) {
	cell := Default[Cell[uint32]]()
	if cell.Get() != 0 {
		t.Fatal("0 == cell.get()")
	}
	var zero Cell[uint32]
	if zero.Get() != 0 {
		t.Fatal("the zero Cell holds zero")
	}
}

func TestCell_cell_set(t *testing.T) {
	cell := CellNew(10)
	cell.Set(20)
	if cell.Get() != 20 {
		t.Fatal("20 == cell.get()")
	}

	text := CellNew("Hello")
	text.Set("World")
	if text.IntoInner() != "World" {
		t.Fatal(`"World" == cell.into_inner()`)
	}
}

func TestCell_cell_replace(t *testing.T) {
	cell := CellNew(10)
	if cell.Replace(20) != 10 || cell.Get() != 20 {
		t.Fatal("replace(20)")
	}

	text := CellNew("Hello")
	if text.Replace("World") != "Hello" || text.IntoInner() != "World" {
		t.Fatal(`replace("World")`)
	}
}

func TestCell_cell_into_inner(t *testing.T) {
	cell := CellNew(10)
	if cell.IntoInner() != 10 {
		t.Fatal("10 == cell.into_inner()")
	}

	text := CellNew("Hello world")
	if text.IntoInner() != "Hello world" {
		t.Fatal(`"Hello world" == cell.into_inner()`)
	}
}

func TestCell_cell_exterior(t *testing.T) {
	type point struct{ x, y, z int }

	f := func(p *Cell[point]) {
		if p.Get().z != 12 {
			t.Fatal("p.get().z == 12")
		}
		p.Set(point{x: 10, y: 11, z: 13})
		if p.Get().z != 13 {
			t.Fatal("p.get().z == 13")
		}
	}

	a := point{x: 10, y: 11, z: 12}
	b := CellNew(a)
	if b.Get().z != 12 {
		t.Fatal("b.get().z == 12")
	}
	f(&b)
	if a.z != 12 || b.Get().z != 13 {
		t.Fatal("the Cell holds its own copy")
	}
}

func TestCell_swap_take_and_get_mut(t *testing.T) {
	// Port of the examples of Cell::swap, Cell::take and Cell::get_mut.
	c1 := CellNew(int32(5))
	c2 := CellNew(int32(10))
	c1.Swap(&c2)
	if c1.Get() != 10 || c2.Get() != 5 {
		t.Fatal("swap")
	}
	c1.Swap(&c1)
	if c1.Get() != 10 {
		t.Fatal("a Cell swapped with itself keeps its value")
	}

	c := CellNew(5)
	five := c.Take()
	if five != 5 || c.IntoInner() != 0 {
		t.Fatal("take leaves the Default")
	}
	mode := CellNew(probeMode_Always)
	if mode.Take() != probeMode_Always || mode.Get() != probeMode_Push {
		t.Fatal("take leaves the registered Default")
	}

	counter := CellNew(5)
	*counter.GetMut() += 1
	if counter.Get() != 6 {
		t.Fatal("get_mut")
	}
}

func TestCell_refcell_default(t *testing.T) {
	cell := Default[RefCell[uint64]]()
	if *cell.Borrow() != 0 {
		t.Fatal("0 == *cell.borrow()")
	}
}

func TestCell_refcell_unsized(t *testing.T) {
	cell := RefCellNew([3]int32{1, 2, 3})
	{
		b := cell.BorrowMut()
		b[0] = 4
		b[2] = 5
	}
	if *cell.Borrow() != [3]int32{4, 2, 5} {
		t.Fatalf("%v", *cell.Borrow())
	}
}

func TestCell_refcell_format(t *testing.T) {
	if !formatterReady() {
		t.Skip("the formatter is not ported yet")
	}
	name := RefCellNew("rust")
	what := RefCellNew("rocks")
	msg := Format("{name} {}", *what.Borrow(), Arg("name", *name.Borrow()))
	if msg != "rust rocks" {
		t.Fatalf("%q", msg)
	}
}

func TestCell_refcell_replace(t *testing.T) {
	a := RefCellNew(0)
	if a.Replace(10) != 0 {
		t.Fatal("a.replace(10) == 0")
	}
	if a.IntoInner() != 10 {
		t.Fatal("a == 10")
	}

	// Port of the example of RefCell::replace_with.
	cell := RefCellNew(5)
	oldValue := cell.ReplaceWith(func(old *int) int { return *old + 1 })
	if oldValue != 5 || !Eq(cell, RefCellNew(6)) {
		t.Fatal("replace_with")
	}
}

func TestCell_refcell_swap(t *testing.T) {
	a, b := RefCellNew(31), RefCellNew(41)
	a.Swap(&b)
	if a.IntoInner() != 41 || b.IntoInner() != 31 {
		t.Fatal("swap")
	}
}

func TestCell_refcell_take_and_get_mut(t *testing.T) {
	// Port of the examples of RefCell::take and RefCell::get_mut.
	c := RefCellNew(5)
	five := c.Take()
	if five != 5 || c.IntoInner() != 0 {
		t.Fatal("take leaves the Default")
	}

	list := RefCellNew([]int{1})
	*list.GetMut() = append(*list.GetMut(), 2)
	if !Eq(*list.Borrow(), []int{1, 2}) {
		t.Fatal("get_mut")
	}
	taken := list.Take()
	if len(taken) != 2 || *list.Borrow() != nil {
		t.Fatal("take of a Vec leaves an empty one")
	}
}

func TestCellOrder(t *testing.T) {
	if !(Cmp(CellNew(1), CellNew(2)) < 0) || Cmp(RefCellNew("b"), RefCellNew("a")) <= 0 || Cmp(CellNew(3), CellNew(3)) != 0 {
		t.Fatal("cells order by their values")
	}
}
