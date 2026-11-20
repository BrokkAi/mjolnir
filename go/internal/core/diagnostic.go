// Port of mj-core/src/diagnostic.rs.

// Provider failure details attached to a single completed turn.

package core

import (
	"math"
	"strconv"
	"strings"

	acp "github.com/BrokkAi/acp-go"
	"github.com/BrokkAi/acp-go/jsonvalue"
)

const QuotaStopReason = "QuotaLimit"

// TurnDiagnostic describes a provider failure of one completed turn.
type TurnDiagnostic struct {
	Message string `json:"message"`
	// LoginFingerprint is the hash of the Claude access token present before
	// the failed prompt. Credential bytes never enter the journal. Older
	// turns have no evidence.
	LoginFingerprint *string `json:"login_fingerprint,omitempty"`
	Code             *string `json:"code,omitempty"`
	HTTPStatus       *uint16 `json:"http_status,omitempty"`
	// ResetAt is the provider-supplied reset value, without guessing a
	// timezone or deadline.
	ResetAt *string `json:"reset_at,omitempty"`
}

// TurnDiagnosticFromACP is the diagnostic of an ACP error response.
func TurnDiagnosticFromACP(err *acp.RPCError) TurnDiagnostic {
	code := acpErrorCodeText(err.Code)
	diagnostic := TurnDiagnostic{Message: err.Message, Code: &code}
	if len(err.Data) > 0 {
		if data, parseErr := jsonvalue.Parse(err.Data); parseErr == nil && data != nil {
			diagnostic.enrich(data)
		}
	}
	return diagnostic
}

// acpErrorCodeText is the Display text of the Rust ACP crate's ErrorCode,
// which is what a diagnostic's code holds for an ACP error: the standard
// name of a known code, "Unknown error" for any other.
func acpErrorCodeText(code int) string {
	switch code {
	case -32700:
		return "Parse error"
	case -32600:
		return "Invalid request"
	case -32601:
		return "Method not found"
	case -32602:
		return "Invalid params"
	case -32603:
		return "Internal error"
	case -32800:
		return "Request cancelled"
	case acp.ErrorCodeAuthRequired:
		return "Authentication required"
	case acp.ErrorCodeResourceNotFound:
		return "Resource not found"
	}
	return "Unknown error"
}

// TurnDiagnosticFromProvider is the diagnostic of a provider error object,
// or nil when it has no message.
func TurnDiagnosticFromProvider(err any) *TurnDiagnostic {
	message, ok := jvString(jvGet(err, "message"))
	if !ok {
		return nil
	}
	diagnostic := TurnDiagnostic{Message: message}
	diagnostic.enrich(err)
	return &diagnostic
}

func (d *TurnDiagnostic) enrich(data any) {
	if message, ok := jvString(jvGet(data, "message")); ok {
		d.Message = message
	}
	if code, ok := jvString(jvGet(data, "code")); ok {
		d.Code = &code
	} else if code, ok := CodexErrorKind(data); ok {
		// Preserve provider metadata as evidence; retryability is Jev's decision.
		d.Code = &code
	}
	details, ok := jvGet(data, "details")
	if !ok {
		details = data
	}
	d.HTTPStatus = nil
	status, ok := jvGet(details, "statusCode")
	if !ok {
		status, ok = jvGet(details, "status")
	}
	if value, isU64 := jvUint64(status, ok); isU64 && value <= math.MaxUint16 {
		narrowed := uint16(value)
		d.HTTPStatus = &narrowed
	}
	d.ResetAt = nil
	reset, ok := jvGet(details, "resetAt")
	if !ok {
		reset, ok = jvGet(details, "reset_at")
	}
	if ok {
		switch reset.(type) {
		case string:
			text := reset.(string)
			d.ResetAt = &text
		case int64, uint64, float64:
			if encoded, err := jsonvalue.Marshal(reset); err == nil {
				text := string(encoded)
				d.ResetAt = &text
			}
		}
	}
}

// IsUsageLimit reports an exhausted account quota. Only apply to trusted
// provider failures, never arbitrary chat or tool text.
func (d *TurnDiagnostic) IsUsageLimit() bool {
	if d.Code != nil {
		switch *d.Code {
		case "usage_limit", "quota_exceeded", "provider.quota_exceeded",
			// Codex's `codexErrorInfo` for an exhausted plan (J-25).
			"usageLimitExceeded", "usage_limit_exceeded":
			return true
		case "limit":
			// Typed non-account limits must never be reclassified from their prose.
			return false
		}
	}
	message := strings.ToLower(d.Message)
	if !strings.Contains(message, "usage limit") && !strings.Contains(message, "usage-limit") &&
		!strings.Contains(message, "quota") {
		return false
	}
	for _, word := range []string{"exceeded", "exhausted", "reached", "used up"} {
		if strings.Contains(message, word) {
			return true
		}
	}
	return false
}

// SessionFailure is a failure record a bridge attaches under
// `_meta.jetbrains.air.sessionFailure` when the client lists `sessionFailure`
// among its AIR capabilities: on the prompt response of a turn that failed
// (whose stop reason still says `end_turn`), and on `session_info_update` for
// warnings and retries. Records that share an `id` are revisions of one
// notice.
type SessionFailure struct {
	ID string
	// Category is `connection`, `access`, `limit`, `request`, `service` or
	// `unknown`.
	Category string
	// Severity is `error` or `warning`; a record without one is an error.
	Severity string
	Title    string
	Details  *string
	// Actions is what the bridge says may help: `retry`, `login`,
	// `new_session`.
	Actions []string
}

// SessionFailureFromMeta reads the failure record from an ACP `_meta`
// object; a nil meta is an absent one.
func SessionFailureFromMeta(meta jsonvalue.Object) *SessionFailure {
	if meta == nil {
		return nil
	}
	record, ok := jvPointer(meta, "/jetbrains/air/sessionFailure")
	if !ok {
		return nil
	}
	text := func(key string) (string, bool) { return jvString(jvGet(record, key)) }
	title, ok := text("title")
	if !ok {
		return nil
	}
	failure := &SessionFailure{Title: title, Severity: "error", Actions: []string{}}
	failure.ID, _ = text("id")
	failure.Category, _ = text("category")
	if severity, ok := text("severity"); ok {
		failure.Severity = severity
	}
	if details, ok := text("details"); ok && strings.TrimSpace(details) != "" {
		failure.Details = &details
	}
	if actions, ok := jvGet(record, "actions"); ok {
		if list, isList := actions.([]any); isList {
			for _, action := range list {
				if name, isString := action.(string); isString {
					failure.Actions = append(failure.Actions, name)
				}
			}
		}
	}
	return failure
}

func (f *SessionFailure) IsError() bool { return f.Severity != "warning" }

// TurnDiagnosticFromSessionFailure is the diagnostic of a turn the bridge
// reported as failed with a typed record. Claude and Codex identify account
// quota as `limit` with no actions. Rate limits offer `retry`, budget/context
// limits `new_session`. Keep those limits distinct from quota even when their
// title mentions usage. A rejected login is `access`; credential sync reads
// its code.
//
// Codex often passes the provider's error body through as the title
// (`{"type":"error","error":{"message":...},"status":400}`); the diagnostic
// then takes the provider's sentence and status from it.
func TurnDiagnosticFromSessionFailure(failure *SessionFailure) TurnDiagnostic {
	code := failure.Category
	switch {
	case failure.Category == "limit" && len(failure.Actions) == 0:
		code = "usageLimitExceeded"
	case failure.Category == "access":
		code = AuthFailureKind
	case failure.Category == "":
		code = "session_failure"
	}
	diagnostic := TurnDiagnostic{Message: failure.Title, Code: &code}
	if body, err := jsonvalue.Parse([]byte(failure.Title)); err == nil {
		message, ok := jvPointer(body, "/error/message")
		if !ok {
			message, ok = jvGet(body, "message")
		}
		if text, isString := jvString(message, ok); isString {
			diagnostic.Message = text
			diagnostic.HTTPStatus = nil
			if status, isU64 := jvUint64(jvGet(body, "status")); isU64 && status <= math.MaxUint16 {
				narrowed := uint16(status)
				diagnostic.HTTPStatus = &narrowed
			}
		}
	}
	if failure.Details != nil {
		diagnostic.Message = diagnostic.Message + ": " + *failure.Details
	}
	return diagnostic
}

// AuthFailureKind is Codex's error kind for a rejected login, which
// credential sync matches.
const AuthFailureKind = "unauthorized"

// CodexErrorKind is the kind of error the Codex bridge names beside the
// JSON-RPC code, in either spelling its versions use: a string such as
// `usageLimitExceeded`, or an object whose single key is the kind.
func CodexErrorKind(data any) (string, bool) {
	info, ok := jvGet(data, "codexErrorInfo")
	if !ok {
		info, ok = jvGet(data, "codex_error_info")
	}
	if !ok {
		return "", false
	}
	switch kind := info.(type) {
	case string:
		return kind, true
	case jsonvalue.Object:
		if keys := kind.Keys(); len(keys) == 1 {
			return keys[0], true
		}
	}
	return "", false
}

// jvGet is serde_json's Value::get with a string key: the member of an
// object, or nothing for any other value.
func jvGet(value any, key string) (any, bool) {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return nil, false
	}
	return object.Get(key)
}

// jvPointer is serde_json's Value::pointer for object keys and array
// indexes.
func jvPointer(value any, pointer string) (any, bool) {
	if pointer == "" {
		return value, true
	}
	if !strings.HasPrefix(pointer, "/") {
		return nil, false
	}
	for _, token := range strings.Split(pointer[1:], "/") {
		token = strings.ReplaceAll(strings.ReplaceAll(token, "~1", "/"), "~0", "~")
		switch target := value.(type) {
		case jsonvalue.Object:
			next, ok := target.Get(token)
			if !ok {
				return nil, false
			}
			value = next
		case []any:
			// serde_json refuses a leading `+` and leading zeros.
			index, err := strconv.ParseUint(token, 10, 64)
			if err != nil || strings.HasPrefix(token, "+") ||
				(len(token) > 1 && token[0] == '0') || index >= uint64(len(target)) {
				return nil, false
			}
			value = target[index]
		default:
			return nil, false
		}
	}
	return value, true
}

// jvString is serde_json's Value::as_str of a lookup result.
func jvString(value any, ok bool) (string, bool) {
	if !ok {
		return "", false
	}
	text, isString := value.(string)
	return text, isString
}

// jvUint64 is serde_json's Value::as_u64 of a lookup result.
func jvUint64(value any, ok bool) (uint64, bool) {
	if !ok {
		return 0, false
	}
	switch number := value.(type) {
	case int64:
		return uint64(number), number >= 0
	case uint64:
		return number, true
	}
	return 0, false
}

// jvInt64 is serde_json's Value::as_i64 of a lookup result.
func jvInt64(value any, ok bool) (int64, bool) {
	if !ok {
		return 0, false
	}
	switch number := value.(type) {
	case int64:
		return number, true
	case uint64:
		return int64(number), number <= math.MaxInt64
	}
	return 0, false
}
