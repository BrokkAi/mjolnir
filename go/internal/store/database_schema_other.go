// Port of mj-store/src/database/schema.rs (StoreIdentity elsewhere than Unix).

//go:build !unix

package store

import "os"

// storeIdentityOf is the empty identity when path exists: SQLite's sharing
// mode keeps an open store from being replaced, so the path alone identifies
// it. False when path cannot be read.
func storeIdentityOf(path string) (storeIdentity, bool) {
	if _, err := os.Stat(path); err != nil {
		return storeIdentity{}, false
	}
	return storeIdentity{}, true
}
