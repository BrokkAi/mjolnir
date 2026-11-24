// Port of mj-cli/src/dashboard/frame_pacer.rs tests.
//
// Rust's start_paused tests run on tokio's paused clock; these run in a
// synctest bubble, whose clock advances only when every goroutine in it
// waits, as the paused clock does.

package cli

import (
	"context"
	"reflect"
	"slices"
	"testing"
	"testing/synctest"
	"time"
)

// pacerFrame is what one frame showed: how many feed messages were applied,
// and the text typed.
type pacerFrame struct {
	applied int
	typed   string
}

// pacerModel is the dashboard loop in miniature: input first, then a
// deferred frame, then the paced feed. A wakeup that was not input drains
// the feed, and the pacer decides whether the next pass draws.
//
// The feed and the keys are channels with room for every message a test
// sends, standing in for unbounded mpsc channels; closing the feed is
// dropping its sender.
type pacerModel struct {
	pacer framePacer
	feed  chan uint32
	keys  chan rune
	// carried is a feed message a cancelled wait had already taken. It is
	// still the oldest message, so it comes before the channel's.
	carried []uint32
	applied []uint32
	typed   string
	frames  []pacerFrame
	wakeups int
}

func newPacerModel(feed chan uint32, keys chan rune) *pacerModel {
	return &pacerModel{pacer: newFramePacer(frameInterval), feed: feed, keys: keys}
}

func (m *pacerModel) frame() {
	now := time.Now()
	if m.pacer.shouldDraw(now) {
		m.frames = append(m.frames, pacerFrame{applied: len(m.applied), typed: m.typed})
		m.pacer.drew(now, now)
	}
}

// pacedFeedResult is what one paced receive answered: a message, or that
// the feed closed (open false), or that it was cancelled (err).
type pacedFeedResult struct {
	message uint32
	open    bool
	err     error
}

// receive is the feed's cancel-safe recv: a carried message first.
func (m *pacerModel) receive(ctx context.Context) pacedFeedResult {
	if len(m.carried) > 0 {
		message := m.carried[0]
		m.carried = m.carried[1:]
		return pacedFeedResult{message: message, open: true}
	}
	select {
	case message, open := <-m.feed:
		return pacedFeedResult{message: message, open: open}
	case <-ctx.Done():
		return pacedFeedResult{err: ctx.Err()}
	}
}

// wake is one wakeup; false once the feed has closed.
func (m *pacerModel) wake() bool {
	now := time.Now()
	gate := m.pacer.feedGate(now)
	deferred := m.pacer.deferredFrame(now)
	before := len(m.applied)
	// Biased: input is taken before anything else that is ready.
	select {
	case key := <-m.keys:
		m.acceptKey(key)
		return true
	default:
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	fed := make(chan pacedFeedResult, 1)
	go func() {
		result, err := paced(ctx, gate, m.receive)
		if err != nil {
			result = pacedFeedResult{err: err}
		}
		fed <- result
	}()
	// stopFeed cancels the paced receive; a message it had already taken
	// stays first in line.
	stopFeed := func() {
		cancel()
		if result := <-fed; result.err == nil && result.open {
			m.carried = append([]uint32{result.message}, m.carried...)
		}
	}
	var deferredFrame <-chan time.Time
	if deferred != nil {
		timer := time.NewTimer(time.Until(*deferred))
		defer timer.Stop()
		deferredFrame = timer.C
	}
	select {
	case key := <-m.keys:
		stopFeed()
		m.acceptKey(key)
		return true
	case <-deferredFrame:
		stopFeed()
	case result := <-fed:
		if result.err == nil && !result.open {
			return false
		}
		m.applied = append(m.applied, result.message)
	}
	m.wakeups++
	m.applied = append(m.applied, m.carried...)
	m.carried = nil
drain:
	for {
		select {
		case message, open := <-m.feed:
			if !open {
				break drain
			}
			m.applied = append(m.applied, message)
		default:
			break drain
		}
	}
	if len(m.applied) > before {
		m.pacer.mark()
	}
	return true
}

func (m *pacerModel) acceptKey(key rune) {
	m.wakeups++
	m.typed += string(key)
	m.pacer.markUrgent()
}

func TestABurstOfFeedUpdatesSharesPacedFrames(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		feed := make(chan uint32, 256)
		keys := make(chan rune, 16)
		model := newPacerModel(feed, keys)
		// 200 updates, one every millisecond: many more than a 60 Hz
		// display can show.
		producerDone := make(chan struct{})
		go func() {
			defer close(producerDone)
			defer close(feed)
			for message := range uint32(200) {
				feed <- message
				time.Sleep(time.Millisecond)
			}
		}()
		for {
			model.frame()
			if !model.wake() {
				break
			}
		}
		model.frame()
		<-producerDone

		want := make([]uint32, 0, 200)
		for message := range uint32(200) {
			want = append(want, message)
		}
		if !slices.Equal(model.applied, want) {
			t.Fatalf("applied %v", model.applied)
		}
		// 200 ms of updates at one frame per interval, plus the first frame
		// and the one that shows the last update.
		bound := 200/int(frameInterval/time.Millisecond) + 2
		if len(model.frames) > bound {
			t.Fatalf("%d frames for 200 updates", len(model.frames))
		}
		if model.wakeups > bound {
			t.Fatalf("%d wakeups", model.wakeups)
		}
		if len(model.frames) == 0 || model.frames[len(model.frames)-1].applied != 200 {
			t.Fatalf("the last update reached the screen: frames %v", model.frames)
		}
	})
}

func TestAKeyIsDrawnBeforeTheQueuedFeedBacklogIsApplied(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		feed := make(chan uint32, 256)
		keys := make(chan rune, 16)
		model := newPacerModel(feed, keys)
		feed <- 0
		if !model.wake() {
			t.Fatal("the feed is open")
		}
		model.frame()
		if want := []pacerFrame{{applied: 1, typed: ""}}; !reflect.DeepEqual(model.frames, want) {
			t.Fatalf("frames %v", model.frames)
		}
		// A paced frame was just drawn, and a backlog is waiting for the
		// next one when the key arrives.
		for message := uint32(1); message < 100; message++ {
			feed <- message
		}
		keys <- 'k'

		if !model.wake() {
			t.Fatal("the feed is open")
		}
		model.frame()
		if last := model.frames[len(model.frames)-1]; last != (pacerFrame{applied: 1, typed: "k"}) {
			t.Fatalf("the key's frame came before the backlog was applied: %v", last)
		}

		if !model.wake() {
			t.Fatal("the feed is open")
		}
		model.frame()
		if last := model.frames[len(model.frames)-1]; last != (pacerFrame{applied: 100, typed: "k"}) {
			t.Fatalf("last frame %v", last)
		}
	})
}

func TestAnExpensiveFrameLeavesTheLoopFreeForAsLongAsItTook(t *testing.T) {
	pacer := newFramePacer(frameInterval)
	start := time.Now()
	ms := func(ms int) time.Time { return start.Add(time.Duration(ms) * time.Millisecond) }
	pacer.mark()
	pacer.drew(start, ms(30))
	pacer.mark()
	if due := pacer.deferredFrame(ms(30)); due == nil || !due.Equal(ms(60)) {
		t.Fatalf("deferred frame %v", due)
	}
	if gate := pacer.feedGate(ms(30)); gate == nil || !gate.Equal(ms(60)) {
		t.Fatalf("feed gate %v", gate)
	}
	if pacer.shouldDraw(ms(59)) {
		t.Fatal("drew before the spacing")
	}
	if !pacer.shouldDraw(ms(60)) {
		t.Fatal("held past the spacing")
	}
	// Input does not wait for the spacing.
	pacer.markUrgent()
	if !pacer.shouldDraw(ms(31)) {
		t.Fatal("input waited for the spacing")
	}
}

func TestInputFramesDoNotHoldBackBackgroundFrames(t *testing.T) {
	pacer := newFramePacer(frameInterval)
	start := time.Now()
	backgroundFrames := 0
	// A key every 5 ms for 200 ms, each followed by a background change.
	for step := range 40 {
		now := start.Add(time.Duration(5*step) * time.Millisecond)
		pacer.markUrgent()
		if !pacer.shouldDraw(now) {
			t.Fatal("a key is drawn at once")
		}
		pacer.drew(now, now)
		pacer.mark()
		if pacer.shouldDraw(now) {
			pacer.drew(now, now)
			backgroundFrames++
		}
	}
	// One per interval, rounded up to the 5 ms the keys arrive on.
	if backgroundFrames < 200/20 {
		t.Fatalf("%d", backgroundFrames)
	}
}
