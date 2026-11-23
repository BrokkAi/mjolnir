// Port of mj-store/src/database/github_watch.rs.
//
// Durable cursors, classifications, and creator watches for GitHub polling.

package store

import (
	"database/sql"

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
	panic("unported: mj-store/src/database/github_watch.rs load_github_classification_failures")
}

func RecordGithubClassificationFailure(owner string, repo string, number int64, sessionID string) error {
	panic("unported: mj-store/src/database/github_watch.rs record_github_classification_failure")
}

// GithubRepoCursorFor is Rust's github_repo_cursor (renamed because Go cannot
// give the function and the GithubRepoCursor type the same name). It
// initializes the watch boundary once, before the first network request.
func GithubRepoCursorFor(owner string, repo string) (GithubRepoCursor, error) {
	panic("unported: mj-store/src/database/github_watch.rs github_repo_cursor")
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
	panic("unported: mj-store/src/database/github_watch.rs record_github_page")
}

// ClaimGithubReconciliation claims before requesting pages so failures and
// restarts cannot exceed the hourly rate.
func ClaimGithubReconciliation(owner string, repo string, nowMs int64) (bool, error) {
	panic("unported: mj-store/src/database/github_watch.rs claim_github_reconciliation")
}

func LoadGithubSeenItems(owner string, repo string) ([]GithubSeenItem, error) {
	panic("unported: mj-store/src/database/github_watch.rs load_github_seen_items")
}

func LoadGithubItemClassifiedSessions(owner string, repo string, number int64) (map[string]struct{}, error) {
	panic("unported: mj-store/src/database/github_watch.rs load_github_item_classified_sessions")
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
	panic("unported: mj-store/src/database/github_watch.rs commit_github_item_classification")
}

func LoadGithubWatches(owner string, repo string) ([]GithubItemWatch, error) {
	panic("unported: mj-store/src/database/github_watch.rs load_github_watches")
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
	panic("unported: mj-store/src/database/github_watch.rs enqueue_github_event")
}

// interestedEventKey is the outbox key of an interested session's copy of a
// lifecycle event. It differs from the creator's key, which is the primary
// key and would otherwise make one of the two inserts a silent no-op.
func interestedEventKey(baseKey string, sessionID string) string {
	return baseKey + ":" + sessionID
}

func githubMailboxEventJSON(key string, body core.MailboxEventBody, wake bool, timestamp string) (string, error) {
	panic("unported: mj-store/src/database/github_watch.rs github_mailbox_event_json")
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

// authoredDuringSessionTurn is whether sessionID itself caused a GitHub
// action: the action's author is the shared credential's account, and it
// happened during one of the session's turns. The session already knows what
// it did, so delivering it, even without a wake, would echo it into its
// context. Runs inside the write transaction that decides delivery. A session
// whose turns cannot be read gets the event.
func authoredDuringSessionTurn(tx *sql.Tx, windowsBySession turnWindows, credentialLogin *string, authorLogin *string, createdAt *string, sessionID string) bool {
	panic("unported: mj-store/src/database/github_watch.rs authored_during_session_turn")
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
	panic("unported: mj-store/src/database/github_watch.rs commit_github_timeline")
}

// loadGithubSessionTurnWindowsFrom is test-only in Rust.
func loadGithubSessionTurnWindowsFrom(path string, sessionID string) ([]GithubSessionTurnWindow, error) {
	panic("unported: mj-store/src/database/github_watch.rs load_github_session_turn_windows_from")
}

// sessionTurnWindows reconstructs turn intervals from the same persisted
// start and completion facts exposed by the transcript projection. An
// unmatched start counts only when the materialized projection still names it
// as the active turn. Turns the harness starts on its own have neither fact;
// see harnessTurnWindows.
func sessionTurnWindows(tx *sql.Tx, sessionID string) ([]GithubSessionTurnWindow, error) {
	panic("unported: mj-store/src/database/github_watch.rs session_turn_windows")
}

// harnessTurnWindows: a turn the harness starts on its own, such as work a
// finished background command woke, records a turn completion but no prompt
// turn start. Its "Agent continued on its own" marker starts it. The agent's
// last message, thought or tool call before the next turn or restart ends it.
// A running session with no active prompt is still in the last such turn.
func harnessTurnWindows(tx *sql.Tx, sessionID string, promptTurnStarts map[int64]struct{}, harnessTurnRunning bool) ([]GithubSessionTurnWindow, error) {
	panic("unported: mj-store/src/database/github_watch.rs harness_turn_windows")
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
	panic("unported: mj-store/src/database/github_watch.rs load_github_session_creation_output_from")
}

// SeedGithubTestTurns is a test fixture (doc(hidden) in Rust).
func SeedGithubTestTurns(sessionID string, completedStartMs int64, completedAtMs int64, activeStartMs int64) error {
	panic("unported: mj-store/src/database/github_watch.rs seed_github_test_turns")
}

// FirstGithubWatchInterestedSession is a session with interest in one of the
// repository's watched items, to take a GitHub credential from when no live
// session or creator can supply one.
func FirstGithubWatchInterestedSession(owner string, repo string) (string, bool, error) {
	panic("unported: mj-store/src/database/github_watch.rs first_github_watch_interested_session")
}

// GithubWatchedRepository is one entry of LoadGithubWatchedRepositories:
// Rust's `(String, String)`, the owner and the repository.
type GithubWatchedRepository struct {
	Owner string
	Repo  string
}

func LoadGithubWatchedRepositories() ([]GithubWatchedRepository, error) {
	panic("unported: mj-store/src/database/github_watch.rs load_github_watched_repositories")
}

// SeedGithubRetryDue is a test fixture (doc(hidden) in Rust).
func SeedGithubRetryDue(owner string, repo string, number int64, sessionID string) error {
	panic("unported: mj-store/src/database/github_watch.rs seed_github_retry_due")
}

// SeedGithubRepoStart is a test fixture (doc(hidden) in Rust).
func SeedGithubRepoStart(owner string, repo string, at string) error {
	panic("unported: mj-store/src/database/github_watch.rs seed_github_repo_start")
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

func scanGithubHistoryFrom[D any, S transcript.HistorySink[D]](path string, sessionID string, history *transcript.SessionHistory[D, S], stop func() bool) ([]transcript.HistoryResult[D], error) {
	panic("unported: mj-store/src/database/github_watch.rs scan_github_history_from")
}

// githubHistoryRows fetches row bodies by rowid for the scan's last pass
// (the Rows type local to scan_github_history_from in Rust).
type githubHistoryRows struct {
	connection sqlConnection
	check      func() error
}

func (r *githubHistoryRows) Body(key int64) (string, error) {
	panic("unported: mj-store/src/database/github_watch.rs Rows::body")
}
