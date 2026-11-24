//go:build linux

// Port of mj-cli/src/terminal_background.rs's libc::FIONREAD on Linux.

package cli

import "golang.org/x/sys/unix"

// terminalFionread is FIONREAD, which Linux names TIOCINQ.
const terminalFionread = unix.TIOCINQ
