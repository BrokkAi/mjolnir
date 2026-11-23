// Port of the tests in mj-github-relevance/src/github_relevance.rs.

package githubrelevance

import (
	"context"
	"encoding/json"
	"errors"
	"math"
	"os"
	"sync"
	"sync/atomic"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// recordedStudent is a StudentOwner whose state the test sets.
type recordedStudent struct {
	t     *testing.T
	mu    sync.Mutex
	state StudentState
	score float64
	calls atomic.Int64
}

func (s *recordedStudent) ManifestRevision() string { return "recorded-boundary" }

func (s *recordedStudent) ScoreSegments(_ context.Context, item, session string) (float64, StudentError) {
	if !json.Valid([]byte(item)) {
		s.t.Errorf("item segment is not JSON: %s", item)
	}
	var parsed struct {
		History json.RawMessage `json:"history"`
	}
	if err := json.Unmarshal([]byte(session), &parsed); err != nil || len(parsed.History) == 0 || parsed.History[0] != '[' {
		s.t.Errorf("session segment has no history array: %s", session)
	}
	s.calls.Add(1)
	s.mu.Lock()
	state := s.state
	s.mu.Unlock()
	if _, ready := state.(StudentStateReady); ready {
		return s.score, nil
	}
	return 0, StudentErrorUnavailable{State: state}
}

func (s *recordedStudent) setState(state StudentState) {
	s.mu.Lock()
	s.state = state
	s.mu.Unlock()
}

// Hard-won: #1513: a readiness pre-check left lazy model preparation dormant.
func TestGithubRelevanceExactBoundariesAndFailureEpisodes(t *testing.T) {
	const child = "MJ_GITHUB_RELEVANCE_TEST_CHILD"
	if _, ok := os.LookupEnv(child); !ok {
		root := t.TempDir()
		// The child selects the Go test named by the last `::` segment.
		targets.NewIsolatedTest(targets.TestName(
			"mj_github_relevance::github_relevance::tests",
			"TestGithubRelevanceExactBoundariesAndFailureEpisodes",
		)).
			Env(child, "1").
			IsolatedStore(root).
			Run()
		return
	}
	// Threshold ordering and failure recovery: an error must never become a
	// rejection.
	const epsilon = 0x1p-52 // f64::EPSILON
	ctx := context.Background()
	evidence := aliases600Evidence(t)
	proxy := NewClassifier(nil, NewNotices(func(notice string) {
		t.Errorf("proxy-only scoring must not announce local model failure: %s", notice)
	}), nil)
	for _, c := range []struct {
		probability float64
		interested  bool
	}{{0.1123, true}, {0.1123 - epsilon, false}} {
		var calls atomic.Int64
		verdict, err := proxy.ClassifyWith(ctx, &evidence, func(_ context.Context, received *core.GithubItemEvidence) (float64, error) {
			if received != &evidence {
				t.Error("GLiDE received other evidence")
			}
			calls.Add(1)
			return c.probability, nil
		})
		if err != nil {
			t.Fatal(err)
		}
		if verdict.Interested != c.interested || calls.Load() != 1 {
			t.Fatalf("%v: interested %v after %d calls", c.probability, verdict.Interested, calls.Load())
		}
	}
	if _, err := proxy.ClassifyWith(ctx, &evidence, func(context.Context, *core.GithubItemEvidence) (float64, error) {
		return 0, errors.New("proxy unavailable")
	}); err == nil {
		t.Fatal("a GLiDE failure must not become a verdict")
	}
	for _, c := range []struct {
		score, provider float64
		interested      bool
		calls           int64
	}{
		{0.2830, 0.0, true, 0},
		{0.2830 - epsilon, 0.1123, true, 1},
		{0.0716, 0.1123 - epsilon, false, 1},
		{0.0716 - epsilon, 1.0, false, 0},
		{math.NaN(), 0.1123, true, 1},
	} {
		student := &recordedStudent{t: t, state: StudentStateReady{}, score: c.score}
		classifier := NewClassifier(student, NewNotices(func(string) {}), nil)
		var used atomic.Int64
		result, err := classifier.ClassifyWith(ctx, &evidence, func(context.Context, *core.GithubItemEvidence) (float64, error) {
			used.Add(1)
			return c.provider, nil
		})
		if err != nil {
			t.Fatal(err)
		}
		if result.Interested != c.interested || used.Load() != c.calls {
			t.Fatalf("score %v: interested %v after %d GLiDE calls", c.score, result.Interested, used.Load())
		}
	}
	student := &recordedStudent{t: t, state: StudentStateLoading{}, score: 0.5}
	var noticesMu sync.Mutex
	var notices []string
	classifier := NewClassifier(student, NewNotices(func(notice string) {
		noticesMu.Lock()
		notices = append(notices, notice)
		noticesMu.Unlock()
	}), nil)
	noticeCount := func() int {
		noticesMu.Lock()
		defer noticesMu.Unlock()
		return len(notices)
	}
	for range 2 {
		if _, err := classifier.ClassifyWith(ctx, &evidence, func(context.Context, *core.GithubItemEvidence) (float64, error) {
			return 0, errors.New("provider unavailable")
		}); err == nil {
			t.Fatal("a GLiDE failure must not become a verdict")
		}
	}
	if student.calls.Load() != 2 {
		t.Fatalf("Loading must still call score_segments to start preparation: %d calls", student.calls.Load())
	}
	student.setState(StudentStateFailed{Kind: StudentFailureKindDownload, Cause: "download failed", NextRetryAtMs: 123})
	verdict, err := classifier.ClassifyWith(ctx, &evidence, func(context.Context, *core.GithubItemEvidence) (float64, error) {
		return 0.1123, nil
	})
	if err != nil || !verdict.Interested {
		t.Fatalf("degraded scoring: %v, %v", verdict, err)
	}
	if noticeCount() != 1 {
		t.Fatalf("notices %v", notices)
	}
	student.setState(StudentStateReady{})
	verdict, err = classifier.ClassifyWith(ctx, &evidence, func(context.Context, *core.GithubItemEvidence) (float64, error) {
		t.Error("unexpected provider")
		return 0, errors.New("unexpected provider")
	})
	if err != nil || !verdict.Interested {
		t.Fatalf("recovered scoring: %v, %v", verdict, err)
	}
	if noticeCount() != 2 {
		t.Fatalf("notices %v", notices)
	}
}
