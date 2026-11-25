// Port of brokk-anvil-client 0.28.6 src/structured_output.rs (the part Mjolnir uses).

package anvilclient

import (
	"bytes"
	"fmt"
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/santhosh-tekuri/jsonschema/v6"
)

const maxInvalidExcerptChars = 400

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

// jsonSchemaInstruction is the in-band schema instruction for providers
// that only support JSON-object mode.
func jsonSchemaInstruction(request *StructuredOutputRequest) string {
	schema, err := jsonvalue.Marshal(request.Schema)
	if err != nil {
		schema = []byte("null")
	}
	return "Return only JSON matching this JSON Schema: " + string(schema)
}

type StructuredOutputSchemaError struct {
	InstanceLocation string `json:"instance_location"`
	SchemaLocation   string `json:"schema_location"`
	Message          string `json:"message"`
}

type StructuredOutputSuccess struct {
	SchemaName string `json:"schema_name"`
	// ValidatedOutput is a JSON value.
	ValidatedOutput   any  `json:"validated_output"`
	CoercionRequested bool `json:"coercion_requested"`
}

type StructuredOutputValidationError struct {
	SchemaName        string                        `json:"schema_name"`
	Errors            []StructuredOutputSchemaError `json:"errors"`
	InvalidExcerpt    string                        `json:"invalid_excerpt"`
	CoercionRequested bool                          `json:"coercion_requested"`
}

// compileSchema is jsonschema::validator_for: the schema compiled under its
// own `$schema`, or Draft 2020-12 when it names none.
func compileSchema(schema any) (*jsonschema.Schema, error) {
	encoded, err := jsonvalue.Marshal(schema)
	if err != nil {
		return nil, err
	}
	document, err := jsonschema.UnmarshalJSON(bytes.NewReader(encoded))
	if err != nil {
		return nil, err
	}
	compiler := jsonschema.NewCompiler()
	if err := compiler.AddResource("structured-output.json", document); err != nil {
		return nil, err
	}
	return compiler.Compile("structured-output.json")
}

// validateResponse validates a response against the request's schema. The
// returned output is the parsed JSON value on success; otherwise the
// validation error. Coercion is not ported: Mjolnir's only caller
// (InferStructured) always disables it.
func validateResponse(request *StructuredOutputRequest, responseText string) (*StructuredOutputSuccess, *StructuredOutputValidationError) {
	parsed, _, err := parseJSONValue(responseText)
	if err != nil {
		return nil, validationErrorResult(request, []StructuredOutputSchemaError{{
			Message: fmt.Sprintf("response is not valid JSON: %v", err),
		}}, responseText)
	}
	compiled, err := compileSchema(request.Schema)
	if err != nil {
		return nil, validationErrorResult(request, []StructuredOutputSchemaError{{
			Message: fmt.Sprintf("schema compilation failed: %v", err),
		}}, responseText)
	}
	instance, err := jsonschema.UnmarshalJSON(strings.NewReader(responseText))
	if err != nil {
		return nil, validationErrorResult(request, []StructuredOutputSchemaError{{
			Message: fmt.Sprintf("response is not valid JSON: %v", err),
		}}, responseText)
	}
	if err := compiled.Validate(instance); err != nil {
		return nil, validationErrorResult(request, collectSchemaErrors(err), responseText)
	}
	return &StructuredOutputSuccess{
		SchemaName:        request.SchemaName,
		ValidatedOutput:   parsed,
		CoercionRequested: request.AllowCoercion,
	}, nil
}

// validationRetryPrompt is the feedback for one bounded model repair after
// schema validation fails. The preceding assistant response remains in
// history, so this message only needs to identify the violations and
// restate the output contract.
func validationRetryPrompt(failure *StructuredOutputValidationError) string {
	violations := make([]string, 0, len(failure.Errors))
	for _, item := range failure.Errors {
		location := item.InstanceLocation
		if location == "" {
			location = "<root>"
		}
		violations = append(violations, fmt.Sprintf("- %s: %s", location, item.Message))
	}
	return "Your previous response failed JSON Schema validation:\n" + strings.Join(violations, "\n") +
		"\nReturn a corrected response matching the requested schema exactly. " +
		"Return only the JSON value, with no markdown or explanation."
}

type NativeResponseFormat struct {
	Name string
	// Schema is a JSON value.
	Schema any
	Strict bool
}

func nativeResponseFormat(request *StructuredOutputRequest) NativeResponseFormat {
	return NativeResponseFormat{Name: request.SchemaName, Schema: request.Schema, Strict: true}
}

func truncateExcerpt(raw string) string {
	if utf8.RuneCountInString(raw) <= maxInvalidExcerptChars {
		return raw
	}
	return string([]rune(raw)[:maxInvalidExcerptChars]) + "..."
}

func validationErrorResult(request *StructuredOutputRequest, errors []StructuredOutputSchemaError, responseText string) *StructuredOutputValidationError {
	return &StructuredOutputValidationError{
		SchemaName:        request.SchemaName,
		Errors:            errors,
		InvalidExcerpt:    truncateExcerpt(responseText),
		CoercionRequested: request.AllowCoercion,
	}
}

// collectSchemaErrors lists each failed keyword as the jsonschema crate's
// error iterator does: where in the instance, where in the schema, and
// what.
func collectSchemaErrors(err error) []StructuredOutputSchemaError {
	validation, ok := err.(*jsonschema.ValidationError)
	if !ok {
		return []StructuredOutputSchemaError{{Message: err.Error()}}
	}
	// The crate's iterator yields only the failing keywords themselves, not
	// the subschemas that contain them: the leaves of the detailed output.
	var errors []StructuredOutputSchemaError
	var collect func(unit *jsonschema.OutputUnit)
	collect = func(unit *jsonschema.OutputUnit) {
		if len(unit.Errors) == 0 && unit.Error != nil {
			errors = append(errors, StructuredOutputSchemaError{
				InstanceLocation: unit.InstanceLocation,
				SchemaLocation:   unit.KeywordLocation,
				Message:          unit.Error.String(),
			})
		}
		for index := range unit.Errors {
			collect(&unit.Errors[index])
		}
	}
	collect(validation.DetailedOutput())
	if len(errors) == 0 {
		errors = append(errors, StructuredOutputSchemaError{Message: validation.Error()})
	}
	return errors
}

// debugSchemaErrors formats errors as Rust's `{:?}` of the Vec does.
func debugSchemaErrors(errors []StructuredOutputSchemaError) string {
	parts := make([]string, 0, len(errors))
	for _, item := range errors {
		parts = append(parts, fmt.Sprintf("StructuredOutputSchemaError { instance_location: %q, schema_location: %q, message: %q }",
			item.InstanceLocation, item.SchemaLocation, item.Message))
	}
	return "[" + strings.Join(parts, ", ") + "]"
}
