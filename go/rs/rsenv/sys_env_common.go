// Port of std 1.99.1 src/sys/env/common.rs, with the parsing of an
// environment entry from src/sys/env/unix.rs (env) and src/sys/env/windows.rs
// (EnvIterator::next)

package rsenv

import (
	"strings"
	"syscall"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rspath"
)

// sysEnv is sys::env::Env: the environment variables of the process as they
// were when it was made, taken from the front.
//
// On Windows Rust walks the block GetEnvironmentStringsW copied as it goes;
// the block is a copy, so reading it all at once gives the same pairs.
type sysEnv struct {
	iter []rs.Tuple2[string, string]
}

func sysEnvNew(env []rs.Tuple2[string, string]) *sysEnv {
	return &sysEnv{iter: env}
}

// FmtDebug is `impl Debug for Env`: the pairs still to come, as a list.
func (e *sysEnv) FmtDebug(f *rs.Formatter) error {
	entries := make([]rs.Tuple2[rspath.PathDebug, rspath.PathDebug], 0, len(e.iter))
	for _, pair := range e.iter {
		entries = append(entries, rs.T2(rspath.Debug(pair.F0), rspath.Debug(pair.F1)))
	}
	return f.DebugList().Entries(entries).Finish()
}

func (e *sysEnv) next() rs.Option[rs.Tuple2[string, string]] {
	if len(e.iter) == 0 {
		return rs.None[rs.Tuple2[string, string]]()
	}
	pair := e.iter[0]
	e.iter = e.iter[1:]
	return rs.Some(pair)
}

// sysEnvSnapshot is sys::env::env: the (variable, value) pairs for all the
// environment variables of the current process.
//
// A Go program without C has one environment, the Go runtime's, which
// syscall.Environ lists (on Windows it reads GetEnvironmentStringsW, as Rust
// does). The Go runtime keeps only the first of two entries with the same
// name, where Rust's Unix code would list both.
func sysEnvSnapshot() *sysEnv {
	var result []rs.Tuple2[string, string]
	for _, entry := range syscall.Environ() {
		if keyValue, ok := sysEnvParse(entry); ok {
			result = append(result, keyValue)
		}
	}
	return sysEnvNew(result)
}

// sysEnvParse splits one entry of the environment into its name and value.
func sysEnvParse(input string) (rs.Tuple2[string, string], bool) {
	// Strategy (copied from glibc): Variable name and value are separated
	// by an ASCII equals sign '='. Since a variable name must not be
	// empty, allow variable names starting with an equals sign. Skip all
	// malformed lines.
	//
	// (Windows allows environment variables to start with an equals symbol
	// too, and Rust splits its entries the same way.)
	if len(input) == 0 {
		return rs.Tuple2[string, string]{}, false
	}
	pos := strings.IndexByte(input[1:], '=')
	if pos < 0 {
		return rs.Tuple2[string, string]{}, false
	}
	pos++
	return rs.T2(input[:pos], input[pos+1:]), true
}
