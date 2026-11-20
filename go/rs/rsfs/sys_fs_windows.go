// Port of std 1.99.1 src/sys/fs/windows.rs, with the parts of
// src/sys/pal/windows/handle.rs (synchronous reads and writes),
// src/sys/pal/windows/api.rs (set_file_information_by_handle) and
// src/sys/time/windows.rs (FILETIME) it depends on

//go:build windows

package rsfs

import (
	"math"
	"os"
	"runtime"
	"strconv"
	"sync"
	"time"
	"unsafe"

	"golang.org/x/sys/windows"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rsio"
	"github.com/BrokkAi/mjolnir/go/rs/rspath"
)

// This file was compiled for windows/amd64 and windows/arm64 but not run on
// Windows when it was written.

// Win32 error codes (WinError) the code below names.
const (
	errorInvalidFunction  = 1
	errorFileNotFound     = 2
	errorPathNotFound     = 3
	errorAccessDenied     = 5
	errorNoMoreFiles      = 18
	errorSharingViolation = 32
	errorLockViolation    = 33
	errorHandleEof        = 38
	errorNotSupported     = 50
	errorBadNetpath       = 53
	errorBadNetName       = 67
	errorInvalidParameter = 87
	errorDirNotEmpty      = 145
	errorNotLocked        = 158
	errorAlreadyExists    = 183
	errorDirectory        = 267
	errorDeletePending    = 303
	errorIoPending        = 997
	errorCantAccessFile   = 1920
)

// NTSTATUS values.
const (
	statusPending       = 0x00000103
	statusEndOfFile     = 0xC0000011
	statusDeletePending = 0xC0000056
)

// Other constants of the Windows API.
const (
	fileGenericWrite                        = 0x00120116 // FILE_GENERIC_WRITE
	fileWriteData                           = 0x00000002 // FILE_WRITE_DATA
	fileListDirectory                       = 0x00000001 // FILE_LIST_DIRECTORY
	accessDelete                            = 0x00010000 // DELETE
	accessSynchronize                       = 0x00100000 // SYNCHRONIZE
	volumeNameDos                           = 0x0        // VOLUME_NAME_DOS
	symlinkFlagRelative                     = 0x1        // SYMLINK_FLAG_RELATIVE
	symbolicLinkFlagDirectory               = 0x1        // SYMBOLIC_LINK_FLAG_DIRECTORY
	symbolicLinkFlagAllowUnprivilegedCreate = 0x2        // SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE
	fileRenameFlagReplaceIfExists           = 0x1        // FILE_RENAME_FLAG_REPLACE_IF_EXISTS
	fileRenameFlagPosixSemantics            = 0x2        // FILE_RENAME_FLAG_POSIX_SEMANTICS
	fileDispositionFlagDelete               = 0x1        // FILE_DISPOSITION_FLAG_DELETE
	fileDispositionFlagPosixSemantics       = 0x2        // FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
	fileDispositionFlagIgnoreReadonly       = 0x10       // FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE
	findExInfoBasic                         = 1          // FindExInfoBasic
	findExSearchNameMatch                   = 0          // FindExSearchNameMatch
	progressContinue                        = 0          // PROGRESS_CONTINUE
	maximumReparseDataBufferSize            = 16 * 1024  // MAXIMUM_REPARSE_DATA_BUFFER_SIZE
)

// win32FindData is WIN32_FIND_DATAW (x/sys's Win32finddata keeps Go 1's
// short FileName).
type win32FindData struct {
	FileAttributes    uint32
	CreationTime      windows.Filetime
	LastAccessTime    windows.Filetime
	LastWriteTime     windows.Filetime
	FileSizeHigh      uint32
	FileSizeLow       uint32
	Reserved0         uint32
	Reserved1         uint32
	FileName          [windows.MAX_PATH]uint16
	AlternateFileName [14]uint16
}

// fileBasicInfo is FILE_BASIC_INFO.
type fileBasicInfo struct {
	CreationTime   int64
	LastAccessTime int64
	LastWriteTime  int64
	ChangeTime     int64
	FileAttributes uint32
	_              uint32
}

// fileAttributeTagInfo is FILE_ATTRIBUTE_TAG_INFO.
type fileAttributeTagInfo struct {
	FileAttributes uint32
	ReparseTag     uint32
}

var (
	copyProgressOnce sync.Once
	copyProgress     uintptr
	copySizes        = struct {
		sync.Mutex
		next  uintptr
		sizes map[uintptr]int64
	}{sizes: map[uintptr]int64{}}
)

// sysFile is sys::fs::File, a Handle: it owns the handle through an
// *os.File, which closes it when the File is dropped (or, as a last
// resort, when the garbage collector finds it unreachable). Reads, writes
// and every other call go to the handle directly, as Rust's do.
type sysFile struct {
	file   *os.File
	handle windows.Handle
}

func newSysFile(handle windows.Handle, name string) *sysFile {
	return &sysFile{file: os.NewFile(uintptr(handle), name), handle: handle}
}

// rawHandle is the handle; using a File after Drop is a bug in the caller.
func (f *sysFile) rawHandle() windows.Handle {
	if f.file == nil {
		panic("rsfs: use of a File after Drop")
	}
	return f.handle
}

// drop is OwnedHandle's Drop, which ignores what CloseHandle reports.
func (f *sysFile) drop() {
	if f.file == nil {
		return
	}
	_ = f.file.Close()
	f.file = nil
	f.handle = windows.InvalidHandle
}

// sysFileAttr is sys::fs::FileAttr.
type sysFileAttr struct {
	attributes         uint32
	creationTime       windows.Filetime
	lastAccessTime     windows.Filetime
	lastWriteTime      windows.Filetime
	changeTime         rs.Option[windows.Filetime]
	fileSize           uint64
	reparseTag         uint32
	volumeSerialNumber rs.Option[uint32]
	numberOfLinks      rs.Option[uint32]
	fileIndex          rs.Option[uint64]
}

// sysFileType is sys::fs::FileType.
type sysFileType struct {
	directory bool
	symlink   bool
}

// sysReadDir is sys::fs::ReadDir. A zero handle is None.
type sysReadDir struct {
	handle windows.Handle
	root   string
	first  *win32FindData
}

// sysDirEntry is sys::fs::DirEntry.
type sysDirEntry struct {
	root string
	data win32FindData
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
	customFlags          uint32
	accessMode           rs.Option[uint32]
	attributes           uint32
	shareMode            uint32
	securityQosFlags     uint32
	inheritHandle        bool
	freezeLastAccessTime bool
	freezeLastWriteTime  bool
}

// sysFilePermissions is sys::fs::FilePermissions.
type sysFilePermissions struct {
	attrs uint32
}

// sysFileTimes is sys::fs::FileTimes.
type sysFileTimes struct {
	accessed rs.Option[windows.Filetime]
	modified rs.Option[windows.Filetime]
	created  rs.Option[windows.Filetime]
}

// sysDirBuilder is sys::fs::DirBuilder.
type sysDirBuilder struct{}

// filetimeDebug is `impl Debug for c::FILETIME`: FILETIME(<the u64>).
type filetimeDebug windows.Filetime

func (t filetimeDebug) FmtDebug(f *rs.Formatter) error {
	return f.DebugTuple("FILETIME").Field(toU64((*windows.Filetime)(&t))).Finish()
}

func optionFiletimeDebug(t rs.Option[windows.Filetime]) any {
	if ft, ok := t.Get(); ok {
		return rs.Some(filetimeDebug(ft))
	}
	return rs.None[filetimeDebug]()
}

func (r *sysReadDir) fmtDebug(f *rs.Formatter) error {
	// This will only be called from std::fs::ReadDir, which will add a "ReadDir()" frame.
	// Thus the result will be e g 'ReadDir("C:\")'
	return rs.DebugFmt(rspath.Debug(r.root), f)
}

// next is `impl Iterator for ReadDir`.
func (r *sysReadDir) next() (sysDirEntry, error, bool) {
	if r.handle == 0 {
		// This iterator was initialized with an `INVALID_HANDLE_VALUE` as its handle.
		// Simply return `None` because this is only the case when `FindFirstFileExW` in
		// the construction of this iterator returns `ERROR_FILE_NOT_FOUND` which means
		// no matchhing files can be found.
		return sysDirEntry{}, nil, false
	}
	if first := r.first; first != nil {
		r.first = nil
		if e, ok := newSysDirEntry(r.root, first); ok {
			return e, nil, true
		}
	}
	var wfd win32FindData
	for {
		ok, _, err := procFindNextFileW.Call(uintptr(r.handle), uintptr(unsafe.Pointer(&wfd)))
		if ok == 0 {
			code := lastErrorOf(err)
			r.drop()
			if code == errorNoMoreFiles {
				return sysDirEntry{}, nil, false
			}
			return sysDirEntry{}, rsio.ErrorFromRawOsError(int32(code)), true
		}
		if e, ok := newSysDirEntry(r.root, &wfd); ok {
			return e, nil, true
		}
	}
}

// drop is FindNextFileHandle's Drop: FindClose.
func (r *sysReadDir) drop() {
	if r.handle == 0 {
		return
	}
	_ = windows.FindClose(r.handle)
	r.handle = 0
	r.first = nil
}

func newSysDirEntry(root string, wfd *win32FindData) (sysDirEntry, bool) {
	// check for '.' and '..'
	name := wfd.FileName[:3]
	if (name[0] == '.' && name[1] == 0) || (name[0] == '.' && name[1] == '.' && name[2] == 0) {
		return sysDirEntry{}, false
	}
	return sysDirEntry{root: root, data: *wfd}, true
}

func (e *sysDirEntry) path() string {
	return rspath.Join(e.root, e.fileName())
}

func (e *sysDirEntry) fileName() string {
	return rspath.OsStringFromWide(truncateUtf16AtNul(e.data.FileName[:]))
}

func (e *sysDirEntry) fileType() (sysFileType, error) {
	return newSysFileType(e.data.FileAttributes /* reparse_tag = */, e.data.Reserved0), nil
}

func (e *sysDirEntry) metadata() (sysFileAttr, error) {
	return fileAttrFromFindData(&e.data), nil
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
		customFlags:          0,
		accessMode:           rs.None[uint32](),
		shareMode:            windows.FILE_SHARE_READ | windows.FILE_SHARE_WRITE | windows.FILE_SHARE_DELETE,
		attributes:           0,
		securityQosFlags:     0,
		inheritHandle:        false,
		freezeLastAccessTime: false,
		freezeLastWriteTime:  false,
	}
}

func (o *sysOpenOptions) setRead(read bool)           { o.read = read }
func (o *sysOpenOptions) setWrite(write bool)         { o.write = write }
func (o *sysOpenOptions) setAppend(append bool)       { o.append = append }
func (o *sysOpenOptions) setTruncate(truncate bool)   { o.truncate = truncate }
func (o *sysOpenOptions) setCreate(create bool)       { o.create = create }
func (o *sysOpenOptions) setCreateNew(createNew bool) { o.createNew = createNew }

func (o *sysOpenOptions) setCustomFlags(flags uint32)     { o.customFlags = flags }
func (o *sysOpenOptions) setAccessMode(accessMode uint32) { o.accessMode = rs.Some(accessMode) }
func (o *sysOpenOptions) setShareMode(shareMode uint32)   { o.shareMode = shareMode }
func (o *sysOpenOptions) setAttributes(attrs uint32)      { o.attributes = attrs }

func (o *sysOpenOptions) setSecurityQosFlags(flags uint32) {
	// We have to set `SECURITY_SQOS_PRESENT` here, because one of the valid flags we can
	// receive is `SECURITY_ANONYMOUS = 0x0`, which we can't check for later on.
	o.securityQosFlags = flags | windows.SECURITY_SQOS_PRESENT
}

func errCreatingRequiresWrite() error {
	return rsio.ErrorNew(rsio.ErrorKind_InvalidInput,
		"creating or truncating a file requires write or append access")
}

func (o *sysOpenOptions) getAccessMode() (uint32, error) {
	if mode, ok := o.accessMode.Get(); ok {
		return mode, nil
	}
	switch {
	case o.read && !o.write && !o.append:
		return windows.GENERIC_READ, nil
	case !o.read && o.write && !o.append:
		return windows.GENERIC_WRITE, nil
	case o.read && o.write && !o.append:
		return windows.GENERIC_READ | windows.GENERIC_WRITE, nil
	case !o.read && o.append:
		return fileGenericWrite &^ fileWriteData, nil
	case o.read && o.append:
		return windows.GENERIC_READ | (fileGenericWrite &^ fileWriteData), nil
	default:
		// If no access mode is set, check if any creation flags are set
		// to provide a more descriptive error message
		if o.create || o.createNew || o.truncate {
			return 0, errCreatingRequiresWrite()
		}
		return 0, rsio.ErrorNew(rsio.ErrorKind_InvalidInput,
			"must specify at least one of read, write, or append access")
	}
}

// getCmodeDisposition is get_cmode_disposition: the creation mode and the
// NT disposition.
func (o *sysOpenOptions) getCmodeDisposition() (uint32, uint32, error) {
	switch {
	case o.write && !o.append:
	case !o.write && !o.append:
		if o.truncate || o.create || o.createNew {
			return 0, 0, errCreatingRequiresWrite()
		}
	case o.append:
		if o.truncate && !o.createNew {
			return 0, 0, errCreatingRequiresWrite()
		}
	}

	switch {
	case o.createNew:
		return windows.CREATE_NEW, windows.FILE_CREATE, nil
	case !o.create && !o.truncate:
		return windows.OPEN_EXISTING, windows.FILE_OPEN, nil
	case o.create && !o.truncate:
		return windows.OPEN_ALWAYS, windows.FILE_OPEN_IF, nil
	case !o.create && o.truncate:
		return windows.TRUNCATE_EXISTING, windows.FILE_OVERWRITE, nil
	default:
		// `CREATE_ALWAYS` has weird semantics so we emulate it using
		// `OPEN_ALWAYS` and a manual truncation step. See #115745.
		return windows.OPEN_ALWAYS, windows.FILE_OVERWRITE_IF, nil
	}
}

func (o *sysOpenOptions) getCreationMode() (uint32, error) {
	mode, _, err := o.getCmodeDisposition()
	return mode, err
}

func (o *sysOpenOptions) getFlagsAndAttributes() uint32 {
	flags := o.customFlags | o.attributes | o.securityQosFlags
	if o.createNew {
		flags |= windows.FILE_FLAG_OPEN_REPARSE_POINT
	}
	return flags
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
		Field("access_mode", o.accessMode).
		Field("attributes", o.attributes).
		Field("share_mode", o.shareMode).
		Field("security_qos_flags", o.securityQosFlags).
		Field("inherit_handle", o.inheritHandle).
		Field("freeze_last_access_time", o.freezeLastAccessTime).
		Field("freeze_last_write_time", o.freezeLastWriteTime).
		Finish()
}

func sysFileOpen(path string, opts *sysOpenOptions) (*sysFile, error) {
	native, err := maybeVerbatim(path)
	if err != nil {
		return nil, err
	}
	return sysFileOpenNative(native, opts)
}

// sysFileOpenNative is File::open_native.
func sysFileOpenNative(path nativePath, opts *sysOpenOptions) (*sysFile, error) {
	creation, err := opts.getCreationMode()
	if err != nil {
		return nil, err
	}
	accessMode, err := opts.getAccessMode()
	if err != nil {
		return nil, err
	}
	sa := windows.SecurityAttributes{
		Length:             uint32(unsafe.Sizeof(windows.SecurityAttributes{})),
		SecurityDescriptor: nil,
		InheritHandle:      boolToUint32(opts.inheritHandle),
	}
	var saPtr *windows.SecurityAttributes
	if opts.inheritHandle {
		saPtr = &sa
	}
	r, _, callErr := procCreateFileW.Call(uintptr(unsafe.Pointer(&path[0])), uintptr(accessMode),
		uintptr(opts.shareMode), uintptr(unsafe.Pointer(saPtr)), uintptr(creation),
		uintptr(opts.getFlagsAndAttributes()), 0)
	lastError := lastErrorOf(callErr)
	handle := windows.Handle(r)
	if handle == windows.InvalidHandle {
		return nil, ioResult(lastError)
	}
	file := newSysFile(handle, rspath.OsStringFromWide(truncateUtf16AtNul(path)))
	if opts.freezeLastAccessTime || opts.freezeLastWriteTime {
		fileTime := windows.Filetime{LowDateTime: 0xFFFFFFFF, HighDateTime: 0xFFFFFFFF}
		var accessed, written *windows.Filetime
		if opts.freezeLastAccessTime {
			accessed = &fileTime
		}
		if opts.freezeLastWriteTime {
			written = &fileTime
		}
		if err := windows.SetFileTime(handle, nil, accessed, written); err != nil {
			file.drop()
			return nil, osError(err)
		}
	}
	// Manual truncation. See #115745.
	if opts.truncate && creation == windows.OPEN_ALWAYS && lastError == errorAlreadyExists {
		// This first tries `FileAllocationInfo` but falls back to
		// `FileEndOfFileInfo` in order to support WINE.
		// If WINE gains support for FileAllocationInfo, we should
		// remove the fallback.
		alloc := int64(0)
		code := setFileInformationByHandle(handle, windows.FileAllocationInfo, unsafe.Pointer(&alloc), 8)
		if code != 0 {
			eof := int64(0)
			code = setFileInformationByHandle(handle, windows.FileEndOfFileInfo, unsafe.Pointer(&eof), 8)
		}
		if code != 0 {
			file.drop()
			return nil, ioResult(code)
		}
	}
	return file, nil
}

func boolToUint32(b bool) uint32 {
	if b {
		return 1
	}
	return 0
}

// setFileInformationByHandle is api::set_file_information_by_handle: 0 or
// the WinError.
func setFileInformationByHandle(handle windows.Handle, class uint32, info unsafe.Pointer, size uint32) uint32 {
	if err := windows.SetFileInformationByHandle(handle, class, (*byte)(info), size); err != nil {
		return winErrorOf(err)
	}
	return 0
}

func (f *sysFile) fsync() error {
	defer runtime.KeepAlive(f)
	return osError(windows.FlushFileBuffers(f.rawHandle()))
}

func (f *sysFile) datasync() error {
	return f.fsync()
}

func (f *sysFile) acquireLock(flags uint32) error {
	defer runtime.KeepAlive(f)
	var overlapped windows.Overlapped
	event, err := windows.CreateEvent(nil, 0, 0, nil)
	if err != nil {
		return osError(err)
	}
	overlapped.HEvent = event
	lockErr := windows.LockFileEx(f.rawHandle(), flags, 0, math.MaxUint32, math.MaxUint32, &overlapped)

	var finalResult error
	if lockErr != nil {
		if winErrorOf(lockErr) == errorIoPending {
			// Wait for the lock to be acquired, and get the lock operation status.
			// This can happen asynchronously, if the file handle was opened for async IO
			var bytesTransferred uint32
			finalResult = osError(windows.GetOverlappedResult(f.rawHandle(), &overlapped, &bytesTransferred, true))
		} else {
			finalResult = osError(lockErr)
		}
	}
	_ = windows.CloseHandle(overlapped.HEvent)
	return finalResult
}

func (f *sysFile) lock() error {
	return f.acquireLock(windows.LOCKFILE_EXCLUSIVE_LOCK)
}

func (f *sysFile) lockShared() error {
	return f.acquireLock(0)
}

func (f *sysFile) tryLockWith(flags uint32) error {
	defer runtime.KeepAlive(f)
	var overlapped windows.Overlapped
	err := windows.LockFileEx(f.rawHandle(), flags, 0, math.MaxUint32, math.MaxUint32, &overlapped)
	if err == nil {
		return nil
	}
	if winErrorOf(err) == errorLockViolation {
		return &TryLockError_WouldBlock{}
	}
	return &TryLockError_Error{F0: osError(err)}
}

func (f *sysFile) tryLock() error {
	return f.tryLockWith(windows.LOCKFILE_EXCLUSIVE_LOCK | windows.LOCKFILE_FAIL_IMMEDIATELY)
}

func (f *sysFile) tryLockShared() error {
	return f.tryLockWith(windows.LOCKFILE_FAIL_IMMEDIATELY)
}

func (f *sysFile) unlockFile() uint32 {
	ok, _, err := procUnlockFile.Call(uintptr(f.rawHandle()), 0, 0, math.MaxUint32, math.MaxUint32)
	if ok == 0 {
		return lastErrorOf(err)
	}
	return 0
}

func (f *sysFile) unlock() error {
	defer runtime.KeepAlive(f)
	// Unlock the handle twice because LockFileEx() allows a file handle to acquire
	// both an exclusive and shared lock, in which case the documentation states that:
	// "...two unlock operations are necessary to unlock the region; the first unlock operation
	// unlocks the exclusive lock, the second unlock operation unlocks the shared lock"
	if code := f.unlockFile(); code != 0 {
		return ioResult(code)
	}
	if code := f.unlockFile(); code != 0 && code != errorNotLocked {
		return ioResult(code)
	}
	return nil
}

func (f *sysFile) truncate(size uint64) error {
	defer runtime.KeepAlive(f)
	info := int64(size) // FILE_END_OF_FILE_INFO { EndOfFile }
	if code := setFileInformationByHandle(f.rawHandle(), windows.FileEndOfFileInfo, unsafe.Pointer(&info), 8); code != 0 {
		return ioResult(code)
	}
	return nil
}

func (f *sysFile) fileAttr() (sysFileAttr, error) {
	defer runtime.KeepAlive(f)
	var info windows.ByHandleFileInformation
	if err := windows.GetFileInformationByHandle(f.rawHandle(), &info); err != nil {
		return sysFileAttr{}, osError(err)
	}
	reparseTag := uint32(0)
	if info.FileAttributes&windows.FILE_ATTRIBUTE_REPARSE_POINT != 0 {
		var attrTag fileAttributeTagInfo
		if err := windows.GetFileInformationByHandleEx(f.rawHandle(), windows.FileAttributeTagInfo,
			(*byte)(unsafe.Pointer(&attrTag)), uint32(unsafe.Sizeof(attrTag))); err != nil {
			return sysFileAttr{}, osError(err)
		}
		if attrTag.FileAttributes&windows.FILE_ATTRIBUTE_REPARSE_POINT != 0 {
			reparseTag = attrTag.ReparseTag
		}
	}
	return sysFileAttr{
		attributes:         info.FileAttributes,
		creationTime:       info.CreationTime,
		lastAccessTime:     info.LastAccessTime,
		lastWriteTime:      info.LastWriteTime,
		changeTime:         rs.None[windows.Filetime](), // Only available in FILE_BASIC_INFO
		fileSize:           uint64(info.FileSizeLow) | uint64(info.FileSizeHigh)<<32,
		reparseTag:         reparseTag,
		volumeSerialNumber: rs.Some(info.VolumeSerialNumber),
		numberOfLinks:      rs.Some(info.NumberOfLinks),
		fileIndex:          rs.Some(uint64(info.FileIndexLow) | uint64(info.FileIndexHigh)<<32),
	}, nil
}

// rtabort is rtabort!: the message, and the process ends at once.
func rtabort(message string) {
	_, _ = os.Stderr.WriteString("fatal runtime error: " + message + ", aborting\n")
	// __fastfail(FAST_FAIL_FATAL_APP_EXIT) ends the process with
	// STATUS_STACK_BUFFER_OVERRUN.
	os.Exit(int(uint32(0xC0000409)))
}

// synchronousRead is Handle::synchronous_read.
func (f *sysFile) synchronousRead(buf []byte, offset rs.Option[uint64]) (int, error) {
	defer runtime.KeepAlive(f)
	ioStatus := windows.IO_STATUS_BLOCK{Status: statusPending}

	// The length is clamped at u32::MAX.
	length := uint32(min(uint64(len(buf)), math.MaxUint32))
	var bufPtr *byte
	if len(buf) != 0 {
		bufPtr = &buf[0]
	}
	var byteOffset *int64
	if n, ok := offset.Get(); ok {
		value := int64(n)
		byteOffset = &value
	}
	r, _, _ := procNtReadFile.Call(uintptr(f.rawHandle()), 0, 0, 0, uintptr(unsafe.Pointer(&ioStatus)),
		uintptr(unsafe.Pointer(bufPtr)), uintptr(length), uintptr(unsafe.Pointer(byteOffset)), 0)
	status := uint32(r)
	if status == statusPending {
		_, _ = windows.WaitForSingleObject(f.rawHandle(), windows.INFINITE)
		status = uint32(ioStatus.Status)
	}
	switch {
	case status == statusPending:
		// If the operation has not completed then abort the process.
		// Doing otherwise means that the buffer and stack may be written to
		// after this function returns.
		rtabort("I/O error: operation failed to complete synchronously")
		panic(rs.Unreachable())
	case status == statusEndOfFile:
		// Return `Ok(0)` when there's nothing more to read.
		return 0, nil
	case ntSuccess(status):
		return int(ioStatus.Information), nil
	default:
		return 0, rsio.ErrorFromRawOsError(int32(rtlNtStatusToDosError(status)))
	}
}

// synchronousWrite is Handle::synchronous_write.
func (f *sysFile) synchronousWrite(buf []byte, offset rs.Option[uint64]) (int, error) {
	defer runtime.KeepAlive(f)
	ioStatus := windows.IO_STATUS_BLOCK{Status: statusPending}

	// The length is clamped at u32::MAX.
	length := uint32(min(uint64(len(buf)), math.MaxUint32))
	var bufPtr *byte
	if len(buf) != 0 {
		bufPtr = &buf[0]
	}
	var byteOffset *int64
	if n, ok := offset.Get(); ok {
		value := int64(n)
		byteOffset = &value
	}
	r, _, _ := procNtWriteFile.Call(uintptr(f.rawHandle()), 0, 0, 0, uintptr(unsafe.Pointer(&ioStatus)),
		uintptr(unsafe.Pointer(bufPtr)), uintptr(length), uintptr(unsafe.Pointer(byteOffset)), 0)
	status := uint32(r)
	if status == statusPending {
		_, _ = windows.WaitForSingleObject(f.rawHandle(), windows.INFINITE)
		status = uint32(ioStatus.Status)
	}
	switch {
	case status == statusPending:
		// If the operation has not completed then abort the process.
		// Doing otherwise means that the buffer may be read and the stack
		// written to after this function returns.
		rtabort("I/O error: operation failed to complete synchronously")
		panic(rs.Unreachable())
	case ntSuccess(status):
		return int(ioStatus.Information), nil
	default:
		return 0, rsio.ErrorFromRawOsError(int32(rtlNtStatusToDosError(status)))
	}
}

func ntSuccess(status uint32) bool {
	return int32(status) >= 0
}

func rtlNtStatusToDosError(status uint32) uint32 {
	r, _, _ := procRtlNtStatusToDosError.Call(uintptr(status))
	return uint32(r)
}

func (f *sysFile) read(buf []byte) (int, error) {
	n, err := f.synchronousRead(buf, rs.None[uint64]())
	if err != nil {
		// The special treatment of BrokenPipe is to deal with Windows
		// pipe semantics, which yields this error when *reading* from
		// a pipe after the other end has closed; we interpret that as
		// EOF on the pipe.
		if rsio.KindOf(err) == rsio.ErrorKind_BrokenPipe {
			return 0, nil
		}
		return 0, err
	}
	return n, nil
}

func (f *sysFile) readAt(buf []byte, offset uint64) (int, error) {
	n, err := f.synchronousRead(buf, rs.Some(offset))
	if err != nil {
		if code, ok := rsio.From(err).RawOsError().Get(); ok && code == errorHandleEof {
			return 0, nil
		}
		return 0, err
	}
	return n, nil
}

func (f *sysFile) readBuf(cursor *rsio.BorrowedCursor) error {
	n, err := f.synchronousRead(cursor.AsMut(), rs.None[uint64]())
	if err != nil {
		// The special treatment of BrokenPipe is to deal with Windows
		// pipe semantics, which yields this error when *reading* from
		// a pipe after the other end has closed; we interpret that as
		// EOF on the pipe.
		if rsio.KindOf(err) == rsio.ErrorKind_BrokenPipe {
			return nil
		}
		return err
	}
	cursor.SetInit()
	cursor.Advance(n)
	return nil
}

func (f *sysFile) write(buf []byte) (int, error) {
	return f.synchronousWrite(buf, rs.None[uint64]())
}

func (f *sysFile) writeAt(buf []byte, offset uint64) (int, error) {
	return f.synchronousWrite(buf, rs.Some(offset))
}

func (f *sysFile) flush() error {
	return nil
}

func (f *sysFile) seek(pos rsio.SeekFrom) (uint64, error) {
	defer runtime.KeepAlive(f)
	var (
		whence uint32
		offset int64
	)
	switch pos := pos.(type) {
	case *rsio.SeekFrom_Start:
		// Casting to `i64` is fine, `SetFilePointerEx` reinterprets this
		// integer as `u64`.
		whence, offset = windows.FILE_BEGIN, int64(pos.F0)
	case *rsio.SeekFrom_End:
		whence, offset = windows.FILE_END, pos.F0
	case *rsio.SeekFrom_Current:
		whence, offset = windows.FILE_CURRENT, pos.F0
	default:
		panic(rs.Unreachable(pos))
	}
	var newpos int64
	ok, _, err := procSetFilePointerEx.Call(uintptr(f.rawHandle()), uintptr(offset),
		uintptr(unsafe.Pointer(&newpos)), uintptr(whence))
	if ok == 0 {
		return 0, ioResult(lastErrorOf(err))
	}
	return uint64(newpos), nil
}

// size is File::size: always Some on Windows.
func (f *sysFile) size() (uint64, error, bool) {
	defer runtime.KeepAlive(f)
	var result int64
	ok, _, err := procGetFileSizeEx.Call(uintptr(f.rawHandle()), uintptr(unsafe.Pointer(&result)))
	if ok == 0 {
		return 0, ioResult(lastErrorOf(err)), true
	}
	return uint64(result), nil, true
}

func (f *sysFile) tell() (uint64, error) {
	return f.seek(&rsio.SeekFrom_Current{F0: 0})
}

func (f *sysFile) duplicate() (*sysFile, error) {
	defer runtime.KeepAlive(f)
	owned, err := rsio.BorrowedHandleBorrowRaw(f.rawHandle()).TryCloneToOwned()
	if err != nil {
		return nil, err
	}
	return newSysFile(owned.IntoRawHandle(), f.file.Name()), nil
}

// reparsePoint is File::reparse_point: the reparse data of the file.
func (f *sysFile) reparsePoint(space []uint64) (uint32, error) {
	defer runtime.KeepAlive(f)
	var bytes uint32
	buffer := unsafe.Slice((*byte)(unsafe.Pointer(&space[0])), len(space)*8)
	err := windows.DeviceIoControl(f.rawHandle(), windows.FSCTL_GET_REPARSE_POINT, nil, 0,
		&buffer[0], uint32(len(buffer)), &bytes, nil)
	if err != nil {
		return 0, osError(err)
	}
	return bytes, nil
}

func (f *sysFile) readlink() (string, error) {
	space := make([]uint64, maximumReparseDataBufferSize/8)
	if _, err := f.reparsePoint(space); err != nil {
		return "", err
	}
	buf := unsafe.Slice((*byte)(unsafe.Pointer(&space[0])), len(space)*8)
	u16 := func(at int) uint16 { return *(*uint16)(unsafe.Pointer(&buf[at])) }
	reparseTag := *(*uint32)(unsafe.Pointer(&buf[0]))
	// The tag's own buffer starts at 8, after ReparseTag, ReparseDataLength
	// and Reserved.
	var pathBuffer, substOff, substLen int
	relative := false
	switch reparseTag {
	case windows.IO_REPARSE_TAG_SYMLINK:
		// SYMBOLIC_LINK_REPARSE_BUFFER: four u16 offsets and lengths, Flags,
		// then PathBuffer.
		pathBuffer = 8 + 12
		substOff = int(u16(8)) / 2
		substLen = int(u16(10)) / 2
		relative = *(*uint32)(unsafe.Pointer(&buf[16]))&symlinkFlagRelative != 0
	case windows.IO_REPARSE_TAG_MOUNT_POINT:
		// MOUNT_POINT_REPARSE_BUFFER: four u16 offsets and lengths, then
		// PathBuffer.
		pathBuffer = 8 + 8
		substOff = int(u16(8)) / 2
		substLen = int(u16(10)) / 2
	default:
		return "", rsio.ErrorFromStaticMessage(rsio.ErrorKind_Uncategorized, "Unsupported reparse point type")
	}
	subst := make([]uint16, substLen)
	for i := range subst {
		subst[i] = u16(pathBuffer + (substOff+i)*2)
	}
	// Absolute paths start with an NT internal namespace prefix `\??\`
	// We should not let it leak through.
	if !relative && hasPrefixU16(subst, []uint16{92, 63, 63, 92}) {
		// Turn `\??\` into `\\?\` (a verbatim path).
		subst[1] = '\\'
		// Attempt to convert to a more user-friendly path.
		user, err := fromWideToUserPath(append(subst, 0))
		if err != nil {
			return "", err
		}
		if len(user) > 0 && user[len(user)-1] == 0 {
			user = user[:len(user)-1]
		}
		return rspath.OsStringFromWide(user), nil
	}
	return rspath.OsStringFromWide(subst), nil
}

func (f *sysFile) setPermissions(perm sysFilePermissions) error {
	defer runtime.KeepAlive(f)
	info := fileBasicInfo{FileAttributes: perm.attrs}
	if code := setFileInformationByHandle(f.rawHandle(), windows.FileBasicInfo, unsafe.Pointer(&info),
		uint32(unsafe.Sizeof(info))); code != 0 {
		return ioResult(code)
	}
	return nil
}

func (f *sysFile) setTimes(times sysFileTimes) error {
	defer runtime.KeepAlive(f)
	isZero := func(t rs.Option[windows.Filetime]) bool {
		ft, ok := t.Get()
		return ok && ft.LowDateTime == 0 && ft.HighDateTime == 0
	}
	if isZero(times.accessed) || isZero(times.modified) || isZero(times.created) {
		return rsio.ErrorFromStaticMessage(rsio.ErrorKind_InvalidInput, "cannot set file timestamp to 0")
	}
	isMax := func(t rs.Option[windows.Filetime]) bool {
		ft, ok := t.Get()
		return ok && ft.LowDateTime == math.MaxUint32 && ft.HighDateTime == math.MaxUint32
	}
	if isMax(times.accessed) || isMax(times.modified) || isMax(times.created) {
		return rsio.ErrorFromStaticMessage(rsio.ErrorKind_InvalidInput,
			"cannot set file timestamp to 0xFFFF_FFFF_FFFF_FFFF")
	}
	pointer := func(t rs.Option[windows.Filetime]) *windows.Filetime {
		if ft, ok := t.Get(); ok {
			return &ft
		}
		return nil
	}
	return osError(windows.SetFileTime(f.rawHandle(), pointer(times.created), pointer(times.accessed),
		pointer(times.modified)))
}

func (f *sysFile) basicInfo() (fileBasicInfo, error) {
	defer runtime.KeepAlive(f)
	var info fileBasicInfo
	if err := windows.GetFileInformationByHandleEx(f.rawHandle(), windows.FileBasicInfo,
		(*byte)(unsafe.Pointer(&info)), uint32(unsafe.Sizeof(info))); err != nil {
		return fileBasicInfo{}, osError(err)
	}
	return info, nil
}

// delete is File::delete; it consumes the file. A result of 0 is Ok.
func (f *sysFile) delete() uint32 {
	defer f.drop()
	// If POSIX delete is not supported for this filesystem then fallback to win32 delete.
	switch result := f.posixDelete(); result {
	case errorInvalidParameter, errorNotSupported, errorInvalidFunction:
		return f.win32Delete()
	default:
		return result
	}
}

func (f *sysFile) posixDelete() uint32 {
	defer runtime.KeepAlive(f)
	info := uint32(fileDispositionFlagDelete | fileDispositionFlagPosixSemantics | fileDispositionFlagIgnoreReadonly)
	return setFileInformationByHandle(f.rawHandle(), windows.FileDispositionInfoEx, unsafe.Pointer(&info), 4)
}

func (f *sysFile) win32Delete() uint32 {
	defer runtime.KeepAlive(f)
	info := uint8(1) // FILE_DISPOSITION_INFO { DeleteFile: true }
	return setFileInformationByHandle(f.rawHandle(), windows.FileDispositionInfo, unsafe.Pointer(&info), 1)
}

// dirBuffSize is DirBuff::BUFFER_SIZE.
const dirBuffSize = 1024

// fillDirBuff is File::fill_dir_buff: whether there were entries, or the
// WinError.
func (f *sysFile) fillDirBuff(buffer []uint64, restart bool) (bool, uint32) {
	defer runtime.KeepAlive(f)
	class := uint32(windows.FileIdBothDirectoryInfo)
	if restart {
		class = windows.FileIdBothDirectoryRestartInfo
	}
	err := windows.GetFileInformationByHandleEx(f.rawHandle(), class, (*byte)(unsafe.Pointer(&buffer[0])),
		uint32(len(buffer)*8))
	if err != nil {
		code := winErrorOf(err)
		if code == errorNoMoreFiles {
			return false, 0
		}
		return false, code
	}
	return true, 0
}

// dirBuffEntries is DirBuffIter over a filled buffer: each name, and
// whether it is a directory, without the `.` and `..` pseudo entries.
func dirBuffEntries(buffer []uint64) [][2]any {
	bytes := unsafe.Slice((*byte)(unsafe.Pointer(&buffer[0])), len(buffer)*8)
	// FILE_ID_BOTH_DIR_INFO may be unaligned in the buffer; read it byte by
	// byte.
	u32 := func(at int) uint32 {
		return uint32(bytes[at]) | uint32(bytes[at+1])<<8 | uint32(bytes[at+2])<<16 | uint32(bytes[at+3])<<24
	}
	var out [][2]any
	cursor := 0
	for {
		nextEntry := int(u32(cursor))
		length := int(u32(cursor + 60))
		attrs := u32(cursor + 56)
		name := make([]uint16, length/2)
		for i := range name {
			at := cursor + 104 + i*2
			name[i] = uint16(bytes[at]) | uint16(bytes[at+1])<<8
		}
		isDirectory := attrs&windows.FILE_ATTRIBUTE_DIRECTORY != 0
		// Skip `.` and `..` pseudo entries.
		if !(len(name) == 1 && name[0] == '.') && !(len(name) == 2 && name[0] == '.' && name[1] == '.') {
			out = append(out, [2]any{name, isDirectory})
		}
		if nextEntry == 0 {
			return out
		}
		cursor += nextEntry
	}
}

func (f *sysFile) fmtDebug(fm *rs.Formatter) error {
	defer runtime.KeepAlive(f)
	return debugPathHandle(f.rawHandle(), fm, "File").Finish()
}

// debugPathHandle is debug_path_handle.
func debugPathHandle(handle windows.Handle, f *rs.Formatter, name string) *rs.DebugStruct {
	// FIXME(#24570): add more info here (e.g., mode)
	b := f.DebugStruct(name)
	b.Field("handle", rawPointerDebug(handle))
	if path, err := getPath(handle); err == nil {
		b.Field("path", rspath.Debug(path))
	}
	return b
}

// rawPointerDebug is the Debug of a raw pointer: its address in hex.
type rawPointerDebug uintptr

func (p rawPointerDebug) FmtDebug(f *rs.Formatter) error {
	return f.WriteStr("0x" + strconv.FormatUint(uint64(p), 16))
}

func (a *sysFileAttr) size() uint64 {
	return a.fileSize
}

func (a *sysFileAttr) perm() sysFilePermissions {
	return sysFilePermissions{attrs: a.attributes}
}

func (a *sysFileAttr) attrs() uint32 {
	return a.attributes
}

func (a *sysFileAttr) fileType() sysFileType {
	return newSysFileType(a.attributes, a.reparseTag)
}

func (a *sysFileAttr) modified() (time.Time, error) {
	return systemTimeFromFiletime(a.lastWriteTime), nil
}

func (a *sysFileAttr) accessed() (time.Time, error) {
	return systemTimeFromFiletime(a.lastAccessTime), nil
}

func (a *sysFileAttr) created() (time.Time, error) {
	return systemTimeFromFiletime(a.creationTime), nil
}

func (a *sysFileAttr) modifiedU64() uint64 { return toU64(&a.lastWriteTime) }
func (a *sysFileAttr) accessedU64() uint64 { return toU64(&a.lastAccessTime) }
func (a *sysFileAttr) createdU64() uint64  { return toU64(&a.creationTime) }

func fileAttrFromFindData(wfd *win32FindData) sysFileAttr {
	reparseTag := uint32(0)
	if wfd.FileAttributes&windows.FILE_ATTRIBUTE_REPARSE_POINT != 0 {
		// reserved unless this is a reparse point
		reparseTag = wfd.Reserved0
	}
	return sysFileAttr{
		attributes:         wfd.FileAttributes,
		creationTime:       wfd.CreationTime,
		lastAccessTime:     wfd.LastAccessTime,
		lastWriteTime:      wfd.LastWriteTime,
		changeTime:         rs.None[windows.Filetime](),
		fileSize:           uint64(wfd.FileSizeHigh)<<32 | uint64(wfd.FileSizeLow),
		reparseTag:         reparseTag,
		volumeSerialNumber: rs.None[uint32](),
		numberOfLinks:      rs.None[uint32](),
		fileIndex:          rs.None[uint64](),
	}
}

func toU64(ft *windows.Filetime) uint64 {
	return uint64(ft.LowDateTime) | uint64(ft.HighDateTime)<<32
}

// The Windows epoch and its 100ns intervals, as rstime counts them.
const (
	intervalsPerSec  = 10_000_000
	windowsEpochSecs = 11_644_473_600
)

// systemTimeFromFiletime is `SystemTime::from(FILETIME)`: a count of 100ns
// intervals since 1601, read as an i64.
func systemTimeFromFiletime(ft windows.Filetime) time.Time {
	intervals := int64(toU64(&ft))
	secs := intervals / intervalsPerSec
	rest := intervals % intervalsPerSec
	if rest < 0 {
		secs--
		rest += intervalsPerSec
	}
	return time.Unix(secs-windowsEpochSecs, rest*100).UTC()
}

// filetimeOf is SystemTime::into_inner: the FILETIME of a time, cut to
// 100ns intervals.
func filetimeOf(t time.Time) windows.Filetime {
	intervals := (t.Unix()+windowsEpochSecs)*intervalsPerSec + int64(t.Nanosecond()/100)
	return windows.Filetime{LowDateTime: uint32(intervals), HighDateTime: uint32(uint64(intervals) >> 32)}
}

func (p sysFilePermissions) readonly() bool {
	return p.attrs&windows.FILE_ATTRIBUTE_READONLY != 0
}

func (p *sysFilePermissions) setReadonly(readonly bool) {
	if readonly {
		p.attrs |= windows.FILE_ATTRIBUTE_READONLY
	} else {
		p.attrs &^= windows.FILE_ATTRIBUTE_READONLY
	}
}

func (p sysFilePermissions) fileAttributes() uint32 {
	return p.attrs
}

func (p sysFilePermissions) fmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("FilePermissions").Field("attrs", p.attrs).Finish()
}

func (t *sysFileTimes) setAccessed(at time.Time) {
	t.accessed = rs.Some(filetimeOf(at))
}

func (t *sysFileTimes) setModified(at time.Time) {
	t.modified = rs.Some(filetimeOf(at))
}

func (t *sysFileTimes) setCreated(at time.Time) {
	t.created = rs.Some(filetimeOf(at))
}

func (t sysFileTimes) fmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("FileTimes").
		Field("accessed", optionFiletimeDebug(t.accessed)).
		Field("modified", optionFiletimeDebug(t.modified)).
		Field("created", optionFiletimeDebug(t.created)).
		Finish()
}

func newSysFileType(attributes, reparseTag uint32) sysFileType {
	isDirectory := attributes&windows.FILE_ATTRIBUTE_DIRECTORY != 0
	isReparsePoint := attributes&windows.FILE_ATTRIBUTE_REPARSE_POINT != 0
	isReparseTagNameSurrogate := reparseTag&0x20000000 != 0
	return sysFileType{directory: isDirectory, symlink: isReparsePoint && isReparseTagNameSurrogate}
}

func (t sysFileType) isDir() bool         { return !t.symlink && t.directory }
func (t sysFileType) isFile() bool        { return !t.symlink && !t.directory }
func (t sysFileType) isSymlink() bool     { return t.symlink }
func (t sysFileType) isSymlinkDir() bool  { return t.symlink && t.directory }
func (t sysFileType) isSymlinkFile() bool { return t.symlink && !t.directory }

func newSysDirBuilder() sysDirBuilder {
	return sysDirBuilder{}
}

func (b *sysDirBuilder) mkdir(p string) error {
	native, err := maybeVerbatim(p)
	if err != nil {
		return err
	}
	return osError(windows.CreateDirectory(&native[0], nil))
}

func (b sysDirBuilder) fmtDebug(f *rs.Formatter) error {
	return f.WriteStr("DirBuilder")
}

func impReaddir(p string) (*sysReadDir, error) {
	// We push a `*` to the end of the path which cause the empty path to be
	// treated as the current directory. So, for consistency with other platforms,
	// we explicitly error on the empty path.
	if p == "" {
		// Return an error code consistent with other ways of opening files.
		// E.g. fs::metadata or File::open.
		return nil, rsio.ErrorFromRawOsError(errorPathNotFound)
	}
	root := p
	star := rspath.Join(p, "*")
	path, err := maybeVerbatim(star)
	if err != nil {
		return nil, err
	}

	var wfd win32FindData
	// this is like FindFirstFileW (see https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-findfirstfileexw),
	// but with FindExInfoBasic it should skip filling WIN32_FIND_DATAW.cAlternateFileName
	// (see https://learn.microsoft.com/en-us/windows/win32/api/minwinbase/ns-minwinbase-win32_find_dataw)
	// (which will be always null string value and currently unused) and should be faster.
	//
	// We can pass FIND_FIRST_EX_LARGE_FETCH to dwAdditionalFlags to speed up things more,
	// but as we don't know user's use profile of this function, lets be conservative.
	r, _, callErr := procFindFirstFileExW.Call(uintptr(unsafe.Pointer(&path[0])), findExInfoBasic,
		uintptr(unsafe.Pointer(&wfd)), findExSearchNameMatch, 0, 0)
	findHandle := windows.Handle(r)
	if findHandle != windows.InvalidHandle {
		first := wfd
		return &sysReadDir{handle: findHandle, root: root, first: &first}, nil
	}
	// The status `ERROR_FILE_NOT_FOUND` is returned by the `FindFirstFileExW` function
	// if no matching files can be found, but not necessarily that the path to find the
	// files in does not exist.
	//
	// Hence, a check for whether the path to search in exists is added when the last
	// os error returned by Windows is `ERROR_FILE_NOT_FOUND` to handle this scenario.
	// If that is the case, an empty `ReadDir` iterator is returned as it returns `None`
	// in the initial `.next()` invocation because `ERROR_NO_MORE_FILES` would have been
	// returned by the `FindNextFileW` function.
	//
	// See issue #120040: https://github.com/rust-lang/rust/issues/120040.
	lastError := lastErrorOf(callErr)
	if lastError == errorFileNotFound {
		return &sysReadDir{handle: 0, root: root}, nil
	}

	// Just return the error constructed from the raw OS error if the above is not the case.
	//
	// Note: `ERROR_PATH_NOT_FOUND` would have been returned by the `FindFirstFileExW` function
	// when the path to search in does not exist in the first place.
	return nil, rsio.ErrorFromRawOsError(int32(lastError))
}

func impUnlink(path nativePath) error {
	if err := windows.DeleteFile(&path[0]); err != nil {
		code := winErrorOf(err)
		// if `DeleteFileW` fails with ERROR_ACCESS_DENIED then try to remove
		// the file while ignoring the readonly attribute.
		// This is accomplished by calling the `posix_delete` function on an open file handle.
		if code == errorAccessDenied {
			opts := newSysOpenOptions()
			opts.setAccessMode(accessDelete)
			opts.setCustomFlags(windows.FILE_FLAG_OPEN_REPARSE_POINT)
			if f, err := sysFileOpenNative(path, &opts); err == nil {
				deleted := f.posixDelete() == 0
				f.drop()
				if deleted {
					return nil
				}
			}
		}
		// return the original error if any of the above fails.
		return ioResult(code)
	}
	return nil
}

func impRename(old, new nativePath) error {
	err := windows.MoveFileEx(&old[0], &new[0], windows.MOVEFILE_REPLACE_EXISTING)
	if err == nil {
		return nil
	}
	code := winErrorOf(err)
	// if `MoveFileExW` fails with ERROR_ACCESS_DENIED then try to move
	// the file while ignoring the readonly attribute.
	// This is accomplished by calling `SetFileInformationByHandle` with `FileRenameInfoEx`.
	if code != errorAccessDenied {
		return ioResult(code)
	}
	opts := newSysOpenOptions()
	opts.setAccessMode(accessDelete)
	opts.setCustomFlags(windows.FILE_FLAG_OPEN_REPARSE_POINT | windows.FILE_FLAG_BACKUP_SEMANTICS)
	f, openErr := sysFileOpenNative(old, &opts)
	if openErr != nil {
		return ioResult(code)
	}
	defer f.drop()

	// Calculate the layout of the `FILE_RENAME_INFO` we pass to `SetFileInformation`
	// This is a dynamically sized struct so we need to get the position of the last field to calculate the actual size.
	newLenWithoutNulInBytes := uint32((len(new) - 1) * 2)
	const offset = 20 // offset_of!(c::FILE_RENAME_INFO, FileName)
	structSize := offset + newLenWithoutNulInBytes + 2
	info := make([]uint64, (structSize+7)/8)
	bytes := unsafe.Slice((*byte)(unsafe.Pointer(&info[0])), len(info)*8)
	*(*uint32)(unsafe.Pointer(&bytes[0])) = fileRenameFlagReplaceIfExists | fileRenameFlagPosixSemantics
	// RootDirectory (at 8) stays null.
	// Don't include the NULL in the size
	*(*uint32)(unsafe.Pointer(&bytes[16])) = newLenWithoutNulInBytes
	for i, unit := range new {
		*(*uint16)(unsafe.Pointer(&bytes[offset+i*2])) = unit
	}

	if result := setFileInformationByHandle(f.rawHandle(), windows.FileRenameInfoEx, unsafe.Pointer(&info[0]),
		structSize); result != 0 {
		if result == errorDirNotEmpty {
			return ioResult(errorDirNotEmpty)
		}
		return ioResult(code)
	}
	return nil
}

func impRmdir(p nativePath) error {
	return osError(windows.RemoveDirectory(&p[0]))
}

func impRemoveDirAll(path nativePath) error {
	// Open a file or directory without following symlinks.
	opts := newSysOpenOptions()
	opts.setAccessMode(fileListDirectory)
	// `FILE_FLAG_BACKUP_SEMANTICS` allows opening directories.
	// `FILE_FLAG_OPEN_REPARSE_POINT` opens a link instead of its target.
	opts.setCustomFlags(windows.FILE_FLAG_BACKUP_SEMANTICS | windows.FILE_FLAG_OPEN_REPARSE_POINT)
	file, err := sysFileOpenNative(path, &opts)
	if err != nil {
		return err
	}

	// Test if the file is not a directory or a symlink to a directory.
	info, err := file.basicInfo()
	if err != nil {
		file.drop()
		return err
	}
	if info.FileAttributes&windows.FILE_ATTRIBUTE_DIRECTORY == 0 {
		file.drop()
		return rsio.ErrorFromRawOsError(errorDirectory)
	}

	// Remove the directory and all its contents.
	if code := removeDirAllIterative(file); code != 0 {
		return ioResult(code)
	}
	return nil
}

func impReadlink(path nativePath) (string, error) {
	// Open the link with no access mode, instead of generic read.
	// By default FILE_LIST_DIRECTORY is denied for the junction "C:\Documents and Settings", so
	// this is needed for a common case.
	opts := newSysOpenOptions()
	opts.setAccessMode(0)
	opts.setCustomFlags(windows.FILE_FLAG_OPEN_REPARSE_POINT | windows.FILE_FLAG_BACKUP_SEMANTICS)
	file, err := sysFileOpenNative(path, &opts)
	if err != nil {
		return "", err
	}
	defer file.drop()
	return file.readlink()
}

func impSymlink(original, link string) error {
	return symlinkInner(original, link, false)
}

// symlinkInner is symlink_inner.
func symlinkInner(original, link string, dir bool) error {
	originalWide, err := toU16s(original)
	if err != nil {
		return err
	}
	linkWide, err := maybeVerbatim(link)
	if err != nil {
		return err
	}
	flags := uint32(0)
	if dir {
		flags = symbolicLinkFlagDirectory
	}
	// Formerly, symlink creation required the SeCreateSymbolicLink privilege. For the Windows 10
	// Creators Update, Microsoft loosened this to allow unprivileged symlink creation if the
	// computer is in Developer Mode, but SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE must be
	// added to dwFlags to opt into this behavior.
	err = windows.CreateSymbolicLink(&linkWide[0], &originalWide[0], flags|symbolicLinkFlagAllowUnprivilegedCreate)
	if err != nil {
		if winErrorOf(err) == errorInvalidParameter {
			// Older Windows objects to SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE,
			// so if we encounter ERROR_INVALID_PARAMETER, retry without that flag.
			return osError(windows.CreateSymbolicLink(&linkWide[0], &originalWide[0], flags))
		}
		return osError(err)
	}
	return nil
}

func impLink(original, link nativePath) error {
	return osError(windows.CreateHardLink(&link[0], &original[0], 0))
}

func impStat(path nativePath) (sysFileAttr, error) {
	attr, err := windowsMetadata(path, reparsePointFollow)
	if err != nil {
		if code, ok := rsio.From(err).RawOsError().Get(); ok && code == errorCantAccessFile {
			if attrs, lerr := impLstat(path); lerr == nil {
				if !attrs.fileType().isSymlink() {
					return attrs, nil
				}
			}
		}
		return sysFileAttr{}, err
	}
	return attr, nil
}

func impLstat(path nativePath) (sysFileAttr, error) {
	return windowsMetadata(path, reparsePointOpen)
}

// The values of ReparsePoint.
const (
	reparsePointFollow = 0
	reparsePointOpen   = windows.FILE_FLAG_OPEN_REPARSE_POINT
)

// windowsMetadata is sys::fs::metadata.
func windowsMetadata(path nativePath, reparse uint32) (sysFileAttr, error) {
	opts := newSysOpenOptions()
	// No read or write permissions are necessary
	opts.setAccessMode(0)
	opts.setCustomFlags(windows.FILE_FLAG_BACKUP_SEMANTICS | reparse)

	// Attempt to open the file normally.
	// If that fails with `ERROR_SHARING_VIOLATION` then retry using `FindFirstFileExW`.
	// If the fallback fails for any reason we return the original error.
	file, err := sysFileOpenNative(path, &opts)
	if err == nil {
		defer file.drop()
		return file.fileAttr()
	}
	code, ok := rsio.From(err).RawOsError().Get()
	if !ok || (code != errorSharingViolation && code != errorAccessDenied) {
		return sysFileAttr{}, err
	}
	// `ERROR_ACCESS_DENIED` is returned when the user doesn't have permission for the resource.
	// One such example is `System Volume Information` as default but can be created as well
	// `ERROR_SHARING_VIOLATION` will almost never be returned.
	// Usually if a file is locked you can still read some metadata.
	// However, there are special system files, such as
	// `C:\hiberfil.sys`, that are locked in a way that denies even that.
	//
	// `FindFirstFileExW` accepts wildcard file names.
	// Fortunately wildcards are not valid file names and
	// `ERROR_SHARING_VIOLATION` means the file exists (but is locked)
	// therefore it's safe to assume the file name given does not
	// include wildcards.
	var wfd win32FindData
	r, _, _ := procFindFirstFileExW.Call(uintptr(unsafe.Pointer(&path[0])), findExInfoBasic,
		uintptr(unsafe.Pointer(&wfd)), findExSearchNameMatch, 0, 0)
	handle := windows.Handle(r)
	if handle == windows.InvalidHandle {
		// This can fail if the user does not have read access to the
		// directory.
		return sysFileAttr{}, err
	}
	// We no longer need the find handle.
	_ = windows.FindClose(handle)

	// `FindFirstFileExW` reads the cached file information from the
	// directory. The downside is that this metadata may be outdated.
	attrs := fileAttrFromFindData(&wfd)
	if reparse == reparsePointFollow && attrs.fileType().isSymlink() {
		return sysFileAttr{}, err
	}
	return attrs, nil
}

func impSetPerm(p nativePath, perm sysFilePermissions) error {
	return osError(windows.SetFileAttributes(&p[0], perm.attrs))
}

// getPath is get_path: the final path of an open handle.
func getPath(handle windows.Handle) (string, error) {
	wide, err := fillUtf16Buf(func(buffer *uint16, size uint32) (uint32, uint32) {
		r, _, err := procGetFinalPathNameByHandleW.Call(uintptr(handle), uintptr(unsafe.Pointer(buffer)),
			uintptr(size), volumeNameDos)
		return uint32(r), lastErrorOf(err)
	})
	if err != nil {
		return "", err
	}
	return rspath.OsStringFromWide(wide), nil
}

func impCanonicalize(p nativePath) (string, error) {
	opts := newSysOpenOptions()
	// No read or write permissions are necessary
	opts.setAccessMode(0)
	// This flag is so we can open directories too
	opts.setCustomFlags(windows.FILE_FLAG_BACKUP_SEMANTICS)
	f, err := sysFileOpenNative(p, &opts)
	if err != nil {
		return "", err
	}
	defer f.drop()
	return getPath(f.rawHandle())
}

// copyProgressRoutine is the LPPROGRESS_ROUTINE of copy. lpData is the key
// of the size this copy records, rather than a Go pointer.
func copyProgressRoutine(totalFileSize, totalBytesTransferred, streamSize, streamBytesTransferred,
	streamNumber, callbackReason, sourceFile, destinationFile, data uintptr) uintptr {
	if uint32(streamNumber) == 1 {
		copySizes.Lock()
		copySizes.sizes[data] = int64(streamBytesTransferred)
		copySizes.Unlock()
	}
	return progressContinue
}

func impCopy(from, to nativePath) (uint64, error) {
	copyProgressOnce.Do(func() { copyProgress = windows.NewCallback(copyProgressRoutine) })
	copySizes.Lock()
	copySizes.next++
	key := copySizes.next
	copySizes.sizes[key] = 0
	copySizes.Unlock()
	defer func() {
		copySizes.Lock()
		delete(copySizes.sizes, key)
		copySizes.Unlock()
	}()

	ok, _, err := procCopyFileExW.Call(uintptr(unsafe.Pointer(&from[0])), uintptr(unsafe.Pointer(&to[0])),
		copyProgress, key, 0, 0)
	if ok == 0 {
		return 0, ioResult(lastErrorOf(err))
	}
	copySizes.Lock()
	size := copySizes.sizes[key]
	copySizes.Unlock()
	return uint64(size), nil
}

// impExists is sys::fs::exists: like `exists`, but reporting I/O errors.
func impExists(path nativePath) (bool, error) {
	// Open the file to ensure any symlinks are followed to their target.
	opts := newSysOpenOptions()
	// No read, write, etc access rights are needed.
	opts.setAccessMode(0)
	// Backup semantics enables opening directories as well as files.
	opts.setCustomFlags(windows.FILE_FLAG_BACKUP_SEMANTICS)
	f, err := sysFileOpenNative(path, &opts)
	if err == nil {
		// The file was opened successfully therefore it must exist,
		f.drop()
		return true, nil
	}
	e := rsio.From(err)
	code, hasCode := e.RawOsError().Get()
	switch {
	// The file definitely does not exist
	case e.Kind() == rsio.ErrorKind_NotFound:
		return false, nil
	// `ERROR_SHARING_VIOLATION` means that the file has been locked by
	// another process. This is often temporary so we simply report it
	// as the file existing.
	case hasCode && code == errorSharingViolation:
		return true, nil
	// `ERROR_CANT_ACCESS_FILE` means that a file exists but that the
	// reparse point could not be handled by `CreateFile`.
	// This can happen for special files such as:
	// * Unix domain sockets which you need to `connect` to
	// * App exec links which require using `CreateProcess`
	case hasCode && code == errorCantAccessFile:
		return true, nil
	// Other errors such as `ERROR_ACCESS_DENIED` may indicate that the
	// file exists. However, these types of errors are usually more
	// permanent so we report them here.
	default:
		return false, err
	}
}
