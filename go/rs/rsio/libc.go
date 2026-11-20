package rsio

// Libc names a C library a Rust program on Linux is linked against.
type Libc uint8

const (
	// Libc_Glibc is the GNU C library, which `mj` is linked against.
	Libc_Glibc Libc = iota
	// Libc_Musl is musl, which the static `mj-worker` is linked against.
	Libc_Musl
)

// LinuxLibc says which C library's behaviour this program reproduces on
// Linux. Rust takes the text of an operating system error from the C
// library's strerror_r, and glibc and musl word many errors differently
// ("Input/output error" and "I/O error"). The two also differ in what
// std::fs can do: only the glibc build uses statx, so only it knows a file's
// creation time, and each has its own realpath behind fs::canonicalize.
//
// The default is glibc. The worker program sets this to Libc_Musl first
// thing in main, before any other goroutine runs; it is not synchronized.
// It has no effect on other operating systems.
var LinuxLibc = Libc_Glibc
