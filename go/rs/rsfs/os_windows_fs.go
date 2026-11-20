// Port of std 1.99.1 src/os/windows/fs.rs

//go:build windows

package rsfs

import "time"

// The extension traits of std::os::windows::fs are methods of the types they
// extend. The unstable parts (MetadataExt's volume_serial_number,
// number_of_links, file_index and change_time, OpenOptionsExt's
// freeze_last_*, junction_point) are not provided.

// SeekRead is FileExt::seek_read: a read at offset; unlike Unix's read_at,
// it moves the file's position.
func (f *File) SeekRead(buf []byte, offset uint64) (int, error) {
	return f.inner.readAt(buf, offset)
}

// SeekWrite is FileExt::seek_write.
func (f *File) SeekWrite(buf []byte, offset uint64) (int, error) {
	return f.inner.writeAt(buf, offset)
}

// AccessMode is OpenOptionsExt::access_mode: the dwDesiredAccess of
// CreateFileW, in place of what read, write and append give.
func (o *OpenOptions) AccessMode(access uint32) *OpenOptions {
	o.f0.setAccessMode(access)
	return o
}

// ShareMode is OpenOptionsExt::share_mode (all three by default).
func (o *OpenOptions) ShareMode(shareMode uint32) *OpenOptions {
	o.f0.setShareMode(shareMode)
	return o
}

// CustomFlags is OpenOptionsExt::custom_flags: flags for the
// dwFlagsAndAttributes of CreateFileW.
func (o *OpenOptions) CustomFlags(flags uint32) *OpenOptions {
	o.f0.setCustomFlags(flags)
	return o
}

// Attributes is OpenOptionsExt::attributes.
func (o *OpenOptions) Attributes(attributes uint32) *OpenOptions {
	o.f0.setAttributes(attributes)
	return o
}

// SecurityQosFlags is OpenOptionsExt::security_qos_flags.
func (o *OpenOptions) SecurityQosFlags(flags uint32) *OpenOptions {
	o.f0.setSecurityQosFlags(flags)
	return o
}

// FileAttributes is MetadataExt::file_attributes.
func (m Metadata) FileAttributes() uint32 { return m.f0.attrs() }

// CreationTime is MetadataExt::creation_time: 100ns intervals since 1601.
func (m Metadata) CreationTime() uint64 { return m.f0.createdU64() }

// LastAccessTime is MetadataExt::last_access_time.
func (m Metadata) LastAccessTime() uint64 { return m.f0.accessedU64() }

// LastWriteTime is MetadataExt::last_write_time.
func (m Metadata) LastWriteTime() uint64 { return m.f0.modifiedU64() }

// FileSize is MetadataExt::file_size.
func (m Metadata) FileSize() uint64 { return m.f0.size() }

// IsSymlinkDir is FileTypeExt::is_symlink_dir.
func (t FileType) IsSymlinkDir() bool { return t.f0.isSymlinkDir() }

// IsSymlinkFile is FileTypeExt::is_symlink_file.
func (t FileType) IsSymlinkFile() bool { return t.f0.isSymlinkFile() }

// SetCreated is FileTimesExt::set_created.
func (t FileTimes) SetCreated(at time.Time) FileTimes {
	t.f0.setCreated(at)
	return t
}

// SymlinkFile is std::os::windows::fs::symlink_file.
func SymlinkFile(original, link string) error {
	return symlinkInner(original, link, false)
}

// SymlinkDir is std::os::windows::fs::symlink_dir.
func SymlinkDir(original, link string) error {
	return symlinkInner(original, link, true)
}
