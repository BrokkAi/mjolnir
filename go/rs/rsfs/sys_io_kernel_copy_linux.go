// Port of std 1.99.1 src/sys/io/kernel_copy/linux.rs (files to files)

//go:build linux

package rsfs

import (
	"io"
	"math"
	"runtime"
	"sync/atomic"

	"golang.org/x/sys/unix"

	"github.com/BrokkAi/mjolnir/go/rs/rsio"
)

// Rust's io::copy moves bytes between two files inside the kernel: with
// copy_file_range between regular files, else with sendfile from a regular
// file, else with splice through a pipe, else by reading and writing. This
// port does the first, which is what fs::copy uses, and goes straight to
// reading and writing where Rust would try sendfile or splice; the bytes
// that arrive are the same.

// SpecCopy is `impl io::SpecCopy for File` (rsio.SpecCopy): io::copy from
// this file to another file in the kernel. It reports how much it copied
// and whether that is all, and leaves the rest to rsio.Copy.
func (f *File) SpecCopy(writer io.Writer) (uint64, bool, error) {
	other, ok := writer.(*File)
	if !ok {
		return 0, false, nil
	}
	readerMetadata, readerErr := f.Metadata()
	writerMetadata, writerErr := other.Metadata()
	if readerErr != nil || writerErr != nil {
		// FdMeta::NoneObtained on either side: no kernel copy.
		return 0, false, nil
	}
	return copyFiles(f, readerMetadata, other, writerMetadata)
}

// copyCachedFileMetadata is io::copy between two CachedFileMetadata, as
// fs::copy makes it: the kernel copy, then the generic one for the rest.
func copyCachedFileMetadata(reader *File, readerMetadata Metadata, writer *File, writerMetadata Metadata) (uint64, error) {
	written, ended, err := copyFiles(reader, readerMetadata, writer, writerMetadata)
	if err != nil {
		return 0, err
	}
	if ended {
		return written, nil
	}
	rest, err := rsio.Copy(plainReader{reader}, writer)
	if err != nil {
		return 0, err
	}
	return written + rest, nil
}

// plainReader is a File read without its kernel copy.
type plainReader struct{ file *File }

func (r plainReader) Read(buf []byte) (int, error)              { return r.file.Read(buf) }
func (r plainReader) ReadBuf(cursor *rsio.BorrowedCursor) error { return r.file.ReadBuf(cursor) }

// copyFiles is Copier::copy for a file read into a file: copy_file_range
// when both are candidates.
func copyFiles(reader *File, readerMetadata Metadata, writer *File, writerMetadata Metadata) (uint64, bool, error) {
	// before direct operations on file descriptors ensure that all source and sink buffers are empty
	// (a File buffers nothing)
	written := uint64(0)
	maxWrite := uint64(math.MaxUint64)

	// copy_file_range will fail on empty procfs files. `read` can determine whether EOF has been reached
	// without extra cost and skip the write, thus there is no benefit in attempting copy_file_range
	if readerMetadata.IsFile() && readerMetadata.Len() > 0 && writerMetadata.IsFile() {
		defer runtime.KeepAlive(reader)
		defer runtime.KeepAlive(writer)
		bytes, err, state := copyRegularFiles(reader.inner.rawFd(), writer.inner.rawFd(), maxWrite)
		switch state {
		case copyEnded:
			return bytes + written, true, nil
		case copyError:
			return 0, false, err
		case copyFallback:
			written += bytes
		}
	}

	return written, false, nil
}

// The outcomes of a CopyResult.
const (
	copyEnded = iota
	copyError
	copyFallback
)

// The states of HAS_COPY_FILE_RANGE.
const (
	notProbed uint32 = iota
	unavailable
	available
)

// hasCopyFileRange is HAS_COPY_FILE_RANGE: Kernel prior to 4.5 don't have
// copy_file_range. We store the availability in a global to avoid
// unnecessary syscalls.
var hasCopyFileRange atomic.Uint32

// invalidFd is INVALID_FD.
const invalidFd = -1

func probeCopyFileRangeSupport() uint32 {
	// In some cases, we cannot determine availability from the first
	// `copy_file_range` call. In this case, we probe with an invalid file
	// descriptor so that the results are easily interpretable.
	_, err := unix.CopyFileRange(invalidFd, nil, invalidFd, nil, 1, 0)
	switch err {
	case unix.EPERM, unix.ENOSYS:
		return unavailable
	case unix.EBADF:
		return available
	case nil:
		panic("unexpected copy_file_range probe success")
	default:
		// Treat other errors as the syscall
		// being unavailable.
		return unavailable
	}
}

// copyRegularFiles is copy_regular_files.
func copyRegularFiles(reader, writer int, maxLen uint64) (uint64, error, int) {
	haveProbed := false
	switch hasCopyFileRange.Load() {
	case notProbed:
		haveProbed = false
	case unavailable:
		return 0, nil, copyFallback
	default:
		haveProbed = true
	}

	written := uint64(0)
	for written < maxLen {
		bytesToCopy := min(maxLen-written, uint64(math.MaxInt))
		// cap to 1GB chunks in case u64::MAX is passed as max_len and the file has a non-zero seek position
		// this allows us to copy large chunks without hitting EOVERFLOW,
		// unless someone sets a file offset close to u64::MAX - 1GB, in which case a fallback would be required
		bytesToCopy = min(bytesToCopy, 0x4000_0000)
		// We actually don't have to adjust the offsets,
		// because copy_file_range adjusts the file offset automatically
		ret, err := unix.CopyFileRange(reader, nil, writer, nil, int(bytesToCopy), 0)

		if !haveProbed && err == nil {
			haveProbed = true
			hasCopyFileRange.Store(available)
		}

		switch {
		case err == nil && ret == 0 && written == 0:
			// fallback to work around several kernel bugs where copy_file_range will fail to
			// copy any bytes and return 0 instead of an error if
			// - reading virtual files from the proc filesystem which appear to have 0 size
			//   but are not empty. noted in coreutils to affect kernels at least up to 5.6.19.
			// - copying from an overlay filesystem in docker. reported to occur on fedora 32.
			return 0, nil, copyFallback
		case err == nil && ret == 0:
			return written, nil, copyEnded // reached EOF
		case err == nil:
			written += uint64(ret)
		default:
			switch err {
			// when file offset + max_length > u64::MAX
			case unix.EOVERFLOW:
				return written, nil, copyFallback
			case unix.ENOSYS, unix.EXDEV, unix.EINVAL, unix.EPERM, unix.EOPNOTSUPP, unix.EBADF:
				if written == 0 {
					if !haveProbed {
						state := available
						if err == unix.ENOSYS || err == unix.EOPNOTSUPP || err == unix.EPERM {
							// EPERM can indicate seccomp filters or an
							// immutable file. To distinguish these
							// cases we probe with invalid file
							// descriptors which should result in EBADF
							// if the syscall is supported and EPERM or
							// ENOSYS if it's not available.
							//
							// For EOPNOTSUPP, see below. In the case of
							// ENOSYS, we try to cover for faulty FUSE
							// drivers.
							state = probeCopyFileRangeSupport()
						}
						hasCopyFileRange.Store(state)
					}

					// Try fallback io::copy if either:
					// - Kernel version is < 4.5 (ENOSYS¹)
					// - Files are mounted on different fs (EXDEV)
					// - copy_file_range is broken in various ways on RHEL/CentOS 7 (EOPNOTSUPP)
					// - copy_file_range file is immutable or syscall is blocked by seccomp¹ (EPERM)
					// - copy_file_range cannot be used with pipes or device nodes (EINVAL)
					// - the writer fd was opened with O_APPEND (EBADF²)
					// and no bytes were written successfully yet. (All these errnos should
					// not be returned if something was already written, but they happen in
					// the wild, see #91152.)
					//
					// ¹ these cases should be detected by the initial probe but we handle them here
					//   anyway in case syscall interception changes during runtime
					// ² actually invalid file descriptors would cause this too, but in that case
					//   the fallback code path is expected to encounter the same error again
					return 0, nil, copyFallback
				}
				return written, osError(err), copyError
			default:
				return written, osError(err), copyError
			}
		}
	}
	return written, nil, copyEnded
}
