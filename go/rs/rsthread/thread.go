// Port of std 1.99.1 src/thread/thread.rs, and the parker of
// src/sys/sync/thread_parking/futex.rs

package rsthread

import (
	"strconv"
	"strings"
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// threadInner is Thread's Inner: what every handle to one thread shares.
type threadInner struct {
	name   rs.Option[string]
	id     ThreadId
	parker parker
	// locals is the thread's thread-local storage (local.go). Only the
	// goroutine running on the thread uses it.
	locals localStorage
}

// Thread is std::thread::Thread: a handle to a thread, with its id, its name
// and its park token. Copies are handles to the same thread, as clones of a
// Rust Thread are.
type Thread struct {
	inner *threadInner
}

// checkThreadName is ThreadNameString::from: Rust keeps a thread's name as a
// C string, and a name with a NUL byte cannot be one. The panic is
// `CString::new(name).expect(..)`'s, showing NulError's Debug form. It is
// attributed to the caller of threadNew's caller.
func checkThreadName(name string) {
	position := strings.IndexByte(name, 0)
	if position < 0 {
		return
	}
	var text strings.Builder
	text.WriteString("thread name may not contain interior null bytes: NulError(")
	text.WriteString(strconv.Itoa(position))
	text.WriteString(", [")
	for i := 0; i < len(name); i++ {
		if i > 0 {
			text.WriteString(", ")
		}
		text.WriteString(strconv.Itoa(int(name[i])))
	}
	text.WriteString("])")
	rs.PanicAt(2, text.String())
}

// threadNew is Thread::new.
func threadNew(id ThreadId, name rs.Option[string]) Thread {
	if text, ok := name.Get(); ok {
		checkThreadName(text)
	}
	return Thread{inner: &threadInner{name: name, id: id, parker: newParker()}}
}

// park blocks until the thread's token is available, and takes it.
func (t Thread) park() { t.inner.parker.park() }

// parkTimeout is park, giving up after dur.
func (t Thread) parkTimeout(dur time.Duration) { t.inner.parker.parkTimeout(dur) }

// Unpark is Thread::unpark: it makes the thread's token available. A thread
// blocked in Park wakes; otherwise its next Park returns at once. Tokens do
// not add up: two unparks before a park are one.
func (t Thread) Unpark() { t.inner.parker.unpark() }

// Id is Thread::id.
func (t Thread) Id() ThreadId { return t.inner.id }

// Name is Thread::name: the name given to the thread when it was spawned,
// "main" for the main thread, and None for any other thread.
func (t Thread) Name() rs.Option[string] {
	if t.inner.name.IsSome() {
		return t.inner.name
	}
	if t.inner.id == mainThreadId {
		return rs.Some("main")
	}
	return rs.None[string]()
}

// hookName is the name the panic hook prints for the thread; "" stands for
// "<unnamed>".
func (t Thread) hookName() string { return t.Name().UnwrapOr("") }

// FmtDebug is the Debug impl: `Thread { id: ThreadId(2), name: Some("worker"), .. }`.
func (t Thread) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Thread").
		Field("id", t.Id()).
		Field("name", t.Name()).
		FinishNonExhaustive()
}

// parker is a thread's park token (sys::sync::thread_parking): at most one
// token is pending, Unpark makes it available and park takes it.
type parker struct {
	token chan struct{}
}

func newParker() parker { return parker{token: make(chan struct{}, 1)} }

func (p *parker) park() { <-p.token }

func (p *parker) parkTimeout(dur time.Duration) {
	timer := time.NewTimer(dur)
	defer timer.Stop()
	select {
	case <-p.token:
	case <-timer.C:
	}
}

func (p *parker) unpark() {
	select {
	case p.token <- struct{}{}:
	default:
	}
}
