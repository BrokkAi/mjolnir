//go:build !windows

package rstime

import "time"

// instantEpsilon is only consulted on Windows.
func instantEpsilon() time.Duration { return 0 }
