// Port of mj-github-watch/src/service.rs.

package githubwatch

import (
	"cmp"
	"context"
	"errors"
	"fmt"
	"maps"
	"net/http"
	"net/netip"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/githubrelevance"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

const (
	maxClassificationsPerPoll       = 50
	itemsPerPage                    = 100
	mergeCloseToleranceMs     int64 = 5_000
	repositoryConcurrency           = 4
	apiTimeout                      = 20 * time.Second
	configRecheckInterval           = 5 * time.Second
	minInterval                     = 10 * time.Second
	maxInterval                     = 60 * 60 * time.Second
)

// githubResponseLimit is the largest GitHub REST API response body read
// (Rust's `8 * 1024 * 1024`, written twice in GithubApi::get).
const githubResponseLimit = 8 * 1024 * 1024

// preparedPair is one item's classifier input before its session history is
// rendered.
type preparedPair struct {
	original core.GithubItem
	base     core.GithubItemEvidence
	cutoffMs int64
}

// pollControl is the live "GitHub events enabled" check a poll consults
// before each step (Rust's PollControl, an `Arc<dyn Fn() -> bool>`).
type pollControl struct {
	enabled func() bool
}

func newPollControl(enabled func() bool) pollControl {
	return pollControl{enabled: enabled}
}

func (c pollControl) checkEnabled() error {
	if c.enabled() {
		return nil
	}
	return githubEventsDisabled{}
}

// githubEventsDisabled is the error a poll stops with when GitHub events are
// disabled while it runs.
type githubEventsDisabled struct{}

func (githubEventsDisabled) Error() string { return "GitHub events disabled during poll" }

func isGithubEventsDisabled(err error) bool {
	return errors.As(err, new(githubEventsDisabled))
}

// githubClassifier is Rust's GithubClassifier trait (its ClassifyFuture is
// Classify's context and result here).
type githubClassifier interface {
	Classify(ctx context.Context, evidence *core.GithubItemEvidence) (core.GithubItemVerdict, error)
}

var _ githubClassifier = (*githubrelevance.Classifier)(nil)

// GithubWatchHost is the daemon capabilities used by the GitHub watch
// service.
type GithubWatchHost interface {
	GithubWatchSnapshot() GithubWatchSnapshot

	// GithubEventsEnabled checks the live setting so disabling delivery
	// stops an in-flight poll.
	GithubEventsEnabled() bool

	PushPublicNotice(text string)

	// GithubTokenForSession is the GitHub credential of the session; nil
	// polls anonymously.
	GithubTokenForSession(ctx context.Context, sessionID string) (*string, error)
}

// GithubWatchSnapshot is a single runtime projection of watch settings and
// session state.
type GithubWatchSnapshot struct {
	Enabled bool
	Config  core.GithubWatchConfig
	Jev     core.JevConfig
	State   core.StateState
}

// Run is the cancellable periodic poller; it returns once ctx is done (Rust's
// `stop` CancellationToken). Poll progress lives only in the database; a
// replacement daemon can resume from the same repo cursors and event keys.
func Run(ctx context.Context, host GithubWatchHost) error {
	panic("unported: mj-github-watch/src/service.rs run")
}

// runPollLoop polls until ctx is done. student is the local relevance
// model, created when local inference is enabled and shut down when it is
// disabled (Rust's `&mut Option<Arc<Student>>`); Run shuts down what is left.
func runPollLoop(ctx context.Context, host GithubWatchHost, student **githubrelevance.Student, notices *githubrelevance.Notices) error {
	panic("unported: mj-github-watch/src/service.rs run_poll_loop")
}

// ownerRepo is a GitHub repository as the watcher keys it: Rust's
// `(String, String)` of the lowercased owner and repository. Its fields are
// those of store.GithubWatchedRepository, so either converts to the other.
type ownerRepo struct {
	Owner string
	Repo  string
}

// compareOwnerRepo orders repositories as Rust orders the tuple.
func compareOwnerRepo(a, b ownerRepo) int {
	return cmp.Or(strings.Compare(a.Owner, b.Owner), strings.Compare(a.Repo, b.Repo))
}

// sessionsByGithubRepo is the live primary sessions of each repository, in
// session ID order. Rust returns a BTreeMap; callers iterate the keys sorted
// by compareOwnerRepo.
func sessionsByGithubRepo(state *core.StateState) map[ownerRepo][]core.SessionRecord {
	sessionsByRepo := map[ownerRepo][]core.SessionRecord{}
	for session := range state.Sessions.Values() {
		// A session started with `--no-github-events` is never a delivery target.
		if !session.State.HasLiveWorker() || state.IsSubagentSession(session.ID) || session.NoGithubEvents {
			continue
		}
		for _, repository := range sessionGithubRepositories(session) {
			sessionsByRepo[repository] = append(sessionsByRepo[repository], *session)
		}
	}
	return sessionsByRepo
}

// sessionGithubRepositories is the GitHub repositories a session's project is
// checked out from, sorted and without duplicates (Rust's BTreeSet).
func sessionGithubRepositories(session *core.SessionRecord) []ownerRepo {
	repositories := map[ownerRepo]struct{}{}
	if session.Project != nil {
		for _, identity := range session.Project.Identities {
			if github, ok := identity.(core.RepositoryIdentityGithub); ok {
				repositories[ownerRepo{Owner: asciiLowercase(github.Owner), Repo: asciiLowercase(github.Repository)}] = struct{}{}
			}
		}
	}
	return slices.SortedFunc(maps.Keys(repositories), compareOwnerRepo)
}

func pollRepository(ctx context.Context, owner, repo string, sessions []core.SessionRecord, config core.GithubWatchConfig, classifier githubClassifier, control pollControl, host GithubWatchHost) error {
	panic("unported: mj-github-watch/src/service.rs poll_repository")
}

func pollRepositoryWithContext(ctx context.Context, owner, repo string, sessions []core.SessionRecord, classifier githubClassifier, api *githubAPI) error {
	panic("unported: mj-github-watch/src/service.rs poll_repository_with_context")
}

func recordRepository(ctx context.Context, owner, repo string, cursor *store.GithubRepoCursor, api *githubAPI) error {
	return acquireRepository(ctx, owner, repo, cursor.UpdatedAt, nil, api)
}

func reconcileRepository(ctx context.Context, owner, repo string, api *githubAPI, now time.Time) error {
	panic("unported: mj-github-watch/src/service.rs reconcile_repository")
}

// acquireRepository records the issues feed from `from`, up to until when it
// is set (a reconciliation pass) or to the newest item otherwise.
func acquireRepository(ctx context.Context, owner, repo, from string, until *string, api *githubAPI) error {
	panic("unported: mj-github-watch/src/service.rs acquire_repository")
}

func classifyRecordedItems(ctx context.Context, owner, repo string, sessions []core.SessionRecord, classifier githubClassifier, watchStartedAt string, api *githubAPI) error {
	panic("unported: mj-github-watch/src/service.rs classify_recorded_items")
}

// prepareSessionEvidence is nil without an error when the session created
// the item: its creator is never told about it and the classifier is not
// asked.
func prepareSessionEvidence(owner, repo string, item *store.GithubSeenItem, session *core.SessionRecord, digest core.SessionContext, credentialLogin *string, control pollControl) (*preparedPair, error) {
	panic("unported: mj-github-watch/src/service.rs prepare_session_evidence")
}

// buildSessionEvidence builds every pair's evidence from one scan of the
// session's transcript. Ending ctx stops the scan and releases its snapshot.
func buildSessionEvidence(ctx context.Context, sessionID string, pairs []preparedPair) ([]core.GithubItemEvidence, error) {
	panic("unported: mj-github-watch/src/service.rs build_session_evidence")
}

// classifyForSession classifies item for session; a nil evidence means the
// session created the item, which is stored as not interested.
func classifyForSession(ctx context.Context, owner, repo string, item store.GithubSeenItem, session core.SessionRecord, evidence *core.GithubItemEvidence, classifier githubClassifier, control pollControl) error {
	panic("unported: mj-github-watch/src/service.rs classify_for_session")
}

func openedBySession(sessionID, createdAt string, number uint64, url string) (bool, error) {
	panic("unported: mj-github-watch/src/service.rs opened_by_session")
}

func sessionDigestForEvidence(sessionID string) (core.SessionContext, error) {
	return spawnBlocking("load GitHub session digest", func() (core.SessionContext, error) {
		materialized, err := store.LoadMaterializedRecentTurns(sessionID, core.SessionTurns)
		if err != nil || materialized == nil {
			return core.SessionContext{}, err
		}
		return transcript.SessionDigest(materialized), nil
	})
}

func pollRepositoryTimelines(ctx context.Context, owner, repo string, api *githubAPI) error {
	panic("unported: mj-github-watch/src/service.rs poll_repository_timelines")
}

func pollItemTimeline(ctx context.Context, watch store.GithubItemWatch, api *githubAPI) error {
	panic("unported: mj-github-watch/src/service.rs poll_item_timeline")
}

type timelineMapping struct {
	events           []store.MappedGithubEvent
	cursorAt         string
	cursorID         int64
	reviewCommentsAt string
	reviewCommentsID int64
}

// mapItemActivity: each source owns its cursor. A later comment read cannot
// advance the timeline past an event that appeared after the timeline read.
func mapItemActivity(watch *store.GithubItemWatch, entries []any, comments []any, mergedAt *string) (timelineMapping, error) {
	panic("unported: mj-github-watch/src/service.rs map_item_activity")
}

// hasCloseWithoutMerge is whether the timeline has a close after the cursor
// with no merge before it in the same open interval. Only such a close needs
// the pull request's merge time to tell a merge from a close.
func hasCloseWithoutMerge(entries []any, since string) bool {
	merged := false
	for _, entry := range entries {
		event, _ := jsonStr(jsonIndex(entry, "event"))
		switch event {
		case "merged":
			merged = true
		case "reopened":
			merged = false
		case "closed":
			if at, ok := commentCreationTimestamp(entry); !merged && ok && !timestampPrecedes(at, since) {
				return true
			}
		}
	}
	return false
}

// sameMoment: GitHub records the merge time and the closing entry within a
// second or two of each other.
func sameMoment(left, right string) bool {
	leftMs, okLeft := timestampMillis(left)
	rightMs, okRight := timestampMillis(right)
	if !okLeft || !okRight {
		return false
	}
	difference := leftMs - rightMs
	if difference < 0 {
		difference = -difference
	}
	return difference <= mergeCloseToleranceMs
}

// mapTimeline: entry payloads supply actors, comment text, review state and
// event IDs. Watch metadata supplies only the item's identity and display
// title.
func mapTimeline(watch *store.GithubItemWatch, entries []any, since *string, mergedAt *string) (timelineMapping, error) {
	panic("unported: mj-github-watch/src/service.rs map_timeline")
}

// parseGithubItem reads one issues-feed entry; false when it has no number
// or timestamps.
func parseGithubItem(value any) (store.GithubSeenItem, bool) {
	number, okNumber := jsonI64(jsonIndex(value, "number"))
	createdAt, okCreated := jsonStr(jsonIndex(value, "created_at"))
	updatedAt, okUpdated := jsonStr(jsonIndex(value, "updated_at"))
	if !okNumber || !okCreated || !okUpdated {
		return store.GithubSeenItem{}, false
	}
	kind := "issue"
	if jsonHas(value, "pull_request") {
		kind = "pull_request"
	}
	title, _ := jsonStr(jsonIndex(value, "title"))
	body, _ := jsonStr(jsonIndex(value, "body"))
	author, _ := jsonStr(jsonIndex(jsonIndex(value, "user"), "login"))
	url, _ := jsonStr(jsonIndex(value, "html_url"))
	return store.GithubSeenItem{Number: number, Kind: kind, Title: title, Body: body, Author: author, URL: url, CreatedAt: createdAt, UpdatedAt: updatedAt}, true
}

func itemKindFromWatch(kind string) core.GithubItemKind {
	if kind == "pull_request" {
		return core.GithubItemKindPullRequest
	}
	return core.GithubItemKindIssue
}

// compareWatermark orders (timestamp, id) watermarks: by instant when both
// timestamps parse, by their text otherwise, then by id.
func compareWatermark(aAt string, aID uint64, bAt string, bID uint64) int {
	a, okA := core.ParseChronoRFC3339(aAt)
	b, okB := core.ParseChronoRFC3339(bAt)
	order := strings.Compare(aAt, bAt)
	if okA && okB {
		order = a.Compare(b)
	}
	return cmp.Or(order, cmp.Compare(aID, bID))
}

func commentCreationTimestamp(comment any) (string, bool) {
	for _, key := range []string{"created_at", "submitted_at", "updated_at"} {
		if at, ok := jsonStr(jsonIndex(comment, key)); ok {
			return at, true
		}
	}
	return "", false
}

func timestampMillis(timestamp string) (int64, bool) {
	parsed, ok := core.ParseChronoRFC3339(timestamp)
	if !ok {
		return 0, false
	}
	return parsed.UnixMilli(), true
}

func timestampPrecedes(timestamp, boundary string) bool {
	parsedTimestamp, okTimestamp := core.ParseChronoRFC3339(timestamp)
	parsedBoundary, okBoundary := core.ParseChronoRFC3339(boundary)
	if okTimestamp && okBoundary {
		return parsedTimestamp.Before(parsedBoundary)
	}
	return timestamp < boundary
}

// overlapTimestamp is one second before timestamp, written with
// milliseconds in timestamp's offset (`Z` for UTC), as chrono's
// `to_rfc3339_opts(SecondsFormat::Millis, true)` writes it; timestamp itself
// when it does not parse.
func overlapTimestamp(timestamp string) string {
	parsed, ok := core.ParseChronoRFC3339(timestamp)
	if !ok {
		return timestamp
	}
	return parsed.Add(-time.Second).Format("2006-01-02T15:04:05.000Z07:00")
}

// truncateUTF8 is text cut to at most maxBytes, at a character boundary.
func truncateUTF8(text string, maxBytes int) string {
	if len(text) <= maxBytes {
		return text
	}
	end := maxBytes
	for !utf8.RuneStart(text[end]) {
		end--
	}
	return text[:end]
}

// spawnBlocking runs work as mj_core::runtime::spawn_blocking does and
// reports a panic in it as an error under what, as Rust's
// `.await.context(what)?` reports the task's JoinError.
func spawnBlocking[T any](what string, work func() (T, error)) (T, error) {
	type outcome struct {
		value T
		err   error
	}
	joined, err := core.SpawnBlocking(func() outcome {
		value, err := work()
		return outcome{value: value, err: err}
	}).Join(context.Background())
	if err != nil {
		var zero T
		return zero, fmt.Errorf("%s: %w", what, err)
	}
	return joined.value, joined.err
}

func blockingDB[T any](work func() (T, error)) (T, error) {
	return spawnBlocking("GitHub watch database task failed", work)
}

// githubCredentialState is shared by every poll that uses one credential.
type githubCredentialState struct {
	// rateLimitMu guards rateLimitUntil, the moment before which no request
	// is sent with this credential; nil when none is pending (Rust's
	// `tokio::sync::Mutex<Option<Instant>>`).
	rateLimitMu    sync.Mutex
	rateLimitUntil *time.Time
	// login holds the login cache: receiving it locks the cache and sending
	// it back unlocks it. Rust holds a tokio Mutex across the lookup, which
	// a dropped lookup releases; here a waiter whose ctx ends stops waiting.
	login chan *githubLoginCache
}

// newGithubCredentialState is GithubCredentialState's Default.
func newGithubCredentialState() *githubCredentialState {
	login := make(chan *githubLoginCache, 1)
	login <- &githubLoginCache{}
	return &githubCredentialState{login: login}
}

type githubLoginCache struct {
	success             *string
	retryAt             *time.Time
	consecutiveFailures uint8
}

// githubCredentials is the process-wide credential states, keyed by the
// SHA-256 of the credential (Rust's
// `static OnceLock<Mutex<HashMap<[u8; 32], Arc<GithubCredentialState>>>>`).
var githubCredentials = struct {
	sync.Mutex
	states map[[32]byte]*githubCredentialState
}{states: map[[32]byte]*githubCredentialState{}}

// credentialState is the shared state of token's credential; nil is
// anonymous access.
func credentialState(token *string) *githubCredentialState {
	panic("unported: mj-github-watch/src/service.rs credential_state")
}

// isLoopbackHost: host is the URL's host as url::Url::host_str gives it, so
// an IPv6 address keeps its brackets and never parses here, as in Rust.
func isLoopbackHost(host string) bool {
	host = strings.TrimSuffix(host, ".")
	if asciiEqualFold(host, "localhost") {
		return true
	}
	// Rust's IpAddr parser takes no zone, and an IPv4-mapped IPv6 address is
	// not an IPv6 loopback address.
	address, err := netip.ParseAddr(host)
	return err == nil && address.Zone() == "" && !address.Is4In6() && address.IsLoopback()
}

type githubAPI struct {
	// base is the API base URL without trailing slashes.
	base   string
	client *http.Client
	// headers are the default headers of every request: Accept, User-Agent
	// and, with a credential, Authorization.
	headers    http.Header
	credential *githubCredentialState
	control    pollControl
}

// queryParam is one query pair; requests keep the order given (Rust's
// `&[(&str, String)]`).
type queryParam struct {
	name  string
	value string
}

// newGithubAPIWithControl is GithubApi::new_with_control: HTTPS, or plain
// HTTP to a loopback host; no redirects; requests time out after
// apiTimeout.
func newGithubAPIWithControl(base string, token *string, control pollControl) (*githubAPI, error) {
	panic("unported: mj-github-watch/src/service.rs GithubApi::new_with_control")
}

func (a *githubAPI) arrayPages(ctx context.Context, path string, query []queryParam) ([]any, error) {
	values, _, err := a.arrayPagesObserved(ctx, path, query)
	return values, err
}

// arrayPagesObserved reads every page of an array resource, with the
// observed time of the first page.
func (a *githubAPI) arrayPagesObserved(ctx context.Context, path string, query []queryParam) ([]any, string, error) {
	panic("unported: mj-github-watch/src/service.rs GithubApi::array_pages_observed")
}

func (a *githubAPI) getJSON(ctx context.Context, path string, query []queryParam) (any, error) {
	panic("unported: mj-github-watch/src/service.rs GithubApi::get_json")
}

// credentialLogin is the credential's GitHub login, cached once known; nil
// while a failed lookup backs off.
func (a *githubAPI) credentialLogin(ctx context.Context) (*string, error) {
	panic("unported: mj-github-watch/src/service.rs GithubApi::credential_login")
}

// get sends a GET for path (relative to the base) with query, after any
// rate-limit deadline of the credential (Rust's GithubApi::get, given the
// request the caller built from the same path and query).
func (a *githubAPI) get(ctx context.Context, path string, query []queryParam) (githubResponse, error) {
	panic("unported: mj-github-watch/src/service.rs GithubApi::get")
}

func (a *githubAPI) waitForRateLimit(ctx context.Context) error {
	panic("unported: mj-github-watch/src/service.rs GithubApi::wait_for_rate_limit")
}

func (a *githubAPI) recordRateLimit(response *http.Response) {
	panic("unported: mj-github-watch/src/service.rs GithubApi::record_rate_limit")
}

// githubHTTPStatus is an unsuccessful GitHub response status.
type githubHTTPStatus struct {
	status     int
	observedAt string
}

func (e githubHTTPStatus) Error() string {
	return "GitHub REST API returned " + httpStatusDisplay(e.status)
}

type githubResponse struct {
	observedAt  string
	value       any
	notModified bool
}

// httpStatusDisplay is the Display of an http::StatusCode: the code and its
// canonical reason, which for a few codes is worded differently from
// net/http's. (A copy of worker's verdictStatusDisplay.)
func httpStatusDisplay(status int) string {
	reason := http.StatusText(status)
	switch status {
	case http.StatusNonAuthoritativeInfo:
		reason = "Non Authoritative Information"
	case http.StatusRequestEntityTooLarge:
		reason = "Payload Too Large"
	case http.StatusRequestURITooLong:
		reason = "URI Too Long"
	case http.StatusRequestedRangeNotSatisfiable:
		reason = "Range Not Satisfiable"
	}
	if reason == "" {
		reason = "<unknown status code>"
	}
	return strconv.Itoa(status) + " " + reason
}

// jsonIndex is serde_json's `value[key]` on a jsonvalue value: the member of
// an object, nil (null) for a missing key or any other value.
func jsonIndex(value any, key string) any {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return nil
	}
	member, _ := object.Get(key)
	return member
}

// jsonHas is serde_json's `value.get(key).is_some()`: true for a member
// holding null too.
func jsonHas(value any, key string) bool {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return false
	}
	_, present := object.Get(key)
	return present
}

// jsonStr is serde_json's Value::as_str.
func jsonStr(value any) (string, bool) {
	text, ok := value.(string)
	return text, ok
}

// jsonI64 is serde_json's Value::as_i64: an integer that fits i64.
func jsonI64(value any) (int64, bool) {
	switch number := value.(type) {
	case int64:
		return number, true
	case uint64:
		if number <= 1<<63-1 {
			return int64(number), true
		}
	}
	return 0, false
}

// jsonArray is serde_json's Value::as_array.
func jsonArray(value any) ([]any, bool) {
	values, ok := value.([]any)
	return values, ok
}

// asciiLowercase is Rust's str::to_ascii_lowercase.
func asciiLowercase(text string) string {
	lowered := []byte(text)
	for i, b := range lowered {
		if 'A' <= b && b <= 'Z' {
			lowered[i] = b + ('a' - 'A')
		}
	}
	return string(lowered)
}

// asciiEqualFold is Rust's str::eq_ignore_ascii_case.
func asciiEqualFold(left, right string) bool {
	return len(left) == len(right) && asciiLowercase(left) == asciiLowercase(right)
}
