// Port of mj-targets/src/targets/container_listing.rs.
//
// Reading the sessions Mjolnir manages out of a container engine's listing.

package targets

import (
	"bytes"
	"cmp"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ManagedSession is one managed session ID in a container listing, with the
// instance label that created it when the container carries one (Rust: the
// tuple (String, Option<String>)).
type ManagedSession struct {
	SessionID string
	Instance  *string
}

// compareManagedSessions orders as Rust's tuples do: by session, then with
// no instance before any instance.
func compareManagedSessions(left, right ManagedSession) int {
	if order := strings.Compare(left.SessionID, right.SessionID); order != 0 {
		return order
	}
	switch {
	case left.Instance == nil && right.Instance == nil:
		return 0
	case left.Instance == nil:
		return -1
	case right.Instance == nil:
		return 1
	}
	return cmp.Compare(*left.Instance, *right.Instance)
}

// ManagedSessionsFromContainerJSON is the managed session IDs in a container
// listing, each with the instance label that created it when the container
// carries one, sorted and without duplicates. stdout is a stream of JSON
// values.
func ManagedSessionsFromContainerJSON(stdout []byte) ([]ManagedSession, error) {
	decoder := json.NewDecoder(bytes.NewReader(stdout))
	var values []any
	for {
		var value any
		err := decoder.Decode(&value)
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return nil, fmt.Errorf("parse container list JSON: %w", err)
		}
		values = append(values, value)
	}
	sessions := []ManagedSession{}
	for _, value := range values {
		collectManagedSessions(value, &sessions)
	}
	slices.SortFunc(sessions, compareManagedSessions)
	sessions = slices.CompactFunc(sessions, func(left, right ManagedSession) bool {
		return compareManagedSessions(left, right) == 0
	})
	return sessions, nil
}

func collectManagedSessions(value any, sessions *[]ManagedSession) {
	switch value := value.(type) {
	case []any:
		for _, item := range value {
			collectManagedSessions(item, sessions)
		}
	case map[string]any:
		for _, labelKey := range []string{"Labels", "labels"} {
			labels, ok := value[labelKey]
			if !ok {
				continue
			}
			managed, ok := labelValue(labels, core.ManagedLabel)
			if !ok || managed != "true" {
				continue
			}
			if session, ok := labelValue(labels, core.SessionLabel); ok {
				entry := ManagedSession{SessionID: session}
				if instance, ok := labelValue(labels, core.InstanceLabel); ok {
					entry.Instance = &instance
				}
				*sessions = append(*sessions, entry)
			}
		}
		for _, item := range value {
			collectManagedSessions(item, sessions)
		}
	}
}

func labelValue(labels any, key string) (string, bool) {
	switch labels := labels.(type) {
	case map[string]any:
		text, ok := labels[key].(string)
		return text, ok
	case string:
		for _, label := range strings.Split(labels, ",") {
			name, value, ok := strings.Cut(strings.TrimSpace(label), "=")
			if ok && name == key {
				return value, true
			}
		}
	}
	return "", false
}
