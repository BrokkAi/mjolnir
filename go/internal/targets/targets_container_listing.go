// Port of mj-targets/src/targets/container_listing.rs.
//
// Reading the sessions Mjolnir manages out of a container engine's listing.

package targets

// ManagedSession is one managed session ID in a container listing, with the
// instance label that created it when the container carries one (Rust: the
// tuple (String, Option<String>)).
type ManagedSession struct {
	SessionID string
	Instance  *string
}

// ManagedSessionsFromContainerJSON is the managed session IDs in a container
// listing, each with the instance label that created it when the container
// carries one, sorted and without duplicates. stdout is a stream of JSON
// values.
func ManagedSessionsFromContainerJSON(stdout []byte) ([]ManagedSession, error) {
	panic("unported: mj-targets/src/targets/container_listing.rs managed_sessions_from_container_json")
}

func collectManagedSessions(value any, sessions *[]ManagedSession) {
	panic("unported: mj-targets/src/targets/container_listing.rs collect_managed_sessions")
}

func labelValue(labels any, key string) (string, bool) {
	panic("unported: mj-targets/src/targets/container_listing.rs label_value")
}
