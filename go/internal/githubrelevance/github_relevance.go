// Port of mj-github-relevance/src/github_relevance.rs.

// Frozen student/GLiDE relevance policy. Evidence arrives bounded exactly
// once.
//
// Rust's ScoreFuture and GlideFuture are boxed futures; in Go the student
// and GLiDE calls block their goroutine and take a context instead.

package githubrelevance

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"math"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
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
	var text string
	switch notice := notice.(type) {
	case StudentNoticeInferenceFailing:
		kind := StudentFailureKindInference
		n.Observe(&ObservedFailure{Kind: &kind, Cause: notice.Cause})
		return
	case StudentNoticeInferenceRecovered:
		n.Observe(nil)
		return
	case StudentNoticeDeviceFallback:
		var device string
		switch notice.From {
		case StudentDeviceCPU:
			device = "CPU"
		case StudentDeviceCUDA:
			device = "CUDA"
		case StudentDeviceCoreML:
			device = "CoreML"
		case StudentDeviceDirectML:
			device = "DirectML"
		}
		switch reason := notice.Reason.(type) {
		case StudentDeviceFallbackReasonSlower:
			text = fmt.Sprintf("GitHub relevance switched from %s to CPU: GPU scoring took %d ms versus %d ms on CPU.", device, reason.GPUMs, reason.CPUMs)
		case StudentDeviceFallbackReasonFailed:
			text = fmt.Sprintf("GitHub relevance switched from %s to CPU because %s failed.", device, failureLabel(reason.Kind))
		}
	case StudentNoticeCUDALibrariesMissing:
		text = fmt.Sprintf("GitHub relevance uses CPU because CUDA libraries are missing: %s. Install CUDA 12 and cuDNN 9, then restart the daemon to enable CUDA.", strings.Join(notice.Missing, ", "))
	case StudentNoticeGPUAfterRestart:
		text = "GitHub relevance continues on CPU. Restart the daemon to enable GPU scoring with the updated preference."
	case StudentNoticePlatformUnsupported:
		text = "Local GitHub relevance scoring is unavailable on this platform. Every GitHub item is scored by GLiDE."
	case StudentNoticeWindowsRuntimeMissing:
		text = fmt.Sprintf("Local GitHub relevance scoring needs the Microsoft Visual C++ Redistributable (x64): missing %s. Every GitHub item is scored by GLiDE while model preparation retries automatically.", strings.Join(notice.Missing, ", "))
	}
	n.notice(text)
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
	started := time.Now()
	var manifestRevision any
	if c.student != nil {
		manifestRevision = c.student.ManifestRevision()
	}
	var attempt *core.Attempt
	if log, err := core.DecisionLogOpen(core.ControllerLogDir()); err != nil {
		slog.Warn("GitHub relevance diagnostic log unavailable", "error", err.Error())
	} else {
		scope := "Hosted GLiDE"
		if c.student != nil {
			scope = "Student with GLiDE middle band"
		}
		opened := log.Start(fmt.Sprintf("github:%s#%d", evidence.Item.Repo, evidence.Item.Number),
			"github-relevance", "Is this GitHub item relevant to this session?", scope)
		attempt = &opened
		defer opened.Release()
	}
	if attempt != nil {
		waiting := "Waiting for GLiDE."
		if c.student != nil {
			waiting = "Waiting for the local model or GLiDE."
		}
		attempt.Update(&waiting, technicalJSON(jsonvalue.Object{{Key: "manifest_revision", Value: manifestRevision}}))
	}
	var failure *string
	var failureKind *StudentFailureKind
	var studentProbability *float64
	if c.student != nil {
		item, session, err := transcript.StudentSegments(evidence)
		if err != nil {
			return core.GithubItemVerdict{}, err
		}
		// Scoring is the owner's atomic decision point and starts lazy
		// preparation.
		probability, studentErr := c.student.ScoreSegments(ctx, item, session)
		if err := ctx.Err(); err != nil {
			// The caller gave up, as dropping the Rust future does: no
			// notice, no GLiDE call, and the attempt records cancellation.
			return core.GithubItemVerdict{}, err
		}
		if studentErr == nil {
			if _, err := validProbability(probability); err != nil {
				studentErr = StudentErrorInference{Cause: err.Error()}
			}
		}
		switch err := studentErr.(type) {
		case nil:
			studentProbability = &probability
		case StudentErrorUnavailable:
			if failed, ok := err.State.(StudentStateFailed); ok {
				kind := failed.Kind
				failureKind = &kind
				text := fmt.Sprintf("%s; automatic retry at epoch milliseconds %d", failed.Cause, failed.NextRetryAtMs)
				failure = &text
			} else {
				text := fmt.Sprintf("model unavailable (%s); automatic preparation is running", err.State.String())
				failure = &text
			}
		case StudentErrorInference:
			kind := StudentFailureKindInference
			failureKind = &kind
			text := fmt.Sprintf("%s; model owner will retry preparation", err.Cause)
			failure = &text
			slog.Warn("GitHub student inference failed; using GLiDE", "cause", err.Cause)
		}
		if failure != nil {
			c.notices.Observe(&ObservedFailure{Kind: failureKind, Cause: *failure})
		} else {
			c.notices.Observe(nil)
		}
	}
	var route Route
	switch {
	case studentProbability != nil && *studentProbability >= notify:
		route = RouteStudentNotify
	case studentProbability != nil && *studentProbability < reject:
		route = RouteStudentReject
	case studentProbability != nil:
		route = RouteGlideBand
	case c.student != nil:
		route = RouteGlideDegraded
	default:
		route = RouteGlideProxy
	}
	var glideProbability *float64
	var verdict core.GithubItemVerdict
	var resultErr error
	switch route {
	case RouteStudentNotify:
		verdict = core.GithubItemVerdict{Interested: true}
	case RouteStudentReject:
		verdict = core.GithubItemVerdict{Interested: false}
	default:
		probability, err := glide(ctx, evidence)
		if err == nil {
			probability, err = validProbability(probability)
		}
		if err != nil {
			resultErr = err
		} else {
			glideProbability = &probability
			verdict = core.GithubItemVerdict{Interested: probability >= glideNotify}
		}
	}
	if attempt != nil {
		var errorText any
		if resultErr != nil {
			errorText = resultErr.Error()
		}
		attempt.Update(nil, technicalJSON(jsonvalue.Object{
			{Key: "student_probability", Value: optionalFloat(studentProbability)},
			{Key: "glide_probability", Value: optionalFloat(glideProbability)},
			{Key: "manifest_revision", Value: manifestRevision},
			{Key: "route", Value: string(route)},
			{Key: "elapsed_ms", Value: uint64(time.Since(started).Milliseconds())},
			{Key: "failure_context", Value: optionalString(failure)},
			{Key: "error", Value: errorText},
		}))
		if resultErr == nil {
			answer := fmt.Sprintf("Interested: %t.", verdict.Interested)
			attempt.Update(&answer, technicalJSON(jsonvalue.Object{}))
			attempt.Finish("classified", "GitHub relevance classified this item.")
		} else {
			attempt.Finish("failed", "GitHub relevance failed; durable retry remains pending.")
		}
	}
	return verdict, resultErr
}

// technicalJSON encodes a diagnostic record's technical fields as json!
// does.
func technicalJSON(fields jsonvalue.Object) json.RawMessage {
	encoded, err := fields.MarshalJSON()
	if err != nil {
		panic("GitHub relevance technical record serializes: " + err.Error())
	}
	return encoded
}

func optionalFloat(value *float64) any {
	if value == nil {
		return nil
	}
	return *value
}

func optionalString(value *string) any {
	if value == nil {
		return nil
	}
	return *value
}

func validProbability(p float64) (float64, error) {
	if math.IsNaN(p) || math.IsInf(p, 0) || p < 0 || p > 1 {
		return 0, fmt.Errorf("invalid relevance probability: %s", rustFloat(p))
	}
	return p, nil
}

// rustFloat formats f as Rust's f64 Display does: plain decimal digits,
// never an exponent, `inf`, `-inf` and `NaN`.
func rustFloat(f float64) string {
	switch {
	case math.IsInf(f, 1):
		return "inf"
	case math.IsInf(f, -1):
		return "-inf"
	case math.IsNaN(f):
		return "NaN"
	}
	return strconv.FormatFloat(f, 'f', -1, 64)
}
