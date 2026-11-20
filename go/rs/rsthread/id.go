// Port of std 1.99.1 src/thread/id.rs

package rsthread

import (
	"math"
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// ThreadId is std::thread::ThreadId: a number that identifies a thread for
// as long as the process runs. Ids are never reused. The main thread's is 1,
// as it is in Rust, where the runtime takes the first id for it before main
// runs; every other thread takes the next one when it is spawned, or, for a
// goroutine rsthread did not start, the first time it asks for its Thread.
//
// ThreadIds compare with == and can key a map.
type ThreadId uint64

var _ = rs.RegisterNewtype[ThreadId]("ThreadId")

// mainThreadId is the id of the main goroutine's thread.
const mainThreadId ThreadId = 1

// threadIdCounter is the last id handed out. The first, 1, is the main
// thread's.
var threadIdCounter = uint64(mainThreadId)

// threadIdNew is ThreadId::new: a new unique thread id.
func threadIdNew() ThreadId {
	last := atomic.LoadUint64(&threadIdCounter)
	for {
		if last == math.MaxUint64 {
			threadIdExhausted()
		}
		id := last + 1
		if atomic.CompareAndSwapUint64(&threadIdCounter, last, id) {
			return ThreadId(id)
		}
		last = atomic.LoadUint64(&threadIdCounter)
	}
}

func threadIdExhausted() {
	rs.PanicAt(0, "failed to generate unique thread ID: bitspace exhausted")
}

// FmtDebug is the derived Debug impl: `ThreadId(2)`.
func (id ThreadId) FmtDebug(f *rs.Formatter) error {
	return f.DebugTuple("ThreadId").Field(uint64(id)).Finish()
}
