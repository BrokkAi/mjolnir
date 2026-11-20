// Port of mj-worker/src/relay/serving.rs.

package worker

import (
	"bufio"
	"io"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ServeRelayJSONLines answers newline-delimited relay requests until the
// reader ends.
func ServeRelayJSONLines(reader *bufio.Reader, writer io.Writer, relay *DurableRelay) error {
	for {
		request, err := core.ReadRelayFrame(reader)
		if err != nil {
			return err
		}
		if request == nil {
			return nil
		}
		response := relay.Handle(*request)
		if err := core.WriteRelayFrame(writer, response); err != nil {
			return err
		}
	}
}
