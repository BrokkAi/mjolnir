// Port of mj-core/src/hex.rs.

package core

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"os"
)

// LowerHex is the lowercase hexadecimal form of b.
func LowerHex(b []byte) string { return hex.EncodeToString(b) }

// FileSHA256 reads a file once to compute its SHA-256 checksum in lowercase
// hexadecimal.
func FileSHA256(path string) (string, error) {
	file, err := os.Open(path)
	if err != nil {
		return "", fmt.Errorf("open file for checksum %s: %w", path, err)
	}
	defer file.Close()
	digest := sha256.New()
	if _, err := io.CopyBuffer(digest, file, make([]byte, 1024*1024)); err != nil {
		return "", fmt.Errorf("hash file %s: %w", path, err)
	}
	return LowerHex(digest.Sum(nil)), nil
}
