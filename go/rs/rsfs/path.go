// Port of std 1.99.1 src/path.rs (the Path methods that touch the file
// system)

package rsfs

// PathMetadata is Path::metadata: MetadataOf.
func PathMetadata(path string) (Metadata, error) {
	return MetadataOf(path)
}

// PathSymlinkMetadata is Path::symlink_metadata: SymlinkMetadata.
func PathSymlinkMetadata(path string) (Metadata, error) {
	return SymlinkMetadata(path)
}

// PathCanonicalize is Path::canonicalize: Canonicalize.
func PathCanonicalize(path string) (string, error) {
	return Canonicalize(path)
}

// PathReadLink is Path::read_link: ReadLink.
func PathReadLink(path string) (string, error) {
	return ReadLink(path)
}

// PathReadDir is Path::read_dir: ReadDirOf.
func PathReadDir(path string) (*ReadDir, error) {
	return ReadDirOf(path)
}

// PathExists is Path::exists: whether metadata succeeds. Every error,
// permission denied included, is false.
func PathExists(path string) bool {
	_, err := MetadataOf(path)
	return err == nil
}

// PathTryExists is Path::try_exists: Exists.
func PathTryExists(path string) (bool, error) {
	return Exists(path)
}

// PathIsFile is Path::is_file, following symbolic links; false on any error.
func PathIsFile(path string) bool {
	m, err := MetadataOf(path)
	return err == nil && m.IsFile()
}

// PathIsDir is Path::is_dir, following symbolic links; false on any error.
func PathIsDir(path string) bool {
	m, err := MetadataOf(path)
	return err == nil && m.IsDir()
}

// PathIsSymlink is Path::is_symlink; false on any error.
func PathIsSymlink(path string) bool {
	m, err := SymlinkMetadata(path)
	return err == nil && m.IsSymlink()
}
