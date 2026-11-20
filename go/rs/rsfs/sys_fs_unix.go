// Port of std 1.99.1 src/sys/fs/unix.rs (the parts shared by Linux and
// macOS) and src/sys/helpers/small_c_string.rs

//go:build unix

package rsfs

import (
	"os"
	"runtime"
	"strconv"
	"strings"
	"time"

	"golang.org/x/sys/unix"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rsio"
	"github.com/BrokkAi/mjolnir/go/rs/rspath"
)

// nativePath is what with_native_path hands on. On Unix it is the path
// itself, checked to hold no NUL byte (run_path_with_cstr).
type nativePath = string

// errNul is small_c_string's NUL_ERR.
var errNul = rsio.ErrorFromStaticMessage(rsio.ErrorKind_InvalidInput,
	"file name contained an unexpected NUL byte")

// withNativePath is sys::helpers::run_path_with_cstr: f with the path as a C
// string, or NUL_ERR when the path holds a NUL byte.
func withNativePath[T any](path string, f func(p nativePath) (T, error)) (T, error) {
	if strings.IndexByte(path, 0) >= 0 {
		var zero T
		return zero, errNul
	}
	return f(path)
}

// osError is io::Error::last_os_error after a system call failed with err.
func osError(err error) error {
	if err == nil {
		return nil
	}
	if errno, ok := err.(unix.Errno); ok {
		return rsio.ErrorFromRawOsError(int32(errno))
	}
	return rsio.From(err)
}

// cvt is sys::cvt: the call's error as an io::Error.
func cvt(err error) error { return osError(err) }

// cvtR is sys::cvt_r: the call again for as long as it fails with EINTR.
func cvtR[T any](f func() (T, error)) (T, error) {
	for {
		value, err := f()
		if err == nil {
			return value, nil
		}
		if err != unix.EINTR {
			var zero T
			return zero, osError(err)
		}
	}
}

// cvtRUnit is cvtR for a call that returns ().
func cvtRUnit(f func() error) error {
	_, err := cvtR(func() (rs.Unit, error) { return rs.Unit{}, f() })
	return err
}

// sysFile is sys::fs::File, a FileDesc: it owns the descriptor through an
// *os.File, which closes it when the File is dropped (or, as a last resort,
// when the garbage collector finds it unreachable). Reads, writes and every
// other call go to the descriptor directly, so that they behave as Rust's
// do: no retry on EINTR, (0, nil) at the end of a file, and a descriptor
// opened with O_NONBLOCK fails with WouldBlock rather than waiting.
type sysFile struct {
	file *os.File
	fd   int
}

func newSysFile(fd int, name string) *sysFile {
	return &sysFile{file: os.NewFile(uintptr(fd), name), fd: fd}
}

// rawFd is the descriptor; using a File after Drop is a bug in the caller.
func (f *sysFile) rawFd() int {
	if f.file == nil {
		panic("rsfs: use of a File after Drop")
	}
	return f.fd
}

// drop is OwnedFd's Drop, which ignores what close reports.
func (f *sysFile) drop() {
	if f.file == nil {
		return
	}
	_ = f.file.Close()
	f.file = nil
	f.fd = -1
}

// stat64 holds the fields of a struct stat Rust reads, as Rust's MetadataExt
// gives them.
type stat64 struct {
	dev, ino, nlink, rdev    uint64
	mode, uid, gid           uint32
	size, blksize, blocks    int64
	atime, atimeNsec         int64
	mtime, mtimeNsec         int64
	ctime, ctimeNsec         int64
	birthtime, birthtimeNsec int64 // Apple only
}

// statxExtraFields is StatxExtraFields: what statx gives beyond stat.
type statxExtraFields struct {
	stxMask      uint32
	stxBtimeSec  int64
	stxBtimeNsec uint32
}

// sysFileAttr is sys::fs::FileAttr. statxExtraFields is set only when the
// attributes came from statx, which Rust uses on Linux with glibc.
type sysFileAttr struct {
	stat             stat64
	statxExtraFields rs.Option[statxExtraFields]
}

func fileAttrFromStat64(stat stat64) sysFileAttr {
	return sysFileAttr{stat: stat, statxExtraFields: rs.None[statxExtraFields]()}
}

// sysReadDir is sys::fs::ReadDir.
type sysReadDir struct {
	inner       *innerReadDir
	endOfStream bool
}

// innerReadDir is the open directory and the path it was opened by.
type innerReadDir struct {
	dirp *dirStream
	root string
}

// sysDirEntry is sys::fs::DirEntry: the name of an entry and what readdir
// said about it.
type sysDirEntry struct {
	root  string
	name  string
	dIno  uint64
	dType uint8
}

// sysOpenOptions is sys::fs::OpenOptions.
type sysOpenOptions struct {
	// generic
	read      bool
	write     bool
	append    bool
	truncate  bool
	create    bool
	createNew bool
	// system-specific
	customFlags int32
	mode        uint32
}

// sysFilePermissions is sys::fs::FilePermissions.
type sysFilePermissions struct {
	mode uint32
}

// sysFileTimes is sys::fs::FileTimes. created is set only on Apple systems.
type sysFileTimes struct {
	accessed rs.Option[time.Time]
	modified rs.Option[time.Time]
	created  rs.Option[time.Time]
}

// sysFileType is sys::fs::FileType: the type bits of st_mode.
type sysFileType struct {
	mode uint32
}

// Equal is `impl PartialEq for FileType`: the type bits only.
func (t *sysFileType) Equal(other *sysFileType) bool {
	return t.masked() == other.masked()
}

// HashKey is `impl Hash for FileType`: the type bits only.
func (t *sysFileType) HashKey() string {
	return strconv.FormatUint(uint64(t.masked()), 10)
}

// sysDirBuilder is sys::fs::DirBuilder.
type sysDirBuilder struct {
	mode uint32
}

func (a *sysFileAttr) size() uint64 {
	return uint64(a.stat.size)
}

func (a *sysFileAttr) perm() sysFilePermissions {
	return sysFilePermissions{mode: a.stat.mode}
}

func (a *sysFileAttr) fileType() sysFileType {
	return sysFileType{mode: a.stat.mode}
}

func (a *sysFileAttr) modified() (time.Time, error) {
	return systemTimeNew(a.stat.mtime, a.stat.mtimeNsec)
}

func (a *sysFileAttr) accessed() (time.Time, error) {
	return systemTimeNew(a.stat.atime, a.stat.atimeNsec)
}

func (p sysFilePermissions) readonly() bool {
	// check if any class (owner, group, others) has write permission
	return p.mode&0o222 == 0
}

func (p *sysFilePermissions) setReadonly(readonly bool) {
	if readonly {
		// remove write permission for all classes; equivalent to `chmod a-w <file>`
		p.mode &^= 0o222
	} else {
		// add write permission for all classes; equivalent to `chmod a+w <file>`
		p.mode |= 0o222
	}
}

func (p sysFilePermissions) modeU32() uint32 {
	return p.mode
}

func (t *sysFileTimes) setAccessed(at time.Time) {
	t.accessed = rs.Some(at)
}

func (t *sysFileTimes) setModified(at time.Time) {
	t.modified = rs.Some(at)
}

func (t sysFileType) isDir() bool {
	return t.is(unix.S_IFDIR)
}

func (t sysFileType) isFile() bool {
	return t.is(unix.S_IFREG)
}

func (t sysFileType) isSymlink() bool {
	return t.is(unix.S_IFLNK)
}

func (t sysFileType) is(mode uint32) bool {
	return t.masked() == mode
}

func (t sysFileType) masked() uint32 {
	return t.mode & unix.S_IFMT
}

func (p sysFilePermissions) fmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("FilePermissions").Field("mode", modeDebug(p.mode)).Finish()
}

func (r *sysReadDir) fmtDebug(f *rs.Formatter) error {
	// This will only be called from std::fs::ReadDir, which will add a "ReadDir()" frame.
	// Thus the result will be e g 'ReadDir("/home")'
	return rs.DebugFmt(rspath.Debug(r.inner.root), f)
}

// next is `impl Iterator for ReadDir`: an entry, an error, or the end
// (ok false). The directory is closed when its stream ends.
func (r *sysReadDir) next() (sysDirEntry, error, bool) {
	if r.endOfStream {
		return sysDirEntry{}, nil, false
	}

	for {
		name, ino, typ, err, ok := r.inner.dirp.readdir()
		if err != nil || !ok {
			// We either encountered an error, or reached the end. Either way,
			// the next call to next() should return None.
			r.endOfStream = true
			r.drop()
			if err != nil {
				return sysDirEntry{}, err, true
			}
			return sysDirEntry{}, nil, false
		}

		if name == "." || name == ".." {
			continue
		}

		return sysDirEntry{root: r.inner.root, name: name, dIno: ino, dType: typ}, nil, true
	}
}

// drop is DirStream's Drop: closedir.
func (r *sysReadDir) drop() {
	r.endOfStream = true
	r.inner.dirp.close()
}

func (e *sysDirEntry) path() string {
	return rspath.Join(e.root, e.name)
}

func (e *sysDirEntry) fileName() string {
	return e.name
}

// metadata is DirEntry::metadata. Rust stats the name relative to the open
// directory with fstatat on glibc and Apple systems, and lstats the joined
// path on musl; this port lstats the joined path everywhere (through statx
// where Rust uses statx), so that an entry does not keep its directory open.
// The two differ only when the directory is renamed or replaced while the
// entry is held, or when the joined path is longer than PATH_MAX.
func (e *sysDirEntry) metadata() (sysFileAttr, error) {
	return withNativePath(e.path(), impLstat)
}

func (e *sysDirEntry) fileType() (sysFileType, error) {
	switch e.dType {
	case unix.DT_CHR:
		return sysFileType{mode: unix.S_IFCHR}, nil
	case unix.DT_FIFO:
		return sysFileType{mode: unix.S_IFIFO}, nil
	case unix.DT_LNK:
		return sysFileType{mode: unix.S_IFLNK}, nil
	case unix.DT_REG:
		return sysFileType{mode: unix.S_IFREG}, nil
	case unix.DT_SOCK:
		return sysFileType{mode: unix.S_IFSOCK}, nil
	case unix.DT_DIR:
		return sysFileType{mode: unix.S_IFDIR}, nil
	case unix.DT_BLK:
		return sysFileType{mode: unix.S_IFBLK}, nil
	default:
		attr, err := e.metadata()
		if err != nil {
			return sysFileType{}, err
		}
		return attr.fileType(), nil
	}
}

func (e *sysDirEntry) ino() uint64 {
	return e.dIno
}

func newSysOpenOptions() sysOpenOptions {
	return sysOpenOptions{
		// generic
		read:      false,
		write:     false,
		append:    false,
		truncate:  false,
		create:    false,
		createNew: false,
		// system-specific
		customFlags: 0,
		mode:        0o666,
	}
}

func (o *sysOpenOptions) setRead(read bool)           { o.read = read }
func (o *sysOpenOptions) setWrite(write bool)         { o.write = write }
func (o *sysOpenOptions) setAppend(append bool)       { o.append = append }
func (o *sysOpenOptions) setTruncate(truncate bool)   { o.truncate = truncate }
func (o *sysOpenOptions) setCreate(create bool)       { o.create = create }
func (o *sysOpenOptions) setCreateNew(createNew bool) { o.createNew = createNew }

func (o *sysOpenOptions) setCustomFlags(flags int32) { o.customFlags = flags }

func (o *sysOpenOptions) setMode(mode uint32) { o.mode = modeT(mode) }

func (o *sysOpenOptions) getAccessMode() (int, error) {
	switch {
	case o.read && !o.write && !o.append:
		return unix.O_RDONLY, nil
	case !o.read && o.write && !o.append:
		return unix.O_WRONLY, nil
	case o.read && o.write && !o.append:
		return unix.O_RDWR, nil
	case !o.read && o.append:
		return unix.O_WRONLY | unix.O_APPEND, nil
	case o.read && o.append:
		return unix.O_RDWR | unix.O_APPEND, nil
	default:
		// If no access mode is set, check if any creation flags are set
		// to provide a more descriptive error message
		if o.create || o.createNew || o.truncate {
			return 0, rsio.ErrorNew(rsio.ErrorKind_InvalidInput,
				"creating or truncating a file requires write or append access")
		}
		return 0, rsio.ErrorNew(rsio.ErrorKind_InvalidInput,
			"must specify at least one of read, write, or append access")
	}
}

func (o *sysOpenOptions) getCreationMode() (int, error) {
	switch {
	case o.write && !o.append:
	case !o.write && !o.append:
		if o.truncate || o.create || o.createNew {
			return 0, rsio.ErrorNew(rsio.ErrorKind_InvalidInput,
				"creating or truncating a file requires write or append access")
		}
	case o.append:
		if o.truncate && !o.createNew {
			return 0, rsio.ErrorNew(rsio.ErrorKind_InvalidInput,
				"creating or truncating a file requires write or append access")
		}
	}

	switch {
	case o.createNew:
		return unix.O_CREAT | unix.O_EXCL, nil
	case !o.create && !o.truncate:
		return 0, nil
	case o.create && !o.truncate:
		return unix.O_CREAT, nil
	case !o.create && o.truncate:
		return unix.O_TRUNC, nil
	default:
		return unix.O_CREAT | unix.O_TRUNC, nil
	}
}

func (o sysOpenOptions) fmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("OpenOptions").
		Field("read", o.read).
		Field("write", o.write).
		Field("append", o.append).
		Field("truncate", o.truncate).
		Field("create", o.create).
		Field("create_new", o.createNew).
		Field("custom_flags", o.customFlags).
		Field("mode", modeDebug(o.mode)).
		Finish()
}

func sysFileOpen(path string, opts *sysOpenOptions) (*sysFile, error) {
	return withNativePath(path, func(path nativePath) (*sysFile, error) {
		return sysFileOpenC(path, opts)
	})
}

func sysFileOpenC(path nativePath, opts *sysOpenOptions) (*sysFile, error) {
	accessMode, err := opts.getAccessMode()
	if err != nil {
		return nil, err
	}
	creationMode, err := opts.getCreationMode()
	if err != nil {
		return nil, err
	}
	flags := unix.O_CLOEXEC | accessMode | creationMode | (int(opts.customFlags) &^ unix.O_ACCMODE)
	// The third argument of `open64` is documented to have type `mode_t`. On
	// some platforms (like macOS, where `open64` is actually `open`), `mode_t` is `u16`.
	// However, since this is a variadic function, C integer promotion rules mean that on
	// the ABI level, this still gets passed as `c_int` (aka `u32` on Unix platforms).
	fd, err := cvtR(func() (int, error) { return unix.Open(path, flags, opts.mode) })
	if err != nil {
		return nil, err
	}
	return newSysFile(fd, path), nil
}

func (f *sysFile) fileAttr() (sysFileAttr, error) {
	defer runtime.KeepAlive(f)
	return fileAttrOf(f.rawFd())
}

func (f *sysFile) fsync() error {
	defer runtime.KeepAlive(f)
	fd := f.rawFd()
	return cvtRUnit(func() error { return osFsync(fd) })
}

func (f *sysFile) datasync() error {
	defer runtime.KeepAlive(f)
	fd := f.rawFd()
	return cvtRUnit(func() error { return osDatasync(fd) })
}

func (f *sysFile) lock() error {
	defer runtime.KeepAlive(f)
	return cvt(unix.Flock(f.rawFd(), unix.LOCK_EX))
}

func (f *sysFile) lockShared() error {
	defer runtime.KeepAlive(f)
	return cvt(unix.Flock(f.rawFd(), unix.LOCK_SH))
}

func (f *sysFile) tryLock() error {
	defer runtime.KeepAlive(f)
	return tryLockResult(cvt(unix.Flock(f.rawFd(), unix.LOCK_EX|unix.LOCK_NB)))
}

func (f *sysFile) tryLockShared() error {
	defer runtime.KeepAlive(f)
	return tryLockResult(cvt(unix.Flock(f.rawFd(), unix.LOCK_SH|unix.LOCK_NB)))
}

func tryLockResult(err error) error {
	if err == nil {
		return nil
	}
	if rsio.KindOf(err) == rsio.ErrorKind_WouldBlock {
		return &TryLockError_WouldBlock{}
	}
	return &TryLockError_Error{F0: err}
}

func (f *sysFile) unlock() error {
	defer runtime.KeepAlive(f)
	return cvt(unix.Flock(f.rawFd(), unix.LOCK_UN))
}

func (f *sysFile) truncate(size uint64) error {
	defer runtime.KeepAlive(f)
	off, err := rs.I64TryFrom(size)
	if err != nil {
		return rsio.ErrorNew(rsio.ErrorKind_InvalidInput, err)
	}
	fd := f.rawFd()
	return cvtRUnit(func() error { return unix.Ftruncate(fd, off) })
}

func (f *sysFile) read(buf []byte) (int, error) {
	defer runtime.KeepAlive(f)
	n, err := unix.Read(f.rawFd(), buf[:min(len(buf), readLimit)])
	if err != nil {
		return 0, cvt(err)
	}
	return n, nil
}

func (f *sysFile) readBuf(cursor *rsio.BorrowedCursor) error {
	defer runtime.KeepAlive(f)
	buf := cursor.AsMut()
	n, err := unix.Read(f.rawFd(), buf[:min(len(buf), readLimit)])
	if err != nil {
		return cvt(err)
	}
	cursor.SetInit()
	cursor.Advance(n)
	return nil
}

func (f *sysFile) readAt(buf []byte, offset uint64) (int, error) {
	defer runtime.KeepAlive(f)
	// EINVAL if offset + count overflows
	n, err := unix.Pread(f.rawFd(), buf[:min(len(buf), readLimit)], int64(offset))
	if err != nil {
		return 0, cvt(err)
	}
	return n, nil
}

func (f *sysFile) write(buf []byte) (int, error) {
	defer runtime.KeepAlive(f)
	n, err := unix.Write(f.rawFd(), buf[:min(len(buf), readLimit)])
	if err != nil {
		return 0, cvt(err)
	}
	return n, nil
}

func (f *sysFile) writeAt(buf []byte, offset uint64) (int, error) {
	defer runtime.KeepAlive(f)
	n, err := unix.Pwrite(f.rawFd(), buf[:min(len(buf), readLimit)], int64(offset))
	if err != nil {
		return 0, cvt(err)
	}
	return n, nil
}

func (f *sysFile) flush() error {
	return nil
}

func (f *sysFile) seek(pos rsio.SeekFrom) (uint64, error) {
	defer runtime.KeepAlive(f)
	var (
		whence int
		off    int64
	)
	switch pos := pos.(type) {
	case *rsio.SeekFrom_Start:
		// Casting to `i64` is fine, too large values will end up as
		// negative which will cause an error in `lseek64`.
		whence, off = unix.SEEK_SET, int64(pos.F0)
	case *rsio.SeekFrom_End:
		whence, off = unix.SEEK_END, pos.F0
	case *rsio.SeekFrom_Current:
		whence, off = unix.SEEK_CUR, pos.F0
	default:
		panic(rs.Unreachable(pos))
	}
	n, err := unix.Seek(f.rawFd(), off, whence)
	if err != nil {
		return 0, cvt(err)
	}
	return uint64(n), nil
}

// size is File::size: Option<io::Result<u64>>, with ok false for None.
func (f *sysFile) size() (uint64, error, bool) {
	attr, err := f.fileAttr()
	if err != nil {
		return 0, err, true
	}
	// Fall back to default implementation if the returned size is 0,
	// we might be in a proc mount.
	if attr.size() == 0 {
		return 0, nil, false
	}
	return attr.size(), nil, true
}

func (f *sysFile) tell() (uint64, error) {
	return f.seek(&rsio.SeekFrom_Current{F0: 0})
}

func (f *sysFile) duplicate() (*sysFile, error) {
	defer runtime.KeepAlive(f)
	owned, err := rsio.BorrowedFdBorrowRaw(rsio.RawFd(f.rawFd())).TryCloneToOwned()
	if err != nil {
		return nil, err
	}
	return newSysFile(int(owned.IntoRawFd()), f.file.Name()), nil
}

func (f *sysFile) setPermissions(perm sysFilePermissions) error {
	defer runtime.KeepAlive(f)
	fd := f.rawFd()
	return cvtRUnit(func() error { return unix.Fchmod(fd, modeT(perm.mode)) })
}

func newSysDirBuilder() sysDirBuilder {
	return sysDirBuilder{mode: 0o777}
}

func (b *sysDirBuilder) mkdir(p string) error {
	return withNativePathUnit(p, func(p nativePath) error {
		return cvt(unix.Mkdir(p, modeT(b.mode)))
	})
}

func (b *sysDirBuilder) setMode(mode uint32) {
	b.mode = modeT(mode)
}

func (b sysDirBuilder) fmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("DirBuilder").Field("mode", modeDebug(b.mode)).Finish()
}

func (f *sysFile) fmtDebug(fm *rs.Formatter) error {
	defer runtime.KeepAlive(f)
	return debugPathFd(f.rawFd(), fm, "File").Finish()
}

// debugPathFd is debug_path_fd.
func debugPathFd(fd int, f *rs.Formatter, name string) *rs.DebugStruct {
	b := f.DebugStruct(name)

	getMode := func(fd int) (read, write, ok bool) {
		mode, err := unix.FcntlInt(uintptr(fd), unix.F_GETFL, 0)
		if err != nil {
			return false, false, false
		}
		switch mode & unix.O_ACCMODE {
		case unix.O_RDONLY:
			return true, false, true
		case unix.O_RDWR:
			return true, true, true
		case unix.O_WRONLY:
			return false, true, true
		default:
			return false, false, false
		}
	}

	b.Field("fd", int32(fd))
	if path, ok := getPathFromFd(fd).Get(); ok {
		b.Field("path", rspath.Debug(path))
	}
	if read, write, ok := getMode(fd); ok {
		b.Field("read", read).Field("write", write)
	}

	return b
}

// modeDebug is `impl Debug for Mode`: the mode in octal, followed by the
// mode format used in `ls -l`, as in `0o100664 (-rw-rw-r--)`.
type modeDebug uint32

func (m modeDebug) FmtDebug(f *rs.Formatter) error {
	mode := uint32(m)
	text := strconv.FormatUint(uint64(mode), 8)
	if len(text) < 6 {
		text = strings.Repeat("0", 6-len(text)) + text
	}
	if err := f.WriteStr("0o" + text); err != nil {
		return err
	}

	var entryType byte
	switch mode & unix.S_IFMT {
	case unix.S_IFDIR:
		entryType = 'd'
	case unix.S_IFBLK:
		entryType = 'b'
	case unix.S_IFCHR:
		entryType = 'c'
	case unix.S_IFLNK:
		entryType = 'l'
	case unix.S_IFIFO:
		entryType = 'p'
	case unix.S_IFREG:
		entryType = '-'
	default:
		return nil
	}

	pick := func(set bool, yes byte) byte {
		if set {
			return yes
		}
		return '-'
	}
	special := func(executable, flag bool, both, flagOnly byte) byte {
		switch {
		case executable && flag:
			return both
		case flag:
			return flagOnly
		case executable:
			return 'x'
		default:
			return '-'
		}
	}

	out := []byte{' ', '(', entryType}
	// Owner permissions
	out = append(out, pick(mode&unix.S_IRUSR != 0, 'r'), pick(mode&unix.S_IWUSR != 0, 'w'))
	out = append(out, special(mode&unix.S_IXUSR != 0, mode&unix.S_ISUID != 0, 's', 'S'))
	// Group permissions
	out = append(out, pick(mode&unix.S_IRGRP != 0, 'r'), pick(mode&unix.S_IWGRP != 0, 'w'))
	out = append(out, special(mode&unix.S_IXGRP != 0, mode&unix.S_ISGID != 0, 's', 'S'))
	// Other permissions
	out = append(out, pick(mode&unix.S_IROTH != 0, 'r'), pick(mode&unix.S_IWOTH != 0, 'w'))
	otherExecutable := mode&unix.S_IXOTH != 0
	sticky := mode&unix.S_ISVTX != 0
	switch {
	case entryType == 'd' && otherExecutable && sticky:
		out = append(out, 't') // searchable and restricted deletion
	case entryType == 'd' && sticky:
		out = append(out, 'T') // restricted deletion
	case otherExecutable:
		out = append(out, 'x') // executable
	default:
		out = append(out, '-')
	}
	out = append(out, ')')
	return f.WriteStr(string(out))
}

func impReaddir(path string) (*sysReadDir, error) {
	dirp, err := withNativePath(path, opendir)
	if err != nil {
		return nil, err
	}
	return &sysReadDir{inner: &innerReadDir{dirp: dirp, root: path}}, nil
}

func impUnlink(p nativePath) error {
	return cvt(unix.Unlink(p))
}

func impRename(old, new nativePath) error {
	return cvt(unix.Rename(old, new))
}

func impSetPerm(p nativePath, perm sysFilePermissions) error {
	return cvtRUnit(func() error { return unix.Chmod(p, modeT(perm.mode)) })
}

func impRmdir(p nativePath) error {
	return cvt(unix.Rmdir(p))
}

func impReadlink(p nativePath) (string, error) {
	buf := make([]byte, 256)

	for {
		bufRead, err := unix.Readlink(p, buf)
		if err != nil {
			return "", cvt(err)
		}

		if bufRead != len(buf) {
			return string(buf[:bufRead]), nil
		}

		// Trigger the internal buffer resizing logic of `Vec` by requiring
		// more space than the current capacity. The length is guaranteed to be
		// the same as the capacity due to the if statement above.
		buf = make([]byte, len(buf)*2)
	}
}

func impSymlink(original, link nativePath) error {
	return cvt(unix.Symlink(original, link))
}

func impLink(original, link nativePath) error {
	// Where we can, use `linkat` instead of `link`; see the comment above
	// this one for details on why.
	return cvt(unix.Linkat(unix.AT_FDCWD, original, unix.AT_FDCWD, link, 0))
}

func impExists(path string) (bool, error) {
	return commonExists(path)
}

// openFrom is open_from: the source of a copy, which must be a regular file.
func openFrom(from string) (*File, Metadata, error) {
	reader, err := FileOpen(from)
	if err != nil {
		return nil, Metadata{}, err
	}
	metadata, err := reader.Metadata()
	if err != nil {
		reader.Drop()
		return nil, Metadata{}, err
	}
	if !metadata.IsFile() {
		reader.Drop()
		return nil, Metadata{}, notFileError
	}
	return reader, metadata, nil
}

// openToAndSetPermissions is open_to_and_set_permissions.
func openToAndSetPermissions(to string, readerMetadata Metadata) (*File, Metadata, error) {
	perm := readerMetadata.Permissions()
	options := OpenOptionsNew()
	// create the file with the correct mode right away
	options.Mode(perm.Mode())
	writer, err := options.Write(true).Create(true).Truncate(true).Open(to)
	if err != nil {
		return nil, Metadata{}, err
	}
	writerMetadata, err := writer.Metadata()
	if err != nil {
		writer.Drop()
		return nil, Metadata{}, err
	}
	if writerMetadata.IsFile() {
		// Set the correct file permissions, in case the file already existed.
		// Don't set the permissions on already existing non-files like
		// pipes/FIFOs or device nodes.
		if err := writer.SetPermissions(perm); err != nil {
			writer.Drop()
			return nil, Metadata{}, err
		}
	}
	return writer, writerMetadata, nil
}

func chown(path string, uid, gid uint32) error {
	return withNativePathUnit(path, func(path nativePath) error {
		return cvt(unix.Chown(path, int(uid), int(gid)))
	})
}

func fchown(fd int, uid, gid uint32) error {
	return cvt(unix.Fchown(fd, int(uid), int(gid)))
}

func lchown(path string, uid, gid uint32) error {
	return withNativePathUnit(path, func(path nativePath) error {
		return cvt(unix.Lchown(path, int(uid), int(gid)))
	})
}

func chroot(dir string) error {
	return withNativePathUnit(dir, func(dir nativePath) error {
		return cvt(unix.Chroot(dir))
	})
}

func mkfifo(path string, mode uint32) error {
	return withNativePathUnit(path, func(path nativePath) error {
		return cvt(unix.Mkfifo(path, modeTTryFrom(mode)))
	})
}

func impRemoveDirAll(p string) error {
	return withNativePathUnit(p, removeDirAllModern)
}

// openatNofollowDironly is remove_dir_impl::openat_nofollow_dironly. A
// parentFd below zero is None.
func openatNofollowDironly(parentFd int, p string) (int, error) {
	dirfd := parentFd
	if dirfd < 0 {
		dirfd = unix.AT_FDCWD
	}
	return cvtR(func() (int, error) {
		return unix.Openat(dirfd, p, unix.O_CLOEXEC|unix.O_RDONLY|unix.O_NOFOLLOW|unix.O_DIRECTORY, 0)
	})
}

// isDirEntry is remove_dir_impl::is_dir: what d_type says, with ok false
// when it says nothing.
func isDirEntry(ent *sysDirEntry) (isDir bool, ok bool) {
	switch ent.dType {
	case unix.DT_UNKNOWN:
		return false, false
	case unix.DT_DIR:
		return true, true
	default:
		return false, true
	}
}

func isEnoent(err error) bool {
	if err == nil {
		return false
	}
	code, ok := rsio.From(err).RawOsError().Get()
	return ok && code == int32(unix.ENOENT)
}

// removeDirAllRecursive is remove_dir_impl::remove_dir_all_recursive. A
// parentFd below zero is None.
func removeDirAllRecursive(parentFd int, path string) error {
	// try opening as directory
	fd, err := openatNofollowDironly(parentFd, path)
	if err != nil {
		if code, ok := rsio.From(err).RawOsError().Get(); ok && (code == int32(unix.ENOTDIR) || code == int32(unix.ELOOP)) {
			// not a directory - don't traverse further
			// (for symlinks, older Linux kernels may return ELOOP instead of ENOTDIR)
			if parentFd >= 0 {
				// unlink...
				return cvt(unix.Unlinkat(parentFd, path, 0))
			}
			// ...unless this was supposed to be the deletion root directory
			return err
		}
		return err
	}

	// open the directory passing ownership of the fd
	dirp, err := fdopendir(fd)
	if err != nil {
		return err
	}
	// a valid root is not needed because we do not call any functions involving the full path
	// of the `DirEntry`s.
	dir := &sysReadDir{inner: &innerReadDir{dirp: dirp, root: ""}}
	defer dir.drop()

	for {
		child, err, ok := dir.next()
		if !ok {
			break
		}
		if err != nil {
			return err
		}
		// we need an inner try block, because if one of these
		// directories has already been deleted, then we need to
		// continue the loop, not return ok.
		var result error
		switch isDir, known := isDirEntry(&child); {
		case known && isDir:
			result = removeDirAllRecursive(fd, child.name)
		case known && !isDir:
			result = cvt(unix.Unlinkat(fd, child.name, 0))
		default:
			// POSIX specifies that calling unlink()/unlinkat(..., 0) on a directory can succeed
			// if the process has the appropriate privileges. This however can causing orphaned
			// directories requiring an fsck e.g. on Solaris and Illumos. So we try recursing
			// into it first instead of trying to unlink() it.
			result = removeDirAllRecursive(fd, child.name)
		}
		if result != nil && !isEnoent(result) {
			return result
		}
	}

	// unlink the directory after removing its contents
	dirfd := parentFd
	if dirfd < 0 {
		dirfd = unix.AT_FDCWD
	}
	return ignoreNotfound(cvt(unix.Unlinkat(dirfd, path, unix.AT_REMOVEDIR)))
}

func removeDirAllModern(p nativePath) error {
	// We cannot just call remove_dir_all_recursive() here because that would not delete a passed
	// symlink. No need to worry about races, because remove_dir_all_recursive() does not recurse
	// into symlinks.
	attr, err := impLstat(p)
	if err != nil {
		return err
	}
	if attr.fileType().isSymlink() {
		return impUnlink(p)
	}
	return removeDirAllRecursive(-1, p)
}

// systemTimeNew is sys::time::SystemTime::new: the time, or InvalidData when
// the nanoseconds are out of range.
func systemTimeNew(sec, nsec int64) (time.Time, error) {
	sec, nsec = normalizeTimespec(sec, nsec)
	if nsec >= 0 && nsec < 1_000_000_000 {
		return time.Unix(sec, nsec).UTC(), nil
	}
	return time.Time{}, rsio.ErrorFromStaticMessage(rsio.ErrorKind_InvalidData, "invalid timestamp")
}

// timespecOf is SystemTime's Timespec: whole seconds since the epoch and
// the nanoseconds past them.
func timespecOf(t time.Time) unix.Timespec {
	return unix.Timespec{Sec: t.Unix(), Nsec: int64(t.Nanosecond())}
}
