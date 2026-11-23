// Port of mj-github-relevance/src/github_relevance.rs.

// Frozen student/GLiDE relevance policy. Evidence arrives bounded exactly
// once.
//
// Rust's ScoreFuture and GlideFuture are boxed futures; in Go the student
// and GLiDE calls block their goroutine and take a context instead.

package githubrelevance

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"sync"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// #1513, accepted labels/panel/band-point90.json operating point.
const (
	notify      = 0.2830
	reject      = 0.0716
	glideNotify = 0.1123
)

func failureLabel(kind StudentFailureKind) string {
	switch kind {
	case StudentFailureKindDownload:
		return "download"
	case StudentFailureKindVerification:
		return "verification"
	case StudentFailureKindLoad:
		return "load"
	case StudentFailureKindInference:
		return "inference"
	case StudentFailureKindWindowsRuntimeMissing:
		return "Windows runtime"
	}
	panic("unknown student failure kind " + string(kind))
}

// StudentOwner owns readiness, preparation/retry and inference concurrency.
// Scoring must run blocking inference off the caller's goroutine.
type StudentOwner interface {
	ManifestRevision() string
	// ScoreSegments is the student's probability for the pair; the error is
	// nil on success.
	ScoreSegments(ctx context.Context, item, session string) (float64, StudentError)
}

// ScoreSegments is Student's StudentOwner method: scoreSegments with any
// other error reported as an inference failure.
func (s *Student) ScoreSegments(ctx context.Context, item, session string) (float64, StudentError) {
	probability, err := s.scoreSegments(ctx, item, session)
	if err == nil {
		return probability, nil
	}
	var studentError StudentError
	if errors.As(err, &studentError) {
		return 0, studentError
	}
	return 0, StudentErrorInference{Cause: err.Error()}
}

// Route is how a verdict was reached; it serializes as its Rust variant
// name.
type Route string

const (
	RouteStudentNotify Route = "StudentNotify"
	RouteStudentReject Route = "StudentReject"
	RouteGlideBand     Route = "GlideBand"
	RouteGlideDegraded Route = "GlideDegraded"
	RouteGlideProxy    Route = "GlideProxy"
)

// Notices turns the student's health into public notices, once per
// degraded episode.
type Notices struct {
	// mu guards degraded.
	mu       sync.Mutex
	degraded bool
	notice   func(string)
}

func NewNotices(notice func(string)) *Notices {
	return &Notices{notice: notice}
}

// ObservedFailure is a failure Notices.Observe reports (Rust's
// (Option<StudentFailureKind>, &str)): Kind is nil while the model is still
// being prepared.
type ObservedFailure struct {
	Kind  *StudentFailureKind
	Cause string
}

// Observe records whether local scoring failed (failure not nil) or worked
// (nil), announcing the start and the end of a degraded episode.
func (n *Notices) Observe(failure *ObservedFailure) {
	n.mu.Lock()
	defer n.mu.Unlock()
	switch {
	case failure != nil && !n.degraded:
		n.degraded = true
		slog.Warn("GitHub local relevance scoring unavailable", "cause", failure.Cause)
		stage := "preparation"
		if failure.Kind != nil {
			stage = failureLabel(*failure.Kind)
		}
		n.notice(fmt.Sprintf("Local GitHub relevance scoring is unavailable (%s). Every GitHub item is scored by GLiDE while the model retries automatically.", stage))
	case failure == nil && n.degraded:
		n.degraded = false
		n.notice("GitHub relevance local model recovered.")
	}
}

// StudentNotice announces one of the student's typed transitions.
func (n *Notices) StudentNotice(notice StudentNotice) {
	panic("unported: mj-github-relevance/src/github_relevance.rs Notices::student_notice")
}

// Classifier decides one GitHub item's relevance: the local student when
// it is confident, hosted GLiDE otherwise.
type Classifier struct {
	// student is nil when local inference is off.
	student StudentOwner
	notices *Notices
	// fastinoKey is the user's own Fastino key, nil for the shared proxy
	// key.
	fastinoKey *string
}

// NewClassifier builds a classifier. Pass a nil interface for student, not
// a nil *Student, when local inference is off.
func NewClassifier(student StudentOwner, notices *Notices, fastinoKey *string) *Classifier {
	return &Classifier{student: student, notices: notices, fastinoKey: fastinoKey}
}

func (c *Classifier) Classify(ctx context.Context, evidence *core.GithubItemEvidence) (core.GithubItemVerdict, error) {
	key := c.fastinoKey
	return c.ClassifyWith(ctx, evidence, func(ctx context.Context, evidence *core.GithubItemEvidence) (float64, error) {
		return ClassifyProbability(ctx, evidence, key)
	})
}

// ClassifyWith classifies with glide as the hosted GLiDE call; it is called
// at most once, with evidence.
func (c *Classifier) ClassifyWith(ctx context.Context, evidence *core.GithubItemEvidence, glide func(ctx context.Context, evidence *core.GithubItemEvidence) (float64, error)) (core.GithubItemVerdict, error) {
	panic("unported: mj-github-relevance/src/github_relevance.rs Classifier::classify_with")
}

func validProbability(p float64) (float64, error) {
	panic("unported: mj-github-relevance/src/github_relevance.rs valid_probability")
}
