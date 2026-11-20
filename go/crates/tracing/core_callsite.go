// Port of tracing-core 0.1.36 src/callsite.rs

package tracing

import (
	"strconv"
	"sync"
	"sync/atomic"
	"unsafe"
	"weak"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// What the crate keeps for each callsite is a cache: the Interest every
// registered subscriber has in it, so that a disabled callsite costs one
// load. That cache is not ported. An event or a span computes the interest
// each time (callsiteInterest), the way the crate computes it when it fills
// the cache, and then asks the current subscriber as the crate does. That
// gives what the cache gives when it is up to date, and cannot give what a
// stale cache gives: a callsite first reached on a thread with no subscriber
// staying disabled for a subscriber installed later on another thread (the
// race mj-worker's DeferToScoped works around).
//
// What is ported as it is is the registry of live dispatchers, from which
// LevelFilter::current and the interest are computed.

// CallsiteIdentifier is tracing::callsite::Identifier: the identity of a
// callsite. Two Metadata describe the same callsite exactly when their
// identifiers are equal.
type CallsiteIdentifier struct {
	metadata *Metadata
}

// FmtDebug is `impl Debug for Identifier`.
func (i CallsiteIdentifier) FmtDebug(f *rs.Formatter) error {
	return f.WriteStr("Identifier(0x" + strconv.FormatUint(uint64(uintptr(unsafe.Pointer(i.metadata))), 16) + ")")
}

// CallsiteRebuildInterestCache is tracing::callsite::rebuild_interest_cache.
// A subscriber calls it when what it enables has changed; here that
// recomputes LevelFilterCurrent, there being no cached interest to rebuild.
func CallsiteRebuildInterestCache() {
	dispatchers.mu.Lock()
	defer dispatchers.mu.Unlock()
	dispatchers.rebuildInterest(dispatchers.rebuilder())
}

// dispatchers is the crate's DISPATCHERS: every dispatcher that is still in
// use. Rust drops a dispatcher from the list when its last handle is dropped;
// here one leaves when it is collected, which only delays the lowering of
// LevelFilterCurrent and keeps an interest at "sometimes" a little longer.
var dispatchers dispatcherRegistry

type dispatcherRegistry struct {
	// mu serialises registrations and rebuilds, as LOCKED_DISPATCHERS'
	// write lock does.
	mu sync.Mutex
	// snapshot is what readers see: the live dispatchers and has_just_one.
	snapshot atomic.Pointer[dispatcherSnapshot]
}

type dispatcherSnapshot struct {
	live []weak.Pointer[Dispatch]
	// hasJustOne is Dispatchers::has_just_one: at most one dispatcher was
	// live when the last one was registered. Rust then asks only the
	// current thread's default.
	hasJustOne bool
}

// rebuilder is Dispatchers::rebuilder: nil stands for Rebuilder::JustOne.
func (d *dispatcherRegistry) rebuilder() []weak.Pointer[Dispatch] {
	snapshot := d.snapshot.Load()
	if snapshot == nil || snapshot.hasJustOne {
		return nil
	}
	return snapshot.live
}

// forEach is Rebuilder::for_each: the current default alone under JustOne,
// every live dispatcher otherwise.
func forEachDispatcher(live []weak.Pointer[Dispatch], justOne bool, f func(*Dispatch)) {
	if justOne {
		DispatcherGetDefault(func(dispatch *Dispatch) rs.Unit {
			f(dispatch)
			return rs.Unit{}
		})
		return
	}
	for _, registered := range live {
		if dispatch := registered.Value(); dispatch != nil {
			f(dispatch)
		}
	}
}

// registerDispatch is callsite::register_dispatch.
func registerDispatch(dispatch *Dispatch) {
	dispatchers.mu.Lock()
	var live []weak.Pointer[Dispatch]
	if snapshot := dispatchers.snapshot.Load(); snapshot != nil {
		for _, registered := range snapshot.live {
			if registered.Value() != nil {
				live = append(live, registered)
			}
		}
	}
	live = append(live, weak.Make(dispatch))
	dispatchers.snapshot.Store(&dispatcherSnapshot{live: live, hasJustOne: len(live) <= 1})
	dispatchers.mu.Unlock()

	dispatch.subscriber.OnRegisterDispatch(dispatch)

	dispatchers.mu.Lock()
	defer dispatchers.mu.Unlock()
	// Rebuilder::Write: every live dispatcher, even when there is one.
	dispatchers.rebuildInterest(live)
}

// rebuildInterest is Callsites::rebuild_interest, without the callsites:
// LevelFilter::current becomes the most verbose hint of the dispatchers the
// rebuilder covers (nil: the current default only).
func (d *dispatcherRegistry) rebuildInterest(live []weak.Pointer[Dispatch]) {
	maxLevel := LevelFilterOff
	forEachDispatcher(live, live == nil, func(dispatch *Dispatch) {
		// If the subscriber did not provide a max level hint, assume
		// that it may enable every level.
		levelHint := dispatch.maxLevelHint().UnwrapOr(LevelFilterTrace)
		if levelHint > maxLevel {
			maxLevel = levelHint
		}
	})
	levelFilterSetMax(maxLevel)
}

// callsiteInterest is rebuild_callsite_interest for a callsite reached now:
// the interest of every dispatcher the rebuilder covers, combined, or never
// when there is none.
func callsiteInterest(metadata *Metadata) SubscriberInterest {
	snapshot := dispatchers.snapshot.Load()
	justOne := snapshot == nil || snapshot.hasJustOne
	var live []weak.Pointer[Dispatch]
	if snapshot != nil {
		live = snapshot.live
	}
	var interest rs.Option[SubscriberInterest]
	forEachDispatcher(live, justOne, func(dispatch *Dispatch) {
		thisInterest := dispatch.RegisterCallsite(metadata)
		if thatInterest, ok := interest.Get(); ok {
			interest = rs.Some(thatInterest.and(thisInterest))
		} else {
			interest = rs.Some(thisInterest)
		}
	})
	return interest.UnwrapOrElse(SubscriberInterestNever)
}
