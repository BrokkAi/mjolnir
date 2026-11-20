// Port of core 1.99.1 src/cell.rs (Cell and RefCell)

package rs

// Cell is Rust's Cell<T>: a value that is read and replaced as a whole
// through a shared reference. Go lets any field reached through a pointer be
// assigned, so a Cell is no more than its value and the methods Rust code
// calls on it. The zero Cell holds T's zero value.
//
// A Cell is not safe for concurrent use, as Cell is not Sync.
//
// Clone copies the value as an assignment does and never calls T's own Clone
// (Cell<T> is Clone only for T: Copy); Eq and Cmp compare the values.
type Cell[T any] struct {
	value T `rs:"value"`
}

var _ = RegisterStruct[Cell[int]]("Cell")

// CellNew is `Cell::new(value)`.
func CellNew[T any](value T) Cell[T] {
	return Cell[T]{value: value}
}

// Get is Cell::get: a copy of the value.
func (c *Cell[T]) Get() T { return c.value }

// Set is Cell::set.
func (c *Cell[T]) Set(value T) { c.value = value }

// Replace is Cell::replace: it stores value and returns the old one.
func (c *Cell[T]) Replace(value T) T {
	old := c.value
	c.value = value
	return old
}

// Take is Cell::take: it returns the value and leaves T's Default.
func (c *Cell[T]) Take() T {
	return Take(&c.value)
}

// Swap is Cell::swap.
func (c *Cell[T]) Swap(other *Cell[T]) {
	c.value, other.value = other.value, c.value
}

// Update is Cell::update: it stores f of the value.
func (c *Cell[T]) Update(f func(T) T) {
	c.value = f(c.value)
}

// IntoInner is Cell::into_inner.
func (c *Cell[T]) IntoInner() T { return c.value }

// GetMut is Cell::get_mut: a pointer to the value.
func (c *Cell[T]) GetMut() *T { return &c.value }

// AsPtr is Cell::as_ptr.
func (c *Cell[T]) AsPtr() *T { return &c.value }

// RefCell is Rust's RefCell<T>: a value that is borrowed, for reading or for
// writing, through a shared reference. Borrow and BorrowMut return a pointer
// to the value.
//
// It does not track borrows. Rust panics ("already borrowed", "already
// mutably borrowed") when a value is borrowed for writing while another
// borrow is alive; that is a bug in the Rust code, and code that has it
// behaves differently here: the second borrow succeeds. There is no guard to
// drop, and no try_borrow, which could only ever answer yes.
//
// A RefCell is not safe for concurrent use, as RefCell is not Sync. Clone,
// Eq and Cmp are those of the value.
type RefCell[T any] struct {
	value T `rs:"value"`
}

var _ = RegisterStruct[RefCell[int]]("RefCell")

// RefCellNew is `RefCell::new(value)`.
func RefCellNew[T any](value T) RefCell[T] {
	return RefCell[T]{value: value}
}

// Borrow is RefCell::borrow: the value, to read.
func (c *RefCell[T]) Borrow() *T { return &c.value }

// BorrowMut is RefCell::borrow_mut: the value, to change.
func (c *RefCell[T]) BorrowMut() *T { return &c.value }

// Replace is RefCell::replace: it stores value and returns the old one.
func (c *RefCell[T]) Replace(value T) T {
	old := c.value
	c.value = value
	return old
}

// ReplaceWith is RefCell::replace_with: it stores f of the value and returns
// the old one.
func (c *RefCell[T]) ReplaceWith(f func(*T) T) T {
	replacement := f(&c.value)
	old := c.value
	c.value = replacement
	return old
}

// Swap is RefCell::swap.
func (c *RefCell[T]) Swap(other *RefCell[T]) {
	c.value, other.value = other.value, c.value
}

// Take is RefCell::take: it returns the value and leaves T's Default.
func (c *RefCell[T]) Take() T {
	return Take(&c.value)
}

// IntoInner is RefCell::into_inner.
func (c *RefCell[T]) IntoInner() T { return c.value }

// GetMut is RefCell::get_mut: a pointer to the value.
func (c *RefCell[T]) GetMut() *T { return &c.value }

// AsPtr is RefCell::as_ptr.
func (c *RefCell[T]) AsPtr() *T { return &c.value }
