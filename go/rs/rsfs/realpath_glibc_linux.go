// Port of glibc 2.44 stdlib/canonicalize.c (realpath_stk, as realpath calls
// it with a NULL buffer), with the parts of
// sysdeps/unix/sysv/linux/getcwd.c it depends on

//go:build linux

package rsfs

import (
	"bytes"

	"golang.org/x/sys/unix"
)

// Rust's fs::canonicalize is the C library's realpath; `mj` is linked with
// glibc, whose realpath resolves the path component by component with
// readlink in buffers that grow as needed.

// glibcMinEloopThreshold is MIN_ELOOP_THRESHOLD.
const glibcMinEloopThreshold = 40

// glibcGetcwd is glibc's getcwd into a buffer that grows (as realpath_stk
// grows it on ERANGE). A result that is not absolute, which Linux gives for
// a directory that cannot be reached from the root, fails with ENOENT.
func glibcGetcwd() ([]byte, error) {
	size := 1024
	for {
		buf := make([]byte, size)
		n, err := unix.Getcwd(buf)
		if err == unix.ERANGE {
			size *= 2
			continue
		}
		if err != nil {
			return nil, err
		}
		if n <= 0 || buf[0] != '/' {
			return nil, unix.ENOENT
		}
		return buf[:bytes.IndexByte(buf, 0)], nil
	}
}

// glibcReadlink is __readlink into a scratch buffer that grows until the
// link fits with room to spare.
func glibcReadlink(path []byte) ([]byte, error) {
	size := 1024
	for {
		buf := make([]byte, size-1)
		n, err := unix.Readlink(string(path), buf)
		if err != nil {
			return nil, err
		}
		if n < size-1 {
			return buf[:n], nil
		}
		size *= 2
	}
}

// fileAccessible is file_accessible.
func fileAccessible(file []byte) error {
	return unix.Faccessat(unix.AT_FDCWD, string(file), unix.F_OK, unix.AT_EACCESS)
}

// suffixRequiresDirCheck is suffix_requires_dir_check: whether end, the rest
// of the name after a component, needs the component to be a searchable
// directory.
func suffixRequiresDirCheck(end []byte) bool {
	at := func(i int) byte {
		if i < len(end) {
			return end[i]
		}
		return 0
	}
	i := 0
	/* If END does not start with a slash, the suffix is OK.  */
	for at(i) == '/' {
		/* Two or more slashes act like a single slash.  */
		for {
			i++
			if at(i) != '/' {
				break
			}
		}

		c := at(i)
		i++
		switch c {
		default:
			return false /* An ordinary file name component is OK.  */
		case 0:
			return true /* Trailing "/" is trouble.  */
		case '.':
			/* Possibly "." or "..".  */
		}
		/* Trailing "/.", or "/.." even if not trailing, is trouble.  */
		if at(i) == 0 || (at(i) == '.' && (at(i+1) == 0 || at(i+1) == '/')) {
			return true
		}
	}

	return false
}

// glibcRealpath is glibc's realpath(name, NULL). Its error is an
// unix.Errno, which is what errno would hold.
func glibcRealpath(nameText string) (string, error) {
	if nameText == "" {
		/* As per Single Unix Specification V2 we must return an error if
		   the name argument points to an empty string.  */
		return "", unix.ENOENT
	}

	name := []byte(nameText)
	var rname []byte
	numLinks := 0

	/* This is always zero for Posix hosts, but can be 2 for MS-Windows
	   and MS-DOS X:/foo/bar file names.  */
	prefixLen := 0

	if name[0] != '/' {
		cwd, err := glibcGetcwd()
		if err != nil {
			return "", err
		}
		rname = append(rname, cwd...)
	} else {
		rname = append(rname, '/')
	}
	dest := len(rname)

	start := 0
	end := 0
	for ; start < len(name); start = end {
		/* Skip sequence of multiple file name separators.  */
		for start < len(name) && name[start] == '/' {
			start++
		}

		/* Find end of component.  */
		end = start
		for end < len(name) && name[end] != '/' {
			end++
		}

		/* Length of this file name component; it can be zero if a file
		   name ends in '/'.  */
		startlen := end - start

		if startlen == 0 {
			break
		} else if startlen == 1 && name[start] == '.' {
			/* nothing */
		} else if startlen == 2 && name[start] == '.' && name[start+1] == '.' {
			/* Back up to previous component, ignore if at root already.  */
			if dest > prefixLen+1 {
				dest--
				for dest > 0 && rname[dest-1] != '/' {
					dest--
				}
			}
		} else {
			rname = rname[:dest]
			if rname[dest-1] != '/' {
				rname = append(rname, '/')
				dest++
			}

			rname = append(rname, name[start:end]...)
			dest += startlen

			buf, err := glibcReadlink(rname[:dest])
			if err == nil {
				numLinks++
				if numLinks > glibcMinEloopThreshold {
					return "", unix.ELOOP
				}

				/* Careful here, end may be a pointer into extra_buf... */
				extra := make([]byte, 0, len(buf)+len(name)-end)
				extra = append(extra, buf...)
				extra = append(extra, name[end:]...)
				name = extra
				end = 0

				if len(buf) > 0 && buf[0] == '/' {
					dest = 0
					rname = append(rname[:0], '/') /* It's an absolute symlink */
					dest++
					/* Install the new prefix to be in effect hereafter.  */
					prefixLen = 0
				} else {
					/* Back up to previous component, ignore if at root
					   already: */
					if dest > prefixLen+1 {
						dest--
						for dest > 0 && rname[dest-1] != '/' {
							dest--
						}
					}
				}
			} else if suffixRequiresDirCheck(name[end:]) {
				// dir_check: is the name so far, with "/" added, a
				// searchable directory?
				check := append(append([]byte(nil), rname[:dest]...), '/')
				if err := fileAccessible(check); err != nil {
					return "", err
				}
			} else if err != unix.EINVAL {
				return "", err
			}
		}
	}
	if dest > prefixLen+1 && rname[dest-1] == '/' {
		dest--
	}

	return string(rname[:dest]), nil
}
