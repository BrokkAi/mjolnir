//go:build linux || darwin

// Port of the tests in mj-cli/src/daemon.rs (a module Rust compiles on
// Linux and macOS only). The container_tests module is in
// daemon_container_lifecycle_test.go and the Linux-only tests in
// daemon_linux_test.go.
//
// Rust runs some of these on tokio's paused clock. The ones whose waits
// involve no I/O run in a testing/synctest bubble, whose clock moves only
// when every goroutine in it is blocked, as a paused tokio clock does. The
// ones that talk to a fake daemon over loopback TCP cannot: a goroutine in a
// socket read keeps a bubble's clock still. They wait the same durations on
// the real clock instead; none is shortened.

package cli

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/netip"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync/atomic"
	"syscall"
	"testing"
	"testing/synctest"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/controller"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/workerstaging"
)

func fakeDaemon(t *testing.T, pid uint32) (net.Listener, client.DaemonMetadata) {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	check(t, err)
	metadata := client.DaemonMetadata{
		ProtocolVersion: client.ProtocolVersion,
		Pid:             pid,
		Address:         listenerAddress(listener),
		Token:           "lifecycle-test",
		StartedAt:       "test",
		BuildVersion:    client.ThisBuild().Published(),
		InstanceID:      "",
		Placement:       client.DaemonPlacement{Kind: client.DaemonPlacementKindHost},
	}
	return listener, metadata
}

// readRequest reads one request frame, reporting a failure with t.Errorf
// (it runs on fake-daemon goroutines).
func readRequest(t *testing.T, stream net.Conn) (client.RequestEnvelope, bool) {
	request, err := client.ReadFrame[client.RequestEnvelope](stream)
	if err != nil {
		t.Errorf("read the request: %v", err)
		return client.RequestEnvelope{}, false
	}
	return request, true
}

func answer(t *testing.T, stream net.Conn, request client.RequestEnvelope, result client.DaemonResult[client.DaemonReply]) {
	if err := client.WriteFrame(stream, client.ResponseEnvelope{
		ProtocolVersion: request.ProtocolVersion,
		RequestID:       request.RequestID,
		Result:          result,
	}); err != nil {
		t.Errorf("write the answer: %v", err)
	}
}

// expectClosed is Rust's `assert_eq!(stream.read(&mut [0]).await?, 0)`: the
// peer closed the connection, within limit.
func expectClosed(t *testing.T, stream net.Conn, limit time.Duration) {
	check(t, stream.SetReadDeadline(time.Now().Add(limit)))
	read, err := stream.Read(make([]byte, 1))
	if read != 0 || !errors.Is(err, io.EOF) {
		t.Errorf("the client left the connection open: %d, %v", read, err)
	}
}

func writeMetadata(t *testing.T, metadata *client.DaemonMetadata) {
	t.Helper()
	body, err := json.Marshal(metadata)
	check(t, err)
	check(t, os.WriteFile(client.MetadataPath(), body, 0o600))
}

// acquiring runs acquire in the background, as Rust's spawned task does.
type acquiring struct {
	cancel context.CancelFunc
	done   chan error
	guard  func()
}

func startAcquiring(acquire func(ctx context.Context) (func(), error)) *acquiring {
	ctx, cancel := context.WithCancel(context.Background())
	task := &acquiring{cancel: cancel, done: make(chan error, 1)}
	go func() {
		release, err := acquire(ctx)
		task.guard = release
		task.done <- err
	}()
	return task
}

// within is whether the acquisition finished within limit (Rust's
// `timeout(limit, &mut task)`), with its result.
func (a *acquiring) within(limit time.Duration) (bool, error) {
	select {
	case err := <-a.done:
		return true, err
	case <-time.After(limit):
		return false, nil
	}
}

func exclusive(path string) func(ctx context.Context) (func(), error) {
	return func(ctx context.Context) (func(), error) {
		guard, err := acquireStartGuard(ctx, path)
		if err != nil {
			return nil, err
		}
		return guard.release, nil
	}
}

func shared(path string) func(ctx context.Context) (func(), error) {
	return func(ctx context.Context) (func(), error) {
		guard, err := acquireSharedStartGuard(ctx, path)
		if err != nil {
			return nil, err
		}
		return guard.release, nil
	}
}

// Hard-won: ab8be9d0a: stop decided the daemon was absent while a launcher still held the startup lock.
func TestStoppingAnAbsentDaemonWaitsForALaunchInProgress(t *testing.T) {
	if rerunInIsolatedChild(t, "MJ_TEST_STOP_WAITS_FOR_DAEMON_LAUNCH") {
		return
	}
	startup, err := acquireStartGuard(context.Background(), filepath.Join(core.DataDir(), "daemon-start.lock"))
	check(t, err)
	if pathExists(client.MetadataPath()) {
		t.Fatal("this isolated store starts without a daemon")
	}
	stopping := startAcquiring(func(ctx context.Context) (func(), error) {
		_, err := stopInstance(ctx, false)
		return func() {}, err
	})
	if finished, _ := stopping.within(150 * time.Millisecond); finished {
		t.Fatal("stop must wait until the launcher finishes before deciding whether the daemon is absent")
	}
	stopping.cancel()
	<-stopping.done
	startup.release()
	stopped, err := stopInstance(context.Background(), false)
	check(t, err)
	if stopped {
		t.Fatal("stopping an absent daemon reported a stopped one")
	}
}

// Hard-won: #1568: readiness dropped a slow Ping before it could return Pong.
func TestStartupAnnouncesWhileAPingIsStillPending(t *testing.T) {
	listener, metadata := fakeDaemon(t, uint32(os.Getpid()))
	defer listener.Close()
	var announced atomic.Int64
	announced.Store(-1)
	wait := runBeside(t, func() {
		stream, err := listener.Accept()
		if err != nil {
			t.Errorf("accept: %v", err)
			return
		}
		defer stream.Close()
		ping, ok := readRequest(t, stream)
		if !ok {
			return
		}
		if _, isPing := ping.Action.(client.DaemonActionPing); !isPing {
			t.Errorf("expected Ping, got %#v", ping.Action)
		}
		time.Sleep(startNoticeDelay + time.Second)
		announcedAt := time.Duration(announced.Load())
		if announcedAt < 0 {
			t.Errorf("no startup notice while Ping is pending")
		} else if announcedAt < startNoticeDelay || announcedAt >= startNoticeDelay+client.RetryDelay {
			t.Errorf("announced at %s", announcedAt)
		}
		answer(t, stream, ping, client.DaemonOk[client.DaemonReply](client.DaemonReplyPong{}))
	})
	outcome := waitForReadyDaemon(context.Background(),
		func(context.Context) bool { return true },
		func(ctx context.Context) (struct{}, error) {
			daemon, err := client.DaemonClientConnect(ctx, metadata)
			if err != nil {
				return struct{}{}, err
			}
			defer daemon.Close()
			return struct{}{}, pingDaemon(ctx, daemon)
		},
		func(waited time.Duration) { announced.Store(int64(waited)) },
	)
	wait(time.Minute)
	if outcome.kind != startupOutcomeReady {
		t.Fatalf("outcome = %+v", outcome)
	}
}

// Hard-won: #1568: a slow blocker query held up startup lock retries.
func TestStartupLockRetriesAndAbortsItsPendingBlockerQuery(t *testing.T) {
	if rerunInIsolatedChild(t, "MJ_TEST_PENDING_STARTUP_BLOCKERS") {
		return
	}
	ctx := context.Background()
	path := filepath.Join(core.DataDir(), "daemon-start.lock")
	held, err := acquireStartGuard(ctx, path)
	check(t, err)
	listener, metadata := fakeDaemon(t, uint32(os.Getpid()))
	defer listener.Close()
	writeMetadata(t, &metadata)
	route, notices := openStartupNoticeRoute()
	defer route.Close()
	type acquired struct {
		lock *core.FileLock
		err  error
	}
	waiting := make(chan acquired, 1)
	go func() {
		lock, err := acquireStartLock(ctx, path, false)
		waiting <- acquired{lock: lock, err: err}
	}()
	accepted := make(chan net.Conn, 1)
	go func() {
		stream, err := listener.Accept()
		if err != nil {
			t.Errorf("accept: %v", err)
			return
		}
		accepted <- stream
	}()
	var stream net.Conn
	select {
	case stream = <-accepted:
	case <-time.After(startNoticeDelay + time.Second):
		t.Fatal("the blocker query did not start")
	}
	defer stream.Close()
	request, err := client.ReadFrame[client.RequestEnvelope](stream)
	check(t, err)
	if _, blockers := request.Action.(client.DaemonActionUpgradeBlockers); !blockers {
		t.Fatalf("expected UpgradeBlockers, got %#v", request.Action)
	}
	noticeCtx, cancel := context.WithTimeout(ctx, blockerPrewarmLead+time.Second)
	notice, ok := notices.Recv(noticeCtx)
	cancel()
	if !ok {
		t.Fatal("no notice while the startup lock was held")
	}
	if !strings.Contains(notice, "another client") {
		t.Fatalf("%s", notice)
	}
	held.release()

	// The peer never replies. Acquiring the lock must end the wait and
	// close the blocker connection before the old two-second RPC cap.
	select {
	case result := <-waiting:
		check(t, result.err)
		releaseStartLock(&result.lock)
	case <-time.After(time.Second):
		t.Fatal("the waiter did not take the released lock")
	}
	expectClosed(t, stream, time.Second)
}

// Hard-won: #1568: a post-ack timeout sent the client back to PrepareUpgrade.
func TestHandoffReleaseTimeoutDoesNotAskForAdmissionAgain(t *testing.T) {
	if rerunInIsolatedChild(t, "MJ_TEST_HANDOFF_RELEASE_TIMEOUT") {
		return
	}
	held, err := controller.ControllerStoreGuardAcquire()
	check(t, err)
	defer held.Drop()
	listener, metadata := fakeDaemon(t, uint32(os.Getpid()))
	defer listener.Close()
	writeMetadata(t, &metadata)
	wait := runBeside(t, func() {
		stream, err := listener.Accept()
		if err != nil {
			t.Errorf("accept: %v", err)
			return
		}
		defer stream.Close()
		request, ok := readRequest(t, stream)
		if !ok {
			return
		}
		if _, prepare := request.Action.(client.DaemonActionPrepareUpgrade); !prepare {
			t.Errorf("expected PrepareUpgrade, got %#v", request.Action)
		}
		// A partially written replacement record does not authorize moving
		// on while the daemon still holds the store.
		if err := os.WriteFile(client.MetadataPath(), []byte("{"), 0o600); err != nil {
			t.Errorf("write the partial record: %v", err)
		}
		time.Sleep(6 * time.Second)
		answer(t, stream, request, client.DaemonOk[client.DaemonReply](client.DaemonReplyDone{}))
	})
	bounded, cancel := context.WithTimeout(context.Background(), client.StopTimeout+10*time.Second)
	err = handOffDaemon(bounded, &metadata)
	cancel()
	if err == nil || !strings.Contains(err.Error(), "accepted the handoff") {
		t.Fatalf("unexpected error: %v", err)
	}
	wait(time.Second)
	expectNoConnection(t, listener, 100*time.Millisecond)
}

func TestHandoffWaitsForStoreReleaseWhenPeerDropsDuringShutdown(t *testing.T) {
	if rerunInIsolatedChild(t, "MJ_TEST_HANDOFF_PEER_DROP") {
		return
	}
	held, err := controller.ControllerStoreGuardAcquire()
	check(t, err)
	listener, metadata := fakeDaemon(t, uint32(os.Getpid()))
	writeMetadata(t, &metadata)
	wait := runBeside(t, func() {
		stream, err := listener.Accept()
		if err != nil {
			t.Errorf("accept: %v", err)
			return
		}
		request, ok := readRequest(t, stream)
		if ok {
			if _, prepare := request.Action.(client.DaemonActionPrepareUpgrade); !prepare {
				t.Errorf("expected PrepareUpgrade, got %#v", request.Action)
			}
		}
		// Drop socket and listener first, then release the store guard
		// shortly afterward, matching real teardown ordering.
		stream.Close()
		listener.Close()
		time.Sleep(300 * time.Millisecond)
		held.Drop()
	})
	bounded, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	err = handOffDaemon(bounded, &metadata)
	cancel()
	check(t, err)
	wait(time.Second)
}

func testDaemonIdentity(buildVersion string) client.DaemonIdentity {
	return client.DaemonIdentity{
		InstanceID:   "test-instance",
		BuildVersion: buildVersion,
		Placement:    client.DaemonPlacement{Kind: client.DaemonPlacementKindHost},
	}
}

// Hard-won: #1568: a draining daemon left the keep-alive displaying Attached.
func TestAttachmentReportsUpgradeRefusalWithinTheSameRefresh(t *testing.T) {
	if rerunInIsolatedChild(t, "MJ_TEST_ATTACHMENT_UPGRADE_REFUSAL") {
		return
	}
	listener, metadata := fakeDaemon(t, uint32(os.Getpid()))
	defer listener.Close()
	writeMetadata(t, &metadata)
	cancellation, cancel := context.WithCancel(context.Background())
	attachment := MaintainAttachment(cancellation, "display-client")
	// The whole exchange has four seconds, as Rust's timeout around it.
	deadline := time.Now().Add(4 * time.Second)
	tcp := listener.(*net.TCPListener)
	check(t, tcp.SetDeadline(deadline))
	stream, err := listener.Accept()
	check(t, err)
	defer stream.Close()
	check(t, stream.SetDeadline(deadline))
	request, err := client.ReadFrame[client.RequestEnvelope](stream)
	check(t, err)
	if _, identity := request.Action.(client.DaemonActionIdentity); !identity {
		t.Fatalf("expected Identity, got %#v", request.Action)
	}
	answer(t, stream, request, client.DaemonOk[client.DaemonReply](client.DaemonReplyIdentity{Identity: testDaemonIdentity(metadata.BuildVersion)}))

	attach, err := listener.Accept()
	check(t, err)
	defer attach.Close()
	check(t, attach.SetDeadline(deadline))
	request, err = client.ReadFrame[client.RequestEnvelope](attach)
	check(t, err)
	if action, ok := request.Action.(client.DaemonActionAttach); !ok || action.ClientID != "display-client" {
		t.Fatalf("expected Attach for display-client, got %#v", request.Action)
	}
	changed := attachment.Presence.Changed()
	select {
	case <-changed:
		t.Fatalf("presence changed while Attach was pending: %#v", attachment.Presence.Borrow())
	case <-time.After(2100 * time.Millisecond):
	}
	if presence := attachment.Presence.Borrow(); presence != (DaemonPresenceAttached{}) {
		t.Fatalf("presence = %#v", presence)
	}
	answer(t, attach, request, client.DaemonOk[client.DaemonReply](client.DaemonReplyUpgradePending{}))
	select {
	case <-changed:
	case <-time.After(time.Until(deadline)):
		t.Fatal("the keep-alive did not report the refusal within 4s")
	}
	if presence := attachment.Presence.Borrow(); presence != (DaemonPresenceMissing{Reason: "daemon upgrade handoff is in progress"}) {
		t.Fatalf("presence = %#v", presence)
	}
	cancel()
	_, err = attachment.Task.Join(context.Background())
	check(t, err)
}

// Hard-won: #1568: a pid-zero record with an unavailable Identity was treated as a host daemon.
func TestPidZeroIsContainerPlacementWhenIdentityFails(t *testing.T) {
	listener, metadata := fakeDaemon(t, 0)
	defer listener.Close()
	metadata.ProtocolVersion--
	wait := runBeside(t, func() {
		stream, err := listener.Accept()
		if err != nil {
			t.Errorf("accept: %v", err)
			return
		}
		defer stream.Close()
		request, ok := readRequest(t, stream)
		if !ok {
			return
		}
		if _, identity := request.Action.(client.DaemonActionIdentity); !identity {
			t.Errorf("expected Identity, got %#v", request.Action)
		}
		answer(t, stream, request, client.DaemonErr[client.DaemonReply]("identity unavailable"))
	})
	identity := daemonIdentityOrLegacy(context.Background(), &metadata)
	wait(10 * time.Second)
	if _, inContainer := identity.Placement.Container(); !inContainer || identity.Placement.Runtime != nil {
		t.Fatalf("placement = %+v", identity.Placement)
	}
}

// Hard-won: #1568: startup ignored corruption even when the daemon owned the store.
func TestACorruptDaemonRecordIsAbsentOnlyWithAFreeHostStore(t *testing.T) {
	if rerunInIsolatedChild(t, "MJ_TEST_CORRUPT_DAEMON_RECORD") {
		return
	}
	ctx := context.Background()
	check(t, os.WriteFile(client.MetadataPath(), []byte("{"), 0o600))
	held, err := controller.ControllerStoreGuardAcquire()
	check(t, err)
	if _, err := decideExistingDaemon(ctx); err == nil || !strings.Contains(err.Error(), "daemon.json") {
		t.Fatalf("startup accepted a corrupt daemon.json while the host store was held: %v", err)
	}
	held.Drop()
	existing, err := decideExistingDaemon(ctx)
	check(t, err)
	if _, absent := existing.(existingDaemonAbsent); !absent {
		t.Fatalf("existing = %#v", existing)
	}
}

// Hard-won: #1566: startup guessed a legacy identity after abandoning a supported daemon's request.
func TestStartupPropagatesIdentityFailureInsteadOfGuessingPlacement(t *testing.T) {
	if rerunInIsolatedChild(t, "MJ_TEST_STARTUP_IDENTITY_FAILURE") {
		return
	}
	for _, buildVersion := range []string{core.PackageVersion, "0.0.1"} {
		listener, err := net.Listen("tcp", "127.0.0.1:0")
		check(t, err)
		metadata := client.DaemonMetadata{
			ProtocolVersion: client.ProtocolVersion,
			Pid:             uint32(os.Getpid()),
			Address:         listenerAddress(listener),
			Token:           "isolated-identity-failure",
			StartedAt:       "test",
			BuildVersion:    buildVersion,
			Placement:       client.DaemonPlacement{Kind: client.DaemonPlacementKindHost},
		}
		writeMetadata(t, &metadata)
		wait := runBeside(t, func() {
			stream, err := listener.Accept()
			if err != nil {
				t.Errorf("accept: %v", err)
				return
			}
			defer stream.Close()
			ping, ok := readRequest(t, stream)
			if !ok {
				return
			}
			if _, isPing := ping.Action.(client.DaemonActionPing); !isPing {
				t.Errorf("expected Ping, got %#v", ping.Action)
			}
			answer(t, stream, ping, client.DaemonOk[client.DaemonReply](client.DaemonReplyPong{}))
			identity, ok := readRequest(t, stream)
			if !ok {
				return
			}
			if _, isIdentity := identity.Action.(client.DaemonActionIdentity); !isIdentity {
				t.Errorf("expected Identity, got %#v", identity.Action)
			}
			answer(t, stream, identity, client.DaemonErr[client.DaemonReply]("identity unavailable"))
			expectClosed(t, stream, 10*time.Second)
		})
		existing, err := decideExistingDaemon(context.Background())
		if err == nil {
			t.Fatalf("startup used a daemon without its identity: %#v", existing)
		}
		if !strings.Contains(err.Error(), "identity unavailable") {
			t.Fatalf("%v", err)
		}
		wait(10 * time.Second)
		listener.Close()
	}
}

// Hard-won: #1566: guessed RPC deadlines blocked startup and same-protocol upgrades under load.
func TestStartupWaitsForSlowIdentityBeforeReplacingAnOlderRelease(t *testing.T) {
	if rerunInIsolatedChild(t, "MJ_TEST_SLOW_STARTUP_IDENTITY") {
		return
	}
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	check(t, err)
	defer listener.Close()
	metadata := client.DaemonMetadata{
		ProtocolVersion: client.ProtocolVersion,
		Pid:             uint32(os.Getpid()),
		Address:         listenerAddress(listener),
		Token:           "isolated-slow-identity",
		StartedAt:       "test",
		BuildVersion:    "0.0.1",
		Placement:       client.DaemonPlacement{Kind: client.DaemonPlacementKindHost},
	}
	writeMetadata(t, &metadata)
	received := make(chan chan struct{}, 1)
	wait := runBeside(t, func() {
		stream, err := listener.Accept()
		if err != nil {
			t.Errorf("accept: %v", err)
			return
		}
		defer stream.Close()
		for _, isIdentity := range []bool{false, true} {
			request, ok := readRequest(t, stream)
			if !ok {
				return
			}
			_, identityAction := request.Action.(client.DaemonActionIdentity)
			_, pingAction := request.Action.(client.DaemonActionPing)
			if isIdentity && !identityAction || !isIdentity && !pingAction {
				t.Errorf("unexpected action %#v", request.Action)
			}
			var reply client.DaemonReply = client.DaemonReplyPong{}
			if isIdentity {
				release := make(chan struct{})
				received <- release
				<-release
				reply = client.DaemonReplyIdentity{Identity: client.DaemonIdentity{
					InstanceID:   "isolated-slow-identity",
					BuildVersion: "0.0.1",
					// Avoid a host store check: this peer represents a
					// container daemon with its own volume and store.
					Placement: client.DaemonPlacement{Kind: client.DaemonPlacementKindContainer, Name: "isolated-slow-identity"},
				}}
			}
			answer(t, stream, request, client.DaemonOk(reply))
		}
	})
	type decided struct {
		existing existingDaemon
		err      error
	}
	decision := make(chan decided, 1)
	go func() {
		existing, err := decideExistingDaemon(context.Background())
		decision <- decided{existing: existing, err: err}
	}()
	var release chan struct{}
	select {
	case result := <-decision:
		t.Fatalf("startup finished before Identity replied: %v", result.err)
	case release = <-received:
	}
	// Wait past the old deadline before releasing Identity's reply.
	time.Sleep(3 * time.Second)
	select {
	case result := <-decision:
		t.Fatalf("startup abandoned its pending Identity request: %#v, %v", result.existing, result.err)
	default:
	}
	close(release)
	result := <-decision
	check(t, result.err)
	if _, replace := result.existing.(existingDaemonReplace); !replace {
		t.Fatalf("existing = %#v", result.existing)
	}
	wait(10 * time.Second)
}

// Hard-won: 413864386: the startup splash displayed frozen blocker ages for thirty seconds.
func TestSplashRefreshesBlockersEachSecondForReplacingAndQueuedClients(t *testing.T) {
	if rerunInIsolatedChild(t, "MJ_TEST_LIVE_HANDOFF_NOTICES") {
		return
	}
	// A live daemon owns the store; the handoff treats a free store as a
	// daemon that has gone.
	store, err := controller.ControllerStoreGuardAcquire()
	check(t, err)
	defer store.Drop()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	check(t, err)
	metadata := client.DaemonMetadata{
		ProtocolVersion: client.ProtocolVersion,
		Pid:             uint32(os.Getpid()),
		Address:         listenerAddress(listener),
		Token:           "isolated-notices",
		StartedAt:       "test",
		BuildVersion:    core.PackageVersion,
		Placement:       client.DaemonPlacement{Kind: client.DaemonPlacementKindHost},
	}
	writeMetadata(t, &metadata)
	// Cancelling a waiter can close either its admission exchange or its
	// background blocker query while the fake daemon is replying.
	disconnected := func(err error) bool {
		return errors.Is(err, io.EOF) || errors.Is(err, io.ErrUnexpectedEOF) ||
			errors.Is(err, syscall.EPIPE) || errors.Is(err, syscall.ECONNRESET)
	}
	serving := make(chan struct{})
	go func() {
		defer close(serving)
		age := 0
		for {
			stream, err := listener.Accept()
			if err != nil {
				// The listener closed: the test is over.
				return
			}
			request, err := client.ReadFrame[client.RequestEnvelope](stream)
			if err != nil {
				stream.Close()
				if !disconnected(err) {
					t.Errorf("read fake daemon request: %v", err)
				}
				continue
			}
			var reply client.DaemonReply
			switch request.Action.(type) {
			case client.DaemonActionIdentity:
				reply = client.DaemonReplyIdentity{Identity: client.DaemonIdentity{
					InstanceID:   "isolated-notices",
					BuildVersion: core.PackageVersion,
					Placement:    client.DaemonPlacement{Kind: client.DaemonPlacementKindHost},
				}}
			case client.DaemonActionPrepareUpgrade:
				reply = client.DaemonReplyUpgradePending{}
			case client.DaemonActionUpgradeBlockers:
				age++
				reply = client.DaemonReplyUpgradeBlockers{Blockers: []string{fmt.Sprintf("session lifecycle (%ds)", age)}}
			default:
				t.Errorf("unexpected management action %#v", request.Action)
				stream.Close()
				continue
			}
			if err := client.WriteFrame(stream, client.ResponseEnvelope{
				ProtocolVersion: request.ProtocolVersion,
				RequestID:       request.RequestID,
				Result:          client.DaemonOk(reply),
			}); err != nil && !disconnected(err) {
				t.Errorf("write fake daemon reply: %v", err)
			}
			stream.Close()
		}
	}()
	route, notices := openStartupNoticeRoute()
	defer route.Close()
	path := filepath.Join(core.DataDir(), "daemon-start.lock")
	held, err := acquireStartGuard(context.Background(), path)
	check(t, err)
	for _, queued := range []bool{false, true} {
		waitingCtx, abort := context.WithCancel(context.Background())
		waiting := make(chan error, 1)
		go func() {
			if queued {
				guard, err := acquireSharedStartGuard(waitingCtx, path)
				if err == nil {
					guard.release()
				}
				waiting <- err
				return
			}
			waiting <- handOffDaemon(waitingCtx, &metadata)
		}()
		receive := func(limit time.Duration, missing string) string {
			t.Helper()
			ctx, cancel := context.WithTimeout(context.Background(), limit)
			defer cancel()
			notice, ok := notices.Recv(ctx)
			if !ok {
				t.Fatal(missing)
			}
			return notice
		}
		first := receive(15*time.Second, "no first notice")
		second := receive(3*time.Second, "the splash age must refresh without waiting thirty seconds")
		if !strings.Contains(first, "session lifecycle (") {
			t.Fatalf("%s", first)
		}
		if !strings.Contains(second, "session lifecycle (") {
			t.Fatalf("%s", second)
		}
		if first == second {
			t.Fatalf("a fresh daemon age must reach the splash: %s", first)
		}
		if queued && !strings.Contains(first, "another client") {
			t.Fatalf("%s", first)
		}
		abort()
		if err := <-waiting; !errors.Is(err, context.Canceled) {
			t.Fatalf("the aborted waiter ended with %v", err)
		}
	}
	held.release()
	listener.Close()
	<-serving
}

// viewerFixture is a daemon that answers WebViewerAccess with statuses in
// turn, and once they run out leaves the next request unanswered until the
// client closes the connection.
func viewerFixture(t *testing.T, statuses []client.WebViewerAccess) (*client.DaemonClient, func()) {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	check(t, err)
	metadata := client.DaemonMetadata{
		ProtocolVersion: client.ProtocolVersion,
		Pid:             1,
		Address:         listenerAddress(listener),
		Token:           "test",
		StartedAt:       "test",
		BuildVersion:    core.PackageVersion,
		Placement:       client.DaemonPlacement{Kind: client.DaemonPlacementKindHost},
	}
	done := make(chan struct{})
	go func() {
		defer close(done)
		defer listener.Close()
		stream, err := listener.Accept()
		if err != nil {
			t.Errorf("accept: %v", err)
			return
		}
		defer stream.Close()
		next := 0
		for {
			request, err := client.ReadFrame[client.RequestEnvelope](stream)
			if err != nil {
				return
			}
			if _, access := request.Action.(client.DaemonActionWebViewerAccess); !access {
				t.Errorf("expected WebViewerAccess, got %#v", request.Action)
			}
			if next == len(statuses) {
				// An exhausted script deliberately leaves this request
				// unanswered. The timed-out client closes it; no response
				// write can race with that expected disconnect.
				expectClosed(t, stream, 10*time.Second)
				return
			}
			answer(t, stream, request, client.DaemonOk[client.DaemonReply](client.DaemonReplyWebViewerAccess{Access: statuses[next]}))
			next++
		}
	}()
	daemon, err := client.DaemonClientConnect(context.Background(), metadata)
	check(t, err)
	return daemon, func() {
		t.Helper()
		select {
		case <-done:
		case <-time.After(10 * time.Second):
			t.Fatal("the viewer fixture did not finish")
		}
	}
}

func TestApiWaitsForTheViewerWithoutARetryCommand(t *testing.T) {
	local := listenerAddressOf(t, "127.0.0.1:1234")
	daemon, wait := viewerFixture(t, []client.WebViewerAccess{
		client.WebViewerAccessStarting{},
		client.WebViewerAccessStarting{},
		client.WebViewerAccessReady{
			ViewerURL:    "http://127.0.0.1:1234",
			ViewerCode:   "test",
			LocalAddress: &local,
		},
	})
	access, err := WaitForWebAccess(context.Background(), daemon)
	check(t, err)
	ready, ok := access.(client.WebViewerAccessReady)
	if !ok || ready.ViewerURL != "http://127.0.0.1:1234" {
		t.Fatalf("access = %s", access)
	}
	daemon.Close()
	wait()
}

func TestViewerStartupReportsFailureAndBoundsAStalledStart(t *testing.T) {
	daemon, wait := viewerFixture(t, []client.WebViewerAccess{
		client.WebViewerAccessStarting{},
		client.WebViewerAccessFailed{
			Address: listenerAddressOf(t, "127.0.0.1:1234"),
			Message: "port unavailable",
		},
	})
	if _, err := WaitForWebAccess(context.Background(), daemon); err == nil || !strings.Contains(err.Error(), "port unavailable") {
		t.Fatalf("unexpected error: %v", err)
	}
	daemon.Close()
	wait()
	daemon, wait = viewerFixture(t, nil)
	start := time.Now()
	timeout := 100 * time.Millisecond
	if _, err := waitForWebAccessWithTimeout(context.Background(), daemon, timeout); err == nil || !strings.Contains(err.Error(), "timed out") {
		t.Fatalf("unexpected error: %v", err)
	}
	if elapsed := time.Since(start); elapsed < timeout {
		t.Fatalf("gave up after %s", elapsed)
	}
	daemon.Close()
	wait()
}

func listenerAddressOf(t *testing.T, text string) netip.AddrPort {
	t.Helper()
	address, err := netip.ParseAddrPort(text)
	check(t, err)
	return address
}

// Hard-won: #1524: a session-launched daemon inherited staged profile homes.
func TestHostDaemonEnvironmentRestoresLoginAndPreservesInstance(t *testing.T) {
	const child = "MJ_TEST_DAEMON_ENVIRONMENT_CHILD"
	if mode, ok := os.LookupEnv(child); ok {
		command := exec.Command("/usr/bin/env")
		environment, found, err := hostDaemonLoginEnvironment()
		check(t, err)
		check(t, configureHostDaemonEnvironment(command, environment, found))
		output, err := core.RunWithInput(command, nil)
		check(t, err)
		if !output.Status.Success() {
			t.Fatalf("env failed: %s", output.Stderr)
		}
		seen := map[string]string{}
		for _, line := range strings.Split(string(output.Stdout), "\n") {
			if name, value, found := strings.Cut(line, "="); found {
				seen[name] = value
			}
		}
		directory, err := os.Getwd()
		check(t, err)
		for _, name := range []string{"MJ_CONFIG_DIR", "MJ_DATA_DIR", "MJ_INSTANCE"} {
			expected := os.Getenv(name)
			if name != "MJ_INSTANCE" {
				expected = filepath.Join(directory, expected)
			}
			if got, present := seen[name]; !present || got != expected {
				t.Fatalf("%s = %q (present %t), want %q", name, got, present, expected)
			}
		}
		if mode == "session" {
			for _, name := range []string{"MJ_SESSION_ID", "MJ_SESSION_BASH_ENV", "MJ_PROFILE_CUSTOM", "BASH_ENV"} {
				if _, present := seen[name]; present {
					t.Fatalf("leaked %s", name)
				}
			}
			if seen["CODEX_HOME"] == "/session/profile" || seen["HOME"] == "/session/home" || seen["TMPDIR"] == "/session/tmp" {
				t.Fatalf("the session's overlay reached the daemon: %v", seen)
			}
		} else if seen["CODEX_HOME"] != "/user/custom-codex" {
			t.Fatalf("CODEX_HOME = %q", seen["CODEX_HOME"])
		}
		if command.Dir != filepath.Join(directory, os.Getenv("MJ_DATA_DIR")) {
			t.Fatalf("current dir = %q", command.Dir)
		}
		return
	}
	directory := t.TempDir()
	check(t, os.Mkdir(filepath.Join(directory, "data"), 0o755))
	executable, err := os.Executable()
	check(t, err)
	for _, mode := range []string{"session", "ordinary"} {
		command := exec.Command(executable, "-test.run=^"+t.Name()+"$", "-test.count=1")
		var environment []string
		for _, entry := range os.Environ() {
			name, _, _ := strings.Cut(entry, "=")
			switch name {
			case "MJ_SESSION_ID", "MJ_SESSION_BASH_ENV", "MJ_CONFIG_DIR", "MJ_DATA_DIR", "MJ_INSTANCE", "CODEX_HOME":
				continue
			}
			environment = append(environment, entry)
		}
		environment = append(environment,
			child+"="+mode,
			"MJ_CONFIG_DIR=config",
			"MJ_DATA_DIR=data",
			"MJ_INSTANCE=1524-daemon-env",
			"CODEX_HOME=/user/custom-codex",
		)
		if mode == "session" {
			environment = append(environment,
				"MJ_SESSION_ID=old-worker-session",
				"MJ_SESSION_BASH_ENV=/session/bash-env",
				"MJ_PROFILE_CUSTOM=profile-value",
				"CODEX_HOME=/session/profile",
				"HOME=/session/home",
				"TMPDIR=/session/tmp",
				"BASH_ENV=/session/bash-env",
			)
		}
		command.Env = environment
		command.Dir = directory
		output, err := core.RunWithInput(command, nil)
		check(t, err)
		if !output.Status.Success() {
			t.Fatalf("%s\n%s", output.Stdout, output.Stderr)
		}
	}
}

// Hard-won: 2be31d67: restart stopped the old daemon before discovering the replacement config was invalid.
func TestAConfigurationTheNewDaemonCannotReadRefusesTheReplacement(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "config.toml")
	check(t, os.WriteFile(path, []byte(fmt.Sprintf(
		"version = %d\n\n[profiles.codex]\nkind = \"codex\"\nhome = \"/home/me/.codex\"\n\n"+
			"[profiles.codex.environment]\nFAKE_TOKEN = { from_secret = \"\" }\n",
		core.ConfigVersion)), 0o644))
	err := ensureConfigLoads(path, "restarted")
	if err == nil {
		t.Fatal("a configuration the daemon cannot read was accepted")
	}
	if !strings.Contains(err.Error(), "from_secret names nothing") {
		t.Fatalf("%v", err)
	}
	if !strings.Contains(err.Error(), "was not restarted") {
		t.Fatalf("%v", err)
	}
}

// Hard-won: 871a4d9b: an unusable profile credential prevented a replacement that should leave other profiles available.
func TestAProfileThatCannotStartDoesNotRefuseTheReplacement(t *testing.T) {
	// The reported failure: a Codex home that authenticates with an API key
	// that neither the profile nor the environment supplies, beside a secret
	// that is not defined. Either makes only that profile unusable; the new
	// daemon reads the configuration and keeps every other profile working.
	directory := t.TempDir()
	home := filepath.Join(directory, "codex")
	check(t, os.Mkdir(home, 0o755))
	check(t, os.WriteFile(filepath.Join(home, "config.toml"), []byte(
		"model_provider = 'deepseek'\n[model_providers.deepseek]\nname = 'DeepSeek'\n"+
			"base_url = 'https://api.deepseek.com'\nwire_api = 'responses'\nenv_key = 'MJ_TEST_UNSET_PROVIDER_KEY'\n"), 0o644))
	path := filepath.Join(directory, "config.toml")
	check(t, os.WriteFile(path, []byte(fmt.Sprintf(
		"version = %d\n\n[profiles.codex]\nkind = \"codex\"\nhome = %s\n\n"+
			"[profiles.other]\nkind = \"codex\"\nhome = \"/home/me/.codex\"\n\n"+
			"[profiles.other.environment]\nFAKE_TOKEN = { from_secret = \"FAKE_TOKEN\" }\n",
		core.ConfigVersion, core.RustDebugString(home))), 0o644))
	check(t, ensureConfigLoads(path, "replaced"))
	config, err := core.ConfigLoadFrom(path)
	check(t, err)
	err = config.Profiles["codex"].EnsureReady("codex")
	if err == nil || !strings.Contains(err.Error(), `MJ_TEST_UNSET_PROVIDER_KEY = { from_secret = "MJ_TEST_UNSET_PROVIDER_KEY" }`) {
		t.Fatalf("%v", err)
	}
	err = config.Profiles["other"].EnsureReady("other")
	if err == nil || !strings.Contains(err.Error(), "FAKE_TOKEN = { from_secret") || !strings.Contains(err.Error(), "secrets.toml") {
		t.Fatalf("%v", err)
	}
}

func TestARestartOntoThisBuildSucceedsAndSaysSo(t *testing.T) {
	identity := testDaemonIdentity("2.0.0")
	verdict, err := restartVerdict(4242, &identity, pointer(true))
	check(t, err)
	if verdict.Pid != 4242 || verdict.RunsThisBuild == nil || !*verdict.RunsThisBuild {
		t.Fatalf("verdict = %+v", verdict)
	}
}

func TestARestartThatCannotIdentifyTheDaemonSucceedsWithoutTheGuarantee(t *testing.T) {
	identity := testDaemonIdentity("2.0.0")
	verdict, err := restartVerdict(7, &identity, nil)
	check(t, err)
	if verdict != (RestartedDaemon{Pid: 7, RunsThisBuild: nil}) {
		t.Fatalf("verdict = %+v", verdict)
	}
}

// Hard-won: ef16209e: restart reported success after another client relaunched the old executable.
func TestARestartOntoAnotherBuildFailsAndNamesBothBuilds(t *testing.T) {
	identity := testDaemonIdentity("1.0.0+" + strings.Repeat("b", 40))
	_, err := restartVerdict(99, &identity, pointer(false))
	if err == nil {
		t.Fatal("a restart onto another build succeeded")
	}
	for _, wanted := range []string{"99", "1.0.0+bbbbbbbb", "not this build", "mj daemon restart"} {
		if !strings.Contains(err.Error(), wanted) {
			t.Fatalf("%q lacks %q", err, wanted)
		}
	}
}

// Hard-won: b7ced524: startup was reported failed after eight seconds while the live daemon was still initializing.
func TestSlowStartupIsAwaitedRatherThanReportedAsAFailure(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		var attempts atomic.Int64
		var announced atomic.Bool
		started := time.Now()
		outcome := waitForReadyDaemon(context.Background(),
			func(context.Context) bool { return true },
			func(context.Context) (string, error) {
				attempts.Add(1)
				if time.Since(started) >= 75*time.Second {
					return "client", nil
				}
				return "", errors.New("connection refused")
			},
			func(time.Duration) { announced.Store(true) },
		)
		if outcome.kind != startupOutcomeReady || outcome.ready != "client" {
			t.Fatalf("outcome = %+v", outcome)
		}
		waited := time.Since(started)
		if waited <= startNoticeDelay || waited >= startTimeout {
			t.Fatalf("the test must cross the notice delay without reaching the bound, but waited %s", waited)
		}
		if !announced.Load() {
			t.Fatal("a long wait must say so")
		}
		if waited < 75*time.Second {
			t.Fatalf("a migration may exceed the old 60-second bound: waited %s", waited)
		}
	})
}

func TestFailedLaunchReportsItsDiagnosticLogAndOnlyItsStderr(t *testing.T) {
	directory := t.TempDir()
	logs := filepath.Join(directory, "logs")
	check(t, os.Mkdir(logs, 0o755))
	stderr := filepath.Join(directory, "daemon.log")
	const old = "old launch must not appear\n"
	check(t, os.WriteFile(stderr, []byte(old+"this launch's stderr\n"), 0o644))
	check(t, os.WriteFile(filepath.Join(logs, "mj-daemon-20260930T000000.000Z-8.log"), []byte("another daemon's diagnostics"), 0o644))
	diagnostics := filepath.Join(logs, "mj-daemon-20260930T000000.000Z-7.log")
	check(t, os.WriteFile(diagnostics, []byte(strings.Repeat("startup line\n", 10_000)+"\nERROR invalid project path\n"), 0o644))
	launched := &launchedDaemon{pid: 7, logOffset: uint64(len(old))}
	path, output := launched.outputSinceLaunch(context.Background(), stderr)
	if path != diagnostics {
		t.Fatalf("path = %q", path)
	}
	if !strings.Contains(output, "ERROR invalid project path") || !strings.Contains(output, "this launch's stderr") {
		t.Fatalf("output = %q", output)
	}
	if strings.Contains(output, "old launch") || strings.Contains(output, "another daemon") {
		t.Fatalf("output = %q", output)
	}
	if len(output) >= 1024 {
		t.Fatalf("output is %d bytes", len(output))
	}
	message := launched.failure("startup timed out", output, path).Error()
	if !strings.Contains(message, diagnostics) {
		t.Fatalf("%s", message)
	}
}

func TestALaunchThatIsStillStartingUsesItsLiveChildsLog(t *testing.T) {
	directory := t.TempDir()
	logs := filepath.Join(directory, "logs")
	check(t, os.Mkdir(logs, 0o755))
	stderr := filepath.Join(directory, "daemon.log")
	check(t, os.WriteFile(stderr, nil, 0o644))
	child := exec.Command("sleep", "60")
	check(t, child.Start())
	diagnostics := filepath.Join(logs, fmt.Sprintf("mj-daemon-20260930T000000.000Z-%d.log", child.Process.Pid))
	check(t, os.WriteFile(diagnostics, []byte("recovering store"), 0o644))
	launched := &launchedDaemon{pid: uint32(os.Getpid())}
	path, output := launched.outputSinceLaunch(context.Background(), stderr)
	check(t, child.Process.Kill())
	// Rust's wait().unwrap(): reaping must work; a killed child's status is
	// an ExitError here, not a failure to wait.
	if err := child.Wait(); err != nil {
		var exit *exec.ExitError
		if !errors.As(err, &exit) {
			t.Fatalf("wait for the child: %v", err)
		}
	}
	if path != diagnostics {
		t.Fatalf("path = %q", path)
	}
	if output != "recovering store" {
		t.Fatalf("output = %q", output)
	}
}

func TestAFailureBeforeLoggingStartsReportsDetachedStderr(t *testing.T) {
	directory := t.TempDir()
	stderr := filepath.Join(directory, "daemon.log")
	check(t, os.WriteFile(stderr, []byte("failed before logger initialization\n"), 0o644))
	launched := &launchedDaemon{pid: 7}
	path, output := launched.outputSinceLaunch(context.Background(), stderr)
	if path != stderr {
		t.Fatalf("path = %q", path)
	}
	if output != "failed before logger initialization" {
		t.Fatalf("output = %q", output)
	}
}

func TestADaemonThatExitsIsReportedAtOnce(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		var attempts atomic.Int64
		started := time.Now()
		outcome := waitForReadyDaemon(context.Background(),
			func(context.Context) bool { return attempts.Load() < 3 },
			func(context.Context) (struct{}, error) {
				attempts.Add(1)
				return struct{}{}, errors.New("connection refused")
			},
			func(time.Duration) { t.Error("an exit must not wait long enough to announce itself") },
		)
		if outcome.kind != startupOutcomeExited {
			t.Fatalf("an exited daemon must be reported as exited: %+v", outcome)
		}
		if elapsed := time.Since(started); elapsed >= startNoticeDelay {
			t.Fatalf("waited %s", elapsed)
		}
	})
}

// Hard-won: #1568: a pending probe prevented the startup deadline from firing.
func TestAnAliveDaemonThatNeverAnswersStopsAtTheBound(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		started := time.Now()
		var attempts atomic.Int64
		outcome := waitForReadyDaemon(context.Background(),
			func(context.Context) bool { return true },
			func(ctx context.Context) (struct{}, error) {
				if attempts.Add(1) == 1 {
					return struct{}{}, errors.New("connection refused")
				}
				<-ctx.Done()
				return struct{}{}, ctx.Err()
			},
			func(time.Duration) {},
		)
		if outcome.kind != startupOutcomeStillStarting || !outcome.probePending ||
			outcome.lastError == nil || outcome.lastError.Error() != "connection refused" {
			t.Fatalf("outcome = %+v", outcome)
		}
		if got := attempts.Load(); got != 2 {
			t.Fatalf("attempts = %d", got)
		}
		if elapsed := time.Since(started); elapsed < startTimeout || elapsed >= startTimeout+client.RetryDelay {
			t.Fatalf("stopped after %s", elapsed)
		}
	})
}

func TestDevelopmentRefreshDetectsWorkersInstalledOrRebuiltAfterStartup(t *testing.T) {
	directory := t.TempDir()
	worker := filepath.Join(directory, "mj-worker")
	started := time.Unix(200, 0)
	resolve := func(arch string) (workerstaging.WorkerBinaryAvailability, error) {
		if arch != "aarch64" || !pathExists(worker) {
			return nil, fmt.Errorf("no worker for %s", arch)
		}
		return workerstaging.WorkerBinaryAvailabilityLocal{Path: worker, Source: "test"}, nil
	}
	changed, err := developmentWorkersChangedSince(started, resolve)
	check(t, err)
	if changed {
		t.Fatal("no worker reads as changed")
	}
	check(t, os.WriteFile(worker, []byte("worker"), 0o644))
	check(t, os.Chtimes(worker, time.Time{}, started.Add(-time.Second)))
	changed, err = developmentWorkersChangedSince(started, resolve)
	check(t, err)
	if changed {
		t.Fatal("a worker older than the daemon reads as changed")
	}
	check(t, os.Chtimes(worker, time.Time{}, started.Add(time.Second)))
	changed, err = developmentWorkersChangedSince(started, resolve)
	check(t, err)
	if !changed {
		t.Fatal("a worker rebuilt after the daemon started reads as unchanged")
	}
}
