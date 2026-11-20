// Port of std 1.99.1 src/sys/fs/common.rs (the parts Unix and Windows use)

package rsfs

import "github.com/BrokkAi/mjolnir/go/rs/rsio"

// notFileError is NOT_FILE_ERROR: what copy reports for a source that is not
// a regular file.
var notFileError = rsio.ErrorFromStaticMessage(rsio.ErrorKind_InvalidInput,
	"the source path is neither a regular file nor a symlink to a regular file")

// commonExists is common::exists: metadata succeeds, or fails with NotFound.
func commonExists(path string) (bool, error) {
	_, err := MetadataOf(path)
	if err == nil {
		return true, nil
	}
	if rsio.KindOf(err) == rsio.ErrorKind_NotFound {
		return false, nil
	}
	return false, err
}

// ignoreNotfound is sys::helpers::ignore_notfound.
func ignoreNotfound(err error) error {
	if err != nil && rsio.KindOf(err) == rsio.ErrorKind_NotFound {
		return nil
	}
	return err
}
