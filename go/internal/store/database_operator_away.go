// Port of mj-store/src/database/operator_away.rs.

// Away notices share the projection transaction and its replay frontier.

package store

import (
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// operatorAwayRequestLimit is how many characters of a captured request an
// away notice repeats.
const operatorAwayRequestLimit = 4096

func boundedRequest(text string) string {
	if utf8.RuneCountInString(text) <= operatorAwayRequestLimit {
		return text
	}
	return string([]rune(text)[:operatorAwayRequestLimit]) + "\n[request truncated]"
}

// enqueue queues an operator-away mailbox event in the projection
// transaction.
func enqueue(tx *Transaction, session string, key string, atMs int64, body core.MailboxEventBody) error {
	panic("unported: mj-store/src/database/operator_away.rs enqueue")
}

// projectEvent captures input requests and their away resolutions as the
// projection applies them.
func projectEvent(tx *Transaction, session string, seq uint64, atMs int64, event core.ApiEventData) error {
	panic("unported: mj-store/src/database/operator_away.rs project_event")
}

// setWith records operator presence; false when it did not change.
func setWith(conn *Connection, away bool, autoAfkWindow time.Duration) (bool, error) {
	panic("unported: mj-store/src/database/operator_away.rs set_with")
}
