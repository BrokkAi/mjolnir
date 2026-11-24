// Port of mj-server/src/server/api/routes.rs (only the wire types so far).

package server

// SessionListQuery is the query of `GET /sessions`. Rust: deny_unknown_fields.
//
// The CLI sends it as a URL query (serde_urlencoded): an absent WorkspaceID
// is left out and All is `true` or `false`.
type SessionListQuery struct {
	WorkspaceID *string `json:"workspace_id"`
	// All defaults to true: older API clients omit it and retain the
	// historical all-sessions list.
	All bool `json:"all" serde:"default"`
}

// DefaultSessionListQuery is Rust's `SessionListQuery::default()`: every
// session.
func DefaultSessionListQuery() SessionListQuery {
	return SessionListQuery{All: true}
}

// SetConfigRequest is the body of `PATCH /sessions/{id}/config`. Rust:
// deny_unknown_fields.
type SetConfigRequest struct {
	Key   string `json:"key"`
	Value string `json:"value"`
}
