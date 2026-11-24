// Port of mj-github-watch/src/service.rs.

package githubwatch

import (
	"cmp"
	"context"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"maps"
	"math"
	"net/http"
	"net/mail"
	"net/netip"
	"net/url"
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
	"github.com/BrokkAi/mjolnir/go/internal/targets"
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
	// Rust holds the host weakly here so the notices do not keep it alive;
	// Go's collector handles the cycle.
	notices := githubrelevance.NewNotices(func(text string) {
		host.PushPublicNotice(text)
	})
	var student *githubrelevance.Student
	result := runPollLoop(ctx, host, &student, notices)
	var shutdown error
	if student != nil {
		shutdown = student.Shutdown(context.Background())
	}
	if shutdown != nil {
		slog.Warn("GitHub student shutdown failed", "error", shutdown.Error())
	}
	if result != nil {
		return result
	}
	return shutdown
}

// runPollLoop polls until ctx is done. student is the local relevance
// model, created when local inference is enabled and shut down when it is
// disabled (Rust's `&mut Option<Arc<Student>>`); Run shuts down what is left.
func runPollLoop(ctx context.Context, host GithubWatchHost, student **githubrelevance.Student, notices *githubrelevance.Notices) error {
	for {
		delay := configRecheckInterval
		if snapshot := host.GithubWatchSnapshot(); snapshot.Enabled {
			// Clamp the seconds before converting: a Duration overflows
			// where Rust's Duration::from_secs does not.
			seconds := min(snapshot.Config.IntervalSeconds, uint64(maxInterval/time.Second))
			delay = max(time.Duration(seconds)*time.Second, minInterval)
		}
		timer := time.NewTimer(delay)
		select {
		case <-ctx.Done():
			timer.Stop()
			return nil
		case <-timer.C:
		}
		if ctx.Err() != nil {
			return nil
		}
		snapshot := host.GithubWatchSnapshot()
		config := snapshot.Config
		if !config.LocalInference && *student != nil {
			previous := *student
			*student = nil
			if err := previous.Shutdown(context.Background()); err != nil {
				return err
			}
		}
		if !snapshot.Enabled {
			continue
		}
		if config.LocalInference {
			if *student == nil {
				*student = githubrelevance.NewStudent(core.DataDir(), config.UseGPU, func(notice githubrelevance.StudentNotice) {
					notices.StudentNotice(notice)
				})
			}
			(*student).SetUseGPU(config.UseGPU)
		}
		jev := snapshot.Jev
		type resolvedKey struct {
			key *string
			err error
		}
		resolved, err := spawnBlocking("resolve Fastino key", func() (resolvedKey, error) {
			key, err := jev.ResolveFastinoKey()
			return resolvedKey{key: key, err: err}, nil
		})
		if err != nil {
			return err
		}
		fastinoKey := resolved.key
		if resolved.err != nil {
			slog.Warn("Jev will use the shared proxy key", "error", resolved.err.Error())
			fastinoKey = nil
		}
		// A nil interface, not a nil *Student, when local inference is off.
		var owner githubrelevance.StudentOwner
		if *student != nil {
			owner = *student
		}
		var classifier githubClassifier = githubrelevance.NewClassifier(owner, notices, fastinoKey)
		repositories := sessionsByGithubRepo(&snapshot.State)
		watchedRepositories, err := spawnBlocking("load watched GitHub repositories", store.LoadGithubWatchedRepositories)
		if err != nil {
			return err
		}
		for _, key := range watchedRepositories {
			repository := ownerRepo(key)
			if _, ok := repositories[repository]; !ok {
				repositories[repository] = nil
			}
		}
		if stopped := pollRepositories(ctx, host, repositories, config, classifier); stopped {
			return nil
		}
	}
}

// repositoryPoll is one repository poll's outcome (Rust's JoinSet output,
// with a panic as its JoinError).
type repositoryPoll struct {
	owner     string
	repo      string
	err       error
	panicked  bool
	recovered any
}

// pollRepositories polls every repository, at most repositoryConcurrency at
// a time, in repository order. It is true when ctx ended, after every poll
// it started has returned.
func pollRepositories(ctx context.Context, host GithubWatchHost, repositories map[ownerRepo][]core.SessionRecord, config core.GithubWatchConfig, classifier githubClassifier) bool {
	// Ending ctx aborts the running polls (Rust's abort_all).
	jobs, abort := context.WithCancel(ctx)
	defer abort()
	keys := slices.SortedFunc(maps.Keys(repositories), compareOwnerRepo)
	results := make(chan repositoryPoll)
	running := 0
	for {
		for running < repositoryConcurrency && len(keys) > 0 {
			key := keys[0]
			keys = keys[1:]
			sessions := repositories[key]
			running++
			go func() {
				result := repositoryPoll{owner: key.Owner, repo: key.Repo}
				defer func() {
					if recovered := recover(); recovered != nil {
						result.panicked = true
						result.recovered = recovered
					}
					results <- result
				}()
				control := newPollControl(host.GithubEventsEnabled)
				result.err = pollRepository(jobs, key.Owner, key.Repo, sessions, config, classifier, control, host)
			}()
		}
		if running == 0 {
			return false
		}
		select {
		case <-ctx.Done():
			abort()
			for ; running > 0; running-- {
				if result := <-results; result.panicked {
					slog.Warn("GitHub watch task failed during shutdown", "error", fmt.Sprintf("task panicked: %v", result.recovered))
				}
			}
			return true
		case result := <-results:
			running--
			switch {
			case result.panicked:
				slog.Warn("GitHub repository poll task panicked", "error", fmt.Sprintf("task panicked: %v", result.recovered))
			case result.err == nil:
				slog.Debug("GitHub repository poll completed", "owner", result.owner, "repo", result.repo)
			case isGithubEventsDisabled(result.err):
				slog.Debug("GitHub repository poll stopped because GitHub events were disabled", "owner", result.owner, "repo", result.repo)
			default:
				slog.Warn("GitHub repository poll failed; durable cursors remain available for retry", "owner", result.owner, "repo", result.repo, "error", result.err.Error())
			}
		}
	}
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
	if ctx.Err() != nil {
		return nil
	}
	if err := control.checkEnabled(); err != nil {
		return err
	}
	watches, err := blockingDB(func() ([]store.GithubItemWatch, error) {
		return store.LoadGithubWatches(owner, repo)
	})
	if err != nil {
		return err
	}
	if len(sessions) == 0 && len(watches) == 0 {
		return nil
	}
	// Any session the repository's items are polled for can lend its
	// credential: a live session, a watch's creator, or an interested session
	// of a watch. Interested sessions that are stopped keep their watches, so
	// a merge must still be read while no session of the repository is live.
	var tokenSession *string
	if len(sessions) > 0 {
		id := sessions[0].ID
		tokenSession = &id
	} else {
		for _, watch := range watches {
			if watch.CreatorSessionID != nil {
				id := *watch.CreatorSessionID
				tokenSession = &id
				break
			}
		}
	}
	if tokenSession == nil {
		type interested struct {
			session string
			ok      bool
		}
		found, err := blockingDB(func() (interested, error) {
			session, ok, err := store.FirstGithubWatchInterestedSession(owner, repo)
			return interested{session: session, ok: ok}, err
		})
		if err != nil {
			return err
		}
		if found.ok {
			tokenSession = &found.session
		}
	}
	if tokenSession == nil {
		slog.Debug("GitHub repository has no session to take a credential from; skipping", "owner", owner, "repo", repo)
		return nil
	}
	token, err := host.GithubTokenForSession(ctx, *tokenSession)
	if err != nil {
		return err
	}
	api, err := newGithubAPIWithControl(config.APIBase, token, control)
	if err != nil {
		return err
	}
	return pollRepositoryWithContext(ctx, owner, repo, sessions, classifier, api)
}

func pollRepositoryWithContext(ctx context.Context, owner, repo string, sessions []core.SessionRecord, classifier githubClassifier, api *githubAPI) error {
	cursor, err := blockingDB(func() (store.GithubRepoCursor, error) {
		return store.GithubRepoCursorFor(owner, repo)
	})
	if err != nil {
		return err
	}
	if err := recordRepository(ctx, owner, repo, &cursor, api); err != nil {
		return err
	}
	if err := reconcileRepository(ctx, owner, repo, api, time.Now().UTC()); err != nil {
		if isGithubEventsDisabled(err) {
			return err
		}
		// Ending ctx aborts the poll, as dropping Rust's future does.
		if ctx.Err() != nil {
			return nil
		}
		slog.Warn("GitHub reconciliation failed; retrying from checkpoint next hour", "owner", owner, "repo", repo, "error", err.Error())
	}
	// Classification and timeline work have separate durable progress. Neither
	// one's failure prevents the other from running.
	var classification error
	var classificationPanic any
	done := make(chan struct{})
	go func() {
		defer close(done)
		// A panic reaches the caller, as it does from Rust's join!.
		defer func() { classificationPanic = recover() }()
		classification = classifyRecordedItems(ctx, owner, repo, sessions, classifier, cursor.WatchStartedAt, api)
	}()
	timeline := pollRepositoryTimelines(ctx, owner, repo, api)
	<-done
	if classificationPanic != nil {
		panic(classificationPanic)
	}
	if classification != nil {
		return classification
	}
	return timeline
}

func recordRepository(ctx context.Context, owner, repo string, cursor *store.GithubRepoCursor, api *githubAPI) error {
	return acquireRepository(ctx, owner, repo, cursor.UpdatedAt, nil, api)
}

func reconcileRepository(ctx context.Context, owner, repo string, api *githubAPI, now time.Time) error {
	if ctx.Err() != nil {
		return nil
	}
	if err := api.control.checkEnabled(); err != nil {
		return err
	}
	// Read the writer's current coverage after main acquisition, not its old snapshot.
	cursor, err := blockingDB(func() (store.GithubRepoCursor, error) {
		return store.GithubRepoCursorFor(owner, repo)
	})
	if err != nil {
		return err
	}
	until := core.FormatChronoRFC3339(now.Add(-time.Hour))
	if timestampPrecedes(cursor.UpdatedAt, until) {
		until = cursor.UpdatedAt
	}
	// Equality must replay the boundary: a crash may leave its tie unfinished.
	if timestampPrecedes(until, cursor.ReconciledThrough) {
		return nil
	}
	claimed, err := blockingDB(func() (bool, error) {
		return store.ClaimGithubReconciliation(owner, repo, now.UnixMilli())
	})
	if err != nil {
		return err
	}
	if !claimed {
		return nil
	}
	// Re-read every main-feed range once its item timestamps are an hour old.
	// Page commits retain coverage across pauses, failed requests and crashes.
	// With offset pages, an item can be missed only if moves within its tied
	// second disturb both the main read and the later reconciliation read.
	return acquireRepository(ctx, owner, repo, cursor.ReconciledThrough, &until, api)
}

// acquireRepository records the issues feed from `from`, up to until when it
// is set (a reconciliation pass) or to the newest item otherwise.
func acquireRepository(ctx context.Context, owner, repo, from string, until *string, api *githubAPI) error {
	// GitHub's `since` is inclusive and offset pages are ordered by update
	// time. After a full page, every item older than its last timestamp has
	// been read, so the next read starts at that timestamp. Only a page that
	// holds nothing newer than `since` itself pages forward by offset, so
	// each request either advances `since` or reads deeper into one second.
	newest := from
	since := overlapTimestamp(newest)
	page := 1
	for {
		if ctx.Err() != nil {
			return nil
		}
		response, err := api.getJSON(ctx, fmt.Sprintf("repos/%s/%s/issues", owner, repo), []queryParam{
			{"state", "all"},
			{"sort", "updated"},
			{"direction", "asc"},
			{"since", since},
			{"per_page", strconv.Itoa(itemsPerPage)},
			{"page", strconv.Itoa(page)},
		})
		if err != nil {
			return err
		}
		if err := api.control.checkEnabled(); err != nil {
			return err
		}
		values, ok := jsonArray(response)
		if !ok {
			return errors.New("GitHub issues response is not an array")
		}
		reachedEnd := false
		var last *string
		items := []store.GithubSeenItem{}
		for _, value := range values {
			item, ok := parseGithubItem(value)
			if !ok {
				slog.Warn("GitHub issues feed contains an invalid item; skipping it", "owner", owner, "repo", repo)
				continue
			}
			if until != nil && timestampPrecedes(*until, item.UpdatedAt) {
				reachedEnd = true
				continue
			}
			if timestampPrecedes(newest, item.UpdatedAt) {
				newest = item.UpdatedAt
			}
			if last == nil || timestampPrecedes(*last, item.UpdatedAt) {
				updatedAt := item.UpdatedAt
				last = &updatedAt
			}
			items = append(items, item)
		}
		shortPage := len(values) < itemsPerPage
		// A short page or the first item beyond the bound proves this range
		// complete, including a gap with no items. Otherwise save the newest
		// timestamp; restarting from its overlap rereads its whole second.
		var completedRange *string
		if until != nil && (reachedEnd || shortPage) {
			completedRange = until
		}
		through := newest
		if completedRange != nil {
			through = *completedRange
		}
		var progress store.GithubFeedProgress = store.GithubFeedProgressMain{Through: through}
		if until != nil {
			progress = store.GithubFeedProgressReconciled{Through: through}
		}
		if _, err := blockingDB(func() (struct{}, error) {
			return struct{}{}, store.RecordGithubPage(owner, repo, items, progress)
		}); err != nil {
			return err
		}
		if completedRange != nil || shortPage {
			break
		}
		if last != nil && timestampPrecedes(since, *last) {
			since = *last
			page = 1
		} else {
			page++
		}
	}
	return nil
}

// pendingClassification is one (item, session) pair awaiting
// classification, with its retry state.
type pendingClassification struct {
	item          store.GithubSeenItem
	session       core.SessionRecord
	lastAttemptMs *int64
	attempts      int64
}

// evidenceOutcome is a pair's prepared evidence: an error, nil evidence when
// the session created the item, or the evidence to classify.
type evidenceOutcome struct {
	evidence *core.GithubItemEvidence
	err      error
}

// classificationSlot is a pair with its place in the never-attempted /
// oldest-retry order; outcome is nil while its evidence is still to be built
// from the session's history scan.
type classificationSlot struct {
	index    int
	item     store.GithubSeenItem
	session  core.SessionRecord
	attempts int64
	outcome  *evidenceOutcome
}

// compareLastAttempt orders Rust's Option<i64>: never attempted first.
func compareLastAttempt(a, b *int64) int {
	switch {
	case a == nil && b == nil:
		return 0
	case a == nil:
		return -1
	case b == nil:
		return 1
	}
	return cmp.Compare(*a, *b)
}

func classifyRecordedItems(ctx context.Context, owner, repo string, sessions []core.SessionRecord, classifier githubClassifier, watchStartedAt string, api *githubAPI) error {
	items, err := blockingDB(func() ([]store.GithubSeenItem, error) {
		return store.LoadGithubSeenItems(owner, repo)
	})
	if err != nil {
		return err
	}
	failures, err := blockingDB(func() (map[store.GithubClassificationFailureKey]store.GithubClassificationFailure, error) {
		return store.LoadGithubClassificationFailures(owner, repo)
	})
	if err != nil {
		return err
	}
	now := core.EpochMillis()
	var pending []pendingClassification
	for _, item := range items {
		if ctx.Err() != nil {
			return nil
		}
		if err := api.control.checkEnabled(); err != nil {
			return err
		}
		if !timestampPrecedes(watchStartedAt, item.CreatedAt) {
			continue
		}
		number := item.Number
		classified, err := blockingDB(func() (map[string]struct{}, error) {
			return store.LoadGithubItemClassifiedSessions(owner, repo, number)
		})
		if err != nil {
			return err
		}
		for _, session := range sessions {
			if _, done := classified[session.ID]; session.NoGithubEvents ||
				timestampPrecedes(item.CreatedAt, session.CreatedAt) || done {
				continue
			}
			failure, failed := failures[store.GithubClassificationFailureKey{Number: item.Number, SessionID: session.ID}]
			if failed && failure.NextAttemptAtMs > now {
				continue
			}
			pair := pendingClassification{item: item, session: session}
			if failed {
				lastAttempt := failure.LastAttemptAtMs
				pair.lastAttemptMs = &lastAttempt
				pair.attempts = failure.Attempts
			}
			pending = append(pending, pair)
		}
	}
	// Each pair owns its retry state. Never-attempted work precedes retries;
	// eligible retries run oldest-first and never consume the cap in backoff.
	slices.SortStableFunc(pending, func(a, b pendingClassification) int {
		return cmp.Or(
			compareLastAttempt(a.lastAttemptMs, b.lastAttemptMs),
			cmp.Compare(a.item.Number, b.item.Number),
			strings.Compare(a.session.ID, b.session.ID),
		)
	})
	if len(pending) == 0 {
		return nil
	}
	pending = pending[:min(len(pending), maxClassificationsPerPoll)]
	login, err := api.credentialLogin(ctx)
	if err != nil {
		return err
	}
	concurrency := len(pending)
	bySession := map[string][]classificationSlot{}
	for index, pair := range pending {
		bySession[pair.session.ID] = append(bySession[pair.session.ID], classificationSlot{
			index: index, item: pair.item, session: pair.session, attempts: pair.attempts,
		})
	}
	prepared := make([]classificationSlot, 0, concurrency)
	for _, sessionID := range slices.Sorted(maps.Keys(bySession)) {
		if ctx.Err() != nil {
			return nil
		}
		if err := api.control.checkEnabled(); err != nil {
			return err
		}
		// One pass over the session's stored transcript feeds every item of
		// the session; no transcript text is held beyond a row at a time.
		digest, digestErr := sessionDigestForEvidence(sessionID)
		pairs := bySession[sessionID]
		slots := make([]classificationSlot, 0, len(pairs))
		var inputs []preparedPair
		for _, slot := range pairs {
			if ctx.Err() != nil {
				return nil
			}
			var pair *preparedPair
			var err error
			if digestErr == nil {
				pair, err = prepareSessionEvidence(owner, repo, &slot.item, &slot.session, digest, login, api.control)
			} else {
				err = errors.New(digestErr.Error())
			}
			switch {
			case err != nil:
				slot.outcome = &evidenceOutcome{err: err}
			case pair == nil:
				slot.outcome = &evidenceOutcome{}
			default:
				inputs = append(inputs, *pair)
			}
			slots = append(slots, slot)
		}
		// Ending this scan's context must also stop the blocking scan and
		// release its snapshot.
		scan, stopScan := context.WithCancel(ctx)
		built, buildErr := buildSessionEvidence(scan, sessionID, inputs)
		stopScan()
		if ctx.Err() != nil {
			return nil
		}
		var outcomes []evidenceOutcome
		if buildErr == nil {
			for index := range built {
				outcomes = append(outcomes, evidenceOutcome{evidence: &built[index]})
			}
		} else {
			message := buildErr.Error()
			for _, slot := range slots {
				if slot.outcome == nil {
					outcomes = append(outcomes, evidenceOutcome{err: errors.New(message)})
				}
			}
		}
		for _, slot := range slots {
			if slot.outcome == nil {
				if len(outcomes) == 0 {
					panic("one result per input")
				}
				slot.outcome = &outcomes[0]
				outcomes = outcomes[1:]
			}
			prepared = append(prepared, slot)
		}
	}
	// Grouping must not change the never-attempted / oldest-retry order.
	slices.SortFunc(prepared, func(a, b classificationSlot) int { return cmp.Compare(a.index, b.index) })
	type classified struct {
		err       error
		recovered any
	}
	results := make(chan classified, len(prepared))
	for _, slot := range prepared {
		go func() {
			var result classified
			// A panic reaches the poll, as it does from Rust's stream.
			defer func() {
				result.recovered = recover()
				results <- result
			}()
			result.err = classifyPreparedPair(ctx, owner, repo, slot, classifier, api.control)
		}()
	}
	var first error
	var recovered any
	for range prepared {
		result := <-results
		if result.recovered != nil && recovered == nil {
			recovered = result.recovered
		}
		if result.err != nil && first == nil {
			first = result.err
		}
	}
	if recovered != nil {
		panic(recovered)
	}
	return first
}

// classifyPreparedPair classifies one prepared pair and records a failure
// for retry with backoff.
func classifyPreparedPair(ctx context.Context, owner, repo string, slot classificationSlot, classifier githubClassifier, control pollControl) error {
	if ctx.Err() != nil {
		return nil
	}
	number := slot.item.Number
	sessionID := slot.session.ID
	err := slot.outcome.err
	if err == nil {
		err = classifyForSession(ctx, owner, repo, slot.item, slot.session, slot.outcome.evidence, classifier, control)
	}
	if err == nil {
		return nil
	}
	if isGithubEventsDisabled(err) {
		return err
	}
	// Ending ctx aborts the poll, as dropping Rust's future does; the
	// interrupted attempt is not a classification failure.
	if ctx.Err() != nil {
		return nil
	}
	attempt := slot.attempts
	if attempt < math.MaxInt64 {
		attempt++
	}
	slog.Warn("GitHub classification failed; retrying with backoff", "owner", owner, "repo", repo, "number", number,
		"session_id", sessionID, "attempt", attempt, "error", err.Error())
	_, err = blockingDB(func() (struct{}, error) {
		return struct{}{}, store.RecordGithubClassificationFailure(owner, repo, number, sessionID)
	})
	return err
}

// prepareSessionEvidence is nil without an error when the session created
// the item: its creator is never told about it and the classifier is not
// asked.
func prepareSessionEvidence(owner, repo string, item *store.GithubSeenItem, session *core.SessionRecord, digest core.SessionContext, credentialLogin *string, control pollControl) (*preparedPair, error) {
	repoLabel := owner + "/" + repo
	kind := itemKindFromWatch(item.Kind)
	// Sessions share one GitHub account, so the author cannot name a session,
	// but this session did not open an item another account authored.
	ownAccount := credentialLogin == nil || asciiEqualFold(*credentialLogin, item.Author)
	created := false
	if ownAccount {
		var err error
		created, err = openedBySession(session.ID, item.CreatedAt, uint64(item.Number), item.URL)
		if err != nil {
			return nil, err
		}
	}
	evidence := core.GithubItemEvidence{
		Item: core.GithubItem{
			Repo:   repoLabel,
			Kind:   kind,
			Number: uint64(item.Number),
			Title:  item.Title,
			Body:   item.Body,
			Author: item.Author,
			URL:    item.URL,
		},
		Session: digest,
	}
	// A bare `#N` in the session names the item's repository only when the
	// session works in no other one.
	bareIsItemRepo := len(sessionGithubRepositories(session)) == 1
	evidence.Session = evidence.Session.ForItem(&evidence.Item, bareIsItemRepo)
	// The creator already knows its item, so it is never told about it and
	// The classifier is not asked; its stored interest is false.
	if created {
		return nil, nil
	}
	if err := control.checkEnabled(); err != nil {
		return nil, err
	}
	original := evidence.Item
	cutoffMs, ok := timestampMillis(item.CreatedAt)
	if !ok {
		return nil, errors.New("invalid GitHub item creation time")
	}
	base, err := spawnBlocking("bound GitHub history evidence", func() (core.GithubItemEvidence, error) {
		bounded, err := evidence.BoundedForRequest()
		if err != nil {
			return core.GithubItemEvidence{}, err
		}
		// serde_json::to_value, redact_json_strings, serde_json::from_value.
		encoded, err := jsonvalue.Marshal(bounded)
		if err != nil {
			return core.GithubItemEvidence{}, err
		}
		value, err := jsonvalue.Parse(encoded)
		if err != nil {
			return core.GithubItemEvidence{}, err
		}
		core.RedactJSONStrings(&value)
		if encoded, err = jsonvalue.Marshal(value); err != nil {
			return core.GithubItemEvidence{}, err
		}
		var redacted core.GithubItemEvidence
		if err := json.Unmarshal(encoded, &redacted); err != nil {
			return core.GithubItemEvidence{}, err
		}
		return redacted, nil
	})
	if err != nil {
		return nil, err
	}
	return &preparedPair{original: original, base: base, cutoffMs: cutoffMs}, nil
}

// errHistoryEvidenceCancelled is the history scan's own cancellation, as
// Rust's `ensure!` words it.
func errHistoryEvidenceCancelled() error {
	return errors.New("GitHub history evidence cancelled")
}

// buildSessionEvidence builds every pair's evidence from one scan of the
// session's transcript. Ending ctx stops the scan and releases its snapshot.
func buildSessionEvidence(ctx context.Context, sessionID string, pairs []preparedPair) ([]core.GithubItemEvidence, error) {
	if len(pairs) == 0 {
		return []core.GithubItemEvidence{}, nil
	}
	stopped := func() bool { return ctx.Err() != nil }
	return spawnBlocking("build GitHub history evidence", func() ([]core.GithubItemEvidence, error) {
		// Items without searchable identifiers get empty history without a scan.
		var specs []transcript.HistoryPairSpec[*transcript.HistoryScanner]
		var scanned []int
		for index := range pairs {
			pair := &pairs[index]
			if stopped() {
				return nil, errHistoryEvidenceCancelled()
			}
			sink, err := transcript.HistoryScannerForItem(&pair.original, &pair.base)
			if err != nil {
				return nil, err
			}
			if sink != nil {
				scanned = append(scanned, index)
				specs = append(specs, transcript.HistoryPairSpec[*transcript.HistoryScanner]{
					CutoffMs: pair.cutoffMs,
					Repo:     pair.original.Repo,
					Number:   pair.original.Number,
					Sink:     sink,
				})
			}
		}
		var results []transcript.HistoryResult[struct{}]
		if len(specs) > 0 {
			history := transcript.NewSessionHistory[struct{}, *transcript.HistoryScanner](sessionID, specs)
			var err error
			if results, err = store.ScanGithubHistory(sessionID, history, stopped); err != nil {
				return nil, err
			}
		}
		evidence := make([]core.GithubItemEvidence, 0, len(pairs))
		for index := range pairs {
			if stopped() {
				return nil, errHistoryEvidenceCancelled()
			}
			spans := []transcript.Span{}
			if len(scanned) > 0 && scanned[0] == index {
				scanned = scanned[1:]
				if len(results) == 0 {
					panic("one result per scanned item")
				}
				spans = results[0].Spans
				results = results[1:]
			}
			built, _, err := githubrelevance.BuildEvidence(&pairs[index].base, spans)
			if err != nil {
				return nil, err
			}
			evidence = append(evidence, built)
		}
		return evidence, nil
	})
}

// classifyForSession classifies item for session; a nil evidence means the
// session created the item, which is stored as not interested.
func classifyForSession(ctx context.Context, owner, repo string, item store.GithubSeenItem, session core.SessionRecord, evidence *core.GithubItemEvidence, classifier githubClassifier, control pollControl) error {
	repoLabel := owner + "/" + repo
	kind := itemKindFromWatch(item.Kind)
	if err := control.checkEnabled(); err != nil {
		return err
	}
	created := evidence == nil
	interested := false
	if evidence != nil {
		verdict, err := classifier.Classify(ctx, evidence)
		if err != nil {
			return err
		}
		if err := control.checkEnabled(); err != nil {
			return err
		}
		interested = verdict.Interested
	}
	var event *store.GithubClassificationEvent
	if interested {
		key := fmt.Sprintf("github:%s#%d:interest:%s", repoLabel, item.Number, session.ID)
		encoded, err := jsonvalue.Marshal(core.MailboxEvent{
			Key:         key,
			Source:      "github",
			Wake:        false,
			CreatedAtMs: uint64(max(core.EpochMillis(), 0)),
			Body: core.MailboxEventBodyNewGithubItem{
				Kind:   kind,
				Number: uint64(item.Number),
				Title:  item.Title,
				Repo:   &repoLabel,
			},
		})
		if err != nil {
			return err
		}
		event = &store.GithubClassificationEvent{Key: key, JSON: string(encoded), Wake: false}
	}
	classification := store.GithubItemClassification{
		Owner:      owner,
		Repo:       repo,
		Number:     item.Number,
		SessionID:  session.ID,
		Kind:       item.Kind,
		Title:      item.Title,
		URL:        item.URL,
		CreatedAt:  item.CreatedAt,
		Interested: interested,
		Created:    created,
	}
	_, err := blockingDB(func() (struct{}, error) {
		_, err := store.CommitGithubItemClassification(classification, event)
		return struct{}{}, err
	})
	return err
}

func openedBySession(sessionID, createdAt string, number uint64, url string) (bool, error) {
	atMs, ok := timestampMillis(createdAt)
	if !ok {
		return false, nil
	}
	references := core.GithubReferences(url)
	if len(references) == 0 {
		return false, nil
	}
	item := references[0]
	output, err := blockingDB(func() ([]string, error) {
		return store.LoadGithubSessionCreationOutput(sessionID, atMs)
	})
	if err != nil {
		return false, err
	}
	for _, text := range output {
		for _, reference := range core.GithubReferences(text) {
			// A bare `#N` cannot name the repository, and sessions work
			// in several; creation output (gh, the API, MCP tools)
			// carries the full URL.
			if reference.Number == number &&
				reference.Owner != nil && item.Owner != nil && asciiEqualFold(*reference.Owner, *item.Owner) &&
				reference.Repo != nil && item.Repo != nil && asciiEqualFold(*reference.Repo, *item.Repo) {
				return true, nil
			}
		}
	}
	return false, nil
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
	watches, err := blockingDB(func() ([]store.GithubItemWatch, error) {
		return store.LoadGithubWatches(owner, repo)
	})
	if err != nil {
		return err
	}
	for _, watch := range watches {
		if ctx.Err() != nil {
			break
		}
		if err := api.control.checkEnabled(); err != nil {
			return err
		}
		if watch.RecordedUpdatedAt == nil {
			continue
		}
		if timestampPrecedes(*watch.RecordedUpdatedAt, watch.ProcessedAt) {
			continue
		}
		number := watch.Number
		if err := pollItemTimeline(ctx, watch, api); err != nil {
			if isGithubEventsDisabled(err) {
				return err
			}
			// Ending ctx aborts the poll, as dropping Rust's future does.
			if ctx.Err() != nil {
				break
			}
			slog.Warn("GitHub item timeline failed; will retry next poll", "owner", owner, "repo", repo, "number", number, "error", err.Error())
		}
	}
	return nil
}

// fetchedActivity is one item's activity as read from GitHub.
type fetchedActivity struct {
	entries     []any
	comments    []any
	mergedAt    *string
	processedAt string
}

func pollItemTimeline(ctx context.Context, watch store.GithubItemWatch, api *githubAPI) error {
	fetch := func() (fetchedActivity, error) {
		entries, processedAt, err := api.arrayPagesObserved(ctx,
			fmt.Sprintf("repos/%s/%s/issues/%d/timeline", watch.Owner, watch.Repo, watch.Number), nil)
		if err != nil {
			return fetchedActivity{}, err
		}
		// A merge that lands commits directly, as Merge Marshall does, leaves
		// only a closed entry in the timeline. Only the pull request itself
		// records that it merged, so read it when such a close appears.
		var mergedAt *string
		if watch.Kind == "pull_request" && hasCloseWithoutMerge(entries, watch.TimelineAt) {
			pull, err := api.getJSON(ctx, fmt.Sprintf("repos/%s/%s/pulls/%d", watch.Owner, watch.Repo, watch.Number), nil)
			if err != nil {
				return fetchedActivity{}, err
			}
			if at, ok := jsonStr(jsonIndex(pull, "merged_at")); ok {
				mergedAt = &at
			}
		}
		// Only a creator is told about review comments, so a watch kept for
		// interested sessions does not pay for the read.
		comments := []any{}
		if watch.Kind == "pull_request" && watch.CreatorSessionID != nil {
			// Use the stored cursor, before either source advances it.
			comments, err = api.arrayPages(ctx,
				fmt.Sprintf("repos/%s/%s/pulls/%d/comments", watch.Owner, watch.Repo, watch.Number),
				[]queryParam{{"since", overlapTimestamp(watch.ReviewCommentsAt)}})
			if err != nil {
				return fetchedActivity{}, err
			}
		}
		return fetchedActivity{entries: entries, comments: comments, mergedAt: mergedAt, processedAt: processedAt}, nil
	}
	fetched, err := fetch()
	if err != nil {
		var status githubHTTPStatus
		if !errors.As(err, &status) || (status.status != http.StatusNotFound && status.status != http.StatusGone) {
			return fmt.Errorf("GitHub activity %s/%s#%d: %w", watch.Owner, watch.Repo, watch.Number, err)
		}
		fetched = fetchedActivity{entries: []any{}, comments: []any{}, processedAt: status.observedAt}
	}
	mapped, err := mapItemActivity(&watch, fetched.entries, fetched.comments, fetched.mergedAt)
	if err != nil {
		return err
	}
	var login *string
	if slices.ContainsFunc(mapped.events, func(event store.MappedGithubEvent) bool { return event.Author != nil }) {
		if login, err = api.credentialLogin(ctx); err != nil {
			return err
		}
	}
	if err := api.control.checkEnabled(); err != nil {
		return err
	}
	_, err = blockingDB(func() (struct{}, error) {
		return struct{}{}, store.CommitGithubTimeline(watch, store.GithubItemProgress{
			TimelineAt:       mapped.cursorAt,
			TimelineID:       mapped.cursorID,
			ReviewCommentsAt: mapped.reviewCommentsAt,
			ReviewCommentsID: mapped.reviewCommentsID,
			ProcessedAt:      fetched.processedAt,
		}, mapped.events, login)
	})
	return err
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
	mapped, err := mapTimeline(watch, entries, &watch.TimelineAt, mergedAt)
	if err != nil {
		return timelineMapping{}, err
	}
	// Apply the stored boundary to update time, replaying ties.
	// A newly published comment can have an older creation time.
	commentsToMap := []any{}
	for _, comment := range comments {
		if at, ok := reviewCommentTimestamp(comment); ok && !timestampPrecedes(at, watch.ReviewCommentsAt) {
			commentsToMap = append(commentsToMap, comment)
		}
	}
	reviewEntries := []any{jsonvalue.Object{
		{Key: "event", Value: "line-commented"},
		{Key: "comments", Value: commentsToMap},
	}}
	reviewMapping, err := mapTimeline(watch, reviewEntries, nil, nil)
	if err != nil {
		return timelineMapping{}, err
	}
	mapped.events = append(mapped.events, reviewMapping.events...)
	for _, comment := range comments {
		timestamp, ok := reviewCommentTimestamp(comment)
		if !ok {
			continue
		}
		id, ok := jsonI64(jsonIndex(comment, "id"))
		if !ok {
			continue
		}
		if _, ok := timestampMillis(timestamp); !ok {
			return timelineMapping{}, errors.New("invalid GitHub review comment timestamp")
		}
		if compareWatermark(timestamp, uint64(id), mapped.reviewCommentsAt, uint64(mapped.reviewCommentsID)) > 0 {
			mapped.reviewCommentsAt = timestamp
			mapped.reviewCommentsID = id
		}
	}
	return mapped, nil
}

// reviewCommentTimestamp is a review comment's update time, or its creation
// time when it has none.
func reviewCommentTimestamp(comment any) (string, bool) {
	if at, ok := jsonStr(jsonIndex(comment, "updated_at")); ok {
		return at, true
	}
	return commentCreationTimestamp(comment)
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
	result := timelineMapping{
		events:           []store.MappedGithubEvent{},
		cursorAt:         watch.TimelineAt,
		cursorID:         watch.TimelineID,
		reviewCommentsAt: watch.ReviewCommentsAt,
		reviewCommentsID: watch.ReviewCommentsID,
	}
	merged := false
	repo := fmt.Sprintf("%s/%s", watch.Owner, watch.Repo)
	itemKind := itemKindFromWatch(watch.Kind)
	number := uint64(watch.Number)
	for _, entry := range entries {
		eventType, _ := jsonStr(jsonIndex(entry, "event"))
		// GitHub emits closed after merged, sometimes in a later second.
		// A reopen starts a new lifecycle interval.
		if eventType == "merged" {
			merged = true
		}
		if eventType == "reopened" {
			merged = false
		}
		values := []any{entry}
		if eventType == "line-commented" {
			comments, ok := jsonArray(jsonIndex(entry, "comments"))
			if !ok {
				return timelineMapping{}, errors.New("GitHub line-commented entry has no comments")
			}
			values = comments
		}
		for _, value := range values {
			timestamp, ok := commentCreationTimestamp(value)
			if !ok {
				continue
			}
			id, ok := jsonI64(jsonIndex(value, "id"))
			if !ok {
				continue
			}
			if _, ok := timestampMillis(timestamp); !ok {
				return timelineMapping{}, errors.New("invalid GitHub timeline timestamp")
			}
			if compareWatermark(timestamp, uint64(id), result.cursorAt, uint64(result.cursorID)) > 0 {
				result.cursorAt = timestamp
				result.cursorID = id
			}
			// IDs belong to different GitHub tables. Replay the boundary second
			// and let the outbox deduplicate, so a lower ID cannot hide an event.
			if since != nil && timestampPrecedes(timestamp, *since) {
				continue
			}
			author, _ := jsonStr(jsonIndex(jsonIndex(value, "user"), "login"))
			text, _ := jsonStr(jsonIndex(value, "body"))
			body := truncateUTF8(text, core.MailboxCommentBodyLimit)
			eventRepo := repo
			var suffix string
			var eventBody core.MailboxEventBody
			var filterAuthor string
			lifecycle := false
			switch {
			case eventType == "commented":
				suffix = "comment"
				eventBody = core.MailboxEventBodyGithubComment{
					ItemKind: itemKind, Number: number, Title: watch.Title,
					Author: author, Body: body, Repo: &eventRepo,
				}
				filterAuthor = author
			case eventType == "reviewed":
				suffix = "review"
				var reviewState *string
				if state, ok := jsonStr(jsonIndex(value, "state")); ok {
					reviewState = &state
				}
				eventBody = core.MailboxEventBodyGithubReview{
					ItemKind: itemKind, Number: number, Title: watch.Title,
					Author: author, Body: body, ReviewState: reviewState, Repo: &eventRepo,
				}
				filterAuthor = author
			case eventType == "line-commented":
				suffix = "review-comment"
				eventBody = core.MailboxEventBodyGithubReviewComment{
					ItemKind: itemKind, Number: number, Title: watch.Title,
					Author: author, Body: body, Repo: &eventRepo,
				}
				filterAuthor = author
			case (eventType == "merged" || eventType == "closed" || eventType == "reopened") && watch.Kind == "pull_request":
				if eventType == "closed" && merged {
					continue
				}
				actor, _ := jsonStr(jsonIndex(jsonIndex(value, "actor"), "login"))
				// Compare times so an earlier close, before a reopen and a
				// later merge, still reads as a close.
				var change core.MailboxPullRequestChange
				switch {
				case eventType == "merged":
					suffix, change = "merged", core.MailboxPullRequestChangeMerged
				case eventType == "closed" && mergedAt != nil && sameMoment(*mergedAt, timestamp):
					suffix, change = "merged", core.MailboxPullRequestChangeMerged
				case eventType == "closed":
					suffix, change = "closed", core.MailboxPullRequestChangeClosedWithoutMerging
				default:
					suffix, change = "reopened", core.MailboxPullRequestChangeReopened
				}
				eventBody = core.MailboxEventBodyGithubPullRequestLifecycle{
					Change: change, Number: number, Title: watch.Title, Actor: actor,
					Recipient: core.LifecycleRecipientCreator, Repo: &eventRepo,
				}
				filterAuthor = actor
				lifecycle = true
			default:
				continue
			}
			result.events = append(result.events, store.MappedGithubEvent{
				Key:       fmt.Sprintf("github:%s/%s#%d:%s:%d", watch.Owner, watch.Repo, watch.Number, suffix, id),
				Timestamp: timestamp,
				Author:    &filterAuthor,
				Body:      eventBody,
				Lifecycle: lifecycle,
			})
		}
	}
	return result, nil
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
	digest := sha256.New()
	if token != nil {
		digest.Write([]byte("token\x00"))
		digest.Write([]byte(*token))
	} else {
		digest.Write([]byte("anonymous"))
	}
	var key [32]byte
	copy(key[:], digest.Sum(nil))
	githubCredentials.Lock()
	defer githubCredentials.Unlock()
	state, ok := githubCredentials.states[key]
	if !ok {
		state = newGithubCredentialState()
		githubCredentials.states[key] = state
	}
	return state
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
	parsed, err := url.Parse(base)
	if err != nil {
		return nil, fmt.Errorf("parse GitHub API base URL: %w", err)
	}
	if parsed.Scheme == "" {
		return nil, errors.New("parse GitHub API base URL: relative URL without a base")
	}
	host := urlHostString(parsed)
	if host == "" {
		if specialURLScheme(parsed.Scheme) {
			return nil, errors.New("parse GitHub API base URL: empty host")
		}
		return nil, errors.New("GitHub API base URL has no host")
	}
	if parsed.Scheme != "https" && (parsed.Scheme != "http" || !isLoopbackHost(host)) {
		return nil, errors.New("GitHub API base URL must use HTTPS except for loopback HTTP hosts")
	}
	credential := credentialState(token)
	headers := http.Header{}
	headers.Set("Accept", "application/vnd.github+json")
	headers.Set("User-Agent", "mjolnir/"+core.PackageVersion)
	targets.InstallRustlsCryptoProvider()
	if token != nil {
		value := "Bearer " + *token
		if !headerValueVisible(value) {
			return nil, errors.New("invalid GitHub credential header: failed to parse header value")
		}
		headers.Set("Authorization", value)
	}
	return &githubAPI{
		base: strings.TrimRight(base, "/"),
		client: &http.Client{
			Timeout: apiTimeout,
			CheckRedirect: func(*http.Request, []*http.Request) error {
				return http.ErrUseLastResponse
			},
		},
		headers:    headers,
		credential: credential,
		control:    control,
	}, nil
}

// urlHostString is url::Url::host_str: an IPv6 address keeps its brackets.
func urlHostString(parsed *url.URL) string {
	host := parsed.Hostname()
	if strings.Contains(host, ":") {
		return "[" + host + "]"
	}
	return host
}

// specialURLScheme: the WHATWG URL parser Rust's url crate implements
// requires a host for these schemes.
func specialURLScheme(scheme string) bool {
	switch scheme {
	case "http", "https", "ws", "wss", "ftp":
		return true
	}
	return false
}

// headerValueVisible is what http::HeaderValue::from_str accepts: visible
// ASCII and tabs.
func headerValueVisible(value string) bool {
	for i := 0; i < len(value); i++ {
		if b := value[i]; b != '\t' && (b < 32 || b >= 127) {
			return false
		}
	}
	return true
}

// formURLEncode is form_urlencoded::byte_serialize, as reqwest's `query`
// writes each name and value.
func formURLEncode(text string) string {
	var encoded strings.Builder
	for i := 0; i < len(text); i++ {
		switch b := text[i]; {
		case 'a' <= b && b <= 'z', 'A' <= b && b <= 'Z', '0' <= b && b <= '9', b == '*', b == '-', b == '.', b == '_':
			encoded.WriteByte(b)
		case b == ' ':
			encoded.WriteByte('+')
		default:
			fmt.Fprintf(&encoded, "%%%02X", b)
		}
	}
	return encoded.String()
}

func (a *githubAPI) arrayPages(ctx context.Context, path string, query []queryParam) ([]any, error) {
	values, _, err := a.arrayPagesObserved(ctx, path, query)
	return values, err
}

// arrayPagesObserved reads every page of an array resource, with the
// observed time of the first page.
func (a *githubAPI) arrayPagesObserved(ctx context.Context, path string, query []queryParam) ([]any, string, error) {
	var observedAt *string
	values := []any{}
	for page := 1; ; page++ {
		pageQuery := append(slices.Clone(query),
			queryParam{"per_page", strconv.Itoa(itemsPerPage)},
			queryParam{"page", strconv.Itoa(page)})
		response, err := a.get(ctx, path, pageQuery)
		if err != nil {
			return nil, "", err
		}
		if response.notModified {
			return nil, "", errors.New("unexpected 304 response for paged GitHub request")
		}
		if observedAt == nil {
			observedAt = &response.observedAt
		}
		pageValues, ok := jsonArray(response.value)
		if !ok {
			return nil, "", errors.New("GitHub response is not an array")
		}
		values = append(values, pageValues...)
		if len(pageValues) < itemsPerPage {
			break
		}
	}
	return values, *observedAt, nil
}

func (a *githubAPI) getJSON(ctx context.Context, path string, query []queryParam) (any, error) {
	response, err := a.get(ctx, path, query)
	if err != nil {
		return nil, err
	}
	if response.notModified {
		return nil, fmt.Errorf("unexpected 304 for GitHub resource %s", path)
	}
	return response.value, nil
}

// credentialLogin is the credential's GitHub login, cached once known; nil
// while a failed lookup backs off.
func (a *githubAPI) credentialLogin(ctx context.Context) (*string, error) {
	if err := a.control.checkEnabled(); err != nil {
		return nil, err
	}
	var cache *githubLoginCache
	select {
	case cache = <-a.credential.login:
	case <-ctx.Done():
		return nil, ctx.Err()
	}
	defer func() { a.credential.login <- cache }()
	if cache.success != nil {
		login := *cache.success
		return &login, nil
	}
	if cache.retryAt != nil && cache.retryAt.After(time.Now()) {
		return nil, nil
	}
	user, err := a.getJSON(ctx, "user", nil)
	if err := a.control.checkEnabled(); err != nil {
		return nil, err
	}
	var login string
	if err == nil {
		var ok bool
		if login, ok = jsonStr(jsonIndex(user, "login")); !ok || login == "" {
			err = errors.New("GitHub credential response has no login")
		}
	}
	if err == nil {
		cache.success = &login
		cache.retryAt = nil
		cache.consecutiveFailures = 0
		result := login
		return &result, nil
	}
	if isGithubEventsDisabled(err) {
		return nil, err
	}
	// Ending ctx abandons the lookup, as dropping Rust's future does; it is
	// not a failed lookup.
	if ctx.Err() != nil {
		return nil, ctx.Err()
	}
	if cache.consecutiveFailures < math.MaxUint8 {
		cache.consecutiveFailures++
	}
	exponent := min(max(cache.consecutiveFailures, 1)-1, 8)
	retryDelay := min(time.Second<<exponent, 5*time.Minute)
	retryAt := time.Now().Add(retryDelay)
	cache.retryAt = &retryAt
	slog.Debug("GitHub credential login is unknown; lookup will retry after backoff",
		"error", err.Error(), "retry_seconds", uint64(retryDelay/time.Second))
	return nil, nil
}

// get sends a GET for path (relative to the base) with query, after any
// rate-limit deadline of the credential (Rust's GithubApi::get, given the
// request the caller built from the same path and query).
func (a *githubAPI) get(ctx context.Context, path string, query []queryParam) (githubResponse, error) {
	if err := a.control.checkEnabled(); err != nil {
		return githubResponse{}, err
	}
	if err := a.waitForRateLimit(ctx); err != nil {
		return githubResponse{}, err
	}
	if err := a.control.checkEnabled(); err != nil {
		return githubResponse{}, err
	}
	target := a.base + "/" + path
	if len(query) > 0 {
		pairs := make([]string, len(query))
		for i, pair := range query {
			pairs[i] = formURLEncode(pair.name) + "=" + formURLEncode(pair.value)
		}
		separator := "?"
		if strings.Contains(target, "?") {
			separator = "&"
		}
		target += separator + strings.Join(pairs, "&")
	}
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, target, nil)
	if err != nil {
		return githubResponse{}, fmt.Errorf("request GitHub REST API: %w", err)
	}
	request.Header = a.headers.Clone()
	response, err := a.client.Do(request)
	if err != nil {
		return githubResponse{}, fmt.Errorf("request GitHub REST API: %w", err)
	}
	defer response.Body.Close()
	a.recordRateLimit(response)
	observed := time.Now()
	if date := response.Header.Get("Date"); date != "" && headerValueVisible(date) {
		if parsed, err := mail.ParseDate(date); err == nil {
			observed = parsed
		}
	}
	// Date may follow the response snapshot, and local clocks may be ahead.
	observedAt := observed.Add(-30 * time.Second).UTC().Format("2006-01-02T15:04:05Z")
	status := response.StatusCode
	if status == http.StatusNotModified {
		return githubResponse{value: nil, notModified: true, observedAt: observedAt}, nil
	}
	if status < 200 || status > 299 {
		return githubResponse{}, githubHTTPStatus{status: status, observedAt: observedAt}
	}
	if response.ContentLength > githubResponseLimit {
		return githubResponse{}, errors.New("GitHub REST API response is larger than 8 MiB")
	}
	body, err := io.ReadAll(io.LimitReader(response.Body, githubResponseLimit+1))
	if err != nil {
		return githubResponse{}, fmt.Errorf("read GitHub REST API response: %w", err)
	}
	if len(body) > githubResponseLimit {
		return githubResponse{}, errors.New("GitHub REST API response is larger than 8 MiB")
	}
	value, err := jsonvalue.Parse(body)
	if err != nil {
		return githubResponse{}, fmt.Errorf("decode GitHub REST API response: %w", err)
	}
	return githubResponse{value: value, notModified: false, observedAt: observedAt}, nil
}

func (a *githubAPI) waitForRateLimit(ctx context.Context) error {
	for {
		if err := a.control.checkEnabled(); err != nil {
			return err
		}
		a.credential.rateLimitMu.Lock()
		deadline := a.credential.rateLimitUntil
		a.credential.rateLimitMu.Unlock()
		if deadline == nil {
			return nil
		}
		// Recheck enablement while waiting out a long deadline.
		expired := time.NewTimer(time.Until(*deadline))
		recheck := time.NewTimer(configRecheckInterval)
		select {
		case <-expired.C:
			recheck.Stop()
		case <-recheck.C:
			expired.Stop()
			continue
		case <-ctx.Done():
			expired.Stop()
			recheck.Stop()
			return ctx.Err()
		}
		now := time.Now()
		a.credential.rateLimitMu.Lock()
		if current := a.credential.rateLimitUntil; current != nil && !current.After(now) {
			a.credential.rateLimitUntil = nil
		}
		a.credential.rateLimitMu.Unlock()
	}
}

func (a *githubAPI) recordRateLimit(response *http.Response) {
	now := time.Now().UTC()
	retryAt, limited := core.GithubRateLimitRetryAt(response.StatusCode, response.Header, now)
	if !limited {
		return
	}
	delay := max(retryAt.Sub(now), 0)
	next := time.Now().Add(delay)
	a.credential.rateLimitMu.Lock()
	if current := a.credential.rateLimitUntil; current == nil || current.Before(next) {
		a.credential.rateLimitUntil = &next
	}
	a.credential.rateLimitMu.Unlock()
	slog.Warn("GitHub API rate limit reached; polling will back off", "retry_seconds", uint64(delay/time.Second))
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
