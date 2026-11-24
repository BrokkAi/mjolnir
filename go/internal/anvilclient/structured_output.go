// Port of brokk-anvil-client 0.28.6 src/structured_output.rs (the part Mjolnir uses).

package anvilclient

// StructuredOutputRequest asks a backend for output matching a JSON Schema.
type StructuredOutputRequest struct {
	SchemaName string `json:"schema_name"`
	// Schema is a JSON value.
	Schema        any  `json:"schema"`
	AllowCoercion bool `json:"allow_coercion"`
	// PreferJSONObject requests basic JSON mode (`response_format: {type:
	// json_object}`) instead of strict `json_schema`. The schema is still
	// used locally for validation and coercion; it just isn't sent to the
	// provider as a wire constraint. Defaults false.
	PreferJSONObject bool `json:"prefer_json_object"`
}
