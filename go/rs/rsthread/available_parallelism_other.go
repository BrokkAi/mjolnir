//go:build !linux && !darwin && !windows

package rsthread

import "github.com/BrokkAi/mjolnir/go/rs"

func availableParallelism() (int, error) {
	panic(rs.Todo("std::thread::available_parallelism: only Linux, macOS and Windows are ported"))
}
