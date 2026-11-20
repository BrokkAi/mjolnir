// Port of std 1.99.1 src/sys/fs/unix.rs (the items for Linux)

//go:build linux

package rsfs

import (
	"encoding/binary"
	"math"
	"os"
	"runtime"
	"strconv"
	"sync/atomic"
	"time"
	"unsafe"

	"golang.org/x/sys/unix"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rsio"
)

// readLimit is FileDesc's READ_LIMIT: ssize_t::MAX.
const readLimit = math.MaxInt

// modeT is `mode as mode_t`; mode_t is 32 bits on Linux.
func modeT(mode uint32) uint32 { return mode }

// modeTTryFrom is `mode.try_into().unwrap()` into mode_t, which cannot fail
// on Linux.
func modeTTryFrom(mode uint32) uint32 { return mode }

// normalizeTimespec is the Apple correction in Timespec::new, which Linux
// does not need.
func normalizeTimespec(sec, nsec int64) (int64, int64) { return sec, nsec }

func osFsync(fd int) error    { return unix.Fsync(fd) }
func osDatasync(fd int) error { return unix.Fdatasync(fd) }

func stat64FromUnix(st *unix.Stat_t) stat64 {
	return stat64{
		dev:       st.Dev,
		ino:       st.Ino,
		nlink:     uint64(st.Nlink),
		rdev:      st.Rdev,
		mode:      st.Mode,
		uid:       st.Uid,
		gid:       st.Gid,
		size:      st.Size,
		blksize:   int64(st.Blksize),
		blocks:    st.Blocks,
		atime:     st.Atim.Sec,
		atimeNsec: st.Atim.Nsec,
		mtime:     st.Mtim.Sec,
		mtimeNsec: st.Mtim.Nsec,
		ctime:     st.Ctim.Sec,
		ctimeNsec: st.Ctim.Nsec,
	}
}

// The states of STATX_SAVED_STATE.
const (
	statxStateUnknown uint32 = iota
	statxStatePresent
	statxStateUnavailable
)

// statxSavedState is STATX_SAVED_STATE.
var statxSavedState atomic.Uint32

// statxMask is `libc::STATX_BASIC_STATS | libc::STATX_BTIME`.
const statxMask = unix.STATX_BASIC_STATS | unix.STATX_BTIME

// tryStatx is try_statx. Rust has it only where the C library exposes statx,
// which is glibc; with musl it is not compiled, which ok false stands for.
//
// We prefer `statx` on Linux if available, which contains file creation time,
// as well as 64-bit timestamps of all kinds.
// Default `stat64` contains no creation time and may have 32-bit `time_t`.
func tryStatx(fd int, path string, flags int, mask int) (sysFileAttr, error, bool) {
	if rsio.LinuxLibc != rsio.Libc_Glibc {
		return sysFileAttr{}, nil, false
	}

	// Linux kernel prior to 4.11 or glibc prior to glibc 2.28 don't support `statx`.
	// We check for it on first failure and remember availability to avoid having to
	// do it again.
	statxAvailability := statxSavedState.Load()
	if statxAvailability == statxStateUnavailable {
		return sysFileAttr{}, nil, false
	}

	var buf unix.Statx_t
	if err := unix.Statx(fd, path, flags, mask, &buf); err != nil {
		if statxSavedState.Load() == statxStatePresent {
			return sysFileAttr{}, osError(err), true
		}

		// We're not yet entirely sure whether `statx` is usable on this kernel
		// or not. Syscalls can return errors from things other than the kernel
		// per se, e.g. `EPERM` can be returned if seccomp is used to block the
		// syscall, or `ENOSYS` might be returned from a faulty FUSE driver.
		//
		// Availability is checked by performing a call which expects `EFAULT`
		// if the syscall is usable.
		//
		// See: https://github.com/rust-lang/rust/issues/65662
		//
		// FIXME what about transient conditions like `ENOMEM`?
		_, _, err2 := unix.Syscall6(unix.SYS_STATX, 0, 0, 0, uintptr(statxMask), 0, 0)
		if err2 == unix.EFAULT {
			statxSavedState.Store(statxStatePresent)
			return sysFileAttr{}, osError(err), true
		}
		statxSavedState.Store(statxStateUnavailable)
		return sysFileAttr{}, nil, false
	}
	if statxAvailability == statxStateUnknown {
		statxSavedState.Store(statxStatePresent)
	}

	stat := stat64{
		dev:       unix.Mkdev(buf.Dev_major, buf.Dev_minor),
		ino:       buf.Ino,
		nlink:     uint64(buf.Nlink),
		mode:      uint32(buf.Mode),
		uid:       buf.Uid,
		gid:       buf.Gid,
		rdev:      unix.Mkdev(buf.Rdev_major, buf.Rdev_minor),
		size:      int64(buf.Size),
		blksize:   int64(buf.Blksize),
		blocks:    int64(buf.Blocks),
		atime:     buf.Atime.Sec,
		atimeNsec: int64(buf.Atime.Nsec),
		mtime:     buf.Mtime.Sec,
		mtimeNsec: int64(buf.Mtime.Nsec),
		ctime:     buf.Ctime.Sec,
		ctimeNsec: int64(buf.Ctime.Nsec),
	}

	extra := statxExtraFields{
		stxMask:      buf.Mask,
		stxBtimeSec:  buf.Btime.Sec,
		stxBtimeNsec: buf.Btime.Nsec,
	}

	return sysFileAttr{stat: stat, statxExtraFields: rs.Some(extra)}, nil, true
}

func (a *sysFileAttr) created() (time.Time, error) {
	if ext, ok := a.statxExtraFields.Get(); ok {
		if ext.stxMask&unix.STATX_BTIME != 0 {
			return systemTimeNew(ext.stxBtimeSec, int64(ext.stxBtimeNsec))
		}
		return time.Time{}, rsio.ErrorFromStaticMessage(rsio.ErrorKind_Unsupported,
			"creation time is not available for the filesystem")
	}

	return time.Time{}, rsio.ErrorFromStaticMessage(rsio.ErrorKind_Unsupported,
		"creation time is not available on this platform currently")
}

// fileAttrOf is File::file_attr.
func fileAttrOf(fd int) (sysFileAttr, error) {
	if attr, err, ok := tryStatx(fd, "", unix.AT_EMPTY_PATH|unix.AT_STATX_SYNC_AS_STAT, statxMask); ok {
		return attr, err
	}

	var stat unix.Stat_t
	if err := unix.Fstat(fd, &stat); err != nil {
		return sysFileAttr{}, cvt(err)
	}
	return fileAttrFromStat64(stat64FromUnix(&stat)), nil
}

func impStat(p nativePath) (sysFileAttr, error) {
	if attr, err, ok := tryStatx(unix.AT_FDCWD, p, unix.AT_STATX_SYNC_AS_STAT, statxMask); ok {
		return attr, err
	}

	var stat unix.Stat_t
	if err := unix.Stat(p, &stat); err != nil {
		return sysFileAttr{}, cvt(err)
	}
	return fileAttrFromStat64(stat64FromUnix(&stat)), nil
}

func impLstat(p nativePath) (sysFileAttr, error) {
	if attr, err, ok := tryStatx(unix.AT_FDCWD, p, unix.AT_SYMLINK_NOFOLLOW|unix.AT_STATX_SYNC_AS_STAT, statxMask); ok {
		return attr, err
	}

	var stat unix.Stat_t
	if err := unix.Lstat(p, &stat); err != nil {
		return sysFileAttr{}, cvt(err)
	}
	return fileAttrFromStat64(stat64FromUnix(&stat)), nil
}

func (t sysFileTimes) fmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("FileTimes").
		Field("accessed", t.accessed).
		Field("modified", t.modified).
		Finish()
}

// fileTimeToTimespec is file_time_to_timespec. Every SystemTime fits a
// 64-bit timespec, so it cannot fail here.
func fileTimeToTimespec(t rs.Option[time.Time]) unix.Timespec {
	if at, ok := t.Get(); ok {
		return timespecOf(at)
	}
	return unix.Timespec{Sec: 0, Nsec: unix.UTIME_OMIT}
}

func (f *sysFile) setTimes(times sysFileTimes) error {
	defer runtime.KeepAlive(f)
	ts := [2]unix.Timespec{fileTimeToTimespec(times.accessed), fileTimeToTimespec(times.modified)}
	// futimens(fd, times) is utimensat(fd, NULL, times, 0).
	_, _, errno := unix.Syscall6(unix.SYS_UTIMENSAT, uintptr(f.rawFd()), 0,
		uintptr(unsafe.Pointer(&ts[0])), 0, 0, 0)
	if errno != 0 {
		return cvt(errno)
	}
	return nil
}

// getPathFromFd is get_path_from_fd.
func getPathFromFd(fd int) rs.Option[string] {
	p := "/proc/self/fd/" + strconv.Itoa(fd)
	path, err := withNativePath(p, impReadlink)
	if err != nil {
		return rs.None[string]()
	}
	return rs.Some(path)
}

// dirStream is a DIR of the C library: an open directory read with
// getdents64 into a buffer, as glibc and musl read it.
type dirStream struct {
	file *os.File // owns the descriptor; nil once closed
	fd   int
	buf  []byte
	pos  int
	end  int
}

func newDirStream(fd int, name string) *dirStream {
	// glibc reads 32 KiB at a time, musl 2 KiB; the entries come in the same
	// order either way.
	size := 32 * 1024
	if rsio.LinuxLibc == rsio.Libc_Musl {
		size = 2048
	}
	return &dirStream{file: os.NewFile(uintptr(fd), name), fd: fd, buf: make([]byte, size)}
}

// opendir is the C library's opendir.
func opendir(name nativePath) (*dirStream, error) {
	if name == "" {
		// POSIX.1-1990 says an empty name gets ENOENT;
		// but `open' might like it fine.
		return nil, cvt(unix.ENOENT)
	}
	flags := unix.O_RDONLY | unix.O_DIRECTORY | unix.O_CLOEXEC
	if rsio.LinuxLibc == rsio.Libc_Glibc {
		flags |= unix.O_NONBLOCK
	}
	fd, err := unix.Open(name, flags, 0)
	if err != nil {
		return nil, cvt(err)
	}
	return newDirStream(fd, name), nil
}

// fdopendir is the C library's fdopendir; it owns fd from then on, and
// closes it when it fails.
func fdopendir(fd int) (*dirStream, error) {
	var stat unix.Stat_t
	if err := unix.Fstat(fd, &stat); err != nil {
		unix.Close(fd)
		return nil, cvt(err)
	}
	if stat.Mode&unix.S_IFMT != unix.S_IFDIR {
		unix.Close(fd)
		return nil, cvt(unix.ENOTDIR)
	}
	return newDirStream(fd, ""), nil
}

// readdir is the C library's readdir: the next entry (with "." and ".."),
// an error, or the end (ok false). ENOENT, which Linux reports for a
// directory deleted while it is read, is the end, as both C libraries take
// it.
func (d *dirStream) readdir() (name string, ino uint64, typ uint8, err error, ok bool) {
	if d.file == nil {
		return "", 0, 0, nil, false
	}
	if d.pos >= d.end {
		n, err := unix.Getdents(d.fd, d.buf)
		runtime.KeepAlive(d)
		if err != nil {
			if err == unix.ENOENT {
				return "", 0, 0, nil, false
			}
			return "", 0, 0, cvt(err), false
		}
		if n <= 0 {
			return "", 0, 0, nil, false
		}
		d.pos, d.end = 0, n
	}
	// struct linux_dirent64: d_ino u64, d_off i64, d_reclen u16, d_type u8,
	// then the NUL-terminated name.
	record := d.buf[d.pos:d.end]
	reclen := int(binary.NativeEndian.Uint16(record[16:18]))
	ino = binary.NativeEndian.Uint64(record[0:8])
	typ = record[18]
	nameBytes := record[19:reclen]
	for i, b := range nameBytes {
		if b == 0 {
			nameBytes = nameBytes[:i]
			break
		}
	}
	d.pos += reclen
	return string(nameBytes), ino, typ, nil, true
}

// close is closedir.
func (d *dirStream) close() {
	if d.file == nil {
		return
	}
	_ = d.file.Close()
	d.file = nil
	d.fd = -1
}

func impCanonicalize(path nativePath) (string, error) {
	var (
		resolved string
		err      error
	)
	switch rsio.LinuxLibc {
	case rsio.Libc_Musl:
		resolved, err = muslRealpath(path)
	default:
		resolved, err = glibcRealpath(path)
	}
	if err != nil {
		return "", cvt(err)
	}
	return resolved, nil
}

func impCopy(from, to string) (uint64, error) {
	reader, readerMetadata, err := openFrom(from)
	if err != nil {
		return 0, err
	}
	defer reader.Drop()
	writer, writerMetadata, err := openToAndSetPermissions(to, readerMetadata)
	if err != nil {
		return 0, err
	}
	defer writer.Drop()

	return copyCachedFileMetadata(reader, readerMetadata, writer, writerMetadata)
}
