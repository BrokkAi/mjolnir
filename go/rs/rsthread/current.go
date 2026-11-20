// Port of std 1.99.1 src/thread/current.rs and src/thread/main_thread.rs

package rsthread

import (
	"runtime"
	"sync"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Which thread a goroutine is on.
//
// Rust keeps the current thread's handle in a thread-local. A goroutine has
// no thread-local storage, so this package keeps a table from goroutine id
// to thread:
//
//   - a goroutine started by Spawn or a scope is entered when it starts and
//     removed when it ends;
//   - the main goroutine (goroutine 1, which runs rs.Main) is the main
//     thread, named "main", with ThreadId 1;
//   - any other goroutine is entered the first time it asks (Current, Park,
//     a LocalKey), as an unnamed thread of its own, and stays in the table.
//     Rust frees a thread's handle when the thread exits; nothing tells
//     this package when such a goroutine ends.
//
// The tokio contract (.agents/docs/go-port/tokio-contract.md, "The runtime
// context of a goroutine") makes go/crates/tokio's goroutine table the one
// owner of this fact once that package builds: every task of a
// current-thread runtime runs on the thread that drives it, so tasks that are
// different goroutines are one thread, which only tokio knows. The table here
// answers for goroutines tokio did not start in the same way tokio's does,
// and these three functions are the only places that read or change it, so
// that moving to tokio.CurrentThreadLocals and tokio.ForgetCurrentGoroutine is
// a change of this file alone.
var goroutineThreads sync.Map // goroutine id (uint64) -> *threadInner

// mainThread is the main goroutine's thread.
var mainThread = &threadInner{name: rs.None[string](), id: mainThreadId, parker: newParker()}

// goid returns the calling goroutine's id, from the header of its stack
// trace, "goroutine 123 [running]:".
func goid() uint64 {
	var buf [64]byte
	n := runtime.Stack(buf[:], false)
	const prefix = len("goroutine ")
	var id uint64
	for i := prefix; i < n; i++ {
		c := buf[i]
		if c < '0' || c > '9' {
			break
		}
		id = id*10 + uint64(c-'0')
	}
	return id
}

// currentInner is current(): the calling goroutine's thread, entered in the
// table if it was not there.
func currentInner() *threadInner {
	gid := goid()
	if found, ok := goroutineThreads.Load(gid); ok {
		return found.(*threadInner)
	}
	inner := mainThread
	if gid != 1 {
		inner = &threadInner{name: rs.None[string](), id: threadIdNew(), parker: newParker()}
	}
	goroutineThreads.Store(gid, inner)
	return inner
}

// setCurrent is set_current: the calling goroutine, which Spawn started, is
// thread from now on.
func setCurrent(thread *threadInner) {
	goroutineThreads.Store(goid(), thread)
}

// dropCurrent is drop_current: the calling goroutine, which Spawn started,
// is ending.
func dropCurrent() {
	goroutineThreads.Delete(goid())
}

// Current is std::thread::current: a handle to the calling thread.
func Current() Thread { return Thread{inner: currentInner()} }

// currentOrUnnamed is current_or_unnamed, which in the port is Current.
func currentOrUnnamed() Thread { return Current() }
