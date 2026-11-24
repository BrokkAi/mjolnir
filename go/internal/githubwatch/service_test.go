// Port of mj-github-watch/src/service.rs's tests and test-only items.

package githubwatch

import (
	"cmp"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"slices"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// alwaysEnabledPollControl is Rust's test-only PollControl::always_enabled.
func alwaysEnabledPollControl() pollControl {
	return newPollControl(func() bool { return true })
}

// newGithubAPI is Rust's test-only GithubApi::new.
func newGithubAPI(base string, token *string) (*githubAPI, error) {
	return newGithubAPIWithControl(base, token, alwaysEnabledPollControl())
}

// sessionDigest is Rust's test-only session_digest.
func sessionDigest(sessionID string) (core.SessionContext, error) {
	return spawnBlocking("load session turns for GitHub classification", func() (core.SessionContext, error) {
		materialized, err := store.LoadMaterializedRecentTurns(sessionID, core.SessionTurns)
		if err != nil || materialized == nil {
			return core.SessionContext{}, err
		}
		return transcript.SessionDigest(materialized), nil
	})
}

const isolatedChildEnv = "MJ_GITHUB_WATCH_INTEGRATION_CHILD"

// forgetCredential drops token's shared state, which lives for the process
// as Rust's static does, so a test repeated in one Go test process (-count)
// starts as it does in a fresh libtest process.
func forgetCredential(token string) {
	state := credentialState(&token)
	githubCredentials.Lock()
	defer githubCredentials.Unlock()
	for key, candidate := range githubCredentials.states {
		if candidate == state {
			delete(githubCredentials.states, key)
		}
	}
}

func ptr[T any](value T) *T { return &value }

// must is a fixture step's value; a failed step fails the test.
func must[T any](value T, err error) T {
	if err != nil {
		panic(err)
	}
	return value
}

func mustNil(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}

func assertEqual[T comparable](t *testing.T, got, want T, message ...any) {
	t.Helper()
	if got != want {
		t.Fatalf("%s: got %v, want %v", fmt.Sprint(message...), got, want)
	}
}

func assertTrue(t *testing.T, condition bool, message ...any) {
	t.Helper()
	if !condition {
		t.Fatal(message...)
	}
}

func repoCursor(t *testing.T) store.GithubRepoCursor {
	t.Helper()
	return must(store.GithubRepoCursorFor("acme", "repo"))
}

func parseTimestamp(t *testing.T, timestamp string) time.Time {
	t.Helper()
	parsed, ok := core.ParseChronoRFC3339(timestamp)
	if !ok {
		t.Fatalf("invalid timestamp %q", timestamp)
	}
	return parsed
}

// secondsTimestamp is chrono's `to_rfc3339_opts(SecondsFormat::Secs, true)`
// of a UTC time.
func secondsTimestamp(at time.Time) string {
	return at.UTC().Format("2006-01-02T15:04:05Z07:00")
}

// secondTicks is Rust's `start + chrono::Duration::seconds(seconds)` at second
// resolution, written as the tests' `at` closures write it.
func secondTicks(start time.Time) func(seconds int64) string {
	return func(seconds int64) string {
		return secondsTimestamp(start.Add(time.Duration(seconds) * time.Second))
	}
}

// nextWholeSecond is `DateTime::from_timestamp(parsed.timestamp() + 1, 0)`.
func nextWholeSecond(t *testing.T, timestamp string) time.Time {
	t.Helper()
	return time.Unix(parseTimestamp(t, timestamp).Unix()+1, 0).UTC()
}

// runIsolatedChild runs the test alone in a child of this test binary with
// its own store; it is true in that child.
func runIsolatedChild(t *testing.T) bool {
	t.Helper()
	if _, ok := os.LookupEnv(isolatedChildEnv); ok {
		return true
	}
	root := t.TempDir()
	targets.NewIsolatedTest(targets.TestName("mj_github_watch::service::tests", t.Name())).
		Env(isolatedChildEnv, "1").
		IsolatedStore(root).
		Run()
	return false
}

func installIsolatedTestWriter(t *testing.T) {
	writer := store.InstallIsolatedTestWriter()
	t.Cleanup(func() {
		if err := writer.Shutdown(); err != nil {
			t.Error(err)
		}
	})
}

// jsonValueOf is the value serde_json holds for a Go test fixture.
func jsonValueOf(t *testing.T, value any) any {
	t.Helper()
	encoded := must(jsonvalue.Marshal(value))
	return must(jsonvalue.Parse(encoded))
}

func writeJSON(w http.ResponseWriter, value any) {
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		w.WriteHeader(http.StatusInternalServerError)
		return
	}
	w.Header().Set("Content-Type", "application/json")
	w.Write(encoded)
}

func githubItem(id, number uint64, title, createdAt string, pullRequest bool) map[string]any {
	item := map[string]any{
		"id":         id,
		"number":     number,
		"title":      title,
		"repository": map[string]any{"full_name": "acme/repo"},
		"body":       "item body",
		"user":       map[string]any{"login": "author"},
		"html_url":   fmt.Sprintf("https://github.com/acme/repo/issues/%d", number),
		"created_at": createdAt,
		"updated_at": createdAt,
	}
	if pullRequest {
		item["pull_request"] = map[string]any{"url": "https://api.github.com/repos/acme/repo/pulls/3"}
		item["html_url"] = fmt.Sprintf("https://github.com/acme/repo/pull/%d", number)
	}
	return item
}

func parsedGithubItem(t *testing.T, item map[string]any) store.GithubSeenItem {
	t.Helper()
	parsed, ok := parseGithubItem(jsonValueOf(t, item))
	if !ok {
		t.Fatal("fixture is not a GitHub item")
	}
	return parsed
}

type fakeClassifier struct {
	calls *atomic.Int64
}

func (c fakeClassifier) Classify(_ context.Context, evidence *core.GithubItemEvidence) (core.GithubItemVerdict, error) {
	c.calls.Add(1)
	if evidence.Session.History == nil {
		panic("watcher must prepare shared history evidence")
	}
	title := asciiLowercase(evidence.Item.Title)
	return core.GithubItemVerdict{Interested: strings.Contains(title, "interesting")}, nil
}

func newFakeClassifier() githubClassifier {
	return fakeClassifier{calls: new(atomic.Int64)}
}

type failingSessionClassifier struct {
	failing *atomic.Bool
}

func (c failingSessionClassifier) Classify(_ context.Context, evidence *core.GithubItemEvidence) (core.GithubItemVerdict, error) {
	if strings.Contains(evidence.Session.Conversation, "FAIL_PAIR") && c.failing.Load() {
		return core.GithubItemVerdict{}, errors.New("permanent session classifier failure")
	}
	return core.GithubItemVerdict{Interested: false}, nil
}

type commentQuery struct {
	number uint64
	since  string
}

type feedMove struct {
	request   int
	index     int
	updatedAt string
}

type bulkFeedMove struct {
	after     int
	indices   []int
	updatedAt string
}

type timelineEvent struct {
	target uint64
	event  map[string]any
}

// fakeGithub is the GitHub REST API the watcher polls in these tests.
type fakeGithub struct {
	t  *testing.T
	mu sync.Mutex

	items              []map[string]any
	timelines          map[uint64][]map[string]any
	reviewComments     map[uint64][]map[string]any
	commentQueries     []commentQuery
	fail               map[uint64]bool
	gone               map[uint64]bool
	feedQueries        []map[string]string
	timelineRequests   []uint64
	observedAt         *string
	moveAfterFeed      *feedMove
	timelineAfterRead  *timelineEvent
	bulkMovesAfterFeed []bulkFeedMove
	failFeedRequest    *int
	pulls              map[uint64]any
	pullRequests       []uint64
}

func newFakeGithub(t *testing.T) *fakeGithub {
	return &fakeGithub{
		t:              t,
		timelines:      map[uint64][]map[string]any{},
		reviewComments: map[uint64][]map[string]any{},
		fail:           map[uint64]bool{},
		gone:           map[uint64]bool{},
		pulls:          map[uint64]any{},
	}
}

// flatQuery is axum's Query<HashMap<String, String>>: the last value of
// each name.
func flatQuery(r *http.Request) map[string]string {
	query := map[string]string{}
	for name, values := range r.URL.Query() {
		query[name] = values[len(values)-1]
	}
	return query
}

// pageOf is `.skip((page - 1) * ITEMS_PER_PAGE).take(ITEMS_PER_PAGE)`.
func pageOf[T any](values []T, query map[string]string) []T {
	page, err := strconv.Atoi(query["page"])
	if err != nil {
		panic(err)
	}
	start := min((page-1)*itemsPerPage, len(values))
	// Never nil: an empty page is `[]`, as Rust's empty Vec.
	return append([]T{}, values[start:min(start+itemsPerPage, len(values))]...)
}

func pathNumber(r *http.Request) uint64 {
	number, err := strconv.ParseUint(r.PathValue("number"), 10, 64)
	if err != nil {
		panic(err)
	}
	return number
}

func (f *fakeGithub) issues(w http.ResponseWriter, r *http.Request) {
	f.mu.Lock()
	defer f.mu.Unlock()
	query := flatQuery(r)
	f.feedQueries = append(f.feedQueries, query)
	if f.failFeedRequest != nil && *f.failFeedRequest == len(f.feedQueries) {
		w.WriteHeader(http.StatusServiceUnavailable)
		return
	}
	for name, want := range map[string]string{"state": "all", "sort": "updated", "direction": "asc"} {
		if query[name] != want {
			f.t.Errorf("issues query %s: got %q, want %q", name, query[name], want)
		}
	}
	var items []map[string]any
	for _, item := range f.items {
		// GitHub's `since` includes items updated at exactly that time.
		if !timestampPrecedes(item["updated_at"].(string), query["since"]) {
			items = append(items, item)
		}
	}
	slices.SortStableFunc(items, func(a, b map[string]any) int {
		return strings.Compare(a["updated_at"].(string), b["updated_at"].(string))
	})
	// Encode before the moves below change the items.
	encoded, err := jsonvalue.Marshal(pageOf(items, query))
	if err != nil {
		panic(err)
	}
	if f.moveAfterFeed != nil && f.moveAfterFeed.request == len(f.feedQueries) {
		f.items[f.moveAfterFeed.index]["updated_at"] = f.moveAfterFeed.updatedAt
		f.moveAfterFeed = nil
	}
	request := len(f.feedQueries)
	moves := f.bulkMovesAfterFeed
	f.bulkMovesAfterFeed = nil
	for _, move := range moves {
		if move.after == request {
			for _, index := range move.indices {
				f.items[index]["updated_at"] = move.updatedAt
			}
		} else {
			f.bulkMovesAfterFeed = append(f.bulkMovesAfterFeed, move)
		}
	}
	w.Header().Set("Content-Type", "application/json")
	w.Write(encoded)
}

func (f *fakeGithub) timeline(w http.ResponseWriter, r *http.Request) {
	f.mu.Lock()
	defer f.mu.Unlock()
	number := pathNumber(r)
	f.timelineRequests = append(f.timelineRequests, number)
	if f.fail[number] {
		w.WriteHeader(http.StatusServiceUnavailable)
		return
	}
	if f.gone[number] {
		w.WriteHeader(http.StatusGone)
		return
	}
	values := pageOf(f.timelines[number], flatQuery(r))
	if f.observedAt != nil {
		at, ok := core.ParseChronoRFC3339(*f.observedAt)
		if !ok {
			panic("invalid observed time")
		}
		w.Header().Set("Date", at.UTC().Format(http.TimeFormat))
	}
	writeJSON(w, values)
	// Make a new event visible only after taking this response snapshot.
	if f.timelineAfterRead != nil {
		f.timelines[f.timelineAfterRead.target] = append(f.timelines[f.timelineAfterRead.target], f.timelineAfterRead.event)
		f.timelineAfterRead = nil
	}
}

func (f *fakeGithub) pull(w http.ResponseWriter, r *http.Request) {
	f.mu.Lock()
	defer f.mu.Unlock()
	number := pathNumber(r)
	f.pullRequests = append(f.pullRequests, number)
	pull, ok := f.pulls[number]
	if !ok {
		w.WriteHeader(http.StatusNotFound)
		return
	}
	writeJSON(w, pull)
}

func (f *fakeGithub) reviewCommentsHandler(w http.ResponseWriter, r *http.Request) {
	f.mu.Lock()
	defer f.mu.Unlock()
	number := pathNumber(r)
	query := flatQuery(r)
	f.commentQueries = append(f.commentQueries, commentQuery{number: number, since: query["since"]})
	var comments []map[string]any
	for _, comment := range f.reviewComments[number] {
		if timestampPrecedes(query["since"], comment["updated_at"].(string)) {
			comments = append(comments, comment)
		}
	}
	writeJSON(w, pageOf(comments, query))
}

// newFakeGithubServer serves fake; withPulls adds the pull request route.
func newFakeGithubServer(fake *fakeGithub, withPulls bool) *httptest.Server {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /repos/{owner}/{repo}/issues", fake.issues)
	mux.HandleFunc("GET /repos/{owner}/{repo}/issues/{number}/timeline", fake.timeline)
	mux.HandleFunc("GET /repos/{owner}/{repo}/pulls/{number}/comments", fake.reviewCommentsHandler)
	if withPulls {
		mux.HandleFunc("GET /repos/{owner}/{repo}/pulls/{number}", fake.pull)
	}
	mux.HandleFunc("GET /user", func(w http.ResponseWriter, _ *http.Request) {
		writeJSON(w, map[string]any{"login": "watcher-bot"})
	})
	return httptest.NewServer(mux)
}

func startFakeGithub(t *testing.T, fake *fakeGithub) (*githubAPI, *httptest.Server) {
	t.Helper()
	server := newFakeGithubServer(fake, true)
	base := server.URL
	return must(newGithubAPI(base, &base)), server
}

// saveCreatorSession: the store keeps GitHub rows only for a session it
// holds.
func saveCreatorSession(t *testing.T) {
	t.Helper()
	session := store.TestSession("creator", "project")
	mustNil(t, store.SaveSession(&session))
}

func watchClassification(number int64, createdAt string) store.GithubItemClassification {
	return store.GithubItemClassification{
		Owner:      "acme",
		Repo:       "repo",
		Number:     number,
		SessionID:  "creator",
		Kind:       "pull_request",
		Title:      "synthetic PR",
		URL:        "https://github.com/acme/repo/pull/1",
		CreatedAt:  createdAt,
		Interested: true,
		Created:    true,
	}
}

func commitClassification(t *testing.T, classification store.GithubItemClassification, event *store.GithubClassificationEvent) {
	t.Helper()
	must(store.CommitGithubItemClassification(classification, event))
}

func decodeMailboxEvent(t *testing.T, encoded string) core.MailboxEvent {
	t.Helper()
	var event core.MailboxEvent
	mustNil(t, json.Unmarshal([]byte(encoded), &event))
	return event
}

func githubWatches(t *testing.T) []store.GithubItemWatch {
	t.Helper()
	return must(store.LoadGithubWatches("acme", "repo"))
}

func seenItems(t *testing.T) []store.GithubSeenItem {
	t.Helper()
	return must(store.LoadGithubSeenItems("acme", "repo"))
}

func pendingMailboxEvents(t *testing.T, limit int) []store.MailboxOutboxEntry {
	t.Helper()
	return must(store.PendingMailboxEvents(limit))
}

func TestGithubLoginRetriesAfterATransientFailure(t *testing.T) {
	var requests atomic.Int64
	mux := http.NewServeMux()
	mux.HandleFunc("GET /user", func(w http.ResponseWriter, _ *http.Request) {
		if requests.Add(1) == 1 {
			w.WriteHeader(http.StatusServiceUnavailable)
			return
		}
		writeJSON(w, map[string]any{"login": "watcher-bot"})
	})
	server := httptest.NewServer(mux)
	defer server.Close()
	token := "watch-login-retry-test-token"
	forgetCredential(token)
	api := must(newGithubAPI(server.URL, &token))
	ctx := context.Background()

	assertTrue(t, must(api.credentialLogin(ctx)) == nil)
	assertTrue(t, must(api.credentialLogin(ctx)) == nil)
	assertEqual(t, requests.Load(), 1)
	time.Sleep(1_100 * time.Millisecond)
	for range 2 {
		login := must(api.credentialLogin(ctx))
		assertTrue(t, login != nil && *login == "watcher-bot", "login", login)
	}
	assertEqual(t, requests.Load(), 2)
}

func TestWatcherStopsClassificationAndRequestsWhenMailboxesAreDisabled(t *testing.T) {
	if !runIsolatedChild(t) {
		return
	}
	installIsolatedTestWriter(t)
	session := store.TestSession("disable-poll-session", "project")
	session.State = core.SessionStateRunning
	mustNil(t, store.SaveSession(&session))
	// The feed returns an item newer than this cursor, so recording it
	// after the opt-out would move the cursor.
	before := repoCursor(t).UpdatedAt

	var enabled atomic.Bool
	enabled.Store(true)
	var requests atomic.Int64
	mux := http.NewServeMux()
	mux.HandleFunc("GET /repos/{owner}/{repo}/issues", func(w http.ResponseWriter, _ *http.Request) {
		requests.Add(1)
		item := githubItem(2, 2, "interesting issue after opt-out",
			core.FormatChronoRFC3339(time.Now().Add(30*time.Second)), false)
		enabled.Store(false)
		writeJSON(w, []any{item})
	})
	mux.HandleFunc("GET /ping", func(w http.ResponseWriter, _ *http.Request) {
		requests.Add(1)
		writeJSON(w, map[string]any{"ok": true})
	})
	server := httptest.NewServer(mux)
	defer server.Close()
	var calls atomic.Int64
	classifier := fakeClassifier{calls: &calls}
	api := must(newGithubAPIWithControl(server.URL, ptr("watch-disable-poll-test-token"),
		newPollControl(enabled.Load)))
	ctx := context.Background()
	err := pollRepositoryWithContext(ctx, "acme", "repo", []core.SessionRecord{session}, classifier, api)
	assertTrue(t, err != nil && isGithubEventsDisabled(err), "error", err)
	assertEqual(t, calls.Load(), 0, "The classifier is not called after opt-out")
	assertEqual(t, requests.Load(), 1)
	_, err = api.getJSON(ctx, "ping", nil)
	assertTrue(t, err != nil && isGithubEventsDisabled(err), "error", err)
	assertEqual(t, requests.Load(), 1, "no request follows opt-out")
	assertEqual(t, repoCursor(t).UpdatedAt, before)
}

func TestGithubRateLimitDeadlineIsSharedAfterFailedRequests(t *testing.T) {
	var accepted atomic.Int64
	mux := http.NewServeMux()
	mux.HandleFunc("GET /retry-after", func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Retry-After", "1")
		w.Header().Set("x-ratelimit-remaining", "0")
		w.WriteHeader(http.StatusTooManyRequests)
	})
	mux.HandleFunc("GET /reset", func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("x-ratelimit-reset", strconv.FormatInt(time.Now().Unix()+2, 10))
		w.Header().Set("x-ratelimit-remaining", "0")
		w.WriteHeader(http.StatusForbidden)
	})
	mux.HandleFunc("GET /ok", func(w http.ResponseWriter, _ *http.Request) {
		accepted.Add(1)
		writeJSON(w, map[string]any{"ok": true})
	})
	server := httptest.NewServer(mux)
	defer server.Close()

	for _, limit := range []struct{ path, token string }{
		{"retry-after", "watch-rate-retry-after-test-token"},
		{"reset", "watch-rate-reset-test-token"},
	} {
		limitedAPI := must(newGithubAPI(server.URL, ptr(limit.token)))
		_, err := limitedAPI.getJSON(context.Background(), limit.path, nil)
		assertTrue(t, err != nil, limit.path, " must fail")
		secondRepositoryAPI := must(newGithubAPI(server.URL, ptr(limit.token)))
		blocked, cancel := context.WithTimeout(context.Background(), 150*time.Millisecond)
		_, err = secondRepositoryAPI.getJSON(blocked, "ok", nil)
		cancel()
		assertTrue(t, errors.Is(err, context.DeadlineExceeded), "the credential deadline must block requests: ", err)
		assertEqual(t, accepted.Load(), 0)
		expires, cancel := context.WithTimeout(context.Background(), 3*time.Second)
		_, err = secondRepositoryAPI.getJSON(expires, "ok", nil)
		cancel()
		if err != nil {
			t.Fatalf("rate-limit deadline expires: %v", err)
		}
		assertEqual(t, accepted.Load(), 1)
		accepted.Store(0)
	}
}

func TestGithubAPIBaseAllowsPlainHTTPOnlyForLoopbackHosts(t *testing.T) {
	_, err := newGithubAPI("http://example.com", ptr("secret"))
	assertTrue(t, err != nil, "plain HTTP to a remote host must be refused")
	for _, base := range []string{"http://127.0.0.1:1234", "http://localhost:1234", "https://api.github.com"} {
		_, err := newGithubAPI(base, nil)
		assertTrue(t, err == nil, base, ": ", err)
	}
}

// Hard-won: 0477819ab: a regenerated merge body rolled back all repository progress.
func TestWatcherRecordsPagesAndRetriesItemsWithoutBlockingOtherWork(t *testing.T) {
	if !runIsolatedChild(t) {
		return
	}
	installIsolatedTestWriter(t)
	saveCreatorSession(t)
	initial := repoCursor(t)
	start := nextWholeSecond(t, initial.WatchStartedAt)
	at := secondTicks(start)
	created := at(2)
	updated := at(5)
	later := at(10)
	commentUpdated := at(12)
	fake := newFakeGithub(t)
	fake.mu.Lock()
	// A full page and a partial page share the same update timestamp.
	for n := uint64(1); n <= 101; n++ {
		item := githubItem(n, n, "interesting synthetic item", created, true)
		item["updated_at"] = updated
		fake.items = append(fake.items, item)
	}
	fake.observedAt = ptr(core.FormatChronoRFC3339(parseTimestamp(t, updated).Add(30 * time.Second)))
	fake.fail[1] = true
	fake.gone[3] = true
	// GitHub exposes the review but omits its line comments from the timeline.
	fake.reviewComments[2] = []map[string]any{{"id": 8, "created_at": updated, "updated_at": updated,
		"user": map[string]any{"login": "reviewer"}, "body": "line comment"}}
	var timeline []map[string]any
	for id := 1000; id < 1100; id++ {
		timeline = append(timeline, map[string]any{"event": "labeled", "id": id, "created_at": created})
	}
	fake.timelines[2] = append(timeline,
		map[string]any{"event": "merged", "id": 200, "created_at": updated, "actor": map[string]any{"login": "merge-bot"}},
		map[string]any{"event": "closed", "id": 201, "created_at": later, "actor": map[string]any{"login": "merge-bot"}},
		map[string]any{"event": "commented", "id": 10, "created_at": updated, "user": map[string]any{"login": "watcher-bot"}, "body": "original"},
		map[string]any{"event": "reviewed", "id": 9, "submitted_at": updated, "user": map[string]any{"login": "reviewer"}, "state": "approved", "body": "review"},
	)
	fake.mu.Unlock()
	for n := int64(1); n <= 3; n++ {
		commitClassification(t, watchClassification(n, created), nil)
	}
	creator := store.TestSession("creator", "project")
	creator.CreatedAt = initial.WatchStartedAt
	mustNil(t, store.SaveSession(&creator))
	mustNil(t, store.SeedGithubTestTurns("creator", start.UnixMilli()-5000, start.UnixMilli()-1000, start.UnixMilli()))
	newSession := store.TestSession("new-session", "project")
	newSession.CreatedAt = later
	mustNil(t, store.SaveSession(&newSession))
	server := newFakeGithubServer(fake, false)
	defer server.Close()
	api := must(newGithubAPI(server.URL, ptr("synthetic-watcher-items")))
	var calls atomic.Int64
	classifier := fakeClassifier{calls: &calls}
	ctx := context.Background()
	mustNil(t, pollRepositoryWithContext(ctx, "acme", "repo", []core.SessionRecord{newSession}, classifier, api))
	assertEqual(t, calls.Load(), 0, "session must not classify items older than itself")
	assertEqual(t, len(seenItems(t)), 101)
	assertEqual(t, repoCursor(t).UpdatedAt, updated)
	watches := githubWatches(t)
	assertEqual(t, watches[0].ProcessedAt, created, "failed item keeps its progress")
	assertEqual(t, watches[1].ProcessedAt, updated)
	assertTrue(t, watches[2].RecordedUpdatedAt != nil && *watches[2].RecordedUpdatedAt == updated,
		"gone item retains its recorded update")
	fake.mu.Lock()
	firstQuery := fake.commentQueries[0]
	fake.mu.Unlock()
	assertEqual(t, firstQuery, commentQuery{number: 2, since: overlapTimestamp(created)},
		"review comments use the stored cursor even when the timeline advances it")
	rows := pendingMailboxEvents(t, 1000)
	assertEqual(t, len(rows), 3)
	var events []core.MailboxEvent
	for _, row := range rows {
		events = append(events, decodeMailboxEvent(t, row.EventJSON))
	}
	lifecycles := 0
	for _, event := range events {
		if _, ok := event.Body.(core.MailboxEventBodyGithubPullRequestLifecycle); ok {
			lifecycles++
		}
	}
	assertEqual(t, lifecycles, 1)
	find := func(suffix string) core.MailboxEvent {
		t.Helper()
		index := slices.IndexFunc(events, func(event core.MailboxEvent) bool { return strings.HasSuffix(event.Key, suffix) })
		if index < 0 {
			t.Fatalf("no event ends with %s", suffix)
		}
		return events[index]
	}
	merge, ok := find(":merged:200").Body.(core.MailboxEventBodyGithubPullRequestLifecycle)
	assertTrue(t, ok && merge.Actor == "merge-bot" && merge.Repo != nil && *merge.Repo == "acme/repo", "merge ", merge)
	assertTrue(t, !slices.ContainsFunc(events, func(event core.MailboxEvent) bool { return strings.HasSuffix(event.Key, ":comment:10") }),
		"the creator's own comment during its turn is not delivered")
	assertTrue(t, find(":review-comment:8").Wake)
	assertTrue(t, find(":review:9").Wake)
	fake.mu.Lock()
	fake.fail = map[uint64]bool{}
	fake.observedAt = ptr(core.FormatChronoRFC3339(parseTimestamp(t, later).Add(30 * time.Second)))
	// The cursor boundary can acquire another item with a lower ID.
	item := githubItem(500, 102, "interesting boundary item", created, false)
	item["updated_at"] = updated
	fake.items = append(fake.items, item)
	fake.items[1]["updated_at"] = later
	fake.timelines[2][102]["body"] = "edited body"
	fake.reviewComments[2][0]["body"] = "edited line comment"
	fake.reviewComments[2][0]["updated_at"] = commentUpdated
	fake.timelines[2][100]["actor"] = map[string]any{"login": "changed-bot"}
	fake.mu.Unlock()
	// Explicitly replay the same IDs with changed bodies. The first values win.
	replay := watches[1]
	replay.TimelineAt = created
	replay.TimelineID = 0
	replay.ReviewCommentsAt = created
	replay.ReviewCommentsID = 0
	replay.RecordedUpdatedAt = ptr(updated)
	mustNil(t, pollItemTimeline(ctx, replay, api))
	mustNil(t, pollRepositoryWithContext(ctx, "acme", "repo", []core.SessionRecord{newSession}, classifier, api))
	assertEqual(t, len(seenItems(t)), 102)
	assertTrue(t, slices.Equal(pendingMailboxEvents(t, 1000), rows), "repeated keys preserve their original bodies")
	assertEqual(t, githubWatches(t)[0].ProcessedAt, later)
	assertEqual(t, githubWatches(t)[1].ReviewCommentsAt, commentUpdated, "review comments advance only their own cursor")
	fake.mu.Lock()
	itemTwoQueries := 0
	for _, query := range fake.commentQueries {
		if query.number == 2 {
			itemTwoQueries++
		}
	}
	fake.mu.Unlock()
	assertEqual(t, itemTwoQueries, 3, "the second repository poll fetches the edited line comment again")
	// Older sessions process the backlog in bounded batches, independent of feed cursor.
	mustNil(t, pollRepositoryWithContext(ctx, "acme", "repo", []core.SessionRecord{creator}, classifier, api))
	assertEqual(t, calls.Load(), int64(maxClassificationsPerPoll))
	mustNil(t, pollRepositoryWithContext(ctx, "acme", "repo", []core.SessionRecord{creator}, classifier, api))
	assertEqual(t, calls.Load(), 99)
}

// Hard-won: review f2c1683: an item moving between offset pages hid its neighbor.
func TestWatcherKeysetRecordingKeepsItemsMovingBetweenPages(t *testing.T) {
	if !runIsolatedChild(t) {
		return
	}
	installIsolatedTestWriter(t)
	cursor := repoCursor(t)
	start := parseTimestamp(t, cursor.UpdatedAt)
	fake := newFakeGithub(t)
	fake.mu.Lock()
	for n := uint64(1); n <= 201; n++ {
		fake.items = append(fake.items, githubItem(n, n, "synthetic",
			core.FormatChronoRFC3339(start.Add(time.Duration(n)*time.Second)), false))
	}
	fake.moveAfterFeed = &feedMove{request: 1, index: 0, updatedAt: core.FormatChronoRFC3339(start.Add(300 * time.Second))}
	fake.mu.Unlock()
	api, server := startFakeGithub(t, fake)
	defer server.Close()
	mustNil(t, recordRepository(context.Background(), "acme", "repo", &cursor, api))
	seen := seenItems(t)
	assertEqual(t, len(seen), 201)
	assertTrue(t, slices.ContainsFunc(seen, func(item store.GithubSeenItem) bool { return item.Number == 101 }),
		"the moving item's neighbor must not be skipped")
	fake.mu.Lock()
	defer fake.mu.Unlock()
	for _, query := range fake.feedQueries {
		assertEqual(t, query["page"], "1")
	}
}

// Hard-won: 5f5e74082: an inclusive `since` refetched a full page straddling two seconds and stopped.
func TestWatcherPagesPastAFullPageThatStraddlesTwoSeconds(t *testing.T) {
	if !runIsolatedChild(t) {
		return
	}
	installIsolatedTestWriter(t)
	cursor := repoCursor(t)
	at := secondTicks(nextWholeSecond(t, cursor.UpdatedAt))
	fake := newFakeGithub(t)
	fake.mu.Lock()
	for n := uint64(1); n <= 200; n++ {
		item := githubItem(n, n, "synthetic", at(1), false)
		if n <= 50 {
			item["updated_at"] = at(1)
		} else {
			item["updated_at"] = at(2)
		}
		fake.items = append(fake.items, item)
	}
	fake.mu.Unlock()
	api, server := startFakeGithub(t, fake)
	defer server.Close()
	mustNil(t, recordRepository(context.Background(), "acme", "repo", &cursor, api))
	assertEqual(t, len(seenItems(t)), 200)
	fake.mu.Lock()
	defer fake.mu.Unlock()
	assertEqual(t, len(fake.feedQueries), 3)
}

type reconciliationRecovery int

const (
	reconciliationRecoveryOldBacklog reconciliationRecovery = iota
	reconciliationRecoveryLongPause
	reconciliationRecoveryFailedPass
)

func checkReconciliationCheckpointRecovery(t *testing.T, scenario reconciliationRecovery) {
	installIsolatedTestWriter(t)
	saveCreatorSession(t)
	now := time.Unix(time.Now().Unix(), 0).UTC()
	origin := now.Add(-4 * time.Hour)
	at := secondTicks(origin)
	var tiedAt, movedAt string
	var readAt time.Time
	var prefixCount int
	switch scenario {
	case reconciliationRecoveryOldBacklog:
		tiedAt, movedAt, readAt, prefixCount = at(3600), at(14400), now, 0
	case reconciliationRecoveryLongPause:
		tiedAt, movedAt, readAt, prefixCount = at(1), at(2), origin.Add(10802*time.Second), 0
	case reconciliationRecoveryFailedPass:
		tiedAt, movedAt, readAt, prefixCount = at(5400), at(5401), origin.Add(3*time.Hour), 100
	}
	repoCursor(t)
	mustNil(t, store.SeedGithubRepoStart("acme", "repo", at(0)))
	cursor := repoCursor(t)
	watch := watchClassification(201, tiedAt)
	watch.Kind = "issue"
	commitClassification(t, watch, nil)
	fake := newFakeGithub(t)
	fake.mu.Lock()
	// Distinct prefix times let the failing pass commit a useful checkpoint
	// before the timestamp tie containing the skipped item.
	for n := 0; n < prefixCount; n++ {
		fake.items = append(fake.items, githubItem(uint64(1000+n), uint64(1000+n), "prefix", at(int64(3600+n)), false))
	}
	for n := uint64(1); n <= 401; n++ {
		fake.items = append(fake.items, githubItem(n, n, "synthetic", tiedAt, false))
	}
	// Moves land after the tie's second and fourth offset pages. Without a
	// prefix, the first request jumps to the tie and the second rereads it.
	requestOffset := 2
	if prefixCount == 0 {
		requestOffset = 1
	}
	var moved []int
	for index := prefixCount + 1; index < prefixCount+101; index++ {
		moved = append(moved, index)
	}
	fake.bulkMovesAfterFeed = []bulkFeedMove{
		{after: 2 + requestOffset, indices: []int{prefixCount}, updatedAt: movedAt},
		{after: 4 + requestOffset, indices: moved, updatedAt: movedAt},
	}
	fake.observedAt = ptr(core.FormatChronoRFC3339(readAt.Add(30 * time.Second)))
	fake.timelines[201] = []map[string]any{{"event": "commented", "id": 201, "created_at": tiedAt,
		"user": map[string]any{"login": "other"}, "body": "recovered activity"}}
	fake.mu.Unlock()
	feedQueries := func() []map[string]string {
		fake.mu.Lock()
		defer fake.mu.Unlock()
		return slices.Clone(fake.feedQueries)
	}
	failFeedRequest := func(request *int) {
		fake.mu.Lock()
		defer fake.mu.Unlock()
		fake.failFeedRequest = request
	}
	hasItem201 := func(items []store.GithubSeenItem) bool {
		return slices.ContainsFunc(items, func(item store.GithubSeenItem) bool { return item.Number == 201 })
	}
	api, server := startFakeGithub(t, fake)
	defer server.Close()
	ctx := context.Background()
	mustNil(t, recordRepository(ctx, "acme", "repo", &cursor, api))
	seen := seenItems(t)
	assertEqual(t, len(seen), prefixCount+400)
	assertTrue(t, !hasItem201(seen))
	mustNil(t, pollRepositoryTimelines(ctx, "acme", "repo", api))
	fake.mu.Lock()
	timelineRequests := len(fake.timelineRequests)
	fake.mu.Unlock()
	assertEqual(t, timelineRequests, 0)
	mainCursor := repoCursor(t)
	assertEqual(t, mainCursor.UpdatedAt, movedAt)
	assertEqual(t, mainCursor.ReconciledThrough, at(0))
	if scenario == reconciliationRecoveryFailedPass {
		failFeedRequest(ptr(len(feedQueries()) + 2))
		err := reconcileRepository(ctx, "acme", "repo", api, readAt)
		assertTrue(t, err != nil && strings.Contains(err.Error(), "503"), "error ", err)
		interrupted := repoCursor(t)
		assertEqual(t, interrupted.UpdatedAt, movedAt)
		assertEqual(t, interrupted.ReconciledThrough, at(3699))
		assertTrue(t, !hasItem201(seenItems(t)))
		requests := len(feedQueries())
		failFeedRequest(nil)
		mustNil(t, reconcileRepository(ctx, "acme", "repo", api, readAt.Add(3599*time.Second)))
		assertEqual(t, len(feedQueries()), requests)
		assertEqual(t, repoCursor(t).ReconciledThrough, interrupted.ReconciledThrough)
		readAt = readAt.Add(time.Hour)
		mustNil(t, reconcileRepository(ctx, "acme", "repo", api, readAt))
		assertEqual(t, feedQueries()[requests]["since"], overlapTimestamp(interrupted.ReconciledThrough))
	} else {
		// A new reader and a much later clock model catch-up or process restart.
		mustNil(t, reconcileRepository(ctx, "acme", "repo", api, readAt))
	}
	seen = seenItems(t)
	assertEqual(t, len(seen), prefixCount+401)
	assertTrue(t, hasItem201(seen))
	reconciled := repoCursor(t)
	assertEqual(t, reconciled.UpdatedAt, movedAt)
	expectedThrough := movedAt
	if scenario == reconciliationRecoveryOldBacklog {
		expectedThrough = at(10800)
	}
	reconciledMs, reconciledOK := timestampMillis(reconciled.ReconciledThrough)
	expectedMs, expectedOK := timestampMillis(expectedThrough)
	assertTrue(t, reconciledOK == expectedOK && reconciledMs == expectedMs,
		"reconciled through ", reconciled.ReconciledThrough, ", want ", expectedThrough)
	assertTrue(t, !timestampPrecedes(core.FormatChronoRFC3339(readAt.Add(-time.Hour)), reconciled.ReconciledThrough))
	mustNil(t, pollRepositoryTimelines(ctx, "acme", "repo", api))
	events := pendingMailboxEvents(t, 100)
	assertEqual(t, len(events), 1)
	assertTrue(t, strings.HasSuffix(events[0].EventKey, "#201:comment:201"), events[0].EventKey)
	if scenario == reconciliationRecoveryFailedPass {
		// A full tied page can commit exactly the upper bound before its
		// remaining offset pages fail. Equality must not abandon that tie.
		failFeedRequest(ptr(len(feedQueries()) + 2))
		readAt = readAt.Add(time.Hour)
		assertTrue(t, reconcileRepository(ctx, "acme", "repo", api, readAt) != nil, "the failing pass must fail")
		partialTie := repoCursor(t)
		assertEqual(t, partialTie.ReconciledThrough, partialTie.UpdatedAt)
		requests := len(feedQueries())
		failFeedRequest(nil)
		readAt = readAt.Add(time.Hour)
		mustNil(t, reconcileRepository(ctx, "acme", "repo", api, readAt))
		queries := feedQueries()
		assertTrue(t, len(queries) >= requests+2, "requests ", len(queries))
		assertEqual(t, queries[requests]["since"], overlapTimestamp(partialTie.ReconciledThrough))
	}
}

// Hard-won: review f2c1683: a catch-up tie was already older than the trailing window.
func TestWatcherReconciliationRecoversSkipsInThreeHourOldBacklog(t *testing.T) {
	if runIsolatedChild(t) {
		checkReconciliationCheckpointRecovery(t, reconciliationRecoveryOldBacklog)
	}
}

// Hard-won: review f2c1683: a skipped item aged out while the repository was paused.
func TestWatcherReconciliationRecoversSkipsAfterAThreeHourPause(t *testing.T) {
	if runIsolatedChild(t) {
		checkReconciliationCheckpointRecovery(t, reconciliationRecoveryLongPause)
	}
}

// Hard-won: review f2c1683: a failed pass lost the next hour's recovery opportunity.
func TestWatcherReconciliationResumesFailedPassFromDurableCheckpoint(t *testing.T) {
	if runIsolatedChild(t) {
		checkReconciliationCheckpointRecovery(t, reconciliationRecoveryFailedPass)
	}
}

// Hard-won: review f2c1683: a later line-comment read skipped unseen timeline events.
func TestWatcherSourceCursorsDoNotSkipInterleavedTimelineEntries(t *testing.T) {
	if !runIsolatedChild(t) {
		return
	}
	installIsolatedTestWriter(t)
	saveCreatorSession(t)
	cursor := repoCursor(t)
	at := secondTicks(nextWholeSecond(t, cursor.UpdatedAt))
	commitClassification(t, watchClassification(1, at(1)), nil)
	fake := newFakeGithub(t)
	fake.mu.Lock()
	item := githubItem(1, 1, "synthetic", at(1), true)
	item["updated_at"] = at(10)
	fake.items = append(fake.items, item)
	fake.observedAt = ptr(at(10))
	fake.timelineAfterRead = &timelineEvent{target: 1, event: map[string]any{"event": "commented", "id": 800,
		"created_at": at(20), "user": map[string]any{"login": "other"}, "body": "arrived after timeline read"}}
	fake.reviewComments[1] = []map[string]any{{"id": 900, "created_at": at(30), "updated_at": at(30),
		"user": map[string]any{"login": "other"}, "body": "later line comment"}}
	fake.mu.Unlock()
	api, server := startFakeGithub(t, fake)
	defer server.Close()
	ctx := context.Background()
	mustNil(t, pollRepositoryWithContext(ctx, "acme", "repo", nil, newFakeClassifier(), api))
	watch := githubWatches(t)[0]
	assertEqual(t, watch.TimelineAt, at(1))
	assertEqual(t, watch.ReviewCommentsAt, at(30))
	fake.mu.Lock()
	fake.items[0]["updated_at"] = at(30)
	fake.observedAt = ptr(at(70))
	fake.mu.Unlock()
	mustNil(t, pollRepositoryWithContext(ctx, "acme", "repo", nil, newFakeClassifier(), api))
	rows := pendingMailboxEvents(t, 100)
	assertEqual(t, len(rows), 2)
	assertTrue(t, slices.ContainsFunc(rows, func(row store.MailboxOutboxEntry) bool {
		return strings.HasSuffix(row.EventKey, ":comment:800")
	}))
	watch = githubWatches(t)[0]
	assertEqual(t, watch.TimelineAt, at(20))
	assertEqual(t, watch.ReviewCommentsAt, at(30))
	assertEqual(t, watch.ProcessedAt, at(40))
}

type keyWake struct {
	key  string
	wake bool
}

// Hard-won: b4476dc76: lifecycle actors during the creator's turn bypassed the own-activity filter.
func TestWatcherFiltersLifecycleActivityByActorAndCreatorTurn(t *testing.T) {
	if !runIsolatedChild(t) {
		return
	}
	installIsolatedTestWriter(t)
	cursor := repoCursor(t)
	start := nextWholeSecond(t, cursor.UpdatedAt)
	at := secondTicks(start)
	seconds := func(count int64) int64 { return start.Add(time.Duration(count) * time.Second).UnixMilli() }
	creator := store.TestSession("creator", "project")
	creator.CreatedAt = cursor.WatchStartedAt
	mustNil(t, store.SaveSession(&creator))
	mustNil(t, store.SeedGithubTestTurns("creator", seconds(2), seconds(3), seconds(20)))
	// A second session was told about every PR. It was working at 8 to
	// 12 s, and its own turn ends long before its next one starts.
	follower := store.TestSession("follower", "project")
	follower.CreatedAt = cursor.WatchStartedAt
	mustNil(t, store.SaveSession(&follower))
	mustNil(t, store.SeedGithubTestTurns("follower", seconds(8), seconds(12), seconds(1000)))

	fake := newFakeGithub(t)
	fake.mu.Lock()
	fake.observedAt = ptr(at(50))
	for number := int64(21); number <= 26; number++ {
		commitClassification(t, watchClassification(number, at(1)), nil)
		interested := watchClassification(number, at(1))
		interested.SessionID = "follower"
		interested.Created = false
		commitClassification(t, interested, nil)
		item := githubItem(uint64(number), uint64(number), "synthetic PR", at(1), true)
		item["updated_at"] = at(40)
		fake.items = append(fake.items, item)
	}
	lifecycle := func(event string, id int64, seconds int64, actor string) map[string]any {
		return map[string]any{"event": event, "id": id, "created_at": at(seconds), "actor": map[string]any{"login": actor}}
	}
	fake.timelines[21] = []map[string]any{lifecycle("closed", 2101, 25, "watcher-bot")}
	fake.timelines[22] = []map[string]any{lifecycle("merged", 2201, 25, "watcher-bot")}
	fake.timelines[23] = []map[string]any{lifecycle("reopened", 2301, 25, "watcher-bot")}
	fake.timelines[24] = []map[string]any{lifecycle("closed", 2401, 10, "watcher-bot")}
	// A comment is for the creator only.
	fake.timelines[25] = []map[string]any{
		lifecycle("closed", 2501, 25, "another-user"),
		{"event": "commented", "id": 2502, "created_at": at(26), "user": map[string]any{"login": "another-user"}, "body": "thanks"},
	}
	// Some GitHub installations expose a merge only as a close entry.
	fake.timelines[26] = []map[string]any{lifecycle("closed", 2601, 25, "watcher-bot")}
	fake.pulls[21] = map[string]any{"merged_at": nil}
	fake.pulls[24] = map[string]any{"merged_at": nil}
	fake.pulls[25] = map[string]any{"merged_at": nil}
	fake.pulls[26] = map[string]any{"merged_at": at(25)}
	fake.mu.Unlock()

	api, server := startFakeGithub(t, fake)
	defer server.Close()
	mustNil(t, pollRepositoryWithContext(context.Background(), "acme", "repo", []core.SessionRecord{creator}, newFakeClassifier(), api))
	rows := pendingMailboxEvents(t, 100)
	rowsFor := func(session string) []keyWake {
		var selected []keyWake
		for _, row := range rows {
			if row.TargetSessionID == session {
				selected = append(selected, keyWake{key: row.EventKey, wake: decodeMailboxEvent(t, row.EventJSON).Wake})
			}
		}
		slices.SortFunc(selected, func(a, b keyWake) int {
			return cmp.Or(strings.Compare(a.key, b.key), compareBool(a.wake, b.wake))
		})
		return selected
	}
	for _, row := range rows {
		assertEqual(t, strings.Contains(row.EventJSON, `"recipient":"interested"`), row.TargetSessionID == "follower",
			"only the interested session's rows carry the recipient")
	}
	assertTrue(t, slices.Equal(rowsFor("creator"), []keyWake{
		{"github:acme/repo#24:closed:2401", true},
		{"github:acme/repo#25:closed:2501", true},
		{"github:acme/repo#25:comment:2502", true},
	}), "same-account actions in the creator turn, including a close mapped as merged, are suppressed; outside-turn and other-account actions are delivered: ", rowsFor("creator"))
	// Hard-won: review ce48727a1: an interested session lost its merge notice when another session sharing the GitHub account merged during its turn.
	assertTrue(t, slices.Equal(rowsFor("follower"), []keyWake{
		{"github:acme/repo#21:closed:2101:follower", false},
		{"github:acme/repo#22:merged:2201:follower", false},
		{"github:acme/repo#23:reopened:2301:follower", false},
		{"github:acme/repo#24:closed:2401:follower", false},
		{"github:acme/repo#25:closed:2501:follower", false},
		{"github:acme/repo#26:merged:2601:follower", false},
	}), "an interested session gets lifecycle events only, queued, including shared-account activity during its own turn: ", rowsFor("follower"))
}

// compareBool orders false before true, as Rust orders bool.
func compareBool(a, b bool) int {
	switch {
	case a == b:
		return 0
	case !a:
		return -1
	}
	return 1
}

type lifecycleChange struct {
	key    string
	change core.MailboxPullRequestChange
	actor  string
}

type batchChange struct {
	session string
	key     string
	change  core.MailboxPullRequestChange
	wake    bool
}

// Hard-won: b4bbe4016: Merge Marshall merges left only closed entries, and
// agents were told their merged pull requests closed without merging.
func TestWatcherReportsACloseAtTheMergeTimeAsAMerge(t *testing.T) {
	if !runIsolatedChild(t) {
		return
	}
	installIsolatedTestWriter(t)
	saveCreatorSession(t)
	cursor := repoCursor(t)
	at := secondTicks(nextWholeSecond(t, cursor.UpdatedAt))
	fake := newFakeGithub(t)
	fake.mu.Lock()
	for _, n := range []int64{11, 12, 13, 14} {
		commitClassification(t, watchClassification(n, at(1)), nil)
		item := githubItem(uint64(n), uint64(n), "synthetic", at(1), true)
		item["updated_at"] = at(30)
		fake.items = append(fake.items, item)
	}
	fake.observedAt = ptr(at(90))
	closed := func(id int64, seconds int64) map[string]any {
		return map[string]any{"event": "closed", "id": id, "created_at": at(seconds),
			"actor": map[string]any{"login": "mergemarshall[bot]"}, "commit_id": "abc"}
	}
	// Merged by landing commits: only a closed entry, one second after the merge time.
	fake.timelines[11] = []map[string]any{closed(300, 6)}
	fake.pulls[11] = map[string]any{"merged_at": at(5)}
	// Closed without merging.
	fake.timelines[12] = []map[string]any{closed(301, 6)}
	fake.pulls[12] = map[string]any{"merged_at": nil}
	// Closed, reopened, then merged later: the first close stays a close.
	fake.timelines[13] = []map[string]any{
		closed(302, 6),
		{"event": "reopened", "id": 303, "created_at": at(10), "actor": map[string]any{"login": "author"}},
		{"event": "merged", "id": 304, "created_at": at(20), "actor": map[string]any{"login": "merger"}},
		closed(305, 20),
	}
	fake.pulls[13] = map[string]any{"merged_at": at(20)}
	// A recorded merge followed by its close needs no pull request read.
	fake.timelines[14] = []map[string]any{
		{"event": "merged", "id": 306, "created_at": at(7), "actor": map[string]any{"login": "merger"}},
		closed(307, 7),
	}
	// A merge batch another account opened: no creator, two interested
	// sessions. It is watched for them, and its review comments, which
	// only a creator is told about, are not read.
	for _, session := range []string{"batch-a", "batch-b"} {
		record := store.TestSession(session, "project")
		mustNil(t, store.SaveSession(&record))
		classification := watchClassification(15, at(1))
		classification.SessionID = session
		classification.Created = false
		commitClassification(t, classification, nil)
	}
	item := githubItem(15, 15, "Merge batch", at(1), true)
	item["updated_at"] = at(30)
	fake.items = append(fake.items, item)
	fake.timelines[15] = []map[string]any{closed(308, 6)}
	fake.pulls[15] = map[string]any{"merged_at": at(5)}
	fake.mu.Unlock()
	api, server := startFakeGithub(t, fake)
	defer server.Close()
	mustNil(t, pollRepositoryWithContext(context.Background(), "acme", "repo", nil, newFakeClassifier(), api))
	rows := pendingMailboxEvents(t, 100)
	var changes []lifecycleChange
	var batch []batchChange
	for _, row := range rows {
		event := decodeMailboxEvent(t, row.EventJSON)
		body, ok := event.Body.(core.MailboxEventBodyGithubPullRequestLifecycle)
		if !ok {
			t.Fatalf("unexpected event %s", row.EventKey)
		}
		if row.TargetSessionID == "creator" {
			changes = append(changes, lifecycleChange{key: row.EventKey, change: body.Change, actor: body.Actor})
		} else {
			batch = append(batch, batchChange{session: row.TargetSessionID, key: row.EventKey, change: body.Change, wake: event.Wake})
		}
	}
	slices.SortFunc(changes, func(a, b lifecycleChange) int { return strings.Compare(a.key, b.key) })
	assertTrue(t, slices.Equal(changes, []lifecycleChange{
		{"github:acme/repo#11:merged:300", core.MailboxPullRequestChangeMerged, "mergemarshall[bot]"},
		{"github:acme/repo#12:closed:301", core.MailboxPullRequestChangeClosedWithoutMerging, "mergemarshall[bot]"},
		{"github:acme/repo#13:closed:302", core.MailboxPullRequestChangeClosedWithoutMerging, "mergemarshall[bot]"},
		{"github:acme/repo#13:merged:304", core.MailboxPullRequestChangeMerged, "merger"},
		{"github:acme/repo#13:reopened:303", core.MailboxPullRequestChangeReopened, "author"},
		{"github:acme/repo#14:merged:306", core.MailboxPullRequestChangeMerged, "merger"},
	}), "creator changes ", changes)
	fake.mu.Lock()
	read := slices.Clone(fake.pullRequests)
	commentQueries := slices.Clone(fake.commentQueries)
	fake.mu.Unlock()
	slices.Sort(read)
	assertTrue(t, slices.Equal(read, []uint64{11, 12, 13, 15}), "pull requests read ", read)
	assertTrue(t, !slices.ContainsFunc(commentQueries, func(query commentQuery) bool { return query.number == 15 }),
		"a watch with no creator reads no review comments")
	slices.SortStableFunc(batch, func(a, b batchChange) int { return strings.Compare(a.session, b.session) })
	assertTrue(t, slices.Equal(batch, []batchChange{
		{"batch-a", "github:acme/repo#15:merged:308:batch-a", core.MailboxPullRequestChangeMerged, false},
		{"batch-b", "github:acme/repo#15:merged:308:batch-b", core.MailboxPullRequestChangeMerged, false},
	}), "batch changes ", batch)
}

// TestAnInterestedSessionIsToldOfAMergeInAnyCommitOrder: the poll's commit
// and a classification commit run in either order, or the classification
// lands between the poll's fetch and its commit. In every order the
// interested session gets exactly one row, and replaying either commit adds
// none.
func TestAnInterestedSessionIsToldOfAMergeInAnyCommitOrder(t *testing.T) {
	if !runIsolatedChild(t) {
		return
	}
	installIsolatedTestWriter(t)
	start := time.Now().UTC()
	at := secondTicks(start)
	for _, id := range []string{"creator", "creator2", "gone", "late", "between", "own"} {
		session := store.TestSession(id, "project")
		session.CreatedAt = at(0)
		mustNil(t, store.SaveSession(&session))
	}
	// The "own" session was working when the merge happened, using the
	// shared account.
	mustNil(t, store.SeedGithubTestTurns("own",
		start.Add(8*time.Second).UnixMilli(),
		start.Add(12*time.Second).UnixMilli(),
		start.Add(1000*time.Second).UnixMilli()))
	interested := func(number int64, session string) store.GithubItemClassification {
		classification := watchClassification(number, at(1))
		classification.SessionID = session
		classification.Created = false
		return classification
	}
	merged := func(number, id int64) store.MappedGithubEvent {
		return store.MappedGithubEvent{
			Key: fmt.Sprintf("github:acme/repo#%d:merged:%d", number, id),
			Body: core.MailboxEventBodyGithubPullRequestLifecycle{
				Change:    core.MailboxPullRequestChangeMerged,
				Number:    uint64(number),
				Title:     "synthetic PR",
				Actor:     "watcher-bot",
				Recipient: core.LifecycleRecipientCreator,
				Repo:      ptr("acme/repo"),
			},
			Author:    ptr("watcher-bot"),
			Timestamp: at(10),
			Lifecycle: true,
		}
	}
	progress := func() store.GithubItemProgress {
		return store.GithubItemProgress{
			TimelineAt:       at(10),
			TimelineID:       1,
			ReviewCommentsAt: at(1),
			ReviewCommentsID: 0,
			ProcessedAt:      at(20),
		}
	}
	for _, number := range []int64{41, 42, 43} {
		commitClassification(t, watchClassification(number, at(1)), nil)
	}
	load := func(number int64) store.GithubItemWatch {
		t.Helper()
		watches := githubWatches(t)
		index := slices.IndexFunc(watches, func(watch store.GithubItemWatch) bool { return watch.Number == number })
		if index < 0 {
			t.Fatalf("no watch for #%d", number)
		}
		return watches[index]
	}
	keysFor := func(session string) []string {
		var keys []string
		for _, row := range pendingMailboxEvents(t, 100) {
			if row.TargetSessionID == session {
				keys = append(keys, row.EventKey)
			}
		}
		slices.Sort(keys)
		return keys
	}
	commitTimeline := func(watch store.GithubItemWatch, events []store.MappedGithubEvent, login *string) {
		t.Helper()
		mustNil(t, store.CommitGithubTimeline(watch, progress(), events, login))
	}

	// The poll commits the merge first; a session is classified afterwards.
	commitTimeline(load(41), []store.MappedGithubEvent{merged(41, 4101)}, ptr("watcher-bot"))
	interest := must(jsonvalue.Marshal(core.MailboxEvent{
		Key:         "github:acme/repo#41:interest:late",
		Source:      "github",
		Wake:        false,
		CreatedAtMs: 1,
		Body:        core.MailboxEventBodyPlainText{Text: "new"},
	}))
	commitClassification(t, interested(41, "late"), &store.GithubClassificationEvent{
		Key: "github:acme/repo#41:interest:late", JSON: string(interest), Wake: false,
	})
	// It hears of the PR before it hears the PR merged.
	var deliveryOrder []string
	for _, row := range pendingMailboxEvents(t, 100) {
		if row.TargetSessionID == "late" {
			deliveryOrder = append(deliveryOrder, row.EventKey)
		}
	}
	assertTrue(t, slices.Equal(deliveryOrder, []string{
		"github:acme/repo#41:interest:late",
		"github:acme/repo#41:merged:4101:late",
	}), "delivery order ", deliveryOrder)
	// The classification lands between the poll's fetch and its commit,
	// which is built from the snapshot taken before it.
	snapshot := load(42)
	commitClassification(t, interested(42, "between"), nil)
	commitTimeline(snapshot, []store.MappedGithubEvent{merged(42, 4201)}, ptr("watcher-bot"))
	// A session working in the shared account when the PR merged is still
	// told when it is classified later: an overlapping turn does not show
	// that it made the merge.
	commitTimeline(load(43), []store.MappedGithubEvent{merged(43, 4301)}, ptr("watcher-bot"))
	// Hard-won: review ce48727a1: a late-classified session mid-turn in the shared account lost the recorded merge.
	commitClassification(t, interested(43, "own"), nil)

	// A watch kept for interested sessions gains its creator while a poll,
	// which did not read review comments for it, is in flight: that poll
	// must not record progress past the comments the creator is owed.
	commitClassification(t, interested(44, "late"), nil)
	snapshot = load(44)
	assertTrue(t, snapshot.CreatorSessionID == nil, "creator ", snapshot.CreatorSessionID)
	commitClassification(t, watchClassification(44, at(1)), nil)
	commitTimeline(snapshot, nil, nil)
	attached := load(44)
	assertTrue(t, attached.CreatorSessionID != nil && *attached.CreatorSessionID == "creator")
	assertEqual(t, attached.ProcessedAt, at(1), "the poll is redone")

	// The creator attaches after a poll already committed progress read
	// without it: the creator is owed everything since the item's creation.
	commitClassification(t, interested(45, "late"), nil)
	commitTimeline(load(45), nil, nil)
	assertEqual(t, load(45).ProcessedAt, at(20))
	commitClassification(t, watchClassification(45, at(1)), nil)
	rewound := load(45)
	assertTrue(t, rewound.CreatorSessionID != nil && *rewound.CreatorSessionID == "creator")
	type cursors struct {
		timelineAt       string
		timelineID       int64
		reviewCommentsAt string
		processedAt      string
	}
	assertEqual(t,
		cursors{rewound.TimelineAt, rewound.TimelineID, rewound.ReviewCommentsAt, rewound.ProcessedAt},
		cursors{at(1), 0, at(1), at(1)})

	// The creator changes while a poll is in flight. One destroyed and
	// replaced by another: the new creator is owed a full read.
	first := watchClassification(46, at(1))
	first.SessionID = "gone"
	commitClassification(t, first, nil)
	commitClassification(t, interested(46, "late"), nil)
	snapshot = load(46)
	mustNil(t, store.DeleteSession("gone"))
	replacement := watchClassification(46, at(1))
	replacement.SessionID = "creator2"
	commitClassification(t, replacement, nil)
	commitTimeline(snapshot, nil, nil)
	assertTrue(t, load(46).CreatorSessionID != nil && *load(46).CreatorSessionID == "creator2")
	assertEqual(t, load(46).ProcessedAt, at(1), "the replacement is owed a full read")
	// A creator attached after a poll's progress was recorded and was then
	// destroyed: the watch has no creator again, but its cursors were
	// rewound, and a poll built before the rewind must not undo that.
	commitClassification(t, interested(47, "late"), nil)
	commitTimeline(load(47), nil, nil)
	snapshot = load(47)
	assertEqual(t, snapshot.ProcessedAt, at(20))
	creatorAttached := watchClassification(47, at(1))
	creatorAttached.SessionID = "creator2"
	commitClassification(t, creatorAttached, nil)
	mustNil(t, store.DeleteSession("creator2"))
	assertTrue(t, load(47).CreatorSessionID == nil)
	commitTimeline(snapshot, nil, nil)
	assertEqual(t, load(47).ProcessedAt, at(1), "the rewind stands")

	// Replays change nothing.
	for _, number := range []int64{41, 42, 43} {
		commitTimeline(load(number), []store.MappedGithubEvent{merged(number, number*100+1)}, ptr("watcher-bot"))
	}
	assertTrue(t, slices.Equal(keysFor("late"), []string{
		"github:acme/repo#41:interest:late",
		"github:acme/repo#41:merged:4101:late",
	}), keysFor("late"))
	assertTrue(t, slices.Equal(keysFor("between"), []string{"github:acme/repo#42:merged:4201:between"}), keysFor("between"))
	assertTrue(t, slices.Equal(keysFor("own"), []string{"github:acme/repo#43:merged:4301:own"}), keysFor("own"))
	assertTrue(t, slices.Equal(keysFor("creator"), []string{
		"github:acme/repo#41:merged:4101",
		"github:acme/repo#42:merged:4201",
		"github:acme/repo#43:merged:4301",
	}), "the creator gets each merge exactly once: ", keysFor("creator"))
	for _, row := range pendingMailboxEvents(t, 100) {
		if strings.Contains(row.EventKey, ":interest:") {
			continue
		}
		event := decodeMailboxEvent(t, row.EventJSON)
		interestedRow := row.TargetSessionID != "creator"
		assertEqual(t, event.Wake, !interestedRow, row.EventKey)
		assertEqual(t, strings.Contains(row.EventJSON, `"recipient":"interested"`), interestedRow, row.EventKey)
	}
}

// Hard-won: review f2c1683: equality at the processed second suppressed later activity.
func TestWatcherRechecksSameSecondUpdatesThenConverges(t *testing.T) {
	if !runIsolatedChild(t) {
		return
	}
	installIsolatedTestWriter(t)
	saveCreatorSession(t)
	cursor := repoCursor(t)
	at := secondTicks(nextWholeSecond(t, cursor.UpdatedAt))
	classification := watchClassification(1, at(1))
	classification.Kind = "issue"
	commitClassification(t, classification, nil)
	fake := newFakeGithub(t)
	fake.mu.Lock()
	item := githubItem(1, 1, "synthetic", at(1), false)
	item["updated_at"] = at(10)
	fake.items = append(fake.items, item)
	fake.observedAt = ptr(at(10))
	fake.timelines[1] = []map[string]any{{"event": "commented", "id": 1, "created_at": at(10),
		"user": map[string]any{"login": "other"}, "body": "first"}}
	fake.mu.Unlock()
	api, server := startFakeGithub(t, fake)
	defer server.Close()
	ctx := context.Background()
	mustNil(t, pollRepositoryWithContext(ctx, "acme", "repo", nil, newFakeClassifier(), api))
	assertEqual(t, githubWatches(t)[0].ProcessedAt, at(-20))
	fake.mu.Lock()
	fake.timelines[1] = append(fake.timelines[1], map[string]any{"event": "commented", "id": 2, "created_at": at(10),
		"user": map[string]any{"login": "other"}, "body": "same-second update"})
	fake.observedAt = ptr(at(41))
	fake.mu.Unlock()
	mustNil(t, pollRepositoryWithContext(ctx, "acme", "repo", nil, newFakeClassifier(), api))
	mustNil(t, pollRepositoryWithContext(ctx, "acme", "repo", nil, newFakeClassifier(), api))
	assertEqual(t, len(pendingMailboxEvents(t, 100)), 2)
	fake.mu.Lock()
	defer fake.mu.Unlock()
	assertEqual(t, len(fake.timelineRequests), 2, "one extra read catches the update and the later Date ends replay")
}

// Hard-won: review f2c1683: Date after the snapshot hid an update between them.
func TestWatcherProcessedMarginRecoversUpdatesBetweenSnapshotAndDate(t *testing.T) {
	if !runIsolatedChild(t) {
		return
	}
	installIsolatedTestWriter(t)
	saveCreatorSession(t)
	cursor := repoCursor(t)
	at := secondTicks(nextWholeSecond(t, cursor.UpdatedAt))
	watch := watchClassification(1, at(1))
	watch.Kind = "issue"
	commitClassification(t, watch, nil)
	fake := newFakeGithub(t)
	fake.mu.Lock()
	item := githubItem(1, 1, "synthetic", at(1), false)
	item["updated_at"] = at(10)
	fake.items = append(fake.items, item)
	fake.observedAt = ptr(at(12))
	fake.timelineAfterRead = &timelineEvent{target: 1, event: map[string]any{"event": "commented", "id": 11,
		"created_at": at(11), "user": map[string]any{"login": "other"}, "body": "between snapshot and Date"}}
	fake.mu.Unlock()
	api, server := startFakeGithub(t, fake)
	defer server.Close()
	ctx := context.Background()
	mustNil(t, pollRepositoryWithContext(ctx, "acme", "repo", nil, newFakeClassifier(), api))
	assertEqual(t, len(pendingMailboxEvents(t, 100)), 0)
	assertEqual(t, githubWatches(t)[0].ProcessedAt, at(-18))
	fake.mu.Lock()
	fake.items[0]["updated_at"] = at(11)
	fake.observedAt = ptr(at(42))
	fake.mu.Unlock()
	mustNil(t, pollRepositoryWithContext(ctx, "acme", "repo", nil, newFakeClassifier(), api))
	mustNil(t, pollRepositoryWithContext(ctx, "acme", "repo", nil, newFakeClassifier(), api))
	events := pendingMailboxEvents(t, 100)
	assertEqual(t, len(events), 1)
	assertTrue(t, strings.HasSuffix(events[0].EventKey, ":comment:11"), events[0].EventKey)
	fake.mu.Lock()
	timelineRequests := len(fake.timelineRequests)
	fake.mu.Unlock()
	assertEqual(t, timelineRequests, 2)
	assertEqual(t, githubWatches(t)[0].ProcessedAt, at(12))
}

func TestClassificationBuildsHistoryEvidenceForEverySession(t *testing.T) {
	if !runIsolatedChild(t) {
		return
	}
	installIsolatedTestWriter(t)
	cursor := repoCursor(t)
	created := core.FormatChronoRFC3339(parseTimestamp(t, cursor.WatchStartedAt).Add(time.Second))
	var items []store.GithubSeenItem
	for n := uint64(1); n <= 2; n++ {
		items = append(items, parsedGithubItem(t, githubItem(n, n, "synthetic", created, false)))
	}
	mustNil(t, store.RecordGithubPage("acme", "repo", items, store.GithubFeedProgressMain{Through: created}))
	var sessions []core.SessionRecord
	for _, id := range []string{"first", "second"} {
		session := store.TestSession(id, "project")
		session.CreatedAt = cursor.WatchStartedAt
		mustNil(t, store.SaveSession(&session))
		sessions = append(sessions, session)
	}
	api := must(newGithubAPI("http://127.0.0.1:1", ptr("snapshot-lifetime")))
	mustNil(t, classifyRecordedItems(context.Background(), "acme", "repo", sessions, newFakeClassifier(), cursor.WatchStartedAt, api))
	assertEqual(t, len(must(store.LoadGithubClassificationFailures("acme", "repo"))), 0)
}

// Hard-won: review 7fb8c222b: queued creator watches bypassed the enablement recheck.
func TestPreparedCreatorWatchRechecksEnablementBeforeCommit(t *testing.T) {
	if !runIsolatedChild(t) {
		return
	}
	installIsolatedTestWriter(t)
	session := store.TestSession("creator", "project")
	mustNil(t, store.SaveSession(&session))
	item := parsedGithubItem(t, githubItem(1, 1, "created item", "2026-11-18T00:00:00Z", true))
	err := classifyForSession(context.Background(), "acme", "repo", item, session, nil, newFakeClassifier(),
		newPollControl(func() bool { return false }))
	assertTrue(t, err != nil && isGithubEventsDisabled(err), "error ", err)
	assertEqual(t, len(githubWatches(t)), 0)
}

// Hard-won: review f2c1683: repeated failures consumed every poll before item 51.
func TestWatcherClassificationBackoffDoesNotStarveHealthySessions(t *testing.T) {
	if !runIsolatedChild(t) {
		return
	}
	installIsolatedTestWriter(t)
	cursor := repoCursor(t)
	created := core.FormatChronoRFC3339(parseTimestamp(t, cursor.WatchStartedAt).Add(time.Second))
	var items []store.GithubSeenItem
	for n := uint64(1); n <= 51; n++ {
		items = append(items, parsedGithubItem(t, githubItem(n, n, "synthetic", created, false)))
	}
	mustNil(t, store.RecordGithubPage("acme", "repo", items, store.GithubFeedProgressMain{Through: created}))
	var sessions []core.SessionRecord
	for _, id := range []string{"bad", "healthy"} {
		session := store.TestSession(id, "project")
		session.CreatedAt = cursor.WatchStartedAt
		mustNil(t, store.SaveSession(&session))
		sessions = append(sessions, session)
	}
	materialized := core.MaterializedSession{
		SessionID:           "bad",
		AppliedEventOrdinal: 1,
		AppliedEventDigest:  strings.Repeat("1", 64),
		LastActivityAtMs:    ptr(int64(1)),
		Execution:           core.MaterializedExecutionStateIdle{},
		Transcript: []*core.TranscriptItem{{
			StableID:        "user:1",
			Position:        1,
			CreatedAtMs:     1,
			LastChangedAtMs: 1,
			Body: core.TranscriptBodyUser{
				Content: []json.RawMessage{json.RawMessage(`{"type":"text","text":"FAIL_PAIR"}`)},
			},
		}},
	}
	mustNil(t, store.SaveMaterializedSession(&materialized))
	assertTrue(t, strings.Contains(must(sessionDigest("bad")).Conversation, "FAIL_PAIR"))
	var failing atomic.Bool
	failing.Store(true)
	classifier := failingSessionClassifier{failing: &failing}
	api := must(newGithubAPI("http://127.0.0.1:1", ptr("classification-backoff")))
	ctx := context.Background()
	classify := func() {
		t.Helper()
		mustNil(t, classifyRecordedItems(ctx, "acme", "repo", sessions, classifier, cursor.WatchStartedAt, api))
	}
	for range 3 {
		classify()
	}
	_, healthy := must(store.LoadGithubItemClassifiedSessions("acme", "repo", 51))["healthy"]
	assertTrue(t, healthy, "the healthy session classifies item 51")
	failures := must(store.LoadGithubClassificationFailures("acme", "repo"))
	assertEqual(t, len(failures), 51)
	badItem := store.GithubClassificationFailureKey{Number: 1, SessionID: "bad"}
	failure := failures[badItem]
	assertEqual(t, failure.Attempts, 1)
	assertEqual(t, failure.NextAttemptAtMs-failure.LastAttemptAtMs, 60_000)
	// Retry durable failures beyond the one-hour cap; they never give up.
	for attempt := int64(2); attempt <= 8; attempt++ {
		mustNil(t, store.SeedGithubRetryDue("acme", "repo", 1, "bad"))
		classify()
		failure := must(store.LoadGithubClassificationFailures("acme", "repo"))[badItem]
		assertEqual(t, failure.Attempts, attempt)
		assertTrue(t, failure.NextAttemptAtMs-failure.LastAttemptAtMs <= 3_600_000)
	}
	failing.Store(false)
	mustNil(t, store.SeedGithubRetryDue("acme", "repo", 1, "bad"))
	classify()
	_, stillFailing := must(store.LoadGithubClassificationFailures("acme", "repo"))[badItem]
	assertTrue(t, !stillFailing)
	_, classified := must(store.LoadGithubItemClassifiedSessions("acme", "repo", 1))["bad"]
	assertTrue(t, classified)
}

// Hard-won: d3fd6f01e: Jev denied items a session's own `gh pr create` opened and claimed bot batches.
func TestClassificationUsesOpeningToolCallsAndTheCredentialAuthor(t *testing.T) {
	if !runIsolatedChild(t) {
		return
	}
	installIsolatedTestWriter(t)
	cursor := repoCursor(t)
	createdTime := parseTimestamp(t, cursor.WatchStartedAt).Add(time.Second)
	createdMs := createdTime.UnixMilli()
	created := core.FormatChronoRFC3339(createdTime)
	opened := githubItem(1, 1, "interesting synthetic change", created, true)
	opened["user"] = map[string]any{"login": "Watcher-Bot"}
	batch := githubItem(2, 2, "interesting created batch", created, true)
	batch["user"] = map[string]any{"login": "merge-bot[bot]"}
	mustNil(t, store.RecordGithubPage("acme", "repo",
		[]store.GithubSeenItem{parsedGithubItem(t, opened), parsedGithubItem(t, batch)},
		store.GithubFeedProgressMain{Through: created}))
	var sessions []core.SessionRecord
	for _, id := range []string{"opener", "other"} {
		session := store.TestSession(id, "project")
		session.CreatedAt = cursor.WatchStartedAt
		mustNil(t, store.SaveSession(&session))
		sessions = append(sessions, session)
	}
	mustNil(t, store.SaveMaterializedSession(&core.MaterializedSession{
		SessionID:           "opener",
		AppliedEventOrdinal: 1,
		AppliedEventDigest:  strings.Repeat("1", 64),
		LastActivityAtMs:    ptr(int64(1)),
		Execution:           core.MaterializedExecutionStateIdle{},
		Transcript: []*core.TranscriptItem{{
			StableID:        "tool:1",
			Position:        1,
			CreatedAtMs:     createdMs - 3000,
			LastChangedAtMs: createdMs + 2000,
			Body: core.TranscriptBodyTool{
				Call: core.OpaqueToolCallFromValue(json.RawMessage(
					`{"toolCallId":"call-1","title":"Open the pull request","kind":"execute","status":"completed","rawInput":{"command":"python open_item.py"}}`)),
				TerminalOutputs: []core.TerminalOutputRecord{{
					TerminalID: "call-1",
					Output:     core.NewPackedText("https://github.com/acme/repo/pull/1\n"),
					Truncated:  false,
					ExitCode:   ptr(uint32(0)),
				}},
			},
		}},
	}))
	api, server := startFakeGithub(t, newFakeGithub(t))
	defer server.Close()
	mustNil(t, classifyRecordedItems(context.Background(), "acme", "repo", sessions, newFakeClassifier(), cursor.WatchStartedAt, api))
	type creatorOf struct {
		number  int64
		creator string
	}
	var creators []creatorOf
	for _, watch := range githubWatches(t) {
		creator := "<none>"
		if watch.CreatorSessionID != nil {
			creator = *watch.CreatorSessionID
		}
		creators = append(creators, creatorOf{watch.Number, creator})
	}
	assertTrue(t, slices.Equal(creators, []creatorOf{{1, "opener"}, {2, "<none>"}}),
		"the opening call claims item 1; another account's batch is watched for its interested sessions, with no creator: ", creators)
	var interest []string
	for _, row := range pendingMailboxEvents(t, 1000) {
		interest = append(interest, row.EventKey)
	}
	slices.Sort(interest)
	assertTrue(t, slices.Equal(interest, []string{
		"github:acme/repo#1:interest:other",
		"github:acme/repo#2:interest:opener",
		"github:acme/repo#2:interest:other",
	}), interest)
}

// TestGithubTimelineReadOnlyDryRun runs with MJ_GITHUB_DRY_RUN pointing to a
// private JSON file containing
// {since, items:[{item:<issues entry>, timeline:[<timeline entries>]}]}.
// This entry point never opens a store or sends a request.
func TestGithubTimelineReadOnlyDryRun(t *testing.T) {
	// Rust: #[ignore = "requires private read-only GitHub capture"].
	if os.Getenv(targets.IncludeIgnoredEnv) != "1" {
		t.Skip("requires private read-only GitHub capture")
	}
	path, ok := os.LookupEnv("MJ_GITHUB_DRY_RUN")
	if !ok {
		t.Fatal("MJ_GITHUB_DRY_RUN")
	}
	capture := must(jsonvalue.Parse(must(os.ReadFile(path))))
	counts := map[string]any{}
	examples := []any{}
	captured, _ := jsonArray(jsonIndex(capture, "items"))
	since, _ := jsonStr(jsonIndex(capture, "since"))
	for _, entry := range captured {
		item, ok := parseGithubItem(jsonIndex(entry, "item"))
		if !ok {
			t.Fatal("captured item is not a GitHub item")
		}
		watch := store.GithubItemWatch{
			Owner:             "brokkai",
			Repo:              "bifrost-dev",
			Number:            item.Number,
			CreatorSessionID:  ptr("dry-run"),
			Kind:              item.Kind,
			Title:             item.Title,
			URL:               item.URL,
			CreatedAt:         item.CreatedAt,
			TimelineAt:        since,
			TimelineID:        0,
			ReviewCommentsAt:  since,
			ReviewCommentsID:  0,
			ProcessedAt:       "",
			RecordedUpdatedAt: ptr(item.UpdatedAt),
		}
		comments, _ := jsonArray(jsonIndex(entry, "review_comments"))
		timeline, _ := jsonArray(jsonIndex(entry, "timeline"))
		var mergedAt *string
		if at, ok := jsonStr(jsonIndex(jsonIndex(entry, "pull"), "merged_at")); ok {
			mergedAt = &at
		}
		mapped := must(mapItemActivity(&watch, timeline, comments, mergedAt))
		for _, event := range mapped.events {
			body := must(jsonvalue.Parse(must(jsonvalue.Marshal(event.Body))))
			kind, _ := jsonStr(jsonIndex(body, "type"))
			count, _ := counts[kind].(int)
			counts[kind] = count + 1
			if len(examples) < 12 {
				// No titles, comment text, review text, or user logins leave this test.
				examples = append(examples, jsonvalue.Object{
					{Key: "key", Value: event.Key},
					{Key: "type", Value: kind},
					{Key: "at", Value: event.Timestamp},
				})
			}
		}
	}
	summary := must(jsonvalue.Marshal(jsonvalue.Object{
		{Key: "items", Value: len(captured)},
		{Key: "since", Value: jsonIndex(capture, "since")},
		{Key: "counts", Value: jsonvalue.ObjectOf(counts)},
		{Key: "examples", Value: examples},
	}))
	fmt.Println("DRY_RUN " + string(summary))
}
