// Port of the tests in mj-cli/src/dashboard/io/spawn.rs (the
// wiki_search_tests and create_wait_tests modules).

package cli

import (
	"context"
	"errors"
	"fmt"
	"net"
	"os"
	"runtime"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"testing/synctest"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/daemon"
)

// Hard-won: 8f4dcf3: detached read panics stranded pending queries and canceled siblings.
func TestWikiTaskPanicsAreReportedWithoutLosingSiblingsAndCancellationAbortsWork(t *testing.T) {
	ctx := context.Background()
	var (
		brokenErr    error
		siblingValue string
		siblingErr   error
		joined       sync.WaitGroup
	)
	// tokio::join! drives both supervisors at once.
	joined.Add(2)
	go func() {
		defer joined.Done()
		_, brokenErr = supervisedWikiTask(ctx, "archive search", func(context.Context) (struct{}, error) {
			panic("archive read panicked")
		})
	}()
	go func() {
		defer joined.Done()
		siblingValue, siblingErr = supervisedWikiTask(ctx, "live search", func(context.Context) (string, error) {
			runtime.Gosched()
			return "live answer", nil
		})
	}()
	joined.Wait()
	if brokenErr == nil {
		t.Fatal("the panicking read must fail")
	}
	if !strings.Contains(brokenErr.Error(), "archive search task failed") {
		t.Fatalf("failure %q", brokenErr)
	}
	if !strings.Contains(brokenErr.Error(), "archive read panicked") {
		t.Fatalf("failure %q", brokenErr)
	}
	check(t, siblingErr)
	if siblingValue != "live answer" {
		t.Fatalf("sibling %q, want the live answer", siblingValue)
	}

	stopped := make(chan struct{})
	ready := make(chan struct{})
	supervisorCtx, abort := context.WithCancel(ctx)
	supervisor := core.Spawn(func() error {
		_, err := supervisedWikiTask(supervisorCtx, "pending read", func(ctx context.Context) (struct{}, error) {
			defer close(stopped)
			close(ready)
			<-ctx.Done()
			return struct{}{}, ctx.Err()
		})
		return err
	})
	<-ready
	abort()
	result, joinErr := supervisor.Join(ctx)
	check(t, joinErr)
	if !errors.Is(result, context.Canceled) {
		t.Fatalf("supervisor %v, want cancelled", result)
	}
	select {
	case <-stopped:
	case <-time.After(5 * time.Second):
		t.Fatal("dropping supervision must abort the pending read")
	}
}

func TestCancellingAQueryDuringDebounceStopsItsBackgroundTask(t *testing.T) {
	// A task that outlives its cancellation would reach a daemon; the
	// isolated instance keeps such a failure away from live sessions.
	if rerunInIsolatedChild(t, "MJ_TEST_DEBOUNCE_CANCELLATION") {
		return
	}
	synctest.Test(t, func(t *testing.T) {
		updates := core.NewUnbounded[dashboardIoUpdate]()
		query := spawnWikiSearch(17, "quokka", 60*time.Second, updates)
		synctest.Wait()
		query()
		// The paused clock runs past the debounce: a task still waiting on
		// it would wake and answer.
		time.Sleep(2 * time.Minute)
		synctest.Wait()
		if update, ok, _ := updates.TryRecv(); ok {
			t.Fatalf("a cancelled search answered: %#v", update)
		}
		updates = core.NewUnbounded[dashboardIoUpdate]()
		query = spawnSessionsTextSearch(18, "quokka", updates)
		synctest.Wait()
		query()
		time.Sleep(2 * time.Minute)
		synctest.Wait()
		if update, ok, _ := updates.TryRecv(); ok {
			t.Fatalf("a cancelled search answered: %#v", update)
		}
	})
}

func TestWikiSyncEpisodeStopsOnWatchLossRefusalOrEpochChange(t *testing.T) {
	if rerunInIsolatedChild(t, "MJ_TEST_WIKI_SYNC_EPISODE") {
		return
	}
	for _, failure := range []string{"loss", "refusal", "epoch"} {
		listener, err := net.Listen("tcp", "127.0.0.1:0")
		check(t, err)
		metadata := client.DaemonMetadata{
			ProtocolVersion: client.ProtocolVersion,
			Pid:             uint32(os.Getpid()),
			Address:         listenerAddress(listener),
			Token:           "wiki-sync-test",
			StartedAt:       "test",
			BuildVersion:    core.PackageVersion,
			InstanceID:      "swsync",
			Placement:       client.DaemonPlacement{Kind: client.DaemonPlacementKindHost},
		}
		served := make(chan error, 1)
		go func() {
			served <- func() error {
				socket, err := listener.Accept()
				if err != nil {
					return err
				}
				defer socket.Close()
				for step := range 2 {
					request, err := client.ReadFrame[client.RequestEnvelope](socket)
					if err != nil {
						return err
					}
					if step == 0 {
						if _, ok := request.Action.(client.DaemonActionWikiSyncRequest); !ok {
							return fmt.Errorf("%s: first request %#v, want a sync request", failure, request.Action)
						}
					} else {
						watch, ok := request.Action.(client.DaemonActionWikiSyncWatch)
						if !ok || watch.Epoch != "original" || watch.After != 1 {
							return fmt.Errorf("%s: second request %#v, want a watch of the original epoch after 1", failure, request.Action)
						}
						if failure == "loss" {
							return nil
						}
					}
					var reply client.DaemonReply
					if step == 1 && failure == "refusal" {
						reply = client.DaemonReplyUpgradePending{}
					} else {
						epoch := "original"
						if step == 1 {
							epoch = "replacement"
						}
						target := uint64(2)
						reply = client.DaemonReplyWikiSyncStatus{Status: client.WikiSyncStatus{
							Epoch:     epoch,
							Version:   1,
							Started:   1,
							Requested: 2,
							Target:    &target,
						}}
					}
					if err := client.WriteFrame(socket, client.ResponseEnvelope{
						ProtocolVersion: request.ProtocolVersion,
						RequestID:       request.RequestID,
						Result:          client.DaemonOk(reply),
					}); err != nil {
						return err
					}
				}
				return nil
			}()
		}()
		daemonClient, err := client.DaemonClientConnect(context.Background(), metadata)
		check(t, err)
		var states []client.WikiSyncStatus
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		result := followWikiSync(ctx, daemonClient, func(state client.WikiSyncStatus) {
			states = append(states, state)
		})
		timedOut := ctx.Err() != nil
		cancel()
		daemonClient.Close()
		if timedOut {
			t.Fatalf("%s: a failed episode must stop without reconnecting", failure)
		}
		if result == nil {
			t.Fatalf("%s: the episode must fail", failure)
		}
		if len(states) != 1 {
			t.Fatalf("%s: replacement state must not reach the surface: %d states", failure, len(states))
		}
		check(t, <-served)
		listener.Close()
	}
}

func createWaitMetadata(listener net.Listener) client.DaemonMetadata {
	return client.DaemonMetadata{
		ProtocolVersion: client.ProtocolVersion,
		Pid:             uint32(os.Getpid()),
		Address:         listenerAddress(listener),
		Token:           "create-wait-test",
		StartedAt:       "test",
		BuildVersion:    core.PackageVersion,
		InstanceID:      "",
		Placement:       client.DaemonPlacement{Kind: client.DaemonPlacementKindHost},
	}
}

// createWaitServe is a fake daemon that serves one scripted answer per
// connection: nil reads the request and drops the connection. The returned
// function waits for the script to finish.
func createWaitServe(t *testing.T, script []*client.DaemonResult[client.DaemonReply]) (client.DaemonMetadata, func()) {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	check(t, err)
	t.Cleanup(func() { listener.Close() })
	metadata := createWaitMetadata(listener)
	served := make(chan error, 1)
	go func() {
		served <- func() error {
			for _, answer := range script {
				stream, err := listener.Accept()
				if err != nil {
					return err
				}
				request, err := client.ReadFrame[client.RequestEnvelope](stream)
				if err != nil {
					stream.Close()
					return err
				}
				if _, ok := request.Action.(client.DaemonActionWaitCreateSession); !ok {
					stream.Close()
					return fmt.Errorf("request %#v, want a create wait", request.Action)
				}
				if answer != nil {
					if err := client.WriteFrame(stream, client.ResponseEnvelope{
						ProtocolVersion: request.ProtocolVersion,
						RequestID:       request.RequestID,
						Result:          *answer,
					}); err != nil {
						stream.Close()
						return err
					}
				}
				stream.Close()
			}
			return nil
		}()
	}()
	return metadata, func() {
		t.Helper()
		check(t, <-served)
	}
}

// Dropped connections, then the answer: the wait reconnects each time and
// ends Finished.
func TestTheCreateWaitReconnectsAfterADroppedConnection(t *testing.T) {
	done := client.DaemonOk[client.DaemonReply](client.DaemonReplyDone{})
	metadata, server := createWaitServe(t, []*client.DaemonResult[client.DaemonReply]{nil, nil, &done})
	cancelled := new(atomic.Bool)
	outcome, err := followCreateWait(context.Background(), "session-1", cancelled, func(ctx context.Context) (*client.DaemonClient, error) {
		return client.DaemonClientConnect(ctx, metadata)
	})
	check(t, err)
	if _, ok := outcome.(daemon.LifecycleSuccessCreated); !ok {
		t.Fatalf("outcome %#v, want Created", outcome)
	}
	server()
}

func TestARefusalEndsTheCreateWaitWithItsReason(t *testing.T) {
	refused := client.DaemonErr[client.DaemonReply]("clone refused")
	metadata, server := createWaitServe(t, []*client.DaemonResult[client.DaemonReply]{&refused})
	cancelled := new(atomic.Bool)
	_, err := followCreateWait(context.Background(), "session-1", cancelled, func(ctx context.Context) (*client.DaemonClient, error) {
		return client.DaemonClientConnect(ctx, metadata)
	})
	if err == nil {
		t.Fatal("a refusal ends the wait")
	}
	if !strings.Contains(err.Error(), "clone refused") {
		t.Fatalf("error %q", err)
	}
	server()
}
