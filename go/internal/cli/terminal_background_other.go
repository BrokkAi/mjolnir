//go:build !unix

// Port of mj-cli/src/terminal_background.rs off Unix.

package cli

import "github.com/BrokkAi/mjolnir/go/internal/chat"

// askTerminalBackground never asks off Unix: the Windows console is read as
// input records, not as the byte stream a reply would arrive on.
func askTerminalBackground() (*chat.Rgb, error) { return nil, nil }
