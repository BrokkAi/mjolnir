// Port of std 1.99.1 src/os/windows/io/handle.rs and src/os/windows/io/raw.rs

//go:build windows

package rsio

import (
	"strconv"

	"golang.org/x/sys/windows"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// RawHandle is std::os::windows::io::RawHandle: a Windows HANDLE.
type RawHandle = windows.Handle

// AsRawHandle is the trait std::os::windows::io::AsRawHandle.
type AsRawHandle interface {
	AsRawHandle() RawHandle
}

// IntoRawHandle is the trait std::os::windows::io::IntoRawHandle: the value
// gives up its handle, and no longer closes it.
type IntoRawHandle interface {
	IntoRawHandle() RawHandle
}

// AsHandle is the trait std::os::windows::io::AsHandle.
type AsHandle interface {
	AsHandle() BorrowedHandle
}

// BorrowedHandle is std::os::windows::io::BorrowedHandle: a handle that
// someone else owns and closes.
type BorrowedHandle struct {
	handle RawHandle
}

// BorrowedHandleBorrowRaw is BorrowedHandle::borrow_raw.
func BorrowedHandleBorrowRaw(handle RawHandle) BorrowedHandle {
	return BorrowedHandle{handle: handle}
}

// TryCloneToOwned is BorrowedHandle::try_clone_to_owned: a new handle to the
// same object, with the same access, not inherited by child processes.
func (b BorrowedHandle) TryCloneToOwned() (*OwnedHandle, error) {
	return b.Duplicate(0, false, windows.DUPLICATE_SAME_ACCESS)
}

// Duplicate is BorrowedHandle::duplicate, which std keeps to itself and its
// process and socket code uses.
func (b BorrowedHandle) Duplicate(access uint32, inherit bool, options uint32) (*OwnedHandle, error) {
	handle := b.handle

	// `Stdin`, `Stdout`, and `Stderr` can all hold null handles, such as
	// in a process with a detached console. `DuplicateHandle` would fail
	// if we passed it a null handle, but we can treat null as a valid
	// handle which doesn't do any I/O, and allow it to be duplicated.
	if handle == 0 {
		return OwnedHandleFromRawHandle(handle), nil
	}

	var ret windows.Handle
	curProc := windows.CurrentProcess()
	if err := windows.DuplicateHandle(curProc, handle, curProc, &ret, access, inherit, options); err != nil {
		return nil, From(err)
	}
	return OwnedHandleFromRawHandle(ret), nil
}

// AsRawHandle is AsRawHandle::as_raw_handle.
func (b BorrowedHandle) AsRawHandle() RawHandle { return b.handle }

// AsHandle is AsHandle::as_handle.
func (b BorrowedHandle) AsHandle() BorrowedHandle { return b }

// IsTerminal is IsTerminal::is_terminal.
func (b BorrowedHandle) IsTerminal() bool { return HandleIsTerminal(b.handle) }

// DebugString is `impl Debug for BorrowedHandle`.
func (b BorrowedHandle) DebugString() string {
	return "BorrowedHandle { handle: " + debugPointer(uintptr(b.handle)) + " }"
}

// debugPointer is the Debug form of a raw pointer: its address in
// hexadecimal.
func debugPointer(address uintptr) string {
	return "0x" + strconv.FormatUint(uint64(address), 16)
}

// OwnedHandle is std::os::windows::io::OwnedHandle: a handle that this value
// owns. Drop closes it.
type OwnedHandle struct {
	handle   RawHandle
	released bool
}

// OwnedHandleFromRawHandle is OwnedHandle::from_raw_handle: the value takes
// ownership of an open handle.
func OwnedHandleFromRawHandle(handle RawHandle) *OwnedHandle {
	return &OwnedHandle{handle: handle}
}

// OwnedHandleFrom is `OwnedHandle::from(value)` for anything that can give
// up its handle: a File, a pipe end, a child process.
func OwnedHandleFrom(value IntoRawHandle) *OwnedHandle {
	return OwnedHandleFromRawHandle(value.IntoRawHandle())
}

// TryClone is OwnedHandle::try_clone.
func (o *OwnedHandle) TryClone() (*OwnedHandle, error) { return o.AsHandle().TryCloneToOwned() }

// AsRawHandle is AsRawHandle::as_raw_handle.
func (o *OwnedHandle) AsRawHandle() RawHandle {
	o.check()
	return o.handle
}

// AsHandle is AsHandle::as_handle.
func (o *OwnedHandle) AsHandle() BorrowedHandle {
	o.check()
	return BorrowedHandle{handle: o.handle}
}

// IntoRawHandle is IntoRawHandle::into_raw_handle: the caller now owns the
// handle.
func (o *OwnedHandle) IntoRawHandle() RawHandle {
	o.check()
	o.released = true
	return o.handle
}

func (o *OwnedHandle) check() {
	if o.released {
		rs.PanicAt(2, "rsio: use of an OwnedHandle after it was dropped or given away")
	}
}

// IsTerminal is IsTerminal::is_terminal.
func (o *OwnedHandle) IsTerminal() bool { return HandleIsTerminal(o.AsRawHandle()) }

// Drop is `impl Drop for OwnedHandle`: it closes the handle. A second call,
// and a call after IntoRawHandle, does nothing.
func (o *OwnedHandle) Drop() {
	if o.released {
		return
	}
	o.released = true
	_ = windows.CloseHandle(o.handle)
}

// DebugString is `impl Debug for OwnedHandle`.
func (o *OwnedHandle) DebugString() string {
	return "OwnedHandle { handle: " + debugPointer(uintptr(o.handle)) + " }"
}

func stdHandle(id uint32) RawHandle {
	handle, err := windows.GetStdHandle(id)
	if err != nil {
		return windows.InvalidHandle
	}
	return handle
}

// AsRawHandle is `impl AsRawHandle for Stdin`.
func (s *StdinHandle) AsRawHandle() RawHandle { return stdHandle(windows.STD_INPUT_HANDLE) }

// AsHandle is `impl AsHandle for Stdin`.
func (s *StdinHandle) AsHandle() BorrowedHandle { return BorrowedHandle{handle: s.AsRawHandle()} }

// AsRawHandle is `impl AsRawHandle for StdinLock`.
func (l *StdinLock) AsRawHandle() RawHandle { return stdHandle(windows.STD_INPUT_HANDLE) }

// AsHandle is `impl AsHandle for StdinLock`.
func (l *StdinLock) AsHandle() BorrowedHandle { return BorrowedHandle{handle: l.AsRawHandle()} }

// AsRawHandle is `impl AsRawHandle for Stdout`.
func (s *StdoutHandle) AsRawHandle() RawHandle { return stdHandle(windows.STD_OUTPUT_HANDLE) }

// AsHandle is `impl AsHandle for Stdout`.
func (s *StdoutHandle) AsHandle() BorrowedHandle { return BorrowedHandle{handle: s.AsRawHandle()} }

// AsRawHandle is `impl AsRawHandle for StdoutLock`.
func (l *StdoutLock) AsRawHandle() RawHandle { return stdHandle(windows.STD_OUTPUT_HANDLE) }

// AsHandle is `impl AsHandle for StdoutLock`.
func (l *StdoutLock) AsHandle() BorrowedHandle { return BorrowedHandle{handle: l.AsRawHandle()} }

// AsRawHandle is `impl AsRawHandle for Stderr`.
func (s *StderrHandle) AsRawHandle() RawHandle { return stdHandle(windows.STD_ERROR_HANDLE) }

// AsHandle is `impl AsHandle for Stderr`.
func (s *StderrHandle) AsHandle() BorrowedHandle { return BorrowedHandle{handle: s.AsRawHandle()} }

// AsRawHandle is `impl AsRawHandle for StderrLock`.
func (l *StderrLock) AsRawHandle() RawHandle { return stdHandle(windows.STD_ERROR_HANDLE) }

// AsHandle is `impl AsHandle for StderrLock`.
func (l *StderrLock) AsHandle() BorrowedHandle { return BorrowedHandle{handle: l.AsRawHandle()} }
