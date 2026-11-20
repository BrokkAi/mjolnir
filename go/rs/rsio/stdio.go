// Port of std 1.99.1 src/io/stdio.rs

package rsio

import (
	"runtime"
	"sync"
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// handleEbadf is stdio::handle_ebadf: a standard stream that is closed reads
// as empty and swallows what is written to it.
func handleEbadf[T any](value T, err error, fallback func() (T, error)) (T, error) {
	if err != nil && isEbadf(err) {
		return fallback()
	}
	return value, err
}

// stdinRaw is StdinRaw: file descriptor 0, unbuffered.
type stdinRaw struct{}

func (stdinRaw) Read(buf []byte) (int, error) {
	n, err := stdinRead(buf)
	return handleEbadf(n, err, func() (int, error) { return 0, nil })
}

func (stdinRaw) ReadBuf(cursor *BorrowedCursor) error {
	n, err := stdinRead(cursor.AsMut())
	if err != nil {
		if isEbadf(err) {
			return nil
		}
		return err
	}
	cursor.Advance(n)
	return nil
}

func (r stdinRaw) ReadExact(buf []byte) error {
	if len(buf) == 0 {
		return nil
	}
	err := DefaultReadExact(rawStdinReader{}, buf)
	if err != nil && isEbadf(err) {
		return errReadExactEof
	}
	return err
}

func (r stdinRaw) ReadToEnd(buf *[]byte) (int, error) {
	n, err := DefaultReadToEnd(rawStdinReader{}, buf, rs.None[int]())
	return handleEbadf(n, err, func() (int, error) { return 0, nil })
}

func (r stdinRaw) ReadToString(buf *string) (int, error) {
	n, err := DefaultReadToString(rawStdinReader{}, buf, rs.None[int]())
	return handleEbadf(n, err, func() (int, error) { return 0, nil })
}

// rawStdinReader is sys::stdio::Stdin: descriptor 0 without the EBADF rule.
type rawStdinReader struct{}

func (rawStdinReader) Read(buf []byte) (int, error) { return stdinRead(buf) }

func (rawStdinReader) ReadBuf(cursor *BorrowedCursor) error {
	n, err := stdinRead(cursor.AsMut())
	if err != nil {
		return err
	}
	cursor.Advance(n)
	return nil
}

// stdoutRaw is StdoutRaw: file descriptor 1, unbuffered.
type stdoutRaw struct{}

func (stdoutRaw) Write(buf []byte) (int, error) {
	n, err := stdoutWrite(buf)
	return handleEbadf(n, err, func() (int, error) { return len(buf), nil })
}

func (stdoutRaw) Flush() error { return nil }

func (stdoutRaw) WriteAll(buf []byte) error {
	err := DefaultWriteAll(rawWriter(stdoutWrite), buf)
	if err != nil && isEbadf(err) {
		return nil
	}
	return err
}

// stderrRaw is StderrRaw: file descriptor 2, unbuffered.
type stderrRaw struct{}

func (stderrRaw) Write(buf []byte) (int, error) {
	n, err := stderrWrite(buf)
	return handleEbadf(n, err, func() (int, error) { return len(buf), nil })
}

func (stderrRaw) Flush() error { return nil }

func (stderrRaw) WriteAll(buf []byte) error {
	err := DefaultWriteAll(rawWriter(stderrWrite), buf)
	if err != nil && isEbadf(err) {
		return nil
	}
	return err
}

// rawWriter makes a write function a writer.
type rawWriter func(buf []byte) (int, error)

func (w rawWriter) Write(buf []byte) (int, error) { return w(buf) }

// reentrantLock is std::sync::ReentrantLock: a lock its holder may take
// again. Rust's holder is a thread; here it is a goroutine, which is what a
// Rust thread or a task between two awaits becomes.
type reentrantLock struct {
	mu    sync.Mutex
	owner atomic.Uint64
	count uint32
}

func (l *reentrantLock) lock() {
	id := goroutineId()
	if l.owner.Load() == id {
		l.increment()
		return
	}
	l.mu.Lock()
	l.owner.Store(id)
	l.count = 1
}

func (l *reentrantLock) tryLock() bool {
	id := goroutineId()
	if l.owner.Load() == id {
		l.increment()
		return true
	}
	if !l.mu.TryLock() {
		return false
	}
	l.owner.Store(id)
	l.count = 1
	return true
}

func (l *reentrantLock) increment() {
	if l.count == ^uint32(0) {
		rs.PanicAt(2, "lock count overflow in reentrant mutex")
	}
	l.count++
}

func (l *reentrantLock) unlock() {
	l.count--
	if l.count == 0 {
		l.owner.Store(0)
		l.mu.Unlock()
	}
}

// goroutineId is the number the Go runtime gives the running goroutine. The
// runtime does not offer it; it is read from the first line of the
// goroutine's stack trace, "goroutine 18 [running]:", as golang.org/x/net
// does for the same purpose.
func goroutineId() uint64 {
	var storage [64]byte
	buf := storage[:runtime.Stack(storage[:], false)]
	const prefix = "goroutine "
	if len(buf) < len(prefix) || string(buf[:len(prefix)]) != prefix {
		panic("rsio: the runtime's stack trace does not start with a goroutine number")
	}
	var id uint64
	for _, digit := range buf[len(prefix):] {
		if digit < '0' || digit > '9' {
			break
		}
		id = id*10 + uint64(digit-'0')
	}
	if id == 0 {
		panic("rsio: the runtime's stack trace does not start with a goroutine number")
	}
	return id
}

// StdinHandle is Rust's io::Stdin: a handle to the standard input of the
// process. All handles share one buffered reader behind a lock; each read
// through a handle takes the lock for its duration.
type StdinHandle struct{}

var (
	stdinHandle StdinHandle
	stdinOnce   sync.Once
	stdinMu     sync.Mutex
	stdinReader *BufReader[stdinRaw]
)

func stdinInstance() *BufReader[stdinRaw] {
	stdinOnce.Do(func() {
		stdinReader = BufReaderWithCapacity(stdinBufSize, stdinRaw{})
	})
	return stdinReader
}

// Stdin is io::stdin.
func Stdin() *StdinHandle { return &stdinHandle }

// Lock is Stdin::lock: it takes the lock on standard input, which the result
// holds until it is dropped. The lock is not reentrant, as in Rust.
func (s *StdinHandle) Lock() *StdinLock {
	reader := stdinInstance()
	stdinMu.Lock()
	return &StdinLock{inner: reader}
}

// ReadLine is Stdin::read_line: it locks, and appends one line to *buf.
func (s *StdinHandle) ReadLine(buf *string) (int, error) {
	lock := s.Lock()
	defer lock.Drop()
	return lock.ReadLine(buf)
}

// Lines is Stdin::lines: an iterator over the lines of standard input, which
// holds the lock until it is dropped.
func (s *StdinHandle) Lines() *LinesIter[*StdinLock] { return Lines(s.Lock()) }

// Read is Read::read.
func (s *StdinHandle) Read(buf []byte) (int, error) {
	lock := s.Lock()
	defer lock.Drop()
	return lock.Read(buf)
}

// ReadBuf is Read::read_buf.
func (s *StdinHandle) ReadBuf(cursor *BorrowedCursor) error {
	lock := s.Lock()
	defer lock.Drop()
	return lock.ReadBuf(cursor)
}

// ReadToEnd is Read::read_to_end.
func (s *StdinHandle) ReadToEnd(buf *[]byte) (int, error) {
	lock := s.Lock()
	defer lock.Drop()
	return lock.ReadToEnd(buf)
}

// ReadToString is Read::read_to_string.
func (s *StdinHandle) ReadToString(buf *string) (int, error) {
	lock := s.Lock()
	defer lock.Drop()
	return lock.ReadToString(buf)
}

// ReadExact is Read::read_exact.
func (s *StdinHandle) ReadExact(buf []byte) error {
	lock := s.Lock()
	defer lock.Drop()
	return lock.ReadExact(buf)
}

// IsTerminal is IsTerminal::is_terminal.
func (s *StdinHandle) IsTerminal() bool { return stdioIsTerminal(0) }

// DebugString is `impl Debug for Stdin`.
func (s *StdinHandle) DebugString() string { return "Stdin { .. }" }

// FmtDebug is `impl Debug for Stdin`.
func (s *StdinHandle) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Stdin").FinishNonExhaustive()
}

// StdinLock is Rust's io::StdinLock: standard input with its lock held. It
// is a BufRead. Drop releases the lock.
type StdinLock struct {
	inner *BufReader[stdinRaw]
}

// Drop releases the lock. A second call does nothing.
func (l *StdinLock) Drop() {
	if l.inner == nil {
		return
	}
	l.inner = nil
	stdinMu.Unlock()
}

func (l *StdinLock) reader() *BufReader[stdinRaw] {
	if l.inner == nil {
		rs.PanicAt(2, "rsio: use of a StdinLock after it was dropped")
	}
	return l.inner
}

// Read is Read::read.
func (l *StdinLock) Read(buf []byte) (int, error) { return l.reader().Read(buf) }

// ReadBuf is Read::read_buf.
func (l *StdinLock) ReadBuf(cursor *BorrowedCursor) error { return l.reader().ReadBuf(cursor) }

// ReadToEnd is Read::read_to_end.
func (l *StdinLock) ReadToEnd(buf *[]byte) (int, error) { return l.reader().ReadToEnd(buf) }

// ReadToString is Read::read_to_string.
func (l *StdinLock) ReadToString(buf *string) (int, error) { return l.reader().ReadToString(buf) }

// ReadExact is Read::read_exact.
func (l *StdinLock) ReadExact(buf []byte) error { return l.reader().ReadExact(buf) }

// ReadBufExact is Read::read_buf_exact.
func (l *StdinLock) ReadBufExact(cursor *BorrowedCursor) error {
	return l.reader().ReadBufExact(cursor)
}

// FillBuf is BufRead::fill_buf.
func (l *StdinLock) FillBuf() ([]byte, error) { return l.reader().FillBuf() }

// Consume is BufRead::consume.
func (l *StdinLock) Consume(amount int) { l.reader().Consume(amount) }

// ReadUntil is BufRead::read_until.
func (l *StdinLock) ReadUntil(delim byte, buf *[]byte) (int, error) {
	return DefaultReadUntil(l.reader(), delim, buf)
}

// ReadLine is BufRead::read_line.
func (l *StdinLock) ReadLine(buf *string) (int, error) {
	return DefaultReadLine(l.reader(), buf)
}

// IsTerminal is IsTerminal::is_terminal.
func (l *StdinLock) IsTerminal() bool { return stdioIsTerminal(0) }

// DebugString is `impl Debug for StdinLock`.
func (l *StdinLock) DebugString() string { return "StdinLock { .. }" }

// FmtDebug is `impl Debug for StdinLock`.
func (l *StdinLock) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("StdinLock").FinishNonExhaustive()
}

// StdoutHandle is Rust's io::Stdout: a handle to the standard output of the
// process. All handles share one LineWriter behind a reentrant lock: output
// is held back until a newline, a Flush, or the end of the process
// (Cleanup).
type StdoutHandle struct{}

var (
	stdoutHandle StdoutHandle
	stdoutLock   reentrantLock
	stdoutOnce   sync.Once
	stdoutWriter *LineWriter[stdoutRaw]
)

func stdoutInstance() {
	stdoutOnce.Do(func() {
		stdoutWriter = LineWriterNew(stdoutRaw{})
	})
}

// Stdout is io::stdout.
func Stdout() *StdoutHandle { return &stdoutHandle }

// Cleanup is std's io::cleanup, which the Rust runtime runs when the process
// ends (on return from main and in process::exit): it writes out what
// standard output still holds and stops buffering it. A program must call
// it before it exits, or a last line without a newline is lost.
//
// Flush the data and disable buffering during shutdown
// by replacing the line writer by one with zero
// buffering capacity.
func Cleanup() {
	initialized := false
	stdoutOnce.Do(func() {
		initialized = true
		stdoutWriter = LineWriterWithCapacity(0, stdoutRaw{})
	})

	if !initialized {
		// The buffer was previously initialized, overwrite it here.
		// We use try_lock() instead of lock(), because someone
		// might have leaked a StdoutLock, which would
		// otherwise cause a deadlock here.
		if stdoutLock.tryLock() {
			old := stdoutWriter
			stdoutWriter = LineWriterWithCapacity(0, stdoutRaw{})
			old.Drop()
			stdoutLock.unlock()
		}
	}
}

// Lock is Stdout::lock: it takes the lock on standard output, which the
// result holds until it is dropped. The goroutine that holds it may take it
// again, and may print.
func (s *StdoutHandle) Lock() *StdoutLock {
	stdoutInstance()
	stdoutLock.lock()
	return &StdoutLock{held: true}
}

// Write is Write::write.
func (s *StdoutHandle) Write(buf []byte) (int, error) {
	lock := s.Lock()
	defer lock.Drop()
	return lock.Write(buf)
}

// Flush is Write::flush.
func (s *StdoutHandle) Flush() error {
	lock := s.Lock()
	defer lock.Drop()
	return lock.Flush()
}

// WriteAll is Write::write_all. The whole buffer is written under one hold
// of the lock.
func (s *StdoutHandle) WriteAll(buf []byte) error {
	lock := s.Lock()
	defer lock.Drop()
	return lock.WriteAll(buf)
}

// IsTerminal is IsTerminal::is_terminal.
func (s *StdoutHandle) IsTerminal() bool { return stdioIsTerminal(1) }

// DebugString is `impl Debug for Stdout`.
func (s *StdoutHandle) DebugString() string { return "Stdout { .. }" }

// FmtDebug is `impl Debug for Stdout`.
func (s *StdoutHandle) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Stdout").FinishNonExhaustive()
}

// StdoutLock is Rust's io::StdoutLock: standard output with its lock held.
// Drop releases the lock; it must be called by the goroutine that locked.
type StdoutLock struct {
	held bool
}

// Drop releases the lock. A second call does nothing.
func (l *StdoutLock) Drop() {
	if !l.held {
		return
	}
	l.held = false
	stdoutLock.unlock()
}

func (l *StdoutLock) writer() *LineWriter[stdoutRaw] {
	if !l.held {
		rs.PanicAt(2, "rsio: use of a StdoutLock after it was dropped")
	}
	return stdoutWriter
}

// Write is Write::write.
func (l *StdoutLock) Write(buf []byte) (int, error) { return l.writer().Write(buf) }

// Flush is Write::flush.
func (l *StdoutLock) Flush() error { return l.writer().Flush() }

// WriteAll is Write::write_all.
func (l *StdoutLock) WriteAll(buf []byte) error { return l.writer().WriteAll(buf) }

// IsTerminal is IsTerminal::is_terminal.
func (l *StdoutLock) IsTerminal() bool { return stdioIsTerminal(1) }

// DebugString is `impl Debug for StdoutLock`.
func (l *StdoutLock) DebugString() string { return "StdoutLock { .. }" }

// FmtDebug is `impl Debug for StdoutLock`.
func (l *StdoutLock) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("StdoutLock").FinishNonExhaustive()
}

// StderrHandle is Rust's io::Stderr: a handle to the standard error of the
// process. It is not buffered; a reentrant lock keeps one write whole.
type StderrHandle struct{}

var (
	stderrHandle StderrHandle
	stderrLock   reentrantLock
)

// Stderr is io::stderr.
func Stderr() *StderrHandle { return &stderrHandle }

// Lock is Stderr::lock: it takes the lock on standard error, which the
// result holds until it is dropped.
func (s *StderrHandle) Lock() *StderrLock {
	stderrLock.lock()
	return &StderrLock{held: true}
}

// Write is Write::write.
func (s *StderrHandle) Write(buf []byte) (int, error) {
	lock := s.Lock()
	defer lock.Drop()
	return lock.Write(buf)
}

// Flush is Write::flush.
func (s *StderrHandle) Flush() error {
	lock := s.Lock()
	defer lock.Drop()
	return lock.Flush()
}

// WriteAll is Write::write_all. The whole buffer is written under one hold
// of the lock.
func (s *StderrHandle) WriteAll(buf []byte) error {
	lock := s.Lock()
	defer lock.Drop()
	return lock.WriteAll(buf)
}

// IsTerminal is IsTerminal::is_terminal.
func (s *StderrHandle) IsTerminal() bool { return stdioIsTerminal(2) }

// DebugString is `impl Debug for Stderr`.
func (s *StderrHandle) DebugString() string { return "Stderr { .. }" }

// FmtDebug is `impl Debug for Stderr`.
func (s *StderrHandle) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Stderr").FinishNonExhaustive()
}

// StderrLock is Rust's io::StderrLock: standard error with its lock held.
// Drop releases the lock; it must be called by the goroutine that locked.
type StderrLock struct {
	held bool
}

// Drop releases the lock. A second call does nothing.
func (l *StderrLock) Drop() {
	if !l.held {
		return
	}
	l.held = false
	stderrLock.unlock()
}

func (l *StderrLock) check() {
	if !l.held {
		rs.PanicAt(2, "rsio: use of a StderrLock after it was dropped")
	}
}

// Write is Write::write.
func (l *StderrLock) Write(buf []byte) (int, error) {
	l.check()
	return stderrRaw{}.Write(buf)
}

// Flush is Write::flush.
func (l *StderrLock) Flush() error {
	l.check()
	return nil
}

// WriteAll is Write::write_all.
func (l *StderrLock) WriteAll(buf []byte) error {
	l.check()
	return stderrRaw{}.WriteAll(buf)
}

// IsTerminal is IsTerminal::is_terminal.
func (l *StderrLock) IsTerminal() bool { return stdioIsTerminal(2) }

// DebugString is `impl Debug for StderrLock`.
func (l *StderrLock) DebugString() string { return "StderrLock { .. }" }

// FmtDebug is `impl Debug for StderrLock`.
func (l *StderrLock) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("StderrLock").FinishNonExhaustive()
}
