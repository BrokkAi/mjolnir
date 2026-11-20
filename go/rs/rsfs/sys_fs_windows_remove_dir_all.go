// Port of std 1.99.1 src/sys/fs/windows/remove_dir_all.rs

//go:build windows

package rsfs

import (
	"sync/atomic"
	"unsafe"

	"golang.org/x/sys/windows"
)

// The maximum number of times to spin when waiting for deletes to complete.
const maxRetries = 50

// ntOpenFile opens a file relative to a directory: the file, or the
// WinError.
func ntOpenFile(access uint32, objectAttribute *windows.OBJECT_ATTRIBUTES, share uint32, options uint32) (*sysFile, uint32) {
	var handle windows.Handle
	ioStatus := windows.IO_STATUS_BLOCK{Status: statusPending}
	r, _, _ := procNtOpenFile.Call(uintptr(unsafe.Pointer(&handle)), uintptr(access),
		uintptr(unsafe.Pointer(objectAttribute)), uintptr(unsafe.Pointer(&ioStatus)), uintptr(share), uintptr(options))
	status := uint32(r)
	if ntSuccess(status) {
		return newSysFile(handle, ""), 0
	}
	// Convert an NTSTATUS to the more familiar Win32 error code (aka "DosError")
	if status == statusDeletePending {
		// We make a special exception for `STATUS_DELETE_PENDING` because
		// otherwise this will be mapped to `ERROR_ACCESS_DENIED` which is
		// very unhelpful because that can also mean a permission error.
		return nil, errorDeletePending
	}
	return nil, rtlNtStatusToDosError(status)
}

// openLinkAttributes is ATTRIBUTES: the `OBJ_DONT_REPARSE` attribute ensures
// that we haven't been tricked into following a symlink. However, it may not
// be available in earlier versions of Windows.
var openLinkAttributes = func() *atomic.Uint32 {
	value := new(atomic.Uint32)
	value.Store(windows.OBJ_DONT_REPARSE)
	return value
}()

// openLinkNoReparse is open_link_no_reparse: the file, nil when it is not
// there, or the WinError.
func openLinkNoReparse(parent *sysFile, path []uint16, access uint32, options uint32) (*sysFile, uint32) {
	// This is implemented using the lower level `NtOpenFile` function as
	// unfortunately opening a file relative to a parent is not supported by
	// win32 functions.
	//
	// See https://learn.microsoft.com/windows/win32/api/winternl/nf-winternl-ntopenfile
	name := windows.NTUnicodeString{Length: uint16(len(path) * 2), MaximumLength: uint16(len(path) * 2)}
	if len(path) != 0 {
		name.Buffer = &path[0]
	}
	object := windows.OBJECT_ATTRIBUTES{
		Length:        uint32(unsafe.Sizeof(windows.OBJECT_ATTRIBUTES{})),
		ObjectName:    &name,
		RootDirectory: parent.rawHandle(),
		Attributes:    openLinkAttributes.Load(),
	}
	share := uint32(windows.FILE_SHARE_DELETE | windows.FILE_SHARE_READ | windows.FILE_SHARE_WRITE)
	options = windows.FILE_OPEN_REPARSE_POINT | options
	file, code := ntOpenFile(access, &object, share, options)

	// Retry without OBJ_DONT_REPARSE if it's not supported.
	if code == errorInvalidParameter && openLinkAttributes.Load() == windows.OBJ_DONT_REPARSE {
		openLinkAttributes.Store(0)
		object.Attributes = 0
		file, code = ntOpenFile(access, &object, share, options)
	}

	// Ignore not found errors
	switch code {
	case 0:
		return file, 0
	case errorFileNotFound, errorPathNotFound, errorBadNetpath, errorBadNetName,
		// `DELETE_PENDING` means something else is already trying to delete it
		// so we assume that will eventually succeed.
		errorDeletePending:
		return nil, 0
	default:
		return nil, code
	}
}

func openDir(parent *sysFile, name []uint16) (*sysFile, uint32) {
	// Open the directory for synchronous directory listing.
	return openLinkNoReparse(parent, name, accessSynchronize|fileListDirectory,
		// "_IO_NONALERT" means that a synchronous call won't be interrupted.
		windows.FILE_SYNCHRONOUS_IO_NONALERT)
}

func deleteChild(parent *sysFile, name []uint16) uint32 {
	// Note that the `delete` function consumes the opened file to ensure it's
	// dropped immediately. See module comments for why this is important.
	file, code := openLinkNoReparse(parent, name, accessDelete, 0)
	if code != 0 {
		return code
	}
	if file == nil {
		return 0
	}
	return file.delete()
}

// retry is retry: f again, while it fails with ignore, up to MAX_RETRIES
// times, yielding the thread between tries.
func retry(f func() uint32, ignore uint32) uint32 {
	i := maxRetries
	for {
		i--
		if i == 0 {
			return f()
		}
		result := f()
		if result != ignore {
			return result
		}
		procSwitchToThread.Call()
	}
}

// removeDirAllIterative is remove_dir_all_iterative: 0, or the WinError.
// It consumes dir.
func removeDirAllIterative(dir *sysFile) uint32 {
	buffer := make([]uint64, dirBuffSize/8)
	dirlist := []*sysFile{dir}
	defer func() {
		for _, open := range dirlist {
			open.drop()
		}
	}()

	restart := true
outer:
	for len(dirlist) != 0 {
		dir := dirlist[len(dirlist)-1]
		dirlist = dirlist[:len(dirlist)-1]
		moreData, code := dir.fillDirBuff(buffer, restart)
		if code != 0 {
			dir.drop()
			return code
		}
		for _, entry := range dirBuffEntries(buffer) {
			name, isDirectory := entry[0].([]uint16), entry[1].(bool)
			if isDirectory {
				subdir, code := openDir(dir, name)
				if code != 0 {
					dir.drop()
					return code
				}
				if subdir == nil {
					continue
				}
				dirlist = append(dirlist, dir, subdir)
				continue outer
			}
			// Attempt to delete, retrying on sharing violation errors as these
			// can often be very temporary. E.g. if something takes just a
			// bit longer than expected to release a file handle.
			if code := retry(func() uint32 { return deleteChild(dir, name) }, errorSharingViolation); code != 0 {
				dir.drop()
				return code
			}
		}
		if moreData {
			dirlist = append(dirlist, dir)
			restart = false
		} else {
			// Attempt to delete, retrying on not empty errors because we may
			// need to wait some time for files to be removed from the filesystem.
			code := retry(func() uint32 { return deleteChild(dir, nil) }, errorDirNotEmpty)
			dir.drop()
			if code != 0 {
				return code
			}
			restart = true
		}
	}
	return 0
}
