//go:build unix && !linux

// Port of mj-cli/src/terminal_background.rs's libc::FIONREAD off Linux.

package cli

// terminalFionread is FIONREAD, _IOR('f', 127, int), as macOS and the BSDs
// define it; golang.org/x/sys/unix does not export it.
const terminalFionread = 0x4004667f
