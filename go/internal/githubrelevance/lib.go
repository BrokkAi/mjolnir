// Port of mj-github-relevance/src/lib.rs.

// Package githubrelevance decides which GitHub events matter to a session:
// the small classifier model that runs on the daemon's machine, the download
// and verification of its weights and native runtime, and the notices built
// from its verdicts. It is the Go port of the mj-github-relevance crate
// (`brokk-mj-github-relevance`, library `mj_github_relevance`).
//
// The Guardian also uses the package's shared model cache and ONNX Runtime
// (guardian_classifier.go).
//
// Library replacements: the ort crate (ONNX Runtime, load-dynamic, API 23)
// becomes the small binding in onnxruntime.go, and the tokenizers crate the
// tokenizer in hf_tokenizer.go (the frozen history tokenizer is still core's
// HistoryTokenizer); reqwest is net/http, tokio's CancellationToken a
// context.Context with its CancelFunc, tokio's Notify a core.Watch or a
// one-slot channel, and tokio's Semaphore a buffered channel holding one
// token per admitted permit.
//
// Rust's test-only items (`enable_test_fixtures_on_startup!` and the
// `signing_key` and `test_isolation` re-exports from mj-targets) belong to
// this package's tests.
package githubrelevance

import (
	"bytes"
	"encoding/json"
	"fmt"
)

// serdeJSON encodes as serde_json::to_vec does: `<`, `>` and `&` stay
// literal and there is no trailing newline.
func serdeJSON(v any) ([]byte, error) {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buffer.Bytes(), []byte("\n")), nil
}

// requireFields is serde's missing-field check for required members of a
// JSON object.
func requireFields(data []byte, names ...string) error {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return err
	}
	for _, name := range names {
		if _, ok := fields[name]; !ok {
			return fmt.Errorf("missing field `%s`", name)
		}
	}
	return nil
}
