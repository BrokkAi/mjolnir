// Port of std 1.99.1 src/os/unix/fs.rs

//go:build unix

package rsfs

import (
	"math"

	"golang.org/x/sys/unix"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rsio"
)

// The extension traits of std::os::unix::fs are methods of the types they
// extend: `file.read_at(..)` with FileExt in scope is file.ReadAt(..),
// `perm.mode()` with PermissionsExt is perm.Mode(), and so on.

// ReadAt is FileExt::read_at: a read at offset, which does not move the
// file's position.
func (f *File) ReadAt(buf []byte, offset uint64) (int, error) {
	return f.inner.readAt(buf, offset)
}

// ReadExactAt is FileExt::read_exact_at.
func (f *File) ReadExactAt(buf []byte, offset uint64) error {
	for len(buf) != 0 {
		n, err := f.ReadAt(buf, offset)
		if err != nil {
			if rsio.From(err).IsInterrupted() {
				continue
			}
			return err
		}
		if n == 0 {
			break
		}
		buf = buf[n:]
		offset += uint64(n)
	}
	if len(buf) != 0 {
		return rsio.ErrorReadExactEof()
	}
	return nil
}

// WriteAt is FileExt::write_at: a write at offset, which does not move the
// file's position.
func (f *File) WriteAt(buf []byte, offset uint64) (int, error) {
	return f.inner.writeAt(buf, offset)
}

// WriteAllAt is FileExt::write_all_at.
func (f *File) WriteAllAt(buf []byte, offset uint64) error {
	for len(buf) != 0 {
		n, err := f.WriteAt(buf, offset)
		if err != nil {
			if rsio.From(err).IsInterrupted() {
				continue
			}
			return err
		}
		if n == 0 {
			return rsio.ErrorWriteAllEof()
		}
		buf = buf[n:]
		offset += uint64(n)
	}
	return nil
}

// Mode is PermissionsExt::mode: the st_mode bits, file type included.
func (p Permissions) Mode() uint32 {
	return p.f0.modeU32()
}

// SetMode is PermissionsExt::set_mode.
func (p *Permissions) SetMode(mode uint32) {
	*p = PermissionsFromMode(mode)
}

// PermissionsFromMode is PermissionsExt::from_mode.
func PermissionsFromMode(mode uint32) Permissions {
	return Permissions{f0: sysFilePermissions{mode: modeT(mode)}}
}

// Mode is OpenOptionsExt::mode: the mode a created file gets, before the
// umask (0o666 by default).
func (o *OpenOptions) Mode(mode uint32) *OpenOptions {
	o.f0.setMode(mode)
	return o
}

// CustomFlags is OpenOptionsExt::custom_flags: flags added to open's; the
// access mode bits are ignored.
func (o *OpenOptions) CustomFlags(flags int32) *OpenOptions {
	o.f0.setCustomFlags(flags)
	return o
}

// Dev is MetadataExt::dev.
func (m Metadata) Dev() uint64 { return m.f0.stat.dev }

// Ino is MetadataExt::ino.
func (m Metadata) Ino() uint64 { return m.f0.stat.ino }

// Mode is MetadataExt::mode: st_mode.
func (m Metadata) Mode() uint32 { return m.f0.stat.mode }

// Nlink is MetadataExt::nlink.
func (m Metadata) Nlink() uint64 { return m.f0.stat.nlink }

// Uid is MetadataExt::uid.
func (m Metadata) Uid() uint32 { return m.f0.stat.uid }

// Gid is MetadataExt::gid.
func (m Metadata) Gid() uint32 { return m.f0.stat.gid }

// Rdev is MetadataExt::rdev.
func (m Metadata) Rdev() uint64 { return m.f0.stat.rdev }

// Size is MetadataExt::size.
func (m Metadata) Size() uint64 { return uint64(m.f0.stat.size) }

// Atime is MetadataExt::atime.
func (m Metadata) Atime() int64 { return m.f0.stat.atime }

// AtimeNsec is MetadataExt::atime_nsec.
func (m Metadata) AtimeNsec() int64 { return m.f0.stat.atimeNsec }

// Mtime is MetadataExt::mtime.
func (m Metadata) Mtime() int64 { return m.f0.stat.mtime }

// MtimeNsec is MetadataExt::mtime_nsec.
func (m Metadata) MtimeNsec() int64 { return m.f0.stat.mtimeNsec }

// Ctime is MetadataExt::ctime.
func (m Metadata) Ctime() int64 { return m.f0.stat.ctime }

// CtimeNsec is MetadataExt::ctime_nsec.
func (m Metadata) CtimeNsec() int64 { return m.f0.stat.ctimeNsec }

// Blksize is MetadataExt::blksize.
func (m Metadata) Blksize() uint64 { return uint64(m.f0.stat.blksize) }

// Blocks is MetadataExt::blocks.
func (m Metadata) Blocks() uint64 { return uint64(m.f0.stat.blocks) }

// IsBlockDevice is FileTypeExt::is_block_device.
func (t FileType) IsBlockDevice() bool { return t.f0.is(unix.S_IFBLK) }

// IsCharDevice is FileTypeExt::is_char_device.
func (t FileType) IsCharDevice() bool { return t.f0.is(unix.S_IFCHR) }

// IsFifo is FileTypeExt::is_fifo.
func (t FileType) IsFifo() bool { return t.f0.is(unix.S_IFIFO) }

// IsSocket is FileTypeExt::is_socket.
func (t FileType) IsSocket() bool { return t.f0.is(unix.S_IFSOCK) }

// Ino is DirEntryExt::ino: the inode number readdir gave.
func (e DirEntry) Ino() uint64 { return e.f0.ino() }

// Symlink is std::os::unix::fs::symlink: link becomes a symbolic link to
// original.
func Symlink(original, link string) error {
	return sysSymlink(original, link)
}

// Mode is DirBuilderExt::mode: the mode new directories get, before the
// umask (0o777 by default).
func (b *DirBuilder) Mode(mode uint32) *DirBuilder {
	b.inner.setMode(mode)
	return b
}

// Chown is std::os::unix::fs::chown; None leaves the id as it is.
func Chown(dir string, uid, gid rs.Option[uint32]) error {
	return chown(dir, uid.UnwrapOr(math.MaxUint32), gid.UnwrapOr(math.MaxUint32))
}

// Fchown is std::os::unix::fs::fchown.
func Fchown(fd rsio.AsFd, uid, gid rs.Option[uint32]) error {
	return fchown(int(fd.AsFd().AsRawFd()), uid.UnwrapOr(math.MaxUint32), gid.UnwrapOr(math.MaxUint32))
}

// Lchown is std::os::unix::fs::lchown, which does not follow a symbolic link.
func Lchown(dir string, uid, gid rs.Option[uint32]) error {
	return lchown(dir, uid.UnwrapOr(math.MaxUint32), gid.UnwrapOr(math.MaxUint32))
}

// Chroot is std::os::unix::fs::chroot.
func Chroot(dir string) error {
	return chroot(dir)
}

// Mkfifo is std::os::unix::fs::mkfifo.
func Mkfifo(path string, permissions Permissions) error {
	return mkfifo(path, permissions.Mode())
}
