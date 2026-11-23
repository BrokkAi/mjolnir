//go:build linux

// The parts of the mbx remote cache wire contract the mbx proxy
// (mbx_proxy.go) serves. Rust takes them from the mbx-cache-protocol crate
// (0.5.23); no Go module provides that contract, so the records, checks and
// canonical JSON the proxy relies on are written here, with that crate's
// shapes and error text.

package microvm

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"hash"
	"io"
	"os"
	"strconv"

	"lukechampine.com/blake3"
)

// Constants of protocol version one.
const (
	microvmMbxProtocolVersion            = 1
	microvmMbxNamespaceHeader            = "mbx-cache-namespace"
	microvmMbxActionResultMediaType      = "application/vnd.mbx.cache-action-result.v1+json"
	microvmMbxTaskActionManifestMedia    = "application/vnd.mbx.cache-task-action-manifest.v1+json"
	microvmMbxBlobMediaType              = "application/octet-stream"
	microvmMbxMaxTaskActionPredictions   = 16 * 1024
	microvmMbxMaxActionPredictionPayload = 256 * 1024
)

// microvmMbxDigest is the algorithm-tagged digest and exact byte length of
// a cache object.
type microvmMbxDigest struct {
	Algorithm string `json:"algorithm"`
	Hash      string `json:"hash"`
	Size      uint64 `json:"size"`
}

// microvmMbxDigestBlake3 is Digest::blake3: the BLAKE3 digest of bytes.
func microvmMbxDigestBlake3(data []byte) microvmMbxDigest {
	sum := blake3.Sum256(data)
	return microvmMbxDigest{Algorithm: "blake3", Hash: hex.EncodeToString(sum[:]), Size: uint64(len(data))}
}

// validate checks the algorithm and the lowercase hexadecimal hash.
func (d microvmMbxDigest) validate() error {
	if d.Algorithm != "blake3" && d.Algorithm != "sha256" {
		return errors.New("unsupported remote cache digest algorithm")
	}
	if len(d.Hash) != 64 {
		return errors.New("invalid remote cache digest")
	}
	for i := 0; i < len(d.Hash); i++ {
		if c := d.Hash[i]; !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f') {
			return errors.New("invalid remote cache digest")
		}
	}
	return nil
}

// matchesFile streams the file at path and reports whether it has this
// digest and length.
func (d microvmMbxDigest) matchesFile(path string) (bool, error) {
	if err := d.validate(); err != nil {
		return false, err
	}
	file, err := os.Open(path)
	if err != nil {
		return false, microvmIOError(err)
	}
	defer file.Close()
	var hasher hash.Hash = sha256.New()
	if d.Algorithm == "blake3" {
		hasher = blake3.New(32, nil)
	}
	size, err := io.Copy(hasher, file)
	if err != nil {
		return false, microvmIOError(err)
	}
	return d.Size == uint64(size) && d.Hash == hex.EncodeToString(hasher.Sum(nil)), nil
}

// UnmarshalJSON decodes as serde does with deny_unknown_fields.
func (d *microvmMbxDigest) UnmarshalJSON(data []byte) error {
	object, err := microvmMbxStrictObject(data, []string{"algorithm", "hash", "size"}, []string{"algorithm", "hash", "size"})
	if err != nil {
		return err
	}
	if err := microvmMbxField(object, "algorithm", &d.Algorithm); err != nil {
		return err
	}
	if err := microvmMbxField(object, "hash", &d.Hash); err != nil {
		return err
	}
	return microvmMbxField(object, "size", &d.Size)
}

// microvmMbxActionResult is a canonical action-result record referencing
// objects in the CAS.
type microvmMbxActionResult struct {
	Action     microvmMbxDigest
	Metadata   *microvmMbxDigest
	OutputRoot *microvmMbxDigest
	Version    uint8
}

// UnmarshalJSON decodes as serde does with deny_unknown_fields.
func (r *microvmMbxActionResult) UnmarshalJSON(data []byte) error {
	object, err := microvmMbxStrictObject(data, []string{"action", "metadata", "output_root", "version"}, []string{"action", "version"})
	if err != nil {
		return err
	}
	if err := microvmMbxField(object, "action", &r.Action); err != nil {
		return err
	}
	for name, target := range map[string]**microvmMbxDigest{"metadata": &r.Metadata, "output_root": &r.OutputRoot} {
		if raw, ok := object[name]; ok && !bytes.Equal(bytes.TrimSpace(raw), []byte("null")) {
			var digest microvmMbxDigest
			if err := json.Unmarshal(raw, &digest); err != nil {
				return err
			}
			*target = &digest
		}
	}
	return microvmMbxField(object, "version", &r.Version)
}

// microvmMbxActionPrediction is the adapter-owned data needed to
// reconstruct an action from a prior task run.
type microvmMbxActionPrediction struct {
	Invocation microvmMbxDigest
	Action     microvmMbxDigest
	Adapter    string
	Payload    string
}

// UnmarshalJSON decodes as serde does with deny_unknown_fields.
func (p *microvmMbxActionPrediction) UnmarshalJSON(data []byte) error {
	fields := []string{"invocation", "action", "adapter", "payload"}
	object, err := microvmMbxStrictObject(data, fields, fields)
	if err != nil {
		return err
	}
	if err := microvmMbxField(object, "invocation", &p.Invocation); err != nil {
		return err
	}
	if err := microvmMbxField(object, "action", &p.Action); err != nil {
		return err
	}
	if err := microvmMbxField(object, "adapter", &p.Adapter); err != nil {
		return err
	}
	return microvmMbxField(object, "payload", &p.Payload)
}

// validate checks the version-one wire invariants, naming the one that
// fails.
func (p microvmMbxActionPrediction) validate() error {
	reason := ""
	switch {
	case p.Action.Algorithm != "blake3" || p.Action.validate() != nil:
		reason = "action digest is not a valid blake3 digest"
	case p.Invocation.Algorithm != "blake3" || p.Invocation.validate() != nil:
		reason = "invocation digest is not a valid blake3 digest"
	case p.Adapter == "":
		reason = "adapter name is empty"
	case !microvmMbxValidAdapterName(p.Adapter):
		reason = fmt.Sprintf("adapter name %s is not alphanumeric, '-', or '_'", microvmDebugString(p.Adapter))
	case len(p.Payload) > microvmMbxMaxActionPredictionPayload:
		reason = fmt.Sprintf("payload is %d bytes, over the %d byte limit", len(p.Payload), microvmMbxMaxActionPredictionPayload)
	case !json.Valid([]byte(p.Payload)):
		reason = "payload is not valid JSON"
	default:
		return nil
	}
	return errors.New("invalid action prediction: " + reason)
}

// microvmMbxTaskActionManifest is the predictions associated with one
// stable task identity.
type microvmMbxTaskActionManifest struct {
	Version     uint8
	Task        string
	Predictions []microvmMbxActionPrediction
}

// UnmarshalJSON decodes as serde does with deny_unknown_fields.
func (m *microvmMbxTaskActionManifest) UnmarshalJSON(data []byte) error {
	fields := []string{"version", "task", "predictions"}
	object, err := microvmMbxStrictObject(data, fields, fields)
	if err != nil {
		return err
	}
	if err := microvmMbxField(object, "version", &m.Version); err != nil {
		return err
	}
	if err := microvmMbxField(object, "task", &m.Task); err != nil {
		return err
	}
	return microvmMbxField(object, "predictions", &m.Predictions)
}

// validate reports whether the manifest satisfies the version-one wire
// invariants.
func (m microvmMbxTaskActionManifest) validate() bool {
	if m.Version != 1 || !microvmMbxValidTaskIdentity(m.Task) || len(m.Predictions) > microvmMbxMaxTaskActionPredictions {
		return false
	}
	invocations := map[microvmMbxDigest]bool{}
	for _, prediction := range m.Predictions {
		if prediction.validate() != nil || invocations[prediction.Invocation] {
			return false
		}
		invocations[prediction.Invocation] = true
	}
	return true
}

// selectorDigest is the digest selecting this task identity's action
// manifest. The task identity must be valid.
func (m microvmMbxTaskActionManifest) selectorDigest() microvmMbxDigest {
	if !microvmMbxValidTaskIdentity(m.Task) {
		panic("manifest task identity must be valid")
	}
	var selector bytes.Buffer
	selector.WriteString(`{"kind":"task_action_manifest","task":`)
	microvmMbxCanonicalString(&selector, m.Task)
	selector.WriteString(`,"version":1}`)
	return microvmMbxDigestBlake3(selector.Bytes())
}

// canonicalJSON is the manifest in the JSON Canonicalization Scheme:
// members sorted by name, no whitespace, and ECMAScript number and string
// forms.
func (m microvmMbxTaskActionManifest) canonicalJSON() []byte {
	var out bytes.Buffer
	digest := func(d microvmMbxDigest) {
		out.WriteString(`{"algorithm":`)
		microvmMbxCanonicalString(&out, d.Algorithm)
		out.WriteString(`,"hash":`)
		microvmMbxCanonicalString(&out, d.Hash)
		out.WriteString(`,"size":`)
		out.WriteString(microvmMbxCanonicalNumber(d.Size))
		out.WriteString(`}`)
	}
	out.WriteString(`{"predictions":[`)
	for i, prediction := range m.Predictions {
		if i > 0 {
			out.WriteString(",")
		}
		out.WriteString(`{"action":`)
		digest(prediction.Action)
		out.WriteString(`,"adapter":`)
		microvmMbxCanonicalString(&out, prediction.Adapter)
		out.WriteString(`,"invocation":`)
		digest(prediction.Invocation)
		out.WriteString(`,"payload":`)
		microvmMbxCanonicalString(&out, prediction.Payload)
		out.WriteString(`}`)
	}
	out.WriteString(`],"task":`)
	microvmMbxCanonicalString(&out, m.Task)
	out.WriteString(`,"version":`)
	out.WriteString(microvmMbxCanonicalNumber(uint64(m.Version)))
	out.WriteString(`}`)
	return out.Bytes()
}

// microvmMbxCanonicalNumber writes an integer as JCS does: as the
// ECMAScript rendering of the nearest double.
func microvmMbxCanonicalNumber(value uint64) string {
	return strconv.FormatFloat(float64(value), 'f', -1, 64)
}

// microvmMbxCanonicalString writes text as JCS does: only `"`, `\` and
// control characters are escaped, everything else is literal UTF-8.
func microvmMbxCanonicalString(out *bytes.Buffer, text string) {
	out.WriteByte('"')
	for i := 0; i < len(text); i++ {
		c := text[i]
		switch c {
		case '"':
			out.WriteString(`\"`)
		case '\\':
			out.WriteString(`\\`)
		case '\b':
			out.WriteString(`\b`)
		case '\f':
			out.WriteString(`\f`)
		case '\n':
			out.WriteString(`\n`)
		case '\r':
			out.WriteString(`\r`)
		case '\t':
			out.WriteString(`\t`)
		default:
			if c < 0x20 {
				fmt.Fprintf(out, `\u%04x`, c)
			} else {
				out.WriteByte(c)
			}
		}
	}
	out.WriteByte('"')
}

func microvmMbxValidAdapterName(value string) bool {
	if value == "" {
		return false
	}
	for i := 0; i < len(value); i++ {
		if c := value[i]; !microvmIsASCIIAlphanumeric(c) && c != '-' && c != '_' {
			return false
		}
	}
	return true
}

func microvmMbxValidTaskIdentity(value string) bool {
	if len(value) != 64 {
		return false
	}
	for i := 0; i < len(value); i++ {
		if c := value[i]; !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f') {
			return false
		}
	}
	return true
}

// microvmMbxCapabilities is the capability record a cache service
// advertises, in serde's member order.
type microvmMbxCapabilities struct {
	Protocol         microvmMbxCapabilityProtocol `json:"protocol"`
	DigestAlgorithms []string                     `json:"digest_algorithms"`
	Compressors      []string                     `json:"compressors"`
	ActionKinds      map[string]any               `json:"action_kinds"`
	Features         microvmMbxCapabilityFeatures `json:"features"`
	Limits           microvmMbxCapabilityLimits   `json:"limits"`
}

type microvmMbxCapabilityProtocol struct {
	Major uint8 `json:"major"`
	Minor uint8 `json:"minor"`
}

type microvmMbxCapabilityFeatures struct {
	ActionManifests    bool `json:"action_manifests"`
	Batch              bool `json:"batch"`
	ActionBatch        bool `json:"action_batch"`
	BlobPacks          bool `json:"blob_packs"`
	BlobPackUploads    bool `json:"blob_pack_uploads"`
	ResumableUploads   bool `json:"resumable_uploads"`
	DelegatedTransfers bool `json:"delegated_transfers"`
	ActionPromises     bool `json:"action_promises"`
}

type microvmMbxCapabilityLimits struct {
	MaxBatchItems      uint64 `json:"max_batch_items"`
	MaxInlineBlobBytes uint64 `json:"max_inline_blob_bytes"`
	MaxBlobBytes       uint64 `json:"max_blob_bytes"`
	MaxPackBytes       uint64 `json:"max_pack_bytes"`
}

// microvmMbxNewCapabilities is Capabilities::new: a baseline advertisement
// claiming no optional features.
func microvmMbxNewCapabilities(major, minor uint8) microvmMbxCapabilities {
	return microvmMbxCapabilities{
		Protocol:         microvmMbxCapabilityProtocol{Major: major, Minor: minor},
		DigestAlgorithms: []string{},
		Compressors:      []string{},
		ActionKinds:      map[string]any{},
	}
}

// microvmMbxStrictObject reads a JSON object as serde's derive does with
// deny_unknown_fields: member names match exactly, an unknown or repeated
// member is an error, and every required member must be present.
func microvmMbxStrictObject(data []byte, known []string, required []string) (map[string]json.RawMessage, error) {
	decoder := json.NewDecoder(bytes.NewReader(data))
	token, err := decoder.Token()
	if err != nil {
		return nil, err
	}
	if delimiter, ok := token.(json.Delim); !ok || delimiter != '{' {
		return nil, fmt.Errorf("invalid type: %v, expected a struct", token)
	}
	object := map[string]json.RawMessage{}
	for decoder.More() {
		token, err := decoder.Token()
		if err != nil {
			return nil, err
		}
		name, _ := token.(string)
		isKnown := false
		for _, field := range known {
			isKnown = isKnown || field == name
		}
		if !isKnown {
			return nil, fmt.Errorf("unknown field `%s`, expected one of `%s`", name, microvmMbxJoinBackticked(known))
		}
		if _, repeated := object[name]; repeated {
			return nil, fmt.Errorf("duplicate field `%s`", name)
		}
		var value json.RawMessage
		if err := decoder.Decode(&value); err != nil {
			return nil, err
		}
		object[name] = value
	}
	for _, name := range required {
		if _, ok := object[name]; !ok {
			return nil, fmt.Errorf("missing field `%s`", name)
		}
	}
	return object, nil
}

func microvmMbxJoinBackticked(names []string) string {
	text := ""
	for i, name := range names {
		if i > 0 {
			text += "`, `"
		}
		text += name
	}
	return text
}

// microvmMbxField decodes a required member, refusing null as serde does
// for a field that is not an Option.
func microvmMbxField(object map[string]json.RawMessage, name string, target any) error {
	raw := object[name]
	if bytes.Equal(bytes.TrimSpace(raw), []byte("null")) {
		return fmt.Errorf("invalid type: null for field `%s`", name)
	}
	return json.Unmarshal(raw, target)
}
