// Port of mj-core/src/attachment.rs.
//
// Session-private immutable image blobs. Internal image references are
// resolved only at the ACP boundary; image bytes never belong in durable
// relay commands.

package core

import (
	"bytes"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"runtime"
	"strings"

	acp "github.com/BrokkAi/acp-go/schema/unstable"
)

const MaxImages = 10
const MaxImageBytes = 700 * 1024
const AttachmentDir = "attachments"
const ArchiveAttachmentDir = "mj-attachments"
const uriPrefix = "mj-attachment:"

type AttachmentRef struct {
	Sha256   string `json:"sha256"`
	MimeType string `json:"mime_type"`
	Size     int    `json:"size"`
	Width    uint32 `json:"width"`
	Height   uint32 `json:"height"`
}

func (r *AttachmentRef) UnmarshalJSON(data []byte) error {
	type plain AttachmentRef
	return transcriptStrictDecode(data, (*plain)(r), true)
}

func NewAttachmentRef(bytes []byte, mimeType string, width, height uint32) (AttachmentRef, error) {
	reference := AttachmentRef{
		Sha256:   LowerHex(attachmentDigest(bytes)),
		MimeType: mimeType,
		Size:     len(bytes),
		Width:    width,
		Height:   height,
	}
	if err := reference.Verify(bytes); err != nil {
		return AttachmentRef{}, err
	}
	return reference, nil
}

func attachmentDigest(bytes []byte) []byte {
	sum := sha256.Sum256(bytes)
	return sum[:]
}

// attachmentIsDigest reports a lowercase hex SHA-256 digest.
func attachmentIsDigest(text string) bool {
	if len(text) != 64 {
		return false
	}
	for i := 0; i < len(text); i++ {
		if b := text[i]; !(b >= '0' && b <= '9') && !(b >= 'a' && b <= 'f') {
			return false
		}
	}
	return true
}

func (r *AttachmentRef) Validate() error {
	if !attachmentIsDigest(r.Sha256) {
		return errors.New("invalid attachment digest")
	}
	if !(r.Size > 0 && r.Size <= MaxImageBytes) {
		return errors.New("image must be at most 700 KiB")
	}
	switch r.MimeType {
	case "image/png", "image/jpeg", "image/webp":
	default:
		return errors.New("unsupported attachment format")
	}
	if !(r.Width > 0 && r.Height > 0 && uint64(r.Width)*uint64(r.Height) <= 64*1024*1024) {
		return errors.New("invalid attachment dimensions")
	}
	return nil
}

func (r *AttachmentRef) Verify(data []byte) error {
	if err := r.Validate(); err != nil {
		return err
	}
	if len(data) != r.Size || LowerHex(attachmentDigest(data)) != r.Sha256 {
		return errors.New("attachment data does not match its digest or size")
	}
	correctFormat := false
	switch r.MimeType {
	case "image/png":
		correctFormat = bytes.HasPrefix(data, []byte("\x89PNG\r\n\x1a\n"))
	case "image/jpeg":
		correctFormat = bytes.HasPrefix(data, []byte("\xff\xd8\xff"))
	case "image/webp":
		correctFormat = bytes.HasPrefix(data, []byte("RIFF")) && len(data) >= 12 && string(data[8:12]) == "WEBP"
	}
	if !correctFormat {
		return errors.New("attachment media type does not match its data")
	}
	return nil
}

func (r *AttachmentRef) URI() string {
	encoded, err := transcriptMarshal(r)
	if err != nil {
		panic("attachment metadata serializes")
	}
	return uriPrefix + base64.RawURLEncoding.EncodeToString(encoded)
}

// AttachmentRefFromURI reads an attachment reference URI. It returns nil
// for a URI that is not one.
func AttachmentRefFromURI(uri string) (*AttachmentRef, error) {
	encoded, ok := strings.CutPrefix(uri, uriPrefix)
	if !ok {
		return nil, nil
	}
	if len(encoded) > 1024 {
		return nil, errors.New("attachment reference is too large")
	}
	decoded, err := base64.RawURLEncoding.DecodeString(encoded)
	if err != nil {
		return nil, fmt.Errorf("decode attachment reference: %w", err)
	}
	var reference AttachmentRef
	if err := json.Unmarshal(decoded, &reference); err != nil {
		return nil, err
	}
	if err := reference.Validate(); err != nil {
		return nil, err
	}
	return &reference, nil
}

func (r *AttachmentRef) ContentBlock() acp.ContentBlock {
	uri := r.URI()
	return acp.ContentBlock{Image: &acp.ImageContent{Data: "", MimeType: r.MimeType, URI: &uri}}
}

// ImageReference reads the attachment reference an image carries, if any.
func ImageReference(image *acp.ImageContent) (*AttachmentRef, error) {
	if image.URI == nil {
		return nil, nil
	}
	reference, err := AttachmentRefFromURI(*image.URI)
	if err != nil {
		return nil, err
	}
	if reference != nil && !(image.Data == "" && image.MimeType == reference.MimeType) {
		return nil, errors.New("attachment reference conflicts with image data")
	}
	return reference, nil
}

func References(prompt []acp.ContentBlock) ([]AttachmentRef, error) {
	images := 0
	result := []AttachmentRef{}
	for i := range prompt {
		if prompt[i].Image == nil {
			continue
		}
		images++
		reference, err := ImageReference(prompt[i].Image)
		if err != nil {
			return nil, err
		}
		if reference != nil {
			result = append(result, *reference)
		}
	}
	// Older workers could persist prompts with more inline images. Keep those
	// historical prompts readable during replay; the limit applies to the
	// content-addressed attachment form introduced by this protocol.
	if len(result) != 0 && images > MaxImages {
		return nil, errors.New("a message can contain at most 10 images")
	}
	return result, nil
}

func HasReferences(prompt []acp.ContentBlock) bool {
	for _, block := range prompt {
		if block.Image != nil && block.Image.URI != nil && strings.HasPrefix(*block.Image.URI, uriPrefix) {
			return true
		}
	}
	return false
}

type AttachmentStore struct {
	root string
}

func NewAttachmentStore(root string) AttachmentStore {
	return AttachmentStore{root: root}
}

func AttachmentStoreController(sessionID string) (AttachmentStore, error) {
	if err := ValidateID("session", sessionID); err != nil {
		return AttachmentStore{}, err
	}
	return NewAttachmentStore(filepath.Join(SessionsDir(), sessionID, AttachmentDir)), nil
}

func AttachmentStoreWorker(root string) AttachmentStore {
	return NewAttachmentStore(filepath.Join(root, AttachmentDir))
}

func (s *AttachmentStore) Root() string { return s.root }

// lock takes the store's file lock. Release it by closing the result.
func (s *AttachmentStore) lock() (*FileLock, error) {
	parent := filepath.Dir(s.root)
	if parent == s.root {
		return nil, errors.New("attachment store has no parent")
	}
	if err := os.MkdirAll(parent, 0o777); err != nil {
		return nil, err
	}
	file, err := os.OpenFile(attachmentWithExtension(s.root, "lock"), os.O_RDWR|os.O_CREATE, 0o666)
	if err != nil {
		return nil, err
	}
	if err := attachmentLockFile(file); err != nil {
		file.Close()
		return nil, fmt.Errorf("lock attachment store: %w", err)
	}
	return NewFileLock(file), nil
}

// attachmentWithExtension is Rust's Path::with_extension: path with its file
// name's extension replaced by (or extended with) extension.
func attachmentWithExtension(path, extension string) string {
	name := filepath.Base(path)
	stem := name
	if dot := strings.LastIndexByte(name, '.'); dot > 0 {
		stem = name[:dot]
	}
	return filepath.Join(filepath.Dir(path), stem+"."+extension)
}

func (s *AttachmentStore) ensureOpen() error {
	_, err := os.Lstat(attachmentWithExtension(s.root, "deleted"))
	if err == nil {
		return errors.New("this session was deleted; the attachment was not saved")
	}
	if errors.Is(err, fs.ErrNotExist) {
		return nil
	}
	return err
}

// RemoveSessionData writes a tombstone that prevents a late
// clipboard/upload task recreating a deleted session's store. The lock also
// coordinates native and daemon processes.
func (s *AttachmentStore) RemoveSessionData() error {
	lock, err := s.lock()
	if err != nil {
		return err
	}
	defer lock.Drop()
	tombstone, err := os.Create(attachmentWithExtension(s.root, "deleted"))
	if err != nil {
		return err
	}
	syncErr := tombstone.Sync()
	tombstone.Close()
	if syncErr != nil {
		return syncErr
	}
	if err := os.RemoveAll(s.root); err != nil && !errors.Is(err, fs.ErrNotExist) {
		return err
	}
	if runtime.GOOS != "windows" {
		return attachmentSyncDir(filepath.Dir(s.root))
	}
	return nil
}

// attachmentSyncDir makes a directory's entries durable (Unix only).
func attachmentSyncDir(path string) error {
	dir, err := os.Open(path)
	if err != nil {
		return err
	}
	defer dir.Close()
	return dir.Sync()
}

// attachmentPersist writes data to a temporary file in dir, syncs it and
// renames it over path in one step, as tempfile's persist does.
func attachmentPersist(dir, path string, data []byte) error {
	file, err := os.CreateTemp(dir, ".tmp")
	if err != nil {
		return err
	}
	name := file.Name()
	if _, err := file.Write(data); err != nil {
		file.Close()
		os.Remove(name)
		return err
	}
	if err := file.Sync(); err != nil {
		file.Close()
		os.Remove(name)
		return err
	}
	if err := file.Close(); err != nil {
		os.Remove(name)
		return err
	}
	if err := os.Rename(name, path); err != nil {
		os.Remove(name)
		return err
	}
	if runtime.GOOS != "windows" {
		return attachmentSyncDir(dir)
	}
	return nil
}

func (s *AttachmentStore) Install(reference *AttachmentRef, data []byte) error {
	lock, err := s.lock()
	if err != nil {
		return err
	}
	defer lock.Drop()
	if err := s.ensureOpen(); err != nil {
		return err
	}
	if err := reference.Verify(data); err != nil {
		return err
	}
	if err := os.MkdirAll(s.root, 0o777); err != nil {
		return fmt.Errorf("create image attachment store: %w", err)
	}
	path := filepath.Join(s.root, reference.Sha256)
	valid, err := s.existingAttachmentValid(reference)
	if err != nil {
		return err
	}
	if valid != nil && *valid {
		return nil
	}
	// The store lock serializes installers, and the rename replaces a corrupt
	// regular blob with one verified above in a single step.
	return attachmentPersist(s.root, path, data)
}

func (s *AttachmentStore) Read(reference *AttachmentRef) ([]byte, error) {
	if err := reference.Validate(); err != nil {
		return nil, err
	}
	path := filepath.Join(s.root, reference.Sha256)
	info, err := os.Lstat(path)
	if err != nil {
		return nil, fmt.Errorf("missing image attachment %s: %w", reference.Sha256, err)
	}
	if !info.Mode().IsRegular() {
		return nil, errors.New("attachment is not a regular file")
	}
	info, err = os.Stat(path)
	if err != nil {
		return nil, err
	}
	if info.Size() != int64(reference.Size) {
		return nil, errors.New("attachment has incorrect size")
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	if err := reference.Verify(data); err != nil {
		return nil, err
	}
	return data, nil
}

func (s *AttachmentStore) Contains(reference *AttachmentRef) (bool, error) {
	if err := reference.Validate(); err != nil {
		return false, err
	}
	valid, err := s.existingAttachmentValid(reference)
	if err != nil {
		return false, err
	}
	return valid != nil && *valid, nil
}

// existingAttachmentValid returns whether the existing path is a valid copy
// of reference. Missing files (nil) and content corruption are ordinary
// cache misses. Filesystem failures and non-regular paths remain errors so
// callers do not hide a broken store or unsafe path.
func (s *AttachmentStore) existingAttachmentValid(reference *AttachmentRef) (*bool, error) {
	path := filepath.Join(s.root, reference.Sha256)
	info, err := os.Lstat(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("inspect image attachment %s: %w", reference.Sha256, err)
	}
	if !info.Mode().IsRegular() {
		return nil, errors.New("attachment is not a regular file")
	}
	valid := false
	if info.Size() != int64(reference.Size) {
		return &valid, nil
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read image attachment %s: %w", reference.Sha256, err)
	}
	valid = reference.Verify(data) == nil
	return &valid, nil
}

// ArchiveArtifacts archives blobs independently of any prompt; references
// already live in canonical history. Unfinished temporary files are never
// exported.
func (s *AttachmentStore) ArchiveArtifacts() ([]NativeArtifact, error) {
	artifacts := []NativeArtifact{}
	entries, err := os.ReadDir(s.root)
	if errors.Is(err, fs.ErrNotExist) {
		return artifacts, nil
	}
	if err != nil {
		return nil, err
	}
	for _, entry := range entries {
		digest := entry.Name()
		if !attachmentIsDigest(digest) {
			continue
		}
		if !entry.Type().IsRegular() {
			return nil, errors.New("attachment is not a regular file")
		}
		info, err := entry.Info()
		if err != nil {
			return nil, err
		}
		if info.Size() > MaxImageBytes {
			return nil, errors.New("attachment exceeds image budget")
		}
		data, err := os.ReadFile(filepath.Join(s.root, digest))
		if err != nil {
			return nil, err
		}
		if LowerHex(attachmentDigest(data)) != digest {
			return nil, errors.New("corrupt attachment in checkpoint")
		}
		artifacts = append(artifacts, NativeArtifact{
			RelativePath: filepath.Join(ArchiveAttachmentDir, digest),
			Data:         NativeArtifactDataBytes(data),
			Mode:         0o600,
		})
	}
	return artifacts, nil
}

func (s *AttachmentStore) RestoreArtifact(relativePath string, data []byte) (bool, error) {
	lock, err := s.lock()
	if err != nil {
		return false, err
	}
	defer lock.Drop()
	if err := s.ensureOpen(); err != nil {
		return false, err
	}
	relative, ok := attachmentStripPrefix(relativePath, ArchiveAttachmentDir)
	if !ok {
		return false, nil
	}
	if len(relative) != 1 {
		return false, errors.New("invalid archived attachment path")
	}
	digest := relative[0]
	if !(len(data) <= MaxImageBytes && len(data) != 0 && LowerHex(attachmentDigest(data)) == digest) {
		return false, errors.New("archived attachment failed integrity check")
	}
	if err := os.MkdirAll(s.root, 0o777); err != nil {
		return false, err
	}
	if err := attachmentPersist(s.root, filepath.Join(s.root, digest), data); err != nil {
		return false, err
	}
	return true, nil
}

// attachmentStripPrefix is Rust's Path::strip_prefix by component: the
// normal components of path after prefix, and whether path starts with it.
func attachmentStripPrefix(path, prefix string) ([]string, bool) {
	components := attachmentComponents(path)
	prefixComponents := attachmentComponents(prefix)
	if len(components) < len(prefixComponents) {
		return nil, false
	}
	for i, component := range prefixComponents {
		if components[i] != component {
			return nil, false
		}
	}
	return components[len(prefixComponents):], true
}

// attachmentComponents splits a relative path into its components as Rust's
// Path::components does: empty and "." components after the first are
// dropped.
func attachmentComponents(path string) []string {
	var components []string
	for i, part := range strings.FieldsFunc(filepath.ToSlash(path), func(r rune) bool { return r == '/' }) {
		if part == "." && i > 0 {
			continue
		}
		components = append(components, part)
	}
	return components
}

func (s *AttachmentStore) Resolve(prompt []acp.ContentBlock) error {
	if _, err := References(prompt); err != nil {
		return err
	}
	for i := range prompt {
		image := prompt[i].Image
		if image == nil {
			continue
		}
		reference, err := ImageReference(image)
		if err != nil {
			return err
		}
		if reference == nil {
			continue
		}
		data, err := s.Read(reference)
		if err != nil {
			return err
		}
		// Replace the image rather than writing through the pointer, which
		// other blocks or the caller may share.
		resolved := *image
		resolved.Data = base64.StdEncoding.EncodeToString(data)
		resolved.URI = nil
		prompt[i].Image = &resolved
	}
	return nil
}
