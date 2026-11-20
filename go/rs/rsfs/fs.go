// Port of std 1.99.1 src/fs.rs

package rsfs

import (
	"io"
	"iter"
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rsio"
	"github.com/BrokkAi/mjolnir/go/rs/rspath"
)

// File is std::fs::File: an open file. It owns its descriptor (its handle on
// Windows) and must be dropped: Drop closes it, as Rust closes a File when it
// goes out of scope. Product code holds it as *File.
//
// Its Read follows Rust's contract, not Go's: it returns (0, nil) at the end
// of the file (see rsio). Wrap it with rsio.GoReader before handing it to a
// Go library.
type File struct {
	inner *sysFile
}

// TryLockError is std::fs::TryLockError: why File::try_lock did not take the
// lock. It is *TryLockError_Error or *TryLockError_WouldBlock, and is what
// TryLock and TryLockShared return as their error.
type TryLockError interface {
	error
	isTryLockError()
}

// TryLockError_Error is TryLockError::Error: the lock could not be acquired
// due to an I/O error on the file. F0 is the io::Error, an *rsio.Error.
type TryLockError_Error struct{ F0 error }

// TryLockError_WouldBlock is TryLockError::WouldBlock: the lock could not be
// acquired at this time because it is held by another handle or process.
type TryLockError_WouldBlock struct{}

func (*TryLockError_Error) isTryLockError()      {}
func (*TryLockError_WouldBlock) isTryLockError() {}

var _ = rs.RegisterEnum[TryLockError]("TryLockError",
	rs.Variant[TryLockError_Error]("Error", rs.ShapeTuple),
	rs.Variant[TryLockError_WouldBlock]("WouldBlock", rs.ShapeUnit),
)

// Metadata is std::fs::Metadata: what the operating system says about a
// file. Clone is a copy.
type Metadata struct {
	f0 sysFileAttr
}

// ReadDir is std::fs::ReadDir: the entries of a directory, in the order the
// file system gives them. It holds the open directory and must be dropped,
// unless it is consumed by Iter.
type ReadDir struct {
	f0 *sysReadDir
}

// DirEntry is std::fs::DirEntry: one entry of a ReadDir.
type DirEntry struct {
	f0 sysDirEntry
}

// OpenOptions is std::fs::OpenOptions: how to open a file.
type OpenOptions struct {
	f0 sysOpenOptions
}

// FileTimes is std::fs::FileTimes: the times to set on a file with
// File.SetTimes. It is a value, as in Rust (Copy).
type FileTimes struct {
	f0 sysFileTimes
}

// Permissions is std::fs::Permissions: the permissions of a file.
type Permissions struct {
	f0 sysFilePermissions
}

// FileType is std::fs::FileType: the type of a file. It is a value, as in
// Rust (Copy).
type FileType struct {
	f0 sysFileType
}

// DirBuilder is std::fs::DirBuilder: how to create a directory.
type DirBuilder struct {
	inner     sysDirBuilder `rs:"inner"`
	recursive bool          `rs:"recursive"`
}

var (
	_ = rs.RegisterTupleStruct[Metadata]("Metadata")
	_ = rs.RegisterTupleStruct[ReadDir]("ReadDir")
	_ = rs.RegisterTupleStruct[DirEntry]("DirEntry")
	_ = rs.RegisterTupleStruct[OpenOptions]("OpenOptions")
	_ = rs.RegisterTupleStruct[FileTimes]("FileTimes")
	_ = rs.RegisterTupleStruct[Permissions]("Permissions")
	_ = rs.RegisterTupleStruct[FileType]("FileType")
	_ = rs.RegisterStruct[DirBuilder]("DirBuilder")
)

// Read is std::fs::read: the whole contents of a file.
func Read(path string) ([]byte, error) {
	file, err := FileOpen(path)
	if err != nil {
		return nil, err
	}
	defer file.Drop()
	size := rs.None[int]()
	if m, err := file.Metadata(); err == nil {
		size = rs.Some(lenToUsize(m.Len()))
	}
	bytes := make([]byte, 0, size.UnwrapOr(0))
	if _, err := rsio.DefaultReadToEnd(file, &bytes, size); err != nil {
		return nil, err
	}
	return bytes, nil
}

// ReadToString is std::fs::read_to_string: the whole contents of a file,
// which must be UTF-8 ("stream did not contain valid UTF-8" otherwise).
func ReadToString(path string) (string, error) {
	file, err := FileOpen(path)
	if err != nil {
		return "", err
	}
	defer file.Drop()
	size := rs.None[int]()
	if m, err := file.Metadata(); err == nil {
		size = rs.Some(lenToUsize(m.Len()))
	}
	var text string
	if _, err := rsio.DefaultReadToString(file, &text, size); err != nil {
		return "", err
	}
	return text, nil
}

// lenToUsize is `usize::try_from(m.len()).unwrap_or(usize::MAX)`.
func lenToUsize(n uint64) int {
	if n > uint64(rs.UsizeMax) {
		return rs.UsizeMax
	}
	return int(n)
}

// Write is std::fs::write: it creates or truncates the file and writes
// contents, which is a string or bytes as Rust's AsRef<[u8]> accepts.
func Write[C ~string | ~[]byte](path string, contents C) error {
	file, err := FileCreate(path)
	if err != nil {
		return err
	}
	defer file.Drop()
	return rsio.WriteAll(file, []byte(contents))
}

// Error is `impl Display for TryLockError`; it makes the variant a Go error.
func (e *TryLockError_Error) Error() string { return e.String() }

// String is `impl Display for TryLockError`.
func (e *TryLockError_Error) String() string { return "lock acquisition failed due to I/O error" }

// FmtDisplay is `impl Display for TryLockError`.
func (e *TryLockError_Error) FmtDisplay(f *rs.Formatter) error { return f.Pad(e.String()) }

// FmtDebug is `impl Debug for TryLockError`: the inner error's Debug.
func (e *TryLockError_Error) FmtDebug(f *rs.Formatter) error { return rs.DebugFmt(e.F0, f) }

// DebugString is `format!("{:?}", e)`.
func (e *TryLockError_Error) DebugString() string { return rs.DebugString(e) }

// Error is `impl Display for TryLockError`; it makes the variant a Go error.
func (e *TryLockError_WouldBlock) Error() string { return e.String() }

// String is `impl Display for TryLockError`.
func (e *TryLockError_WouldBlock) String() string {
	return "lock acquisition failed because the operation would block"
}

// FmtDisplay is `impl Display for TryLockError`.
func (e *TryLockError_WouldBlock) FmtDisplay(f *rs.Formatter) error { return f.Pad(e.String()) }

// FmtDebug is `impl Debug for TryLockError`: `"WouldBlock".fmt(f)`, with
// fmt::Debug the trait in scope, so the name between quotes.
func (e *TryLockError_WouldBlock) FmtDebug(f *rs.Formatter) error {
	return rs.DebugFmt("WouldBlock", f)
}

// DebugString is `format!("{:?}", e)`.
func (e *TryLockError_WouldBlock) DebugString() string { return rs.DebugString(e) }

// ErrorFromTryLockError is `impl From<TryLockError> for io::Error`: the I/O
// error itself, or the kind WouldBlock.
func ErrorFromTryLockError(err TryLockError) *rsio.Error {
	switch err := err.(type) {
	case *TryLockError_Error:
		return rsio.From(err.F0)
	case *TryLockError_WouldBlock:
		return rsio.ErrorFrom(rsio.ErrorKind_WouldBlock)
	default:
		panic(rs.Unreachable(err))
	}
}

// FileOpen is File::open: the file opened for reading.
func FileOpen(path string) (*File, error) {
	return OpenOptionsNew().Read(true).Open(path)
}

// FileCreate is File::create: the file opened for writing, created when it
// does not exist and truncated when it does.
func FileCreate(path string) (*File, error) {
	return OpenOptionsNew().Write(true).Create(true).Truncate(true).Open(path)
}

// FileCreateNew is File::create_new: a new file opened for reading and
// writing; it fails when the path exists.
func FileCreateNew(path string) (*File, error) {
	return OpenOptionsNew().Read(true).Write(true).CreateNew(true).Open(path)
}

// FileOptions is File::options: OpenOptionsNew.
func FileOptions() *OpenOptions {
	return OpenOptionsNew()
}

// SyncAll is File::sync_all.
func (f *File) SyncAll() error {
	return f.inner.fsync()
}

// SyncData is File::sync_data.
func (f *File) SyncData() error {
	return f.inner.datasync()
}

// Lock is File::lock: an exclusive advisory lock, waiting for it.
func (f *File) Lock() error {
	return f.inner.lock()
}

// LockShared is File::lock_shared: a shared advisory lock, waiting for it.
func (f *File) LockShared() error {
	return f.inner.lockShared()
}

// TryLock is File::try_lock: the error is nil, *TryLockError_WouldBlock or
// *TryLockError_Error.
func (f *File) TryLock() error {
	return f.inner.tryLock()
}

// TryLockShared is File::try_lock_shared, as TryLock.
func (f *File) TryLockShared() error {
	return f.inner.tryLockShared()
}

// Unlock is File::unlock.
func (f *File) Unlock() error {
	return f.inner.unlock()
}

// SetLen is File::set_len.
func (f *File) SetLen(size uint64) error {
	return f.inner.truncate(size)
}

// Metadata is File::metadata.
func (f *File) Metadata() (Metadata, error) {
	attr, err := f.inner.fileAttr()
	if err != nil {
		return Metadata{}, err
	}
	return Metadata{f0: attr}, nil
}

// TryClone is File::try_clone: a second File over the same open file.
func (f *File) TryClone() (*File, error) {
	inner, err := f.inner.duplicate()
	if err != nil {
		return nil, err
	}
	return &File{inner: inner}, nil
}

// SetPermissions is File::set_permissions.
func (f *File) SetPermissions(perm Permissions) error {
	return f.inner.setPermissions(perm.f0)
}

// SetTimes is File::set_times.
func (f *File) SetTimes(times FileTimes) error {
	return f.inner.setTimes(times.f0)
}

// SetModified is File::set_modified.
func (f *File) SetModified(t time.Time) error {
	return f.SetTimes(FileTimesNew().SetModified(t))
}

// Drop closes the file. A second call does nothing.
func (f *File) Drop() {
	if f == nil || f.inner == nil {
		return
	}
	f.inner.drop()
}

// FmtDebug is `impl Debug for File`.
func (f *File) FmtDebug(fm *rs.Formatter) error {
	return f.inner.fmtDebug(fm)
}

// DebugString is `format!("{:?}", file)`.
func (f *File) DebugString() string { return rs.DebugString(f) }

// bufferCapacityRequired is fs::buffer_capacity_required: the bytes left
// between the position and the end, when both are known.
func bufferCapacityRequired(file *File) rs.Option[int] {
	m, err := file.Metadata()
	if err != nil {
		return rs.None[int]()
	}
	size := m.Len()
	pos, err := file.StreamPosition()
	if err != nil {
		return rs.None[int]()
	}
	// Don't worry about `usize` overflow because reading will fail regardless
	// in that case.
	if size < pos {
		return rs.Some(0)
	}
	return rs.Some(int(size - pos))
}

// Read is `impl Read for File`, read: Rust's contract, (0, nil) at the end.
func (f *File) Read(buf []byte) (int, error) {
	return f.inner.read(buf)
}

// ReadBuf is `impl Read for File`, read_buf.
func (f *File) ReadBuf(cursor *rsio.BorrowedCursor) error {
	return f.inner.readBuf(cursor)
}

// ReadToEnd is `impl Read for File`, read_to_end, which reserves space in
// the buffer based on the file size when available.
func (f *File) ReadToEnd(buf *[]byte) (int, error) {
	size := bufferCapacityRequired(f)
	reserve(buf, size.UnwrapOr(0))
	return rsio.DefaultReadToEnd(f, buf, size)
}

// ReadToString is `impl Read for File`, read_to_string, which reserves space
// in the buffer based on the file size when available.
func (f *File) ReadToString(buf *string) (int, error) {
	size := bufferCapacityRequired(f)
	return rsio.DefaultReadToString(f, buf, size)
}

// reserve is Vec::try_reserve: at least additional bytes of spare capacity,
// growing as a Vec grows (doubling, and never below 8).
func reserve(buf *[]byte, additional int) {
	if cap(*buf)-len(*buf) >= additional {
		return
	}
	required := len(*buf) + additional
	grown := make([]byte, len(*buf), max(cap(*buf)*2, required, 8))
	copy(grown, *buf)
	*buf = grown
}

// Write is `impl Write for File`, write.
func (f *File) Write(buf []byte) (int, error) {
	return f.inner.write(buf)
}

// Flush is `impl Write for File`, flush.
func (f *File) Flush() error {
	return f.inner.flush()
}

// SeekTo is `impl Seek for File`, seek.
func (f *File) SeekTo(pos rsio.SeekFrom) (uint64, error) {
	return f.inner.seek(pos)
}

// Seek is SeekTo through Go's io.Seeker signature, for rsio.Seek and Go code.
func (f *File) Seek(offset int64, whence int) (int64, error) {
	var pos rsio.SeekFrom
	switch {
	case whence == io.SeekStart && offset >= 0:
		pos = &rsio.SeekFrom_Start{F0: uint64(offset)}
	case whence == io.SeekCurrent:
		pos = &rsio.SeekFrom_Current{F0: offset}
	case whence == io.SeekEnd:
		pos = &rsio.SeekFrom_End{F0: offset}
	default:
		// What rsio answers for a position Go can express and Rust cannot.
		return 0, rsio.ErrorFromStaticMessage(rsio.ErrorKind_InvalidInput,
			"invalid seek to a negative or overflowing position")
	}
	n, err := f.inner.seek(pos)
	return int64(n), err
}

// StreamLen is `impl Seek for File`, stream_len.
func (f *File) StreamLen() (uint64, error) {
	if result, err, ok := f.inner.size(); ok {
		return result, err
	}
	return rsio.StreamLenDefault(f)
}

// StreamPosition is `impl Seek for File`, stream_position.
func (f *File) StreamPosition() (uint64, error) {
	return f.inner.tell()
}

// OpenOptionsNew is OpenOptions::new: nothing set.
func OpenOptionsNew() *OpenOptions {
	return &OpenOptions{f0: newSysOpenOptions()}
}

// Read is OpenOptions::read.
func (o *OpenOptions) Read(read bool) *OpenOptions {
	o.f0.setRead(read)
	return o
}

// Write is OpenOptions::write.
func (o *OpenOptions) Write(write bool) *OpenOptions {
	o.f0.setWrite(write)
	return o
}

// Append is OpenOptions::append.
func (o *OpenOptions) Append(append bool) *OpenOptions {
	o.f0.setAppend(append)
	return o
}

// Truncate is OpenOptions::truncate.
func (o *OpenOptions) Truncate(truncate bool) *OpenOptions {
	o.f0.setTruncate(truncate)
	return o
}

// Create is OpenOptions::create.
func (o *OpenOptions) Create(create bool) *OpenOptions {
	o.f0.setCreate(create)
	return o
}

// CreateNew is OpenOptions::create_new.
func (o *OpenOptions) CreateNew(createNew bool) *OpenOptions {
	o.f0.setCreateNew(createNew)
	return o
}

// Open is OpenOptions::open.
func (o *OpenOptions) Open(path string) (*File, error) {
	return o.open(path)
}

func (o *OpenOptions) open(path string) (*File, error) {
	inner, err := sysFileOpen(path, &o.f0)
	if err != nil {
		return nil, err
	}
	return &File{inner: inner}, nil
}

// FmtDebug is the derived Debug: `OpenOptions(OpenOptions { .. })`.
func (o OpenOptions) FmtDebug(f *rs.Formatter) error {
	return f.DebugTuple("OpenOptions").Field(debugWith(o.f0.fmtDebug)).Finish()
}

// FileType is Metadata::file_type.
func (m Metadata) FileType() FileType {
	return FileType{f0: m.f0.fileType()}
}

// IsDir is Metadata::is_dir.
func (m Metadata) IsDir() bool {
	return m.FileType().IsDir()
}

// IsFile is Metadata::is_file.
func (m Metadata) IsFile() bool {
	return m.FileType().IsFile()
}

// IsSymlink is Metadata::is_symlink.
func (m Metadata) IsSymlink() bool {
	return m.FileType().IsSymlink()
}

// Len is Metadata::len: the size in bytes.
func (m Metadata) Len() uint64 {
	return m.f0.size()
}

// Permissions is Metadata::permissions.
func (m Metadata) Permissions() Permissions {
	return Permissions{f0: m.f0.perm()}
}

// Modified is Metadata::modified.
func (m Metadata) Modified() (time.Time, error) {
	return m.f0.modified()
}

// Accessed is Metadata::accessed.
func (m Metadata) Accessed() (time.Time, error) {
	return m.f0.accessed()
}

// Created is Metadata::created.
func (m Metadata) Created() (time.Time, error) {
	return m.f0.created()
}

// FmtDebug is `impl Debug for Metadata`.
func (m Metadata) FmtDebug(f *rs.Formatter) error {
	debug := f.DebugStruct("Metadata")
	debug.Field("file_type", m.FileType())
	debug.Field("permissions", m.Permissions())
	debug.Field("len", m.Len())
	if modified, err := m.Modified(); err == nil {
		debug.Field("modified", modified)
	}
	if accessed, err := m.Accessed(); err == nil {
		debug.Field("accessed", accessed)
	}
	if created, err := m.Created(); err == nil {
		debug.Field("created", created)
	}
	return debug.FinishNonExhaustive()
}

// FileTimesNew is FileTimes::new: no time set.
func FileTimesNew() FileTimes {
	return FileTimes{}
}

// SetAccessed is FileTimes::set_accessed.
func (t FileTimes) SetAccessed(at time.Time) FileTimes {
	t.f0.setAccessed(at)
	return t
}

// SetModified is FileTimes::set_modified.
func (t FileTimes) SetModified(at time.Time) FileTimes {
	t.f0.setModified(at)
	return t
}

// FmtDebug is the derived Debug: `FileTimes(FileTimes { .. })`.
func (t FileTimes) FmtDebug(f *rs.Formatter) error {
	return f.DebugTuple("FileTimes").Field(debugWith(t.f0.fmtDebug)).Finish()
}

// Readonly is Permissions::readonly.
func (p Permissions) Readonly() bool {
	return p.f0.readonly()
}

// SetReadonly is Permissions::set_readonly.
func (p *Permissions) SetReadonly(readonly bool) {
	p.f0.setReadonly(readonly)
}

// FmtDebug is the derived Debug: `Permissions(FilePermissions { .. })`.
func (p Permissions) FmtDebug(f *rs.Formatter) error {
	return f.DebugTuple("Permissions").Field(debugWith(p.f0.fmtDebug)).Finish()
}

// IsDir is FileType::is_dir.
func (t FileType) IsDir() bool {
	return t.f0.isDir()
}

// IsFile is FileType::is_file.
func (t FileType) IsFile() bool {
	return t.f0.isFile()
}

// IsSymlink is FileType::is_symlink.
func (t FileType) IsSymlink() bool {
	return t.f0.isSymlink()
}

// FmtDebug is `impl Debug for FileType`.
func (t FileType) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("FileType").
		Field("is_file", t.IsFile()).
		Field("is_dir", t.IsDir()).
		Field("is_symlink", t.IsSymlink()).
		FinishNonExhaustive()
}

// Next is `impl Iterator for ReadDir`: the next entry, an error, or None at
// the end. The directory is closed at the end.
func (r *ReadDir) Next() rs.Option[rs.Result[DirEntry]] {
	entry, err, ok := r.f0.next()
	if !ok {
		return rs.None[rs.Result[DirEntry]]()
	}
	if err != nil {
		return rs.Some(rs.Err[DirEntry](err))
	}
	return rs.Some(rs.Ok(DirEntry{f0: entry}))
}

// Iter consumes the ReadDir, as Rust's `for entry in read_dir`: it yields
// every entry, and drops the ReadDir when the loop ends, however it ends.
func (r *ReadDir) Iter() iter.Seq[rs.Result[DirEntry]] {
	return func(yield func(rs.Result[DirEntry]) bool) {
		defer r.Drop()
		for {
			next, ok := r.Next().Get()
			if !ok || !yield(next) {
				return
			}
		}
	}
}

// Drop closes the directory. A second call does nothing.
func (r *ReadDir) Drop() {
	if r == nil || r.f0 == nil {
		return
	}
	r.f0.drop()
}

// FmtDebug is the derived Debug: `ReadDir("<root>")`.
func (r *ReadDir) FmtDebug(f *rs.Formatter) error {
	return f.DebugTuple("ReadDir").Field(debugWith(r.f0.fmtDebug)).Finish()
}

// Path is DirEntry::path: the directory read joined with the entry's name.
func (e DirEntry) Path() string {
	return e.f0.path()
}

// Metadata is DirEntry::metadata: the entry's own metadata, not following a
// symbolic link.
func (e DirEntry) Metadata() (Metadata, error) {
	attr, err := e.f0.metadata()
	if err != nil {
		return Metadata{}, err
	}
	return Metadata{f0: attr}, nil
}

// FileType is DirEntry::file_type: the entry's type, not following a
// symbolic link.
func (e DirEntry) FileType() (FileType, error) {
	fileType, err := e.f0.fileType()
	if err != nil {
		return FileType{}, err
	}
	return FileType{f0: fileType}, nil
}

// FileName is DirEntry::file_name.
func (e DirEntry) FileName() string {
	return e.f0.fileName()
}

// FmtDebug is `impl Debug for DirEntry`: `DirEntry("<path>")`.
func (e DirEntry) FmtDebug(f *rs.Formatter) error {
	return f.DebugTuple("DirEntry").Field(rspath.Debug(e.Path())).Finish()
}

// RemoveFile is std::fs::remove_file.
func RemoveFile(path string) error {
	return sysRemoveFile(path)
}

// MetadataOf is std::fs::metadata, following symbolic links. (Go cannot name
// both the function and the type Metadata; the type keeps the name.)
func MetadataOf(path string) (Metadata, error) {
	attr, err := sysMetadata(path)
	if err != nil {
		return Metadata{}, err
	}
	return Metadata{f0: attr}, nil
}

// SymlinkMetadata is std::fs::symlink_metadata, not following a symbolic
// link at the end of the path.
func SymlinkMetadata(path string) (Metadata, error) {
	attr, err := sysSymlinkMetadata(path)
	if err != nil {
		return Metadata{}, err
	}
	return Metadata{f0: attr}, nil
}

// Rename is std::fs::rename.
func Rename(from, to string) error {
	return sysRename(from, to)
}

// Copy is std::fs::copy: the bytes copied.
func Copy(from, to string) (uint64, error) {
	return sysCopy(from, to)
}

// HardLink is std::fs::hard_link.
func HardLink(original, link string) error {
	return sysHardLink(original, link)
}

// SoftLink is the deprecated std::fs::soft_link.
func SoftLink(original, link string) error {
	return sysSymlink(original, link)
}

// ReadLink is std::fs::read_link.
func ReadLink(path string) (string, error) {
	return sysReadLink(path)
}

// Canonicalize is std::fs::canonicalize.
func Canonicalize(path string) (string, error) {
	return sysCanonicalize(path)
}

// CreateDir is std::fs::create_dir.
func CreateDir(path string) error {
	return DirBuilderNew().Create(path)
}

// CreateDirAll is std::fs::create_dir_all.
func CreateDirAll(path string) error {
	return DirBuilderNew().Recursive(true).Create(path)
}

// RemoveDir is std::fs::remove_dir.
func RemoveDir(path string) error {
	return sysRemoveDir(path)
}

// RemoveDirAll is std::fs::remove_dir_all.
func RemoveDirAll(path string) error {
	return sysRemoveDirAll(path)
}

// ReadDirOf is std::fs::read_dir. (Go cannot name both the function and the
// type ReadDir; the type keeps the name.)
func ReadDirOf(path string) (*ReadDir, error) {
	inner, err := sysReadDirOf(path)
	if err != nil {
		return nil, err
	}
	return &ReadDir{f0: inner}, nil
}

// SetPermissions is std::fs::set_permissions.
func SetPermissions(path string, perm Permissions) error {
	return sysSetPermissions(path, perm.f0)
}

// DirBuilderNew is DirBuilder::new.
func DirBuilderNew() *DirBuilder {
	return &DirBuilder{inner: newSysDirBuilder(), recursive: false}
}

// Recursive is DirBuilder::recursive.
func (b *DirBuilder) Recursive(recursive bool) *DirBuilder {
	b.recursive = recursive
	return b
}

// Create is DirBuilder::create.
func (b *DirBuilder) Create(path string) error {
	return b.create(path)
}

func (b *DirBuilder) create(path string) error {
	if b.recursive {
		return b.createDirAll(path)
	}
	return b.inner.mkdir(path)
}

func (b *DirBuilder) createDirAll(path string) error {
	// if path's parent is None, it is "/" path, which should
	// return Ok immediately
	if rspath.Eq(path, "") || rspath.Parent(path).IsNone() {
		return nil
	}

	uncreatedDirs := 0

	for ancestor := range rspath.Ancestors(path).All() {
		// for relative paths like "foo/bar", the parent of
		// "foo" will be "" which there's no need to invoke
		// a mkdir syscall on
		if rspath.Eq(ancestor, "") || rspath.Parent(ancestor).IsNone() {
			break
		}

		err := b.inner.mkdir(ancestor)
		if err == nil {
			break
		}
		kind := rsio.KindOf(err)
		if kind == rsio.ErrorKind_NotFound {
			uncreatedDirs++
			continue
		}
		// we check if the err is AlreadyExists for two reasons
		//    - in case the path exists as a *file*
		//    - and to avoid calls to .is_dir() in case of other errs
		//      (i.e. PermissionDenied)
		if kind == rsio.ErrorKind_AlreadyExists && PathIsDir(ancestor) {
			break
		}
		return err
	}

	// collect only the uncreated directories w/o letting the vec resize
	uncreatedDirsVec := make([]string, 0, uncreatedDirs)
	ancestors := rspath.Ancestors(path)
	for len(uncreatedDirsVec) < uncreatedDirs {
		uncreatedDirsVec = append(uncreatedDirsVec, ancestors.Next().Unwrap())
	}

	for i := len(uncreatedDirsVec) - 1; i >= 0; i-- {
		uncreatedDir := uncreatedDirsVec[i]
		if err := b.inner.mkdir(uncreatedDir); err != nil {
			if rsio.KindOf(err) != rsio.ErrorKind_AlreadyExists || !PathIsDir(uncreatedDir) {
				return err
			}
		}
	}

	return nil
}

// FmtDebug is the derived Debug: `DirBuilder { inner: .., recursive: .. }`.
func (b *DirBuilder) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("DirBuilder").
		Field("inner", debugWith(b.inner.fmtDebug)).
		Field("recursive", b.recursive).
		Finish()
}

// Exists is std::fs::exists: whether the path names something, following
// symbolic links, with errors other than NotFound reported.
func Exists(path string) (bool, error) {
	return sysExists(path)
}

// debugWith is a value whose Debug is the given function.
type debugWith func(f *rs.Formatter) error

func (d debugWith) FmtDebug(f *rs.Formatter) error { return d(f) }
