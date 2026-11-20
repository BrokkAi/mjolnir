// Port of std 1.99.1 src/sys/args/common.rs

package rsenv

import (
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rspath"
)

// sysArgs is sys::args::Args: the arguments, taken from either end.
type sysArgs struct {
	iter []string
}

// FmtDebug is `impl Debug for Args`: the arguments still to come, as a list.
func (a *sysArgs) FmtDebug(f *rs.Formatter) error {
	entries := make([]rspath.PathDebug, 0, len(a.iter))
	for _, arg := range a.iter {
		entries = append(entries, rspath.Debug(arg))
	}
	return f.DebugList().Entries(entries).Finish()
}

func (a *sysArgs) next() rs.Option[string] {
	if len(a.iter) == 0 {
		return rs.None[string]()
	}
	arg := a.iter[0]
	a.iter = a.iter[1:]
	return rs.Some(arg)
}

func (a *sysArgs) nextBack() rs.Option[string] {
	if len(a.iter) == 0 {
		return rs.None[string]()
	}
	arg := a.iter[len(a.iter)-1]
	a.iter = a.iter[:len(a.iter)-1]
	return rs.Some(arg)
}

func (a *sysArgs) len() int {
	return len(a.iter)
}
