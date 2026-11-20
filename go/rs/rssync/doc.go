// Package rssync stands in for Rust's std::sync, std::sync::atomic and
// std::sync::mpsc (Rust 1.99.1: std/src/sync/, its poison/ and mpmc/
// directories, and core/src/sync/atomic.rs).
//
// # Handles
//
// Mutex, RwLock, Condvar, Once, OnceLock, LazyLock and Barrier are small
// handles to shared state. Copying a handle, or a struct that holds one,
// copies the handle, not the lock: both copies are the same mutex, as a
// moved Mutex is in Rust. The zero value is not a lock; using one panics
// with a message naming the constructor. rs.Default builds real ones for
// the fields of a struct whose Default is derived. Statics are written with
// the constructors:
//
//	static CACHE: Mutex<Vec<u8>> = Mutex::new(Vec::new());  var CACHE = rssync.MutexNew[[]byte](nil)
//	static CELL: OnceLock<String> = OnceLock::new();        var CELL = rssync.OnceLockNew[string]()
//	static NEXT: AtomicU64 = AtomicU64::new(1);             var NEXT = rssync.AtomicU64New(1)
//
// # Locks and poisoning
//
//	m.lock().unwrap()                              m.Lock()
//	m.lock().expect("..")                          m.LockExpect("..")
//	m.lock().unwrap_or_else(|e| e.into_inner())    m.LockInner()
//	match m.lock() { Ok(g) => .., Err(p) => .. }   g, poisoned := m.LockResult()
//	m.try_lock()                                   g, err := m.TryLock()
//	m.lock().unwrap().push(x);                     m.With(func(v *[]int) { *v = append(*v, x) })
//	*guard = x                                     *guard.Get() = x
//	drop(guard)                                    guard.Drop()
//
// and the same for RwLock with Read and Write. A guard is dropped with
// `defer guard.Drop()` on the line after it is taken, or earlier where Rust
// drops it earlier; Drop is idempotent.
//
// Poisoning is real. Rust poisons a lock when its guard is dropped while the
// thread panics, if the guard was taken before the panic began. Here the
// guard's Drop, run by `defer` while a panic unwinds the goroutine, sees the
// panic with recover, poisons the lock, unlocks it and panics again with the
// same value. A guard taken inside a destructor that runs during the panic
// does not poison, as in Rust, and runtime.Goexit (how a cancelled tokio task
// ends) is not a panic and poisons nothing. A guard that another deferred
// function drops (a struct holding a guard, dropped in its own Drop) cannot
// see the panic: such a Drop calls recover itself and uses DropPanicking (see
// MutexGuard.DropPanicking). poison.go has the details.
//
// # Condvar
//
// A wait takes the guard and gives it back with the lock held again: the Go
// guard is the same object before and after, so a deferred Drop on it stays
// right. Wait, WaitTimeout (with WaitTimeoutResult.TimedOut), the *Inner,
// *Expect and *Result forms, CondvarWaitWhile and CondvarWaitTimeoutWhile.
//
// # Atomics
//
// AtomicBool, AtomicU8, AtomicU64, AtomicUsize (holding the port's usize,
// int) and AtomicI64, with Rust's method names. Each takes an Ordering, which
// is accepted and checked where Rust checks it (a Release load, an Acquire
// store and a Release or AcqRel failure ordering panic with Rust's text) and
// otherwise ignored: every Go atomic operation is sequentially consistent.
// The zero value of an atomic is a usable zero. Share an atomic by pointer;
// copying the Go value makes a second, independent atomic. CompareExchange
// and TryUpdate return the value and a bool for Rust's Ok and Err.
//
// # mpsc
//
//	let (tx, rx) = mpsc::channel();          tx, rx := rssync.MpscChannel[T]()
//	let (tx, rx) = mpsc::sync_channel(n);    tx, rx := rssync.MpscSyncChannel[T](n)
//	tx.send(x)                               err := tx.Send(x)   // nil or *MpscSendError[T]
//	tx.try_send(x)                           err := tx.TrySend(x) // *MpscTrySendError_Full[T], *MpscTrySendError_Disconnected[T]
//	rx.recv()                                v, err := rx.Recv()  // err is MpscRecvError{}
//	rx.try_recv()                            v, err := rx.TryRecv() // MpscTryRecvError_Empty, _Disconnected
//	rx.recv_timeout(d)                       v, err := rx.RecvTimeout(d) // MpscRecvTimeoutError_Timeout, _Disconnected
//	for x in rx.iter() / rx.try_iter()       for x := range rx.Iter() / rx.TryIter()
//	for x in rx                              for x := range rx.IntoIter()
//	tx.clone(), drop(tx), drop(rx)           tx.Clone(), tx.Drop(), rx.Drop()
//
// Senders and receivers are handles like the locks: a copy is a move, Clone
// makes another sender, and Drop (idempotent) gives one up. When the last
// sender is dropped the receiver gets the remaining messages and then
// RecvError; when the receiver is dropped, sends fail and the messages still
// in the channel are dropped (their Drop runs). The three flavours of Rust's
// channel are reproduced: unbounded (channel), bounded (sync_channel(n)) and
// rendezvous (sync_channel(0)), with Rust's error text and Debug forms.
//
// # Not covered
//
// The unstable parts of std::sync: the nonpoison module, mapped guards,
// ReentrantLock, Exclusive, mpmc, oneshot, the lock value accessors
// (Mutex::set, replace, get_cloned), OnceLock::try_insert and
// get_or_try_init, SyncSender::send_timeout, Receiver::recv_deadline and
// is_disconnected. Of the atomics: AtomicPtr, the widths Mjolnir does not
// use (U16, U32, I8, I16, I32, Isize), get_mut, from_mut, as_ptr, fence and
// compiler_fence, and the deprecated compare_and_swap. Rust's guards cannot
// leave their thread; the port does not check that.
package rssync
