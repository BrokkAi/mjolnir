// Port of the tests in mj-cli/src/dashboard/attachment.rs.

package cli

import (
	"context"
	"errors"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

// attachmentResult is one (generation, result) report of an attach.
type attachmentResult struct {
	generation uint64
	err        error
}

func TestATransitionRetiresAnAttachAndRearmsTheSameSelectionOnce(t *testing.T) {
	attachment := newSessionAttachment()
	reported := make(chan uint64, 1)
	sessionAttachmentSpawn(
		attachment,
		"moving",
		attachTimeout,
		func(context.Context) (struct{}, error) { return struct{}{}, nil },
		func(generation uint64, _ struct{}, _ error) { reported <- generation },
	)
	generation := <-reported
	attachment.deferAttach()
	attachment.deferAttach()
	if attachment.accepts(generation, pointer("moving")) {
		t.Fatal("a deferred attach must not accept its old completion")
	}
	if !attachment.selectSession("moving") {
		t.Fatal("the same selection must be rearmed once")
	}
	if attachment.selectSession("moving") {
		t.Fatal("the same selection must be rearmed only once")
	}
}

func TestRetiringAWarmChatRearmsOnlyItsOwnSelection(t *testing.T) {
	attachment := newSessionAttachment()
	if !attachment.selectSession("created") {
		t.Fatal("first selection")
	}
	attachment.retire("another-workspace")
	if attachment.selectSession("created") {
		t.Fatal("retiring another session must not rearm this selection")
	}
	attachment.retire("created")
	if !attachment.selectSession("created") {
		t.Fatal("retiring this session must rearm its selection")
	}
	if attachment.selectSession("created") {
		t.Fatal("the selection is rearmed once")
	}
}

func TestFailedOpenDoesNotRetryOnBackgroundWakeupsButCanBeRetriedExplicitly(t *testing.T) {
	attachment := newSessionAttachment()
	reported := make(chan attachmentResult, 2)
	if !attachment.selectSession("muse") {
		t.Fatal("first selection")
	}
	sessionAttachmentSpawn(
		attachment,
		"muse",
		attachTimeout,
		func(context.Context) (struct{}, error) { return struct{}{}, errors.New("not managed") },
		func(generation uint64, _ struct{}, err error) { reported <- attachmentResult{generation, err} },
	)
	failed := <-reported
	if failed.err == nil {
		t.Fatal("the open must fail")
	}
	for range 1024 {
		if attachment.selectSession("muse") {
			t.Fatal("a failed open must not be retried by a background wakeup")
		}
	}
	sessionAttachmentSpawn(
		attachment,
		"muse",
		attachTimeout,
		func(context.Context) (struct{}, error) { return struct{}{}, nil },
		func(generation uint64, _ struct{}, err error) { reported <- attachmentResult{generation, err} },
	)
	retried := <-reported
	if retried.err != nil {
		t.Fatalf("retry: %v", retried.err)
	}
	if !attachment.accepts(retried.generation, pointer("muse")) {
		t.Fatal("the explicit retry must be accepted")
	}
	// A selection event can precede the next call to select/spawn.
	if attachment.accepts(retried.generation, pointer("codex")) {
		t.Fatal("a completion for another selection must be refused")
	}
	if attachment.accepts(failed.generation, pointer("muse")) {
		t.Fatal("the failed attempt's completion must be refused")
	}
}

func TestSwitchingSessionsAbortsHungOpenAndOpensTheNewSelection(t *testing.T) {
	attachment := newSessionAttachment()
	started := make(chan struct{})
	released := make(chan struct{})
	var cancelledReported atomic.Bool
	sessionAttachmentSpawn(
		attachment,
		"muse",
		attachTimeout,
		func(ctx context.Context) (struct{}, error) {
			defer close(released)
			close(started)
			<-ctx.Done()
			return struct{}{}, ctx.Err()
		},
		func(uint64, struct{}, error) { cancelledReported.Store(true) },
	)
	<-started
	reported := make(chan attachmentResult, 1)
	if !attachment.selectSession("codex") {
		t.Fatal("selecting another session")
	}
	sessionAttachmentSpawn(
		attachment,
		"codex",
		attachTimeout,
		func(context.Context) (struct{}, error) { return struct{}{}, nil },
		func(generation uint64, _ struct{}, err error) { reported <- attachmentResult{generation, err} },
	)
	var opened attachmentResult
	select {
	case opened = <-reported:
	case <-time.After(time.Second):
		t.Fatal("the new selection did not open")
	}
	if !attachment.accepts(opened.generation, pointer("codex")) {
		t.Fatal("the new selection's completion must be accepted")
	}
	if opened.err != nil {
		t.Fatalf("open: %v", opened.err)
	}
	select {
	case <-released:
	case <-time.After(time.Second):
		t.Fatal("the hung open was not dropped")
	}
	if cancelledReported.Load() {
		t.Fatal("cancelled attempt must not report")
	}
}

func TestTimeoutDropsHungWorkAndCancelDoesNotRestartTheSameSelection(t *testing.T) {
	attachment := newSessionAttachment()
	reported := make(chan attachmentResult, 1)
	released := make(chan struct{})
	sessionAttachmentSpawn(
		attachment,
		"muse",
		20*time.Millisecond,
		func(ctx context.Context) (struct{}, error) {
			defer close(released)
			<-ctx.Done()
			return struct{}{}, ctx.Err()
		},
		func(generation uint64, _ struct{}, err error) { reported <- attachmentResult{generation, err} },
	)
	var timedOut attachmentResult
	select {
	case timedOut = <-reported:
	case <-time.After(time.Second):
		t.Fatal("the hung open did not time out")
	}
	if timedOut.err == nil || !strings.Contains(timedOut.err.Error(), "did not respond") {
		t.Fatalf("timeout error %v", timedOut.err)
	}
	select {
	case <-released:
	case <-time.After(time.Second):
		t.Fatal("the timed-out work was not dropped")
	}
	attachment.cancel()
	if attachment.accepts(timedOut.generation, pointer("muse")) {
		t.Fatal("a cancelled attachment must refuse the old completion")
	}
	if attachment.selectSession("muse") {
		t.Fatal("cancel must not restart the same selection")
	}
	if !attachment.selectSession("codex") {
		t.Fatal("another selection")
	}
	if !attachment.selectSession("muse") {
		t.Fatal("selecting back")
	}
}

func TestPanickedOpenReportsFailureInsteadOfLeavingTheSpinnerRunning(t *testing.T) {
	attachment := newSessionAttachment()
	reported := make(chan error, 1)
	sessionAttachmentSpawn(
		attachment,
		"muse",
		attachTimeout,
		func(context.Context) (struct{}, error) { panic("fake preparation panic") },
		func(_ uint64, _ struct{}, err error) { reported <- err },
	)
	err := <-reported
	if err == nil || !strings.Contains(err.Error(), "task failed") {
		t.Fatalf("panic report %v", err)
	}
}
