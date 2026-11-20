// Port of mj-client/src/daemon/chunk_bytes.rs.

// Binary reply fragments use one base64 string instead of JSON integers.

package client

import (
	"encoding/base64"
	"encoding/json"
)

// chunkBytesSerialize writes bytes as one standard, padded base64 JSON
// string. An empty or nil slice is "".
func chunkBytesSerialize(bytes []byte) ([]byte, error) {
	return daemonMarshalJSON(base64.StdEncoding.EncodeToString(bytes))
}

// chunkBytesDeserialize reads the base64 JSON string chunkBytesSerialize
// writes.
func chunkBytesDeserialize(data []byte) ([]byte, error) {
	var encoded string
	if err := json.Unmarshal(data, &encoded); err != nil {
		return nil, err
	}
	return base64.StdEncoding.DecodeString(encoded)
}
