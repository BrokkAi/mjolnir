//go:build linux || darwin

// Port of mj-client/src/executable.rs (the Linux and macOS file identity).

package client

// ExecutableFileIdentity is a file, identified by what it is rather than by
// what it is called.
//
// A running executable can be renamed (NFS moves an unlinked open file to a
// `.nfs*` name) or replaced at its original path, so names prove nothing.
// Device and inode distinguish two open files cheaply, and two names for one
// inode — a hard link — correctly compare equal.
type ExecutableFileIdentity struct {
	device uint64
	inode  uint64
}

// ExecutableFileIdentityOf is Rust's executable_file_identity: the identity
// of the file at path, following symbolic links.
func ExecutableFileIdentityOf(path string) (ExecutableFileIdentity, error) {
	panic("unported: mj-client/src/executable.rs executable_file_identity")
}
