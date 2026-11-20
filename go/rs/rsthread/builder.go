// Port of std 1.99.1 src/thread/builder.rs

package rsthread

import (
	"github.com/BrokkAi/mjolnir/go/rs"
)

// Builder is std::thread::Builder: the configuration of a thread to spawn,
// its name and stack size. A goroutine's stack grows as it needs to, so the
// stack size is kept only to be shown.
type Builder struct {
	name      rs.Option[string]
	stackSize rs.Option[int]
	noHooks   bool
}

// BuilderNew is Builder::new.
func BuilderNew() Builder {
	return Builder{name: rs.None[string](), stackSize: rs.None[int](), noHooks: false}
}

// Name is Builder::name. A name with a NUL byte makes the spawn panic, as in
// Rust.
func (b Builder) Name(name string) Builder {
	b.name = rs.Some(name)
	return b
}

// StackSize is Builder::stack_size.
func (b Builder) StackSize(size int) Builder {
	b.stackSize = rs.Some(size)
	return b
}

// BuilderSpawn is Builder::spawn: it starts f on a new thread with this
// configuration. Rust's error is the operating system refusing to create a
// thread, which cannot happen to a goroutine: the error is always nil.
func BuilderSpawn[T any](b Builder, f func() T) (*JoinHandle[T], error) {
	return &JoinHandle[T]{inner: spawnUnchecked(b.name, nil, f)}, nil
}

// FmtDebug is the derived Debug impl:
// `Builder { name: Some("worker"), stack_size: Some(4096), no_hooks: false }`.
func (b Builder) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Builder").
		Field("name", b.name).
		Field("stack_size", b.stackSize).
		Field("no_hooks", b.noHooks).
		Finish()
}
