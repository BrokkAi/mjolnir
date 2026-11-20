// Port of mj-transcript/examples/summary_probe.rs.

// Command summary_probe is an offline diagnostic: it renders a canonical
// snapshot read from standard input through the production summary.
package main

import (
	"encoding/json"
	"fmt"
	"io"
	"os"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

func main() {
	if err := run(); err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		os.Exit(1)
	}
}

func run() error {
	input, err := io.ReadAll(os.Stdin)
	if err != nil {
		return err
	}
	var snapshot core.CanonicalSessionSnapshot
	if err := json.Unmarshal(input, &snapshot); err != nil {
		return err
	}
	summary := transcript.TranscriptSummaryFromSnapshot(&snapshot)
	encoded, err := jsonvalue.Marshal(summary.Render(48 * 1024))
	if err != nil {
		return err
	}
	fmt.Println(string(encoded))
	return nil
}
