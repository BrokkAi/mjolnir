package tracing

import (
	"context"
	"runtime"
	"sync"
	"sync/atomic"
)

// The crates keep three things in thread-local storage: the default
// dispatcher of each thread (tracing-core's CURRENT_STATE), the stack of
// spans each thread has entered (tracing-subscriber's Registry), and the
// levels of the spans an EnvFilter has seen entered (its `scope`). What
// "thread" means is decided here, behind LocalStore, so that the async
// runtime can give it the meaning Rust code running on tokio sees.
//
// Without a runtime, every goroutine is a thread of its own: a default set
// with DispatcherSetDefault, and a span entered with Span.Enter, are seen by
// the goroutine that did it and by no other, as a Rust thread's are.
//
// The runtime replaces that with RuntimeHooksSet. Under go/crates/tokio a
// thread is one of its logical threads, so that every task of a
// current-thread runtime sees the default its test installed, as tasks
// polled on one Rust thread do. The runtime also says how a span entered
// around a future is exited when the future parks and entered again when it
// resumes (Instrument).

// LocalStore is the thread-local storage of one thread. Only code running on
// that thread uses it, so it needs no lock, as a thread-local needs none.
//
// tokio.ThreadLocals has this method set, so the runtime can hand its own
// storage over as it is.
type LocalStore interface {
	// Get returns the value stored under key.
	Get(key any) (any, bool)
	// Set stores value under key.
	Set(key, value any)
	// Delete removes the value stored under key.
	Delete(key any)
}

// RuntimeHooks is what the async runtime gives this package so that its
// thread-local state follows the runtime's threads and polls.
type RuntimeHooks struct {
	// ThreadLocals returns the storage of the thread the calling goroutine
	// runs on.
	ThreadLocals func() LocalStore
	// PollScope runs body, which awaits with ctx, as a future that calls
	// enter now and each time it is resumed after parking, and exit each time
	// it parks and when body ends, whether it returns, panics or is
	// cancelled. It is what `Instrumented::poll` does around each poll.
	PollScope func(ctx context.Context, enter, exit func(), body func())
}

var runtimeHooks atomic.Pointer[RuntimeHooks]

// RuntimeHooksSet installs the hooks of the async runtime. The runtime calls
// it once, when its package is initialised, before any code can have stored
// thread-local state; state stored under the previous hooks is not carried
// over.
func RuntimeHooksSet(hooks RuntimeHooks) {
	runtimeHooks.Store(&hooks)
}

// ThreadLocals returns the storage of the thread the calling goroutine runs
// on: the runtime's, when RuntimeHooksSet installed one, and otherwise the
// goroutine's own.
func ThreadLocals() LocalStore {
	if hooks := runtimeHooks.Load(); hooks != nil && hooks.ThreadLocals != nil {
		return hooks.ThreadLocals()
	}
	return goroutineLocals{gid: goid()}
}

// pollScope runs body as RuntimeHooks.PollScope does. Without a runtime a
// future is a plain call that never parks, so enter and exit bracket it once.
func pollScope(ctx context.Context, enter, exit func(), body func()) {
	if hooks := runtimeHooks.Load(); hooks != nil && hooks.PollScope != nil {
		hooks.PollScope(ctx, enter, exit, body)
		return
	}
	enter()
	defer exit()
	body()
}

// goroutineLocals is the storage of one goroutine, kept in a table keyed by
// goroutine id. A goroutine's entry exists only while it stores something,
// so a goroutine that ends with nothing stored leaves nothing behind.
type goroutineLocals struct {
	gid uint64
}

const localShards = 64

type localShard struct {
	mu sync.Mutex
	m  map[uint64]map[any]any
}

var localTable [localShards]localShard

func (g goroutineLocals) Get(key any) (any, bool) {
	shard := &localTable[g.gid%localShards]
	shard.mu.Lock()
	defer shard.mu.Unlock()
	value, ok := shard.m[g.gid][key]
	return value, ok
}

func (g goroutineLocals) Set(key, value any) {
	shard := &localTable[g.gid%localShards]
	shard.mu.Lock()
	defer shard.mu.Unlock()
	if shard.m == nil {
		shard.m = make(map[uint64]map[any]any)
	}
	values := shard.m[g.gid]
	if values == nil {
		values = make(map[any]any)
		shard.m[g.gid] = values
	}
	values[key] = value
}

func (g goroutineLocals) Delete(key any) {
	shard := &localTable[g.gid%localShards]
	shard.mu.Lock()
	defer shard.mu.Unlock()
	values := shard.m[g.gid]
	delete(values, key)
	if len(values) == 0 {
		delete(shard.m, g.gid)
	}
}

// goid returns the calling goroutine's id, the number the Go runtime prints
// in a stack trace header, "goroutine 123 [running]:". Ids are never reused,
// so a stale entry cannot be mistaken for a new goroutine's.
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
