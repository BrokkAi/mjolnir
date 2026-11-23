// Port of mj-chat/src/chat/pasted_paths.rs (Linux).

package chat

// windowsPathOnThisHost translates a pasted Windows path with `wslpath -u`,
// which knows custom automount roots and `\\wsl.localhost\…` paths that a
// fixed `/mnt/<drive>` mapping gets wrong.
func windowsPathOnThisHost(path string) (string, error) {
	panic("unported: mj-chat/src/chat/pasted_paths.rs windows_path_on_this_host")
}
