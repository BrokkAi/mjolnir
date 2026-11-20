// Port of mj-client/src/session_image.rs.

// Project image build operations, shared by the HTTP API and its clients.

package client

import (
	"encoding/json"
	"errors"
	"fmt"
)

// ImageRequest asks the daemon to build a project image. Unknown fields are
// refused (serde deny_unknown_fields).
type ImageRequest struct {
	Path      string            `json:"path"`
	Bundle    *string           `json:"bundle"`
	Target    *string           `json:"target"`
	Name      *string           `json:"name"`
	SessionID *string           `json:"session_id"`
	Files     map[string]string `json:"files"`
	Print     bool              `json:"print"`
}

// MarshalJSON writes an absent file table as {}, as serde writes an empty
// BTreeMap.
func (r ImageRequest) MarshalJSON() ([]byte, error) {
	type plain ImageRequest
	value := plain(r)
	if value.Files == nil {
		value.Files = map[string]string{}
	}
	return feedMarshalJSON(value)
}

// UnmarshalJSON requires `path` and `files`, defaults `print`, and refuses
// unknown fields.
func (r *ImageRequest) UnmarshalJSON(data []byte) error {
	type plain ImageRequest
	var value plain
	if err := feedDecodeStruct(data, &value, true, "path", "files"); err != nil {
		return err
	}
	*r = ImageRequest(value)
	return nil
}

// ImageStarted names the operation an image request started.
type ImageStarted struct {
	OperationID string `json:"operation_id"`
}

func (s *ImageStarted) UnmarshalJSON(data []byte) error {
	type plain ImageStarted
	return feedDecodeStruct(data, (*plain)(s), false, "operation_id")
}

// ImageFollow is the log of an image operation from an offset, and its
// result once it has one.
type ImageFollow struct {
	Log        string `json:"log"`
	NextOffset int    `json:"next_offset"`
	// Result is nil while the operation runs.
	Result *ImageOutcome `json:"result"`
}

func (f *ImageFollow) UnmarshalJSON(data []byte) error {
	type plain ImageFollow
	return feedDecodeStruct(data, (*plain)(f), false, "log", "next_offset")
}

// ImageOutcome is Rust's Result<ImageResult, String>: exactly one of Ok and
// Err is set. It serializes as serde writes a Result: {"Ok":...} or
// {"Err":"..."}.
type ImageOutcome struct {
	Ok  *ImageResult
	Err *string
}

func (o ImageOutcome) MarshalJSON() ([]byte, error) {
	if o.Err != nil {
		return feedMarshalJSON(map[string]string{"Err": *o.Err})
	}
	if o.Ok == nil {
		return nil, errors.New("image outcome holds neither a result nor an error")
	}
	return feedMarshalJSON(map[string]ImageResult{"Ok": *o.Ok})
}

func (o *ImageOutcome) UnmarshalJSON(data []byte) error {
	var members map[string]json.RawMessage
	if err := json.Unmarshal(data, &members); err != nil {
		return err
	}
	if len(members) != 1 {
		return errors.New("invalid value: map, expected map with a single key")
	}
	for tag, content := range members {
		switch tag {
		case "Ok":
			var result ImageResult
			if err := json.Unmarshal(content, &result); err != nil {
				return err
			}
			*o = ImageOutcome{Ok: &result}
		case "Err":
			var message string
			if err := json.Unmarshal(content, &message); err != nil {
				return err
			}
			*o = ImageOutcome{Err: &message}
		default:
			return fmt.Errorf("unknown variant `%s`, expected `Ok` or `Err`", tag)
		}
	}
	return nil
}

// ImageResult is what a finished image operation produced.
type ImageResult struct {
	Containerfile *string       `json:"containerfile"`
	Image         *string       `json:"image"`
	Skipped       bool          `json:"skipped"`
	Status        []ImageStatus `json:"status"`
}

func (r ImageResult) MarshalJSON() ([]byte, error) {
	type plain ImageResult
	value := plain(r)
	if value.Status == nil {
		value.Status = []ImageStatus{}
	}
	return feedMarshalJSON(value)
}

func (r *ImageResult) UnmarshalJSON(data []byte) error {
	type plain ImageResult
	return feedDecodeStruct(data, (*plain)(r), false, "skipped", "status")
}

// ImageStatus is whether one platform's image is current.
type ImageStatus struct {
	Platform string `json:"platform"`
	Image    string `json:"image"`
	// Current is nil when no configured builder can inspect this platform.
	Current *bool `json:"current"`
}

func (s *ImageStatus) UnmarshalJSON(data []byte) error {
	type plain ImageStatus
	return feedDecodeStruct(data, (*plain)(s), false, "platform", "image")
}
