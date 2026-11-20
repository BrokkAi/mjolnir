// Port of std 1.99.1 src/sys/fs/unix.rs (the items for Apple systems) and
// src/os/macos/fs.rs (FileTimesExt)

//go:build darwin

package rsfs

import (
	"math"
	"runtime"
	"time"
	"unsafe"

	"github.com/ebitengine/purego"
	"golang.org/x/sys/unix"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rsio"
)

// On macOS Rust calls the C library for what the system calls do not cover:
// opendir and readdir, realpath, fcopyfile and fsetattrlist. Go has no C
// here, so these are called in libSystem through purego, whose calls report
// errno as the C library left it. This file was compiled for darwin but not
// run on macOS when it was written.

// readLimit is FileDesc's READ_LIMIT on Apple systems: c_int::MAX, because
// their libc rejects any read larger than INT_MAX.
const readLimit = math.MaxInt32

// modeT is `mode as mode_t`; mode_t is 16 bits on Apple systems.
func modeT(mode uint32) uint32 { return mode & 0xffff }

// modeTTryFrom is `mode.try_into().unwrap()` into the 16-bit mode_t.
func modeTTryFrom(mode uint32) uint32 {
	return uint32(rs.Unwrap(rs.U16TryFrom(mode)))
}

// normalizeTimespec is the correction Timespec::new makes on Apple systems.
//
// On Apple OS, dates before epoch are represented differently than on other
// Unix platforms: e.g. 1/10th of a second before epoch is represented as `seconds=-1`
// and `nanoseconds=100_000_000` on other platforms, but is `seconds=0` and
// `nanoseconds=-900_000_000` on Apple OS.
//
// To compensate, we first detect this special case by checking if both
// seconds and nanoseconds are in range, and then correct the value for seconds
// and nanoseconds to match the common unix representation.
func normalizeTimespec(sec, nsec int64) (int64, int64) {
	if (sec <= 0 && sec > math.MinInt64) && (nsec < 0 && nsec > -1_000_000_000) {
		return sec - 1, nsec + 1_000_000_000
	}
	return sec, nsec
}

func osFsync(fd int) error {
	_, err := unix.FcntlInt(uintptr(fd), unix.F_FULLFSYNC, 0)
	return err
}

func osDatasync(fd int) error {
	_, err := unix.FcntlInt(uintptr(fd), unix.F_FULLFSYNC, 0)
	return err
}

func stat64FromUnix(st *unix.Stat_t) stat64 {
	return stat64{
		dev:           uint64(int64(st.Dev)),
		ino:           st.Ino,
		nlink:         uint64(st.Nlink),
		rdev:          uint64(int64(st.Rdev)),
		mode:          uint32(st.Mode),
		uid:           st.Uid,
		gid:           st.Gid,
		size:          st.Size,
		blksize:       int64(st.Blksize),
		blocks:        st.Blocks,
		atime:         st.Atim.Sec,
		atimeNsec:     st.Atim.Nsec,
		mtime:         st.Mtim.Sec,
		mtimeNsec:     st.Mtim.Nsec,
		ctime:         st.Ctim.Sec,
		ctimeNsec:     st.Ctim.Nsec,
		birthtime:     st.Btim.Sec,
		birthtimeNsec: st.Btim.Nsec,
	}
}

func (a *sysFileAttr) created() (time.Time, error) {
	return systemTimeNew(a.stat.birthtime, a.stat.birthtimeNsec)
}

// fileAttrOf is File::file_attr.
func fileAttrOf(fd int) (sysFileAttr, error) {
	var stat unix.Stat_t
	if err := unix.Fstat(fd, &stat); err != nil {
		return sysFileAttr{}, cvt(err)
	}
	return fileAttrFromStat64(stat64FromUnix(&stat)), nil
}

func impStat(p nativePath) (sysFileAttr, error) {
	var stat unix.Stat_t
	if err := unix.Stat(p, &stat); err != nil {
		return sysFileAttr{}, cvt(err)
	}
	return fileAttrFromStat64(stat64FromUnix(&stat)), nil
}

func impLstat(p nativePath) (sysFileAttr, error) {
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
		Field("created", t.created).
		Finish()
}

func (t *sysFileTimes) setCreated(at time.Time) {
	t.created = rs.Some(at)
}

// SetCreated is FileTimesExt::set_created (std::os::macos::fs).
func (t FileTimes) SetCreated(at time.Time) FileTimes {
	t.f0.setCreated(at)
	return t
}

// libSystem functions, found once.
var (
	libcOpendir            = libSystemSymbol(darwinInode64("opendir"))
	libcFdopendir          = libSystemSymbol(darwinInode64("fdopendir"))
	libcReaddir            = libSystemSymbol(darwinInode64("readdir"))
	libcClosedir           = libSystemSymbol("closedir")
	libcRealpath           = libSystemSymbol("realpath$DARWIN_EXTSN")
	libcFree               = libSystemSymbol("free")
	libcFsetattrlist       = libSystemSymbol("fsetattrlist")
	libcFcopyfile          = libSystemSymbol("fcopyfile")
	libcCopyfileStateAlloc = libSystemSymbol("copyfile_state_alloc")
	libcCopyfileStateFree  = libSystemSymbol("copyfile_state_free")
	libcCopyfileStateGet   = libSystemSymbol("copyfile_state_get")
	libcError              = libSystemSymbol("__error")
)

// darwinInode64 is the name the libc crate links on x86_64 macOS, where the
// plain name is the old 32-bit-inode function.
func darwinInode64(name string) string {
	if runtime.GOARCH == "amd64" {
		return name + "$INODE64"
	}
	return name
}

func libSystemSymbol(name string) uintptr {
	address, err := purego.Dlsym(purego.RTLD_DEFAULT, name)
	if err != nil {
		panic("rsfs: libSystem has no " + name + ": " + err.Error())
	}
	return address
}

// cString is a path as the C library takes it.
func cString(s string) []byte {
	b := make([]byte, len(s)+1)
	copy(b, s)
	return b
}

// pointerOf reads a pointer the C library returned as an integer.
func pointerOf(address uintptr) unsafe.Pointer {
	return *(*unsafe.Pointer)(unsafe.Pointer(&address))
}

// errnoOf is the errno purego captured after a call.
func errnoOf(errno uintptr) error {
	return unix.Errno(errno)
}

// The constants of TimesAttrlist.
const (
	attrBitMapCount = 5          // ATTR_BIT_MAP_COUNT
	attrCmnCrtime   = 0x00000200 // ATTR_CMN_CRTIME
	attrCmnModtime  = 0x00000400 // ATTR_CMN_MODTIME
	attrCmnAcctime  = 0x00001000 // ATTR_CMN_ACCTIME
)

// timesAttrlist is TimesAttrlist: the times to set, in the order
// setattrlist reads them, and the attrlist that names them.
type timesAttrlist struct {
	buf      [3]unix.Timespec
	attrlist unix.Attrlist
	numTimes int
}

func timesAttrlistFromTimes(times *sysFileTimes) timesAttrlist {
	this := timesAttrlist{}
	this.attrlist.Bitmapcount = attrBitMapCount
	if at, ok := times.created.Get(); ok {
		this.buf[this.numTimes] = timespecOf(at)
		this.numTimes++
		this.attrlist.Commonattr |= attrCmnCrtime
	}
	if at, ok := times.modified.Get(); ok {
		this.buf[this.numTimes] = timespecOf(at)
		this.numTimes++
		this.attrlist.Commonattr |= attrCmnModtime
	}
	if at, ok := times.accessed.Get(); ok {
		this.buf[this.numTimes] = timespecOf(at)
		this.numTimes++
		this.attrlist.Commonattr |= attrCmnAcctime
	}
	return this
}

func (f *sysFile) setTimes(times sysFileTimes) error {
	defer runtime.KeepAlive(f)
	ta := timesAttrlistFromTimes(&times)
	r, _, errno := purego.SyscallN(libcFsetattrlist, uintptr(f.rawFd()),
		uintptr(unsafe.Pointer(&ta.attrlist)), uintptr(unsafe.Pointer(&ta.buf[0])),
		uintptr(ta.numTimes)*unsafe.Sizeof(unix.Timespec{}), 0)
	if int32(r) == -1 {
		return cvt(errnoOf(errno))
	}
	return nil
}

// getPathFromFd is get_path_from_fd: fcntl F_GETPATH into a PATH_MAX buffer.
//
// FIXME: The use of PATH_MAX is generally not encouraged, but it
// is inevitable in this case because Apple targets and NetBSD define `fcntl`
// with `F_GETPATH` in terms of `MAXPATHLEN`, and there are no
// alternatives. If a better method is invented, it should be used
// instead.
func getPathFromFd(fd int) rs.Option[string] {
	buf := make([]byte, unix.PathMax)
	_, err := unix.FcntlInt(uintptr(fd), unix.F_GETPATH, int(uintptr(unsafe.Pointer(&buf[0]))))
	runtime.KeepAlive(buf)
	if err != nil {
		return rs.None[string]()
	}
	for i, b := range buf {
		if b == 0 {
			return rs.Some(string(buf[:i]))
		}
	}
	panic(rs.Unreachable())
}

// dirStream is the C library's DIR.
type dirStream struct {
	dir unsafe.Pointer // nil once closed
}

// opendir is the C library's opendir.
func opendir(name nativePath) (*dirStream, error) {
	path := cString(name)
	r, _, errno := purego.SyscallN(libcOpendir, uintptr(unsafe.Pointer(&path[0])))
	if r == 0 {
		return nil, cvt(errnoOf(errno))
	}
	return &dirStream{dir: pointerOf(r)}, nil
}

// fdopendir is the C library's fdopendir; it owns fd from then on, and
// closes it when it fails.
func fdopendir(fd int) (*dirStream, error) {
	r, _, errno := purego.SyscallN(libcFdopendir, uintptr(fd))
	if r == 0 {
		err := cvt(errnoOf(errno))
		unix.Close(fd)
		return nil, err
	}
	return &dirStream{dir: pointerOf(r)}, nil
}

// readdir is the C library's readdir: the next entry (with "." and ".."),
// an error, or the end (ok false).
func (d *dirStream) readdir() (name string, ino uint64, typ uint8, err error, ok bool) {
	if d.dir == nil {
		return "", 0, 0, nil, false
	}
	// To distinguish between errors and end-of-directory, errno is cleared
	// beforehand; purego reports the value readdir left. errno belongs to the
	// thread, so both calls are made on one.
	runtime.LockOSThread()
	clearErrno()
	r, _, errno := purego.SyscallN(libcReaddir, uintptr(d.dir))
	runtime.UnlockOSThread()
	if r == 0 {
		if errno == 0 {
			return "", 0, 0, nil, false
		}
		return "", 0, 0, cvt(errnoOf(errno)), false
	}
	// struct dirent: d_ino u64, d_seekoff u64, d_reclen u16, d_namlen u16,
	// d_type u8, then d_name.
	entry := unsafe.Slice((*byte)(pointerOf(r)), 21)
	ino = *(*uint64)(unsafe.Pointer(&entry[0]))
	namlen := int(*(*uint16)(unsafe.Pointer(&entry[18])))
	typ = entry[20]
	nameBytes := unsafe.Slice((*byte)(unsafe.Add(pointerOf(r), 21)), namlen)
	return string(nameBytes), ino, typ, nil, true
}

// clearErrno is set_errno(0) before readdir.
func clearErrno() {
	r, _, _ := purego.SyscallN(libcError)
	*(*int32)(pointerOf(r)) = 0
}

// close is closedir.
func (d *dirStream) close() {
	if d.dir == nil {
		return
	}
	purego.SyscallN(libcClosedir, uintptr(d.dir))
	d.dir = nil
}

func impCanonicalize(path nativePath) (string, error) {
	cpath := cString(path)
	r, _, errno := purego.SyscallN(libcRealpath, uintptr(unsafe.Pointer(&cpath[0])), 0)
	if r == 0 {
		return "", cvt(errnoOf(errno))
	}
	resolved := pointerOf(r)
	n := 0
	for *(*byte)(unsafe.Add(resolved, n)) != 0 {
		n++
	}
	out := string(unsafe.Slice((*byte)(resolved), n))
	purego.SyscallN(libcFree, r)
	return out, nil
}

// The flags of copyfile.
const (
	copyfileAcl         = 1 << 0
	copyfileStat        = 1 << 1
	copyfileXattr       = 1 << 2
	copyfileData        = 1 << 3
	copyfileSecurity    = copyfileStat | copyfileAcl
	copyfileMetadata    = copyfileSecurity | copyfileXattr
	copyfileAll         = copyfileMetadata | copyfileData
	copyfileStateCopied = 8
)

func impCopy(from, to string) (uint64, error) {
	reader, readerMetadata, err := openFrom(from)
	if err != nil {
		return 0, err
	}
	defer reader.Drop()

	clonefileErr := withNativePathUnit(to, func(to nativePath) error {
		return cvt(unix.Fclonefileat(int(reader.AsRawFd()), unix.AT_FDCWD, to, 0))
	})
	if clonefileErr == nil {
		return readerMetadata.Len(), nil
	}
	code, ok := rsio.From(clonefileErr).RawOsError().Get()
	// `fclonefileat` will fail on non-APFS volumes, if the
	// destination already exists, or if the source and destination
	// are on different devices. In all these cases `fcopyfile`
	// should succeed.
	if !ok || (code != int32(unix.ENOTSUP) && code != int32(unix.EEXIST) && code != int32(unix.EXDEV)) {
		return 0, clonefileErr
	}

	// Fall back to using `fcopyfile` if `fclonefileat` does not succeed.
	writer, writerMetadata, err := openToAndSetPermissions(to, readerMetadata)
	if err != nil {
		return 0, err
	}
	defer writer.Drop()

	// We ensure that `FreeOnDrop` never contains a null pointer so it is
	// always safe to call `copyfile_state_free`
	state, _, errno := purego.SyscallN(libcCopyfileStateAlloc)
	if state == 0 {
		return 0, cvt(errnoOf(errno))
	}
	// `copyfile_state_free` returns -1 if the `to` or `from` files
	// cannot be closed. However, this is not considered an error.
	defer purego.SyscallN(libcCopyfileStateFree, state)

	flags := uintptr(copyfileData)
	if writerMetadata.IsFile() {
		flags = copyfileAll
	}

	r, _, errno := purego.SyscallN(libcFcopyfile, uintptr(reader.AsRawFd()), uintptr(writer.AsRawFd()), state, flags)
	if int32(r) == -1 {
		return 0, cvt(errnoOf(errno))
	}

	var bytesCopied int64
	r, _, errno = purego.SyscallN(libcCopyfileStateGet, state, copyfileStateCopied, uintptr(unsafe.Pointer(&bytesCopied)))
	if int32(r) == -1 {
		return 0, cvt(errnoOf(errno))
	}
	return uint64(bytesCopied), nil
}
