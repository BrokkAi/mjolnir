// Port of mj-cli/src/dashboard/frame_pacer.rs.
//
// When the dashboard draws.
//
// Applying a background message and drawing a frame are separate decisions.
// A message only marks the surface out of date; the frame that shows it is
// drawn on a cadence, so a burst of updates from many working sessions costs
// one frame per interval rather than one per message. Input is the
// exception: the frame that shows a key is drawn as soon as the key is
// handled, before any queued background message is applied.
//
// framePacer is the one owner of that decision. The loop reports what each
// wakeup changed and asks the pacer whether to draw now, when the next paced
// frame is due, and when background feeds may next be taken.
//
// tokio's Instant is time.Time with its monotonic reading; under a synctest
// bubble it follows the bubble's clock, as tokio's paused clock does.

package cli

import (
	"context"
	"time"
)

// frameInterval is the shortest gap between two frames drawn for background
// changes.
//
// Input never waits for it, so it bounds only how often streaming output and
// other feed updates reach the screen. 16 ms matches a 60 Hz display: a
// stream still advances every refresh the terminal can show, while a burst
// of feed messages that arrives within one refresh shares a frame.
const frameInterval = 16 * time.Millisecond

// pacedFrameSpacing: paced frames start at least this many frame-build times
// apart. When a frame costs more than half the interval, as it does for a
// large surface on a busy host, a fixed interval would leave the loop drawing
// back to back; spacing frames by their own cost keeps half of the loop free,
// so a key finds it idle at least half of the time.
const pacedFrameSpacing = 2

type framePacer struct {
	interval time.Duration
	// nextPaced is when the next paced frame may start; nil before the
	// first. Frames drawn for input do not move it, so continuous input
	// cannot hold background updates back.
	nextPaced *time.Time
	// dirty: the visible state changed since the last frame.
	dirty bool
	// urgent: the change must be drawn now: input, or a message whose own
	// bookkeeping assumes the next frame shows it.
	urgent bool
}

func newFramePacer(interval time.Duration) framePacer {
	return framePacer{interval: interval}
}

// mark: something visible changed; draw it with the next paced frame.
func (p *framePacer) mark() {
	p.dirty = true
}

// markUrgent: something visible changed that must be drawn without waiting.
func (p *framePacer) markUrgent() {
	p.dirty = true
	p.urgent = true
}

// pending is whether a change has not reached the screen yet.
func (p *framePacer) pending() bool {
	return p.dirty
}

func (p *framePacer) shouldDraw(now time.Time) bool {
	return p.dirty && (p.urgent || p.nextPaced == nil || !now.Before(*p.nextPaced))
}

// drew records a frame that started at started and finished at finished.
// Every frame shows the whole current state, so it settles every pending
// change; only a paced frame sets when the next one may start.
func (p *framePacer) drew(started time.Time, finished time.Time) {
	if !p.urgent {
		cost := max(finished.Sub(started), 0)
		next := started.Add(max(p.interval, cost*pacedFrameSpacing))
		p.nextPaced = &next
	}
	p.dirty = false
	p.urgent = false
}

// feedGate is when background feeds may next be taken, if not now. Holding
// them until a paced frame could show their result is what makes a burst
// share one wakeup and one frame.
func (p *framePacer) feedGate(now time.Time) *time.Time {
	if p.nextPaced == nil || !p.nextPaced.After(now) {
		return nil
	}
	due := *p.nextPaced
	return &due
}

// deferredFrame is when a deferred change is due on screen, if one is
// waiting.
func (p *framePacer) deferredFrame(now time.Time) *time.Time {
	if !p.dirty || p.urgent {
		return nil
	}
	return p.feedGate(now)
}

// paced holds future until gate, then runs it. Nothing runs before the gate,
// so a cancel-safe future stays cancel safe. When ctx ends first, future
// never runs and paced returns ctx's error.
func paced[T any](ctx context.Context, gate *time.Time, future func(ctx context.Context) T) (T, error) {
	if gate != nil {
		timer := time.NewTimer(time.Until(*gate))
		defer timer.Stop()
		select {
		case <-timer.C:
		case <-ctx.Done():
			var zero T
			return zero, ctx.Err()
		}
	}
	return future(ctx), nil
}

// never is a far deadline for a disabled select timer arm, which is still
// built.
func never() time.Time {
	return time.Now().Add(86_400 * time.Second)
}
