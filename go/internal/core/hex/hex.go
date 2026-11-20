// Package hex renders bytes as lowercase hexadecimal, as mj-core/src/hex.rs does.
package hex

import "encoding/hex"

// LowerHex is the lowercase hexadecimal form of b.
func LowerHex(b []byte) string { return hex.EncodeToString(b) }
