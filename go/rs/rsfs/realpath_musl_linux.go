// Port of musl 1.2.5 src/misc/realpath.c, with the parts of
// src/unistd/readlink.c and src/unistd/getcwd.c it depends on

//go:build linux

package rsfs

import (
	"bytes"

	"golang.org/x/sys/unix"
)

// Rust's fs::canonicalize is the C library's realpath, and the static
// mj-worker is linked with musl, whose realpath is its own: it resolves the
// path component by component with readlink, in a fixed PATH_MAX buffer.

const (
	muslPathMax    = 4096 // PATH_MAX
	muslSymloopMax = 40   // SYMLOOP_MAX
)

func muslSlashLen(s []byte) int {
	n := 0
	for n < len(s) && s[n] == '/' {
		n++
	}
	return n
}

// muslReadlink is musl's readlink, which reads into a one byte buffer when
// it is given none, and then reports no bytes.
func muslReadlink(path []byte, buf []byte) (int, error) {
	if len(buf) == 0 {
		var dummy [1]byte
		r, err := unix.Readlink(string(path), dummy[:])
		if err != nil {
			return -1, err
		}
		if r > 0 {
			r = 0
		}
		return r, nil
	}
	r, err := unix.Readlink(string(path), buf)
	if err != nil {
		return -1, err
	}
	return r, nil
}

// muslGetcwd is musl's getcwd into a caller's buffer.
func muslGetcwd(buf []byte) (int, error) {
	ret, err := unix.Getcwd(buf)
	if err != nil {
		return 0, err
	}
	if ret == 0 || buf[0] != '/' {
		return 0, unix.ENOENT
	}
	return bytes.IndexByte(buf, 0), nil
}

// muslRealpath is musl's realpath(filename, NULL). Its error is an
// unix.Errno, which is what errno would hold.
func muslRealpath(filename string) (string, error) {
	var stack [muslPathMax + 1]byte
	var output [muslPathMax]byte
	var p, q, l, l0, cnt, nup int
	checkDir := byte(0)

	l = min(len(filename), len(stack))
	if l == 0 {
		return "", unix.ENOENT
	}
	if l >= muslPathMax {
		return "", unix.ENAMETOOLONG
	}
	p = len(stack) - l - 1
	q = 0
	copy(stack[p:], filename)
	stack[p+l] = 0

	// at reports the byte at i of a NUL-terminated buffer.
	strchrnul := func(from int) int {
		for i := from; ; i++ {
			if stack[i] == 0 || stack[i] == '/' {
				return i
			}
		}
	}

	/* Main loop. Each iteration pops the next part from stack of
	 * remaining path components and consumes any slashes that follow.
	 * If not a link, it's moved to output; if a link, contents are
	 * pushed to the stack. */
	advance := false // the for loop's increment; skipped after restart
	for {
		if advance {
			p += muslSlashLen(stack[p:])
		}
		advance = true

		/* If stack starts with /, the whole component is / or //
		 * and the output state must be reset. */
		if stack[p] == '/' {
			checkDir = 0
			nup = 0
			q = 0
			output[q] = '/'
			q++
			p++
			/* Initial // is special. */
			if stack[p] == '/' && stack[p+1] != '/' {
				output[q] = '/'
				q++
			}
			continue
		}

		z := strchrnul(p)
		l = z - p
		l0 = l

		if l == 0 && checkDir == 0 {
			break
		}

		/* Skip any . component but preserve check_dir status. */
		if l == 1 && stack[p] == '.' {
			p += l
			continue
		}

		/* Copy next component onto output at least temporarily, to
		 * call readlink, but wait to advance output position until
		 * determining it's not a link. */
		if q != 0 && output[q-1] != '/' {
			if p == 0 {
				return "", unix.ENAMETOOLONG
			}
			p--
			stack[p] = '/'
			l++
		}
		if q+l >= muslPathMax {
			return "", unix.ENAMETOOLONG
		}
		copy(output[q:], stack[p:p+l])
		output[q+l] = 0
		p += l

		up := false
		skipReadlink := false
		if l0 == 2 && stack[p-2] == '.' && stack[p-1] == '.' {
			up = true
			/* Any non-.. path components we could cancel start
			 * after nup repetitions of the 3-byte string "../";
			 * if there are none, accumulate .. components to
			 * later apply to cwd, if needed. */
			if q <= 3*nup {
				nup++
				q += l
				continue
			}
			/* When previous components are already known to be
			 * directories, processing .. can skip readlink. */
			if checkDir == 0 {
				skipReadlink = true
			}
		}
		k := -1
		var err error
		if !skipReadlink {
			k, err = muslReadlink(output[:q+l], stack[:p])
			if k == p {
				return "", unix.ENAMETOOLONG
			}
			if k == 0 {
				return "", unix.ENOENT
			}
		}
		if k < 0 {
			if !skipReadlink && err != unix.EINVAL {
				return "", err
			}
			checkDir = 0
			if up {
				for q != 0 && output[q-1] != '/' {
					q--
				}
				if q > 1 && (q > 2 || output[0] != '/') {
					q--
				}
				continue
			}
			if l0 != 0 {
				q += l
			}
			checkDir = stack[p]
			continue
		}
		cnt++
		if cnt == muslSymloopMax {
			return "", unix.ELOOP
		}

		/* If link contents end in /, strip any slashes already on
		 * stack to avoid /->// or //->/// or spurious toolong. */
		if stack[k-1] == '/' {
			for stack[p] == '/' {
				p++
			}
		}
		p -= k
		copy(stack[p:p+k], stack[:k])

		/* Skip the stack advancement in case we have a new
		 * absolute base path. */
		advance = false
	}

	output[q] = 0

	if output[0] != '/' {
		n, err := muslGetcwd(stack[:])
		if err != nil {
			return "", err
		}
		l = n
		/* Cancel any initial .. components. */
		p = 0
		for ; nup > 0; nup-- {
			for l > 1 && stack[l-1] != '/' {
				l--
			}
			if l > 1 {
				l--
			}
			p += 2
			if p < q {
				p++
			}
		}
		if q-p != 0 && stack[l-1] != '/' {
			stack[l] = '/'
			l++
		}
		if l+(q-p)+1 >= muslPathMax {
			return "", unix.ENAMETOOLONG
		}
		copy(output[l:], output[p:q+1])
		copy(output[:l], stack[:l])
		q = l + q - p
	}

	return string(output[:q]), nil
}
