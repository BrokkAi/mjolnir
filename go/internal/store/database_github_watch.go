// Port of mj-store/src/database/github_watch.rs.
//
// Durable cursors, classifications, and creator watches for GitHub polling.

package store

import (
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"math"
	"slices"
	"sort"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

type GithubRepoCursor struct {
	UpdatedAt         string
	WatchStartedAt    string
	ReconciledThrough string
}

type GithubSeenItem struct {
	Number    int64
	Kind      string
	Title     string
	URL       string
	Author    string
	Body      string
	CreatedAt string
	UpdatedAt string
}

type GithubItemClassification struct {
	Owner      string
	Repo       string
	Number     int64
	SessionID  string
	Kind       string
	Title      string
	URL        string
	CreatedAt  string
	Interested bool
	Created    bool
}

type GithubItemWatch struct {
	Owner  string
	Repo   string
	Number int64
	// CreatorSessionID is nil for a watch kept for interested sessions only:
	// a PR another account opened, or one whose creating session is gone.
	CreatorSessionID  *string
	Kind              string
	Title             string
	URL               string
	CreatedAt         string
	TimelineAt        string
	TimelineID        int64
	ReviewCommentsAt  string
	ReviewCommentsID  int64
	ProcessedAt       string
	RecordedUpdatedAt *string
	// Lifecycle is the latest lifecycle event the timeline committed for this
	// item.
	Lifecycle *RecordedLifecycle
}

// RecordedLifecycle is a lifecycle event as recorded on its watch, so a
// session that is classified after the event was committed can still be
// told. BodyJSON is a serialized MailboxEventBody addressed to the creator.
type RecordedLifecycle struct {
	Key      string
	BodyJSON string
	At       string
}

type GithubSessionTurnWindow struct {
	StartedAtMs   int64
	CompletedAtMs *int64
}

type GithubClassificationFailure struct {
	Attempts        int64
	LastAttemptAtMs int64
	NextAttemptAtMs int64
}

// GithubClassificationFailureKey keys LoadGithubClassificationFailures:
// Rust's `(i64, String)`, the item number and the session id.
type GithubClassificationFailureKey struct {
	Number    int64
	SessionID string
}

func LoadGithubClassificationFailures(owner string, repo string) (map[GithubClassificationFailureKey]GithubClassificationFailure, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	rows, err := connection.Query("SELECT number, session_id, attempts, last_attempt_at_ms, next_attempt_at_ms FROM github_watch_classification_failures WHERE owner=?1 AND repo=?2", owner, repo)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	failures := map[GithubClassificationFailureKey]GithubClassificationFailure{}
	for rows.Next() {
		var key GithubClassificationFailureKey
		var failure GithubClassificationFailure
		if err := rows.Scan(&key.Number, &key.SessionID, &failure.Attempts, &failure.LastAttemptAtMs, &failure.NextAttemptAtMs); err != nil {
			return nil, err
		}
		failures[key] = failure
	}
	return failures, rows.Err()
}

func RecordGithubClassificationFailure(owner string, repo string, number int64, sessionID string) error {
	_, err := submitDatabaseWrite("record_github_classification_failure", func(connection *Connection) (struct{}, error) {
		tx, err := connection.Begin()
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		var classified bool
		if err := tx.QueryRow("SELECT EXISTS(SELECT 1 FROM github_watch_classifications WHERE owner=?1 AND repo=?2 AND number=?3 AND session_id=?4)",
			owner, repo, number, sessionID).Scan(&classified); err != nil {
			return struct{}{}, err
		}
		if classified {
			return struct{}{}, tx.Commit()
		}
		var stored int64
		err = tx.QueryRow("SELECT attempts FROM github_watch_classification_failures WHERE owner=?1 AND repo=?2 AND number=?3 AND session_id=?4",
			owner, repo, number, sessionID).Scan(&stored)
		if err != nil && !errors.Is(err, sql.ErrNoRows) {
			return struct{}{}, err
		}
		attempts := githubSaturatingAdd(stored, 1)
		now := core.EpochMillis()
		delayMs := min(60_000*(int64(1)<<min(attempts-1, 6)), 3_600_000)
		if _, err := tx.Exec(`INSERT INTO github_watch_classification_failures(owner,repo,number,session_id,attempts,last_attempt_at_ms,next_attempt_at_ms)
            VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(owner,repo,number,session_id) DO UPDATE SET
            attempts=excluded.attempts,last_attempt_at_ms=excluded.last_attempt_at_ms,next_attempt_at_ms=excluded.next_attempt_at_ms`,
			owner, repo, number, sessionID, attempts, now, githubSaturatingAdd(now, delayMs)); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, tx.Commit()
	})
	return err
}

// githubSaturatingAdd is Rust's i64::saturating_add.
func githubSaturatingAdd(a, b int64) int64 {
	sum := a + b
	if b > 0 && sum < a {
		return math.MaxInt64
	}
	if b < 0 && sum > a {
		return math.MinInt64
	}
	return sum
}

// GithubRepoCursorFor is Rust's github_repo_cursor (renamed because Go cannot
// give the function and the GithubRepoCursor type the same name). It
// initializes the watch boundary once, before the first network request.
func GithubRepoCursorFor(owner string, repo string) (GithubRepoCursor, error) {
	return submitDatabaseWrite("github_repo_cursor", func(connection *Connection) (GithubRepoCursor, error) {
		now := core.FormatChronoRFC3339(time.Now())
		if _, err := connection.Exec(`INSERT INTO github_watch_cursors(owner, repo, updated_at, watch_started_at, reconciled_through)
             VALUES (?1, ?2, ?3, ?3, ?3) ON CONFLICT(owner, repo) DO NOTHING`,
			owner, repo, now); err != nil {
			return GithubRepoCursor{}, err
		}
		var cursor GithubRepoCursor
		err := connection.QueryRow("SELECT updated_at, watch_started_at, reconciled_through FROM github_watch_cursors WHERE owner=?1 AND repo=?2",
			owner, repo).Scan(&cursor.UpdatedAt, &cursor.WatchStartedAt, &cursor.ReconciledThrough)
		return cursor, err
	})
}

// GithubFeedProgress is how far a recorded feed page moves the repository's
// cursors.
type GithubFeedProgress interface {
	isGithubFeedProgress()
}

type GithubFeedProgressMain struct{ Through string }

type GithubFeedProgressReconciled struct{ Through string }

func (GithubFeedProgressMain) isGithubFeedProgress()       {}
func (GithubFeedProgressReconciled) isGithubFeedProgress() {}

// RecordGithubPage: recording never depends on classification or delivery.
// Each page is durable.
func RecordGithubPage(owner string, repo string, items []GithubSeenItem, progress GithubFeedProgress) error {
	_, err := submitDatabaseWrite("record_github_page", func(connection *Connection) (struct{}, error) {
		tx, err := connection.Begin()
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		for _, item := range items {
			if _, err := tx.Exec(`INSERT INTO github_seen_items(owner, repo, number, kind, title, url, author, body, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(owner, repo, number) DO UPDATE SET kind=excluded.kind, title=excluded.title,
                    url=excluded.url, author=excluded.author, body=excluded.body,
                    created_at=excluded.created_at, updated_at=excluded.updated_at
                 WHERE julianday(excluded.updated_at) >= julianday(github_seen_items.updated_at)`,
				owner, repo, item.Number, item.Kind, item.Title, item.URL, item.Author, item.Body, item.CreatedAt, item.UpdatedAt); err != nil {
				return struct{}{}, err
			}
		}
		var query, through string
		switch progress := progress.(type) {
		case GithubFeedProgressMain:
			query = `UPDATE github_watch_cursors SET updated_at=?3 WHERE owner=?1 AND repo=?2
                 AND julianday(updated_at) <= julianday(?3)`
			through = progress.Through
		case GithubFeedProgressReconciled:
			query = `UPDATE github_watch_cursors SET reconciled_through=?3 WHERE owner=?1 AND repo=?2
                 AND julianday(reconciled_through) <= julianday(?3)`
			through = progress.Through
		default:
			return struct{}{}, fmt.Errorf("unknown GitHub feed progress %T", progress)
		}
		if _, err := tx.Exec(query, owner, repo, through); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, tx.Commit()
	})
	return err
}

// ClaimGithubReconciliation claims before requesting pages so failures and
// restarts cannot exceed the hourly rate.
func ClaimGithubReconciliation(owner string, repo string, nowMs int64) (bool, error) {
	return submitDatabaseWrite("claim_github_reconciliation", func(connection *Connection) (bool, error) {
		result, err := connection.Exec(`UPDATE github_watch_cursors SET last_reconciled_at_ms=?3
             WHERE owner=?1 AND repo=?2 AND (last_reconciled_at_ms IS NULL OR last_reconciled_at_ms <= ?4)`,
			owner, repo, nowMs, githubSaturatingAdd(nowMs, -3_600_000))
		if err != nil {
			return false, err
		}
		changed, err := result.RowsAffected()
		return changed != 0, err
	})
}

func LoadGithubSeenItems(owner string, repo string) ([]GithubSeenItem, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	rows, err := connection.Query(`SELECT number, kind, title, url, author, body, created_at, updated_at FROM github_seen_items
         WHERE owner=?1 AND repo=?2 ORDER BY created_at, number`, owner, repo)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	items := []GithubSeenItem{}
	for rows.Next() {
		var item GithubSeenItem
		if err := rows.Scan(&item.Number, &item.Kind, &item.Title, &item.URL, &item.Author, &item.Body, &item.CreatedAt, &item.UpdatedAt); err != nil {
			return nil, err
		}
		items = append(items, item)
	}
	return items, rows.Err()
}

func LoadGithubItemClassifiedSessions(owner string, repo string, number int64) (map[string]struct{}, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	sessions, err := queryTextColumn(connection, `SELECT session_id FROM github_watch_classifications
         WHERE owner=?1 AND repo=?2 AND number=?3`, owner, repo, number)
	if err != nil {
		return nil, err
	}
	set := make(map[string]struct{}, len(sessions))
	for _, session := range sessions {
		set[session] = struct{}{}
	}
	return set, nil
}

// GithubClassificationEvent is the interest event a classification commits
// with: Rust's `(String, String, bool)`, the outbox key, the event JSON and
// whether it wakes the session.
type GithubClassificationEvent struct {
	Key  string
	JSON string
	Wake bool
}

// CommitGithubItemClassification: classification, creator watch, and
// resulting interest event share one transaction. A process restart cannot
// leave one without the others.
func CommitGithubItemClassification(classification GithubItemClassification, event *GithubClassificationEvent) (bool, error) {
	hasEvent := event != nil
	type outcome struct{ inserted, enqueuedLifecycle bool }
	result, err := submitDatabaseWrite("commit_github_item_classification", func(connection *Connection) (outcome, error) {
		tx, err := connection.Begin()
		if err != nil {
			return outcome{}, err
		}
		defer tx.Rollback()
		enqueuedLifecycle := false
		inserted, err := tx.Exec(`INSERT INTO github_watch_classifications(owner, repo, number, session_id,
                    kind, title, url, created_at, interested, created)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(owner, repo, number, session_id) DO NOTHING`,
			classification.Owner, classification.Repo, classification.Number, classification.SessionID,
			classification.Kind, classification.Title, classification.URL, classification.CreatedAt,
			classification.Interested, classification.Created)
		if err != nil {
			return outcome{}, err
		}
		if _, err := tx.Exec("DELETE FROM github_watch_classification_failures WHERE owner=?1 AND repo=?2 AND number=?3 AND session_id=?4",
			classification.Owner, classification.Repo, classification.Number, classification.SessionID); err != nil {
			return outcome{}, err
		}
		if changed, err := inserted.RowsAffected(); err != nil {
			return outcome{}, err
		} else if changed == 0 {
			return outcome{}, tx.Commit()
		}
		// The open notice goes first: the outbox delivers in creation order, and
		// a recorded merge must not reach the session before it hears of the PR.
		if event != nil {
			if _, err := enqueueGithubEvent(tx, event.Key, classification.SessionID, event.JSON, event.Wake); err != nil {
				return outcome{}, err
			}
		}
		if classification.Created {
			// A watch kept for interested sessions gains its creator here. What
			// the watch read while it had none was read for interested sessions
			// only, with no review comments, so the creator is owed the whole
			// timeline: the cursors go back to the item's creation. The
			// interested sessions' rows have their own keys, so nothing is
			// delivered twice.
			if _, err := tx.Exec(`INSERT INTO github_watch_items(owner, repo, number, creator_session_id,
                        kind, title, url, created_at, timeline_at, timeline_id, review_comments_at, review_comments_id, processed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, 0, ?8, 0, ?8)
                 ON CONFLICT(owner, repo, number) DO UPDATE
                    SET creator_session_id = excluded.creator_session_id,
                        timeline_at = excluded.timeline_at, timeline_id = 0,
                        review_comments_at = excluded.review_comments_at, review_comments_id = 0,
                        processed_at = excluded.processed_at
                    WHERE github_watch_items.creator_session_id IS NULL`,
				classification.Owner, classification.Repo, classification.Number, classification.SessionID,
				classification.Kind, classification.Title, classification.URL, classification.CreatedAt); err != nil {
				return outcome{}, err
			}
		} else if classification.Interested && classification.Kind == "pull_request" {
			// Every PR with an interested session is watched, so its merge
			// reaches that session. The watch starts at the PR's creation, so
			// its first poll reads the whole timeline.
			if _, err := tx.Exec(`INSERT INTO github_watch_items(owner, repo, number, creator_session_id,
                        kind, title, url, created_at, timeline_at, timeline_id, review_comments_at, review_comments_id, processed_at)
                 VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, ?7, 0, ?7, 0, ?7)
                 ON CONFLICT(owner, repo, number) DO NOTHING`,
				classification.Owner, classification.Repo, classification.Number,
				classification.Kind, classification.Title, classification.URL, classification.CreatedAt); err != nil {
				return outcome{}, err
			}
			// The timeline commit that recorded the latest lifecycle event may
			// already have run. SQLite orders the two write transactions, so
			// either that commit saw this classification and delivered the
			// event, or this one sees the recorded event and delivers it.
			var key, bodyJSON, at *string
			err := tx.QueryRow(`SELECT lifecycle_event_key, lifecycle_event_json, lifecycle_at
                     FROM github_watch_items WHERE owner=?1 AND repo=?2 AND number=?3`,
				classification.Owner, classification.Repo, classification.Number).Scan(&key, &bodyJSON, &at)
			if err != nil && !errors.Is(err, sql.ErrNoRows) {
				return outcome{}, err
			}
			// No own-activity filter here: sessions share one GitHub account,
			// so an overlapping turn does not show that this session made the
			// change, and a wrong skip would lose the notice for good. A
			// queued echo of its own action costs one line.
			if recorded := recordedLifecycle(key, bodyJSON, at); recorded != nil {
				body, err := core.DecodeMailboxEventBody([]byte(recorded.BodyJSON))
				if err != nil {
					return outcome{}, fmt.Errorf("decode the recorded GitHub lifecycle event: %w", err)
				}
				eventKey := interestedEventKey(recorded.Key, classification.SessionID)
				eventJSON, err := githubMailboxEventJSON(eventKey, interestedLifecycleBody(body), false, recorded.At)
				if err != nil {
					return outcome{}, err
				}
				enqueuedLifecycle, err = enqueueGithubEvent(tx, eventKey, classification.SessionID, eventJSON, false)
				if err != nil {
					return outcome{}, err
				}
			}
		}
		if err := tx.Commit(); err != nil {
			return outcome{}, err
		}
		return outcome{inserted: true, enqueuedLifecycle: enqueuedLifecycle}, nil
	})
	if err != nil {
		return false, err
	}
	if result.inserted && (hasEvent || result.enqueuedLifecycle) {
		NotifyMailboxOutboxChanged()
	}
	return result.inserted, nil
}

func LoadGithubWatches(owner string, repo string) ([]GithubItemWatch, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	rows, err := connection.Query(`SELECT w.owner, w.repo, w.number, w.creator_session_id, w.kind, w.title, w.url, w.created_at,
                w.timeline_at, w.timeline_id, w.review_comments_at, w.review_comments_id, w.processed_at, s.updated_at,
                w.lifecycle_event_key, w.lifecycle_event_json, w.lifecycle_at
         FROM github_watch_items w LEFT JOIN github_seen_items s
           ON w.owner=s.owner AND w.repo=s.repo AND w.number=s.number
         WHERE w.owner=?1 AND w.repo=?2 ORDER BY w.number`, owner, repo)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	watches := []GithubItemWatch{}
	for rows.Next() {
		var watch GithubItemWatch
		var key, bodyJSON, at *string
		if err := rows.Scan(&watch.Owner, &watch.Repo, &watch.Number, &watch.CreatorSessionID, &watch.Kind, &watch.Title,
			&watch.URL, &watch.CreatedAt, &watch.TimelineAt, &watch.TimelineID, &watch.ReviewCommentsAt,
			&watch.ReviewCommentsID, &watch.ProcessedAt, &watch.RecordedUpdatedAt, &key, &bodyJSON, &at); err != nil {
			return nil, err
		}
		watch.Lifecycle = recordedLifecycle(key, bodyJSON, at)
		watches = append(watches, watch)
	}
	return watches, rows.Err()
}

// recordedLifecycle: the four columns are written together; a row with a key
// but no body or time carries no event.
func recordedLifecycle(key *string, bodyJSON *string, at *string) *RecordedLifecycle {
	if key == nil || bodyJSON == nil || at == nil {
		return nil
	}
	return &RecordedLifecycle{Key: *key, BodyJSON: *bodyJSON, At: *at}
}

// enqueueGithubEvent is whether a row was stored. A key already present, and
// a target that is gone, store nothing.
func enqueueGithubEvent(tx *sql.Tx, key string, target string, json string, wake bool) (bool, error) {
	result, err := tx.Exec(`INSERT INTO mailbox_outbox(event_key, target_session_id, event_json, wake, unpark, created_at)
         VALUES (?1, ?2, ?3, ?4, 0, ?5) ON CONFLICT(event_key) DO NOTHING`,
		key, target, json, wake, core.FormatChronoRFC3339(time.Now()))
	if err != nil {
		return false, err
	}
	changed, err := result.RowsAffected()
	return changed > 0, err
}

// interestedEventKey is the outbox key of an interested session's copy of a
// lifecycle event. It differs from the creator's key, which is the primary
// key and would otherwise make one of the two inserts a silent no-op.
func interestedEventKey(baseKey string, sessionID string) string {
	return baseKey + ":" + sessionID
}

func githubMailboxEventJSON(key string, body core.MailboxEventBody, wake bool, timestamp string) (string, error) {
	at, ok := core.ParseChronoRFC3339(timestamp)
	if !ok {
		return "", errors.New("invalid GitHub timeline timestamp")
	}
	createdAtMs := uint64(max(at.UnixMilli(), 0))
	encoded, err := databaseMarshalJSON(core.MailboxEvent{
		Key:         key,
		Source:      "github",
		Wake:        wake,
		CreatedAtMs: createdAtMs,
		Body:        body,
	})
	if err != nil {
		return "", err
	}
	return string(encoded), nil
}

// interestedLifecycleBody is a lifecycle body as an interested session
// receives it: the same change, worded so it does not claim the PR is
// theirs. Other bodies pass through.
func interestedLifecycleBody(body core.MailboxEventBody) core.MailboxEventBody {
	if lifecycle, ok := body.(core.MailboxEventBodyGithubPullRequestLifecycle); ok {
		lifecycle.Recipient = core.LifecycleRecipientInterested
		return lifecycle
	}
	return body
}

// MappedGithubEvent is an event mapped from a timeline read, before
// recipients are known.
type MappedGithubEvent struct {
	// Key is the creator's outbox key, and the base of every interested
	// session's.
	Key  string
	Body core.MailboxEventBody
	// Author is the GitHub login that caused it, for the own-activity filter.
	Author    *string
	Timestamp string
	// Lifecycle: merged, closed or reopened: also delivered to interested
	// sessions.
	Lifecycle bool
}

// turnWindows caches each session's turn windows for one transaction; a nil
// entry is a session whose turns could not be read.
type turnWindows map[string]*[]GithubSessionTurnWindow

// githubASCIIEqualFold is Rust's str::eq_ignore_ascii_case.
func githubASCIIEqualFold(a, b string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := 0; i < len(a); i++ {
		x, y := a[i], b[i]
		if 'A' <= x && x <= 'Z' {
			x += 'a' - 'A'
		}
		if 'A' <= y && y <= 'Z' {
			y += 'a' - 'A'
		}
		if x != y {
			return false
		}
	}
	return true
}

// authoredDuringSessionTurn is whether sessionID itself caused a GitHub
// action: the action's author is the shared credential's account, and it
// happened during one of the session's turns. The session already knows what
// it did, so delivering it, even without a wake, would echo it into its
// context. Runs inside the write transaction that decides delivery. A session
// whose turns cannot be read gets the event.
func authoredDuringSessionTurn(tx *sql.Tx, windowsBySession turnWindows, credentialLogin *string, authorLogin *string, createdAt *string, sessionID string) bool {
	if credentialLogin == nil || authorLogin == nil {
		return false
	}
	if !githubASCIIEqualFold(*credentialLogin, *authorLogin) {
		return false
	}
	if createdAt == nil {
		return false
	}
	at, ok := core.ParseChronoRFC3339(*createdAt)
	if !ok {
		return false
	}
	createdAtMs := floorToSecond(at.UnixMilli())
	windows, cached := windowsBySession[sessionID]
	if !cached {
		read, err := sessionTurnWindows(tx, sessionID)
		if err != nil {
			slog.Warn("could not read transcript turn windows for GitHub activity; delivering it",
				"session_id", sessionID, "error", err.Error())
			windows = nil
		} else {
			windows = &read
		}
		windowsBySession[sessionID] = windows
	}
	if windows == nil {
		return false
	}
	for _, window := range *windows {
		if createdAtMs >= floorToSecond(window.StartedAtMs) &&
			(window.CompletedAtMs == nil || createdAtMs < ceilToSecond(*window.CompletedAtMs)) {
			return true
		}
	}
	return false
}

func floorToSecond(timestampMs int64) int64 {
	// Euclidean division, as Rust's div_euclid rounds toward negative infinity.
	quotient := timestampMs / 1000
	if timestampMs%1000 < 0 {
		quotient--
	}
	return quotient * 1000
}

func ceilToSecond(timestampMs int64) int64 {
	floor := floorToSecond(timestampMs)
	if floor == timestampMs {
		return timestampMs
	}
	return floor + 1000
}

type GithubItemProgress struct {
	TimelineAt       string
	TimelineID       int64
	ReviewCommentsAt string
	ReviewCommentsID int64
	ProcessedAt      string
}

// CommitGithubTimeline: only this item's progress advances with its events.
// A failure cannot roll back a feed page or another item's deliveries.
//
// This transaction owns the decision of who is told. It reads the item's
// recipients and applies the own-activity filter against the same snapshot
// that it writes, so a session classified before it runs is a recipient, and
// one classified after it reads the lifecycle event recorded here (see
// CommitGithubItemClassification). Neither order loses a notice.
func CommitGithubTimeline(watch GithubItemWatch, progress GithubItemProgress, events []MappedGithubEvent, credentialLogin *string) error {
	enqueued, err := submitDatabaseWrite("commit_github_timeline", func(connection *Connection) (bool, error) {
		tx, err := connection.Begin()
		if err != nil {
			return false, err
		}
		defer tx.Rollback()
		enqueued := false
		// The row, not the caller's snapshot, says who the creator is now.
		var creator *string
		err = tx.QueryRow("SELECT creator_session_id FROM github_watch_items WHERE owner=?1 AND repo=?2 AND number=?3",
			watch.Owner, watch.Repo, watch.Number).Scan(&creator)
		rowExists := true
		if errors.Is(err, sql.ErrNoRows) {
			rowExists = false
		} else if err != nil {
			return false, err
		}
		// No row: the last session that cared about the item is gone.
		advance := false
		if rowExists {
			// The poll was planned for the creator it saw. If the creator changed
			// since, whether one attached to a creator-less watch (so review
			// comments were not read) or another replaced a destroyed one (so
			// the cursors were rewound), the new creator is owed a full read:
			// leave the progress where it is and let the next poll redo it.
			advance = githubEqualOptionalString(creator, watch.CreatorSessionID)
			interested, err := queryTextColumn(tx, `SELECT session_id FROM github_watch_classifications
                     WHERE owner=?1 AND repo=?2 AND number=?3 AND interested=1 AND created=0
                     ORDER BY session_id`, watch.Owner, watch.Repo, watch.Number)
			if err != nil {
				return false, err
			}
			windows := turnWindows{}
			for i := range events {
				event := &events[i]
				timestamp := event.Timestamp
				if creator != nil && !authoredDuringSessionTurn(tx, windows, credentialLogin, event.Author, &timestamp, *creator) {
					eventJSON, err := githubMailboxEventJSON(event.Key, event.Body, true, event.Timestamp)
					if err != nil {
						return false, err
					}
					stored, err := enqueueGithubEvent(tx, event.Key, *creator, eventJSON, true)
					if err != nil {
						return false, err
					}
					enqueued = enqueued || stored
				}
				if !event.Lifecycle {
					continue
				}
				for _, session := range interested {
					if creator != nil && session == *creator {
						continue
					}
					// No own-activity filter for interested sessions; see
					// CommitGithubItemClassification.
					key := interestedEventKey(event.Key, session)
					eventJSON, err := githubMailboxEventJSON(key, interestedLifecycleBody(event.Body), false, event.Timestamp)
					if err != nil {
						return false, err
					}
					stored, err := enqueueGithubEvent(tx, key, session, eventJSON, false)
					if err != nil {
						return false, err
					}
					enqueued = enqueued || stored
				}
			}
			// The latest lifecycle event, delivered or filtered, is what a
			// session classified later is told.
			var latest *MappedGithubEvent
			var latestAt time.Time
			for i := range events {
				event := &events[i]
				if !event.Lifecycle {
					continue
				}
				at, ok := core.ParseChronoRFC3339(event.Timestamp)
				if !ok {
					continue
				}
				// Rust's max_by_key keeps the last of equal maxima.
				if latest == nil || !at.Before(latestAt) {
					latest, latestAt = event, at
				}
			}
			if latest != nil {
				body, err := databaseMarshalJSON(latest.Body)
				if err != nil {
					return false, err
				}
				if _, err := tx.Exec(`UPDATE github_watch_items
                     SET lifecycle_event_key=?4, lifecycle_event_json=?5, lifecycle_at=?6
                     WHERE owner=?1 AND repo=?2 AND number=?3
                       AND (lifecycle_at IS NULL OR julianday(lifecycle_at) <= julianday(?6))`,
					watch.Owner, watch.Repo, watch.Number, latest.Key, string(body), latest.Timestamp); err != nil {
					return false, err
				}
			}
		}
		if advance {
			if _, err := tx.Exec(`UPDATE github_watch_items SET timeline_at=?4, timeline_id=?5, review_comments_at=?6, review_comments_id=?7, processed_at=?8
                 WHERE owner=?1 AND repo=?2 AND number=?3
                   AND timeline_at=?9 AND timeline_id=?10 AND review_comments_at=?11
                   AND review_comments_id=?12 AND processed_at=?13`,
				watch.Owner, watch.Repo, watch.Number,
				progress.TimelineAt, progress.TimelineID, progress.ReviewCommentsAt, progress.ReviewCommentsID, progress.ProcessedAt,
				watch.TimelineAt, watch.TimelineID, watch.ReviewCommentsAt, watch.ReviewCommentsID, watch.ProcessedAt); err != nil {
				return false, err
			}
		}
		if err := tx.Commit(); err != nil {
			return false, err
		}
		return enqueued, nil
	})
	if err != nil {
		return err
	}
	if enqueued {
		NotifyMailboxOutboxChanged()
	}
	return nil
}

// githubEqualOptionalString is Option<String> equality.
func githubEqualOptionalString(a, b *string) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	return *a == *b
}

// loadGithubSessionTurnWindowsFrom is test-only in Rust.
func loadGithubSessionTurnWindowsFrom(path string, sessionID string) ([]GithubSessionTurnWindow, error) {
	connection, err := openReader(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	tx, err := connection.Begin()
	if err != nil {
		return nil, err
	}
	defer tx.Rollback()
	windows, err := sessionTurnWindows(tx, sessionID)
	if err != nil {
		return nil, err
	}
	if err := tx.Commit(); err != nil {
		return nil, err
	}
	return windows, nil
}

// sessionTurnWindows reconstructs turn intervals from the same persisted
// start and completion facts exposed by the transcript projection. An
// unmatched start counts only when the materialized projection still names it
// as the active turn. Turns the harness starts on its own have neither fact;
// see harnessTurnWindows.
func sessionTurnWindows(tx *sql.Tx, sessionID string) ([]GithubSessionTurnWindow, error) {
	bodies, err := queryTextColumn(tx, `SELECT body FROM api_events
             WHERE session_id=?1
               AND json_extract(body, '$.type') IN ('turn_started', 'turn_ended')
             ORDER BY seq`, sessionID)
	if err != nil {
		return nil, err
	}
	starts := map[string]int64{}
	promptTurnStarts := map[int64]struct{}{}
	windows := []GithubSessionTurnWindow{}
	for _, body := range bodies {
		event, err := core.UnmarshalApiEventData([]byte(body))
		if err != nil {
			return nil, err
		}
		switch event := event.(type) {
		case core.ApiEventDataTurnStarted:
			promptTurnStarts[int64(event.Turn.TurnStartPosition)] = struct{}{}
			starts[event.Turn.CommandID] = event.Turn.StartedAtMs
		case core.ApiEventDataTurnEnded:
			if startedAtMs, ok := starts[event.Turn.CommandID]; ok {
				delete(starts, event.Turn.CommandID)
				completedAtMs := event.Turn.CompletedAtMs
				windows = append(windows, GithubSessionTurnWindow{StartedAtMs: startedAtMs, CompletedAtMs: &completedAtMs})
			}
		}
	}
	var running bool
	var activeTurnJSON *string
	err = tx.QueryRow(`SELECT execution_state='running', active_turn_json
             FROM materialized_sessions WHERE session_id=?1`, sessionID).Scan(&running, &activeTurnJSON)
	if errors.Is(err, sql.ErrNoRows) {
		running, activeTurnJSON = false, nil
	} else if err != nil {
		return nil, err
	}
	var activeTurn *core.MaterializedTurn
	if activeTurnJSON != nil {
		if err := json.Unmarshal([]byte(*activeTurnJSON), &activeTurn); err != nil {
			return nil, err
		}
	}
	harnessTurnRunning := running && activeTurn == nil
	if activeTurn != nil {
		startedAtMs, ok := starts[activeTurn.CommandID]
		if ok {
			delete(starts, activeTurn.CommandID)
		} else {
			startedAtMs = activeTurn.StartedAtMs
		}
		windows = append(windows, GithubSessionTurnWindow{StartedAtMs: startedAtMs})
	}
	harness, err := harnessTurnWindows(tx, sessionID, promptTurnStarts, harnessTurnRunning)
	if err != nil {
		return nil, err
	}
	return append(windows, harness...), nil
}

// harnessTurnWindows: a turn the harness starts on its own, such as work a
// finished background command woke, records a turn completion but no prompt
// turn start. Its "Agent continued on its own" marker starts it. The agent's
// last message, thought or tool call before the next turn or restart ends it.
// A running session with no active prompt is still in the last such turn.
func harnessTurnWindows(tx *sql.Tx, sessionID string, promptTurnStarts map[int64]struct{}, harnessTurnRunning bool) ([]GithubSessionTurnWindow, error) {
	type marker struct {
		position    int64
		startsTurn  bool
		startedAtMs int64
	}
	rows, err := tx.Query(`SELECT position, stable_id GLOB ?2, created_at_ms
             FROM materialized_transcript_items
             WHERE session_id=?1 AND (stable_id GLOB ?2 OR stable_id GLOB ?3)
             ORDER BY position`,
		sessionID, core.HarnessTurnItemPrefix+"*", core.SessionRestartItemPrefix+"*")
	if err != nil {
		return nil, err
	}
	var markers []marker
	for rows.Next() {
		var m marker
		if err := rows.Scan(&m.position, &m.startsTurn, &m.startedAtMs); err != nil {
			rows.Close()
			return nil, err
		}
		markers = append(markers, m)
	}
	if err := rows.Err(); err != nil {
		rows.Close()
		return nil, err
	}
	rows.Close()
	boundarySet := map[int64]struct{}{}
	for _, m := range markers {
		boundarySet[m.position] = struct{}{}
	}
	for position := range promptTurnStarts {
		boundarySet[position] = struct{}{}
	}
	boundaries := make([]int64, 0, len(boundarySet))
	for position := range boundarySet {
		boundaries = append(boundaries, position)
	}
	slices.Sort(boundaries)
	lastWork, err := tx.Prepare(`SELECT MAX(last_changed_at_ms) FROM materialized_transcript_items
         WHERE session_id=?1 AND position > ?2 AND position < ?3
           AND (stable_id GLOB 'agent:*' OR stable_id GLOB 'thought:*'
                OR stable_id GLOB 'tool:*')`)
	if err != nil {
		return nil, err
	}
	defer lastWork.Close()
	windows := []GithubSessionTurnWindow{}
	for _, m := range markers {
		// A prompt with no text starts its turn with the same marker.
		if _, prompt := promptTurnStarts[m.position]; !m.startsTurn || prompt {
			continue
		}
		var next *int64
		if index := sort.Search(len(boundaries), func(i int) bool { return boundaries[i] > m.position }); index < len(boundaries) {
			next = &boundaries[index]
		}
		var completedAtMs *int64
		if !(next == nil && harnessTurnRunning) {
			bound := int64(math.MaxInt64)
			if next != nil {
				bound = *next
			}
			var lastChangedAtMs *int64
			if err := lastWork.QueryRow(sessionID, m.position, bound).Scan(&lastChangedAtMs); err != nil {
				return nil, err
			}
			// A turn that produced nothing could not have posted anything.
			if lastChangedAtMs == nil {
				continue
			}
			completedAtMs = lastChangedAtMs
		}
		windows = append(windows, GithubSessionTurnWindow{StartedAtMs: m.startedAtMs, CompletedAtMs: completedAtMs})
	}
	return windows, nil
}

// LoadGithubSessionCreationOutput is the output of this session's tool calls
// that were running within a second of atMs; GitHub timestamps have
// one-second precision. Nothing can print a new item's URL before it exists,
// so this ignores how the command was composed. Later turn text is not
// evidence: a session that merely mentions another session's new item
// afterwards would otherwise claim it.
func LoadGithubSessionCreationOutput(sessionID string, atMs int64) ([]string, error) {
	return loadGithubSessionCreationOutputFrom(DatabasePath(), sessionID, atMs)
}

func loadGithubSessionCreationOutputFrom(path string, sessionID string, atMs int64) ([]string, error) {
	connection, err := openReader(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	rows, err := connection.Query(`SELECT stable_id, body_json FROM materialized_transcript_items
         WHERE session_id=?1 AND created_at_ms <= ?2 + 1000
           AND (last_changed_at_ms >= ?2 - 1000
                OR json_extract(body_json, '$.call.status') IN ('pending', 'in_progress'))
           AND json_extract(body_json, '$.kind') = 'tool'`, sessionID, atMs)
	if err != nil {
		return nil, err
	}
	type stored struct{ stableID, body string }
	var bodies []stored
	for rows.Next() {
		var row stored
		if err := rows.Scan(&row.stableID, &row.body); err != nil {
			rows.Close()
			return nil, err
		}
		bodies = append(bodies, row)
	}
	if err := rows.Err(); err != nil {
		rows.Close()
		return nil, err
	}
	rows.Close()
	items := make([]*core.TranscriptItem, 0, len(bodies))
	for _, row := range bodies {
		body, err := core.DecodeTranscriptBody([]byte(row.body))
		if err != nil {
			return nil, err
		}
		items = append(items, &core.TranscriptItem{StableID: row.stableID, Body: body})
	}
	if err := hydrateTerminalOutputs(connection, sessionID, items); err != nil {
		return nil, err
	}
	outputs := core.TerminalOutputsOf(derefTranscriptItems(items))
	output := []string{}
	for _, item := range items {
		tool, ok := item.Body.(core.TranscriptBodyTool)
		if !ok {
			continue
		}
		call, err := tool.Call.Decode()
		if err != nil {
			return nil, err
		}
		// An absent status is ACP's default, pending.
		status := core.ToolStatusPending
		if call.Status != nil {
			status = core.ToolStatusOf(*call.Status)
		}
		output = append(output, transcript.ToolContentDetails(call.Content, outputs.ForTool(item.StableID), call.RawOutput, status)...)
		// MCP tools can supply structured output without a text block.
		if call.RawOutput != nil {
			output = append(output, githubDisplayJSON(call.RawOutput))
		}
	}
	return output, nil
}

// githubDisplayJSON is serde_json::Value's Display: compact, members in
// their stored order.
func githubDisplayJSON(raw json.RawMessage) string {
	value, err := jsonvalue.Parse(raw)
	if err != nil {
		return string(raw)
	}
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		return string(raw)
	}
	return string(encoded)
}

// derefTranscriptItems copies shared transcript items into the value slice
// the core joins read (Rust's iterator of references).
func derefTranscriptItems(items []*core.TranscriptItem) []core.TranscriptItem {
	values := make([]core.TranscriptItem, len(items))
	for i, item := range items {
		values[i] = *item
	}
	return values
}

// SeedGithubTestTurns is a test fixture (doc(hidden) in Rust).
func SeedGithubTestTurns(sessionID string, completedStartMs int64, completedAtMs int64, activeStartMs int64) error {
	_, err := submitDatabaseWrite("seed_github_test_turns", func(connection *Connection) (struct{}, error) {
		completedStart := fmt.Sprintf(`{"type":"turn_started","data":{"turn":{"command_id":"completed-turn","turn_start_position":1,"started_at_ms":%d}}}`, completedStartMs)
		completedEnd := fmt.Sprintf(`{"type":"turn_ended","data":{"turn":{"command_id":"completed-turn","completed_ordinal":1,"completed_at_ms":%d,"outcome":{"kind":"completed"}}}}`, completedAtMs)
		activeStart := fmt.Sprintf(`{"type":"turn_started","data":{"turn":{"command_id":"active-turn","turn_start_position":2,"started_at_ms":%d}}}`, activeStartMs)
		activeTurn := fmt.Sprintf(`{"command_id":"active-turn","turn_start_position":2,"started_at_ms":%d}`, activeStartMs)
		tx, err := connection.Begin()
		if err != nil {
			return struct{}{}, err
		}
		defer tx.Rollback()
		for _, seeded := range []struct {
			timestamp int64
			event     string
		}{
			{completedStartMs, completedStart},
			{completedAtMs, completedEnd},
			{activeStartMs, activeStart},
		} {
			if _, err := tx.Exec("INSERT INTO api_events(session_id, recorded_at_ms, body) VALUES (?1, ?2, ?3)",
				sessionID, seeded.timestamp, seeded.event); err != nil {
				return struct{}{}, err
			}
		}
		if _, err := tx.Exec(`INSERT INTO materialized_sessions(session_id, execution_state,
                    running_started_at_ms, active_turn_json)
             VALUES (?1, 'running', ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET execution_state='running',
                    running_started_at_ms=excluded.running_started_at_ms,
                    active_turn_json=excluded.active_turn_json`,
			sessionID, activeStartMs, activeTurn); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, tx.Commit()
	})
	return err
}

// FirstGithubWatchInterestedSession is a session with interest in one of the
// repository's watched items, to take a GitHub credential from when no live
// session or creator can supply one.
func FirstGithubWatchInterestedSession(owner string, repo string) (string, bool, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return "", false, err
	}
	defer connection.Close()
	var session string
	err = connection.QueryRow(`SELECT c.session_id FROM github_watch_items w
             JOIN github_watch_classifications c
               ON c.owner=w.owner AND c.repo=w.repo AND c.number=w.number
             WHERE w.owner=?1 AND w.repo=?2 AND c.interested=1 AND c.created=0
             ORDER BY c.session_id LIMIT 1`, owner, repo).Scan(&session)
	if errors.Is(err, sql.ErrNoRows) {
		return "", false, nil
	}
	if err != nil {
		return "", false, err
	}
	return session, true, nil
}

// GithubWatchedRepository is one entry of LoadGithubWatchedRepositories:
// Rust's `(String, String)`, the owner and the repository.
type GithubWatchedRepository struct {
	Owner string
	Repo  string
}

func LoadGithubWatchedRepositories() ([]GithubWatchedRepository, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	rows, err := connection.Query("SELECT DISTINCT owner, repo FROM github_watch_items ORDER BY owner, repo")
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	repositories := []GithubWatchedRepository{}
	for rows.Next() {
		var repository GithubWatchedRepository
		if err := rows.Scan(&repository.Owner, &repository.Repo); err != nil {
			return nil, err
		}
		repositories = append(repositories, repository)
	}
	return repositories, rows.Err()
}

// SeedGithubRetryDue is a test fixture (doc(hidden) in Rust).
func SeedGithubRetryDue(owner string, repo string, number int64, sessionID string) error {
	_, err := submitDatabaseWrite("seed_github_retry_due", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec("UPDATE github_watch_classification_failures SET next_attempt_at_ms=0 WHERE owner=?1 AND repo=?2 AND number=?3 AND session_id=?4",
			owner, repo, number, sessionID)
		return struct{}{}, err
	})
	return err
}

// SeedGithubRepoStart is a test fixture (doc(hidden) in Rust).
func SeedGithubRepoStart(owner string, repo string, at string) error {
	_, err := submitDatabaseWrite("seed_github_repo_start", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec(`UPDATE github_watch_cursors SET updated_at=?3, watch_started_at=?3,
            reconciled_through=?3, last_reconciled_at_ms=NULL WHERE owner=?1 AND repo=?2`,
			owner, repo, at)
		return struct{}{}, err
	})
	return err
}

// GithubHistoryScanCancelled: the scan stopped because its caller asked it
// to.
type GithubHistoryScanCancelled struct{}

func (GithubHistoryScanCancelled) Error() string { return "GitHub history scan cancelled" }

// ScanGithubHistory reads one session's transcript from a single read
// snapshot and streams it through history, which renders it for every item
// of the session: a light pass that counts rows for the headers, then the
// bodies in order, one at a time, then the rows around the windows that were
// selected, each fetched once by its rowid for all items. Nothing is
// collected: memory does not grow with the transcript. Raw JSON preserves
// persisted tool-call field order for the frozen Python renderer.
//
// Rows created or changed after the latest cutoff are withheld from every
// item, so they are only counted, from their timestamps. stop is checked for
// every row; when it returns true the scan ends with
// GithubHistoryScanCancelled and the snapshot is released.
func ScanGithubHistory[D any, S transcript.HistorySink[D]](sessionID string, history *transcript.SessionHistory[D, S], stop func() bool) ([]transcript.HistoryResult[D], error) {
	return scanGithubHistoryFrom(DatabasePath(), sessionID, history, stop)
}

// githubHistoryText is rusqlite's text reading of a column value scanned into
// an any: the string for TEXT, absent for every other storage class.
func githubHistoryText(value any, column int) (*string, error) {
	text, ok := value.(string)
	if !ok {
		return nil, nil
	}
	if !utf8.ValidString(text) {
		return nil, fmt.Errorf("Invalid UTF-8 sequence in column %d", column)
	}
	return &text, nil
}

func scanGithubHistoryFrom[D any, S transcript.HistorySink[D]](path string, sessionID string, history *transcript.SessionHistory[D, S], stop func() bool) ([]transcript.HistoryResult[D], error) {
	latest, ok := history.MaxCutoffMs()
	if !ok {
		return []transcript.HistoryResult[D]{}, nil
	}
	check := func() error {
		if stop() {
			return GithubHistoryScanCancelled{}
		}
		return nil
	}
	reader, err := openReader(path)
	if err != nil {
		return nil, err
	}
	defer reader.Close()
	connection, err := reader.Begin()
	if err != nil {
		return nil, err
	}
	defer connection.Rollback()
	err = func() error {
		rows, err := connection.Query(`SELECT created_at_ms, last_changed_at_ms FROM materialized_transcript_items
             WHERE session_id=?1 AND (created_at_ms > ?2 OR last_changed_at_ms > ?2)`, sessionID, latest)
		if err != nil {
			return err
		}
		defer rows.Close()
		for rows.Next() {
			if err := check(); err != nil {
				return err
			}
			var meta transcript.RowMeta
			if err := rows.Scan(&meta.CreatedAtMs, &meta.LastChangedAtMs); err != nil {
				return err
			}
			// Withheld from every item whatever it is, so its kind is not read.
			history.CountRow(&meta)
		}
		return rows.Err()
	}()
	if err != nil {
		return nil, err
	}
	err = func() error {
		// SQLite parses each body once for its kind and, for messages, the
		// few fields that decide notices; rows of other kinds only have their
		// timestamps counted, because a pair with an earlier cutoff withholds
		// them as changed or later whatever they are.
		// json_type keeps the renderer's typing: `as_str` accepts only JSON
		// strings and `as_u64` only integers, while json_extract would turn
		// `true` into 1 and an object or array into text.
		rows, err := connection.Query(`SELECT created_at_ms, last_changed_at_ms, kind,
                    CASE WHEN kind = 'message' AND json_type(body_json, '$.event.body.type') = 'text'
                         THEN json_extract(body_json, '$.event.body.type') END,
                    CASE WHEN kind = 'message' AND json_type(body_json, '$.event.body.number') = 'integer'
                         THEN json_extract(body_json, '$.event.body.number') END,
                    CASE WHEN kind = 'message' AND json_type(body_json, '$.event.body.repo') = 'text'
                         THEN json_extract(body_json, '$.event.body.repo') END
             FROM (SELECT created_at_ms, last_changed_at_ms, body_json,
                          json_extract(body_json, '$.kind') AS kind
                   FROM materialized_transcript_items
                   WHERE session_id=?1 AND created_at_ms <= ?2 AND last_changed_at_ms <= ?2)`, sessionID, latest)
		if err != nil {
			return err
		}
		defer rows.Close()
		for rows.Next() {
			if err := check(); err != nil {
				return err
			}
			var meta transcript.RowMeta
			var kind, eventType, eventNumber, eventRepo any
			if err := rows.Scan(&meta.CreatedAtMs, &meta.LastChangedAtMs, &kind, &eventType, &eventNumber, &eventRepo); err != nil {
				return err
			}
			for _, column := range []struct {
				value  any
				index  int
				target *string
			}{{kind, 2, &meta.Kind}, {eventType, 3, &meta.EventType}} {
				text, err := githubHistoryText(column.value, column.index)
				if err != nil {
					return err
				}
				if text != nil {
					*column.target = *text
				}
			}
			// A JSON integer; a float such as 42.0 is not one for serde_json.
			if number, ok := eventNumber.(int64); ok && number >= 0 {
				unsigned := uint64(number)
				meta.EventNumber = &unsigned
			}
			meta.EventRepo, err = githubHistoryText(eventRepo, 5)
			if err != nil {
				return err
			}
			history.CountRow(&meta)
		}
		return rows.Err()
	}()
	if err != nil {
		return nil, err
	}
	if err := history.Begin(); err != nil {
		return nil, err
	}
	err = func() error {
		// Only kinds the transcript shows, as in the count above; the rest are
		// never decoded.
		rows, err := connection.Query(`SELECT rowid, created_at_ms, last_changed_at_ms, body_json
             FROM materialized_transcript_items
             WHERE session_id=?1 AND created_at_ms <= ?2 AND last_changed_at_ms <= ?2
               AND json_extract(body_json, '$.kind') IN ('user', 'agent', 'tool', 'message')
             ORDER BY position, stable_id`, sessionID, latest)
		if err != nil {
			return err
		}
		defer rows.Close()
		for rows.Next() {
			if err := check(); err != nil {
				return err
			}
			var key, createdAtMs, lastChangedAtMs int64
			var body any
			if err := rows.Scan(&key, &createdAtMs, &lastChangedAtMs, &body); err != nil {
				return err
			}
			text, ok := body.(string)
			if !ok {
				return fmt.Errorf("Invalid column type %s at index: 3, name: body_json", sqlRowStorageClass(body))
			}
			if err := history.Row(key, createdAtMs, lastChangedAtMs, text); err != nil {
				return err
			}
		}
		return rows.Err()
	}()
	if err != nil {
		return nil, err
	}
	return history.Complete(&githubHistoryRows{connection: connection, check: check}, check)
}

// sqlRowStorageClass names the storage class of a column value scanned into
// an any, as rusqlite's Type does.
func sqlRowStorageClass(value any) string {
	switch value.(type) {
	case nil:
		return "Null"
	case int64:
		return "Integer"
	case float64:
		return "Real"
	case string:
		return "Text"
	case []byte:
		return "Blob"
	}
	return fmt.Sprintf("%T", value)
}

// githubHistoryRows fetches row bodies by rowid for the scan's last pass
// (the Rows type local to scan_github_history_from in Rust).
type githubHistoryRows struct {
	connection Queryer
	check      func() error
}

func (r *githubHistoryRows) Body(key int64) (string, error) {
	if err := r.check(); err != nil {
		return "", err
	}
	var body string
	err := r.connection.QueryRow("SELECT body_json FROM materialized_transcript_items WHERE rowid=?1", key).Scan(&body)
	return body, err
}
