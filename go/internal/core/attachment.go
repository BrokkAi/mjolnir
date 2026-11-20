// Port of mj-core/src/attachment.rs.
//
// Session-private immutable image blobs. Internal image references are
// resolved only at the ACP boundary; image bytes never belong in durable
// relay commands.

package core

import (
	"crypto/sha256"
	"encoding/base64"
	"path/filepath"
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

func (r *AttachmentRef) Validate() error {
	panic("unported: mj-core/src/attachment.rs AttachmentRef::validate")
}

func (r *AttachmentRef) Verify(bytes []byte) error {
	panic("unported: mj-core/src/attachment.rs AttachmentRef::verify")
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
	panic("unported: mj-core/src/attachment.rs AttachmentRef::from_uri")
}

func (r *AttachmentRef) ContentBlock() acp.ContentBlock {
	uri := r.URI()
	return acp.ContentBlock{Image: &acp.ImageContent{Data: "", MimeType: r.MimeType, URI: &uri}}
}

// ImageReference reads the attachment reference an image carries, if any.
func ImageReference(image *acp.ImageContent) (*AttachmentRef, error) {
	panic("unported: mj-core/src/attachment.rs image_reference")
}

func References(prompt []acp.ContentBlock) ([]AttachmentRef, error) {
	panic("unported: mj-core/src/attachment.rs references")
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
	panic("unported: mj-core/src/attachment.rs AttachmentStore::lock")
}

func (s *AttachmentStore) ensureOpen() error {
	panic("unported: mj-core/src/attachment.rs AttachmentStore::ensure_open")
}

// RemoveSessionData writes a tombstone that prevents a late
// clipboard/upload task recreating a deleted session's store. The lock also
// coordinates native and daemon processes.
func (s *AttachmentStore) RemoveSessionData() error {
	panic("unported: mj-core/src/attachment.rs AttachmentStore::remove_session_data")
}

func (s *AttachmentStore) Install(reference *AttachmentRef, bytes []byte) error {
	panic("unported: mj-core/src/attachment.rs AttachmentStore::install")
}

func (s *AttachmentStore) Read(reference *AttachmentRef) ([]byte, error) {
	panic("unported: mj-core/src/attachment.rs AttachmentStore::read")
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
	panic("unported: mj-core/src/attachment.rs AttachmentStore::existing_attachment_valid")
}

// ArchiveArtifacts archives blobs independently of any prompt; references
// already live in canonical history. Unfinished temporary files are never
// exported.
func (s *AttachmentStore) ArchiveArtifacts() ([]NativeArtifact, error) {
	panic("unported: mj-core/src/attachment.rs AttachmentStore::archive_artifacts")
}

func (s *AttachmentStore) RestoreArtifact(relativePath string, bytes []byte) (bool, error) {
	panic("unported: mj-core/src/attachment.rs AttachmentStore::restore_artifact")
}

func (s *AttachmentStore) Resolve(prompt []acp.ContentBlock) error {
	panic("unported: mj-core/src/attachment.rs AttachmentStore::resolve")
}
