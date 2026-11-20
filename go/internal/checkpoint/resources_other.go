// Port of mj-checkpoint/src/resources.rs: the `cfg(not(unix))` stream
// producer.

//go:build !unix

package checkpoint

import (
	"errors"
	"io"
)

// StreamResource needs a Unix controller.
func StreamResource(source string, consume func(io.Reader) error) error {
	return errors.New("streaming attached resources requires a Unix controller")
}
