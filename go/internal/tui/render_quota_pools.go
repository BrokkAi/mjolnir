// Port of mj-tui/src/render/quota_pools.rs.
//
// Provider identity is resolved off the UI thread; pooling uses only
// snapshots.
package tui

import (
	"cmp"
	"fmt"
	"slices"
	"strings"
	"unicode"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// periodSeconds is the real length of a recognised window label, used when
// the report gives no duration.
func periodSeconds(window *client.QuotaWindow) int64 {
	if window.DurationSeconds != nil {
		return *window.DurationSeconds
	}
	switch quotaPoolsASCIILower(window.Label) {
	case "5h", "5-hour", "5 hour":
		return 18000
	case "week", "weekly", "7d":
		return 604800
	case "month", "monthly":
		return 2_592_000
	}
	return 0
}

// quotaPoolsASCIILower is Rust's str::to_ascii_lowercase: only ASCII letters
// change.
func quotaPoolsASCIILower(text string) string {
	return strings.Map(func(r rune) rune {
		if r >= 'A' && r <= 'Z' {
			return r + ('a' - 'A')
		}
		return r
	}, text)
}

// readableWindows is a profile's pooled allowances: its Week window and its
// Month window each join their own pool; without either, the longest
// readable window stands in.
func readableWindows(report *client.ProfileQuota) []*client.QuotaWindow {
	panic("unported: mj-tui/src/render/quota_pools.rs readable_windows")
}

// Providers is the profile and provider read for it, and the refresh
// generation that read it, by profile id.
type Providers = map[string]ResolvedProvider

// ResolvedProvider is one entry of [Providers] (Rust's tuple
// `(HarnessProfile, QuotaProvider, u64)`).
type ResolvedProvider struct {
	Profile    core.HarnessProfile
	Provider   core.QuotaProvider
	Generation uint64
}

// ProviderTask is one profile's provider being read in the background.
type ProviderTask struct {
	profile    core.HarnessProfile
	generation uint64
	handle     *providerTaskHandle
}

// providerTaskHandle is Rust's JoinHandle<QuotaProvider>: the goroutine
// reading one profile's provider. done closes when it has finished;
// provider and panicked are written before that and read only after.
type providerTaskHandle struct {
	done     chan struct{}
	provider core.QuotaProvider
	panicked bool
}

// spawnProviderTask reads profile's quota provider on its own goroutine. A
// panic while reading is recorded rather than lost, and reported as a
// failed discovery when the task is joined.
func spawnProviderTask(profile core.HarnessProfile) *providerTaskHandle {
	handle := &providerTaskHandle{done: make(chan struct{})}
	go func() {
		defer close(handle.done)
		defer func() {
			if recover() != nil {
				handle.panicked = true
			}
		}()
		handle.provider = profile.QuotaProvider()
	}()
	return handle
}

// isFinished is JoinHandle::is_finished.
func (h *providerTaskHandle) isFinished() bool {
	select {
	case <-h.done:
		return true
	default:
		return false
	}
}

// join is JoinHandle::join: the provider, or false when the read panicked.
func (h *providerTaskHandle) join() (core.QuotaProvider, bool) {
	<-h.done
	return h.provider, !h.panicked
}

// ProviderDiscovery resolves quota providers. Codex and OpenCode providers
// come from files, so each profile is read on its own background goroutine
// and published as soon as it finishes. The zero value is Rust's Default.
type ProviderDiscovery struct {
	resolved         Providers
	tasks            map[string]*ProviderTask
	generation       uint64
	refreshRequested bool
	// spawnFailures is profiles whose discovery could not be started,
	// reported on the next poll. Rust records a thread the OS refused; a
	// goroutine always starts, so nothing adds to it in Go.
	spawnFailures []string
}

// GoString is the Rust Debug impl:
// `ProviderDiscovery { resolved: ["a"], in_flight: ["b"] }`.
func (d *ProviderDiscovery) GoString() string {
	keys := func(names []string) string {
		slices.Sort(names)
		quoted := make([]string, len(names))
		for i, name := range names {
			quoted[i] = fmt.Sprintf("%q", name)
		}
		return "[" + strings.Join(quoted, ", ") + "]"
	}
	var resolved, inFlight []string
	for id := range d.resolved {
		resolved = append(resolved, id)
	}
	for id := range d.tasks {
		inFlight = append(inFlight, id)
	}
	return fmt.Sprintf("ProviderDiscovery { resolved: %s, in_flight: %s }", keys(resolved), keys(inFlight))
}

// readsFiles reports whether a profile's provider is read from files, and
// so off the UI thread.
func readsFiles(profile *core.HarnessProfile) bool {
	return profile.Kind == core.HarnessKindCodex || profile.Kind == core.HarnessKindOpenCode
}

func providerUnavailable(profile *core.HarnessProfile) core.QuotaProvider {
	return core.QuotaProviderOther{Value: fmt.Sprintf("%s (provider unavailable)", profile.Kind.DisplayName())}
}

// providerDiscoveryStart is ProviderDiscovery::start: discovery with a task
// already started for every enabled file-backed profile.
func providerDiscoveryStart(config *core.Config) ProviderDiscovery {
	var discovery ProviderDiscovery
	discovery.spawnMissing(config)
	return discovery
}

// requestRefresh makes the next poll read every provider again.
func (d *ProviderDiscovery) requestRefresh() {
	d.refreshRequested = true
}

// spawnMissing starts one task per enabled file-backed profile that has no
// reading for its current configuration and none in flight.
func (d *ProviderDiscovery) spawnMissing(config *core.Config) {
	panic("unported: mj-tui/src/render/quota_pools.rs ProviderDiscovery::spawn_missing")
}

// provider is the provider read for profile id, while the reading is for
// this configuration of the profile; false while it is still being read.
func (d *ProviderDiscovery) provider(id string, profile *core.HarnessProfile) (core.QuotaProvider, bool) {
	panic("unported: mj-tui/src/render/quota_pools.rs ProviderDiscovery::provider")
}

// ready reports whether any task has finished and is waiting for a poll.
func (d *ProviderDiscovery) ready() bool {
	for _, task := range d.tasks {
		if task.handle.isFinished() {
			return true
		}
	}
	return false
}

// finish joins a finished task, and reports whether its read failed. A
// refresh or configuration change since it started makes its reading stale,
// and it is dropped to be read again.
func (d *ProviderDiscovery) finish(id string, task *ProviderTask) bool {
	panic("unported: mj-tui/src/render/quota_pools.rs ProviderDiscovery::finish")
}

// poll publishes finished readings and starts missing ones. It returns a
// notice naming the profiles whose discovery failed, if any did.
func (d *ProviderDiscovery) poll(config *core.Config) (string, bool) {
	panic("unported: mj-tui/src/render/quota_pools.rs ProviderDiscovery::poll")
}

// settle waits for every task in flight and publishes it. Only tests use
// this.
func (d *ProviderDiscovery) settle() {
	panic("unported: mj-tui/src/render/quota_pools.rs ProviderDiscovery::settle")
}

// PoolKey identifies one pooled allowance. It is comparable, so it can key a
// map or set (Rust keeps it in a BTreeSet): Rust's `scope: Option<String>`
// is Scope with HasScope.
type PoolKey struct {
	Provider core.QuotaProvider
	Scope    string
	HasScope bool
	Period   PoolPeriod
}

// compare is Rust's derived Ord: provider, then scope (none first), then
// period.
func (k PoolKey) compare(other PoolKey) int {
	if c := compareQuotaProviders(k.Provider, other.Provider); c != 0 {
		return c
	}
	if k.HasScope != other.HasScope {
		if k.HasScope {
			return 1
		}
		return -1
	}
	if c := cmp.Compare(k.Scope, other.Scope); c != 0 {
		return c
	}
	return comparePoolPeriods(k.Period, other.Period)
}

// compareQuotaProviders is QuotaProvider's derived Ord: declaration order,
// then Other's name.
func compareQuotaProviders(a, b core.QuotaProvider) int {
	if c := cmp.Compare(core.QuotaProviderOrdinal(a), core.QuotaProviderOrdinal(b)); c != 0 {
		return c
	}
	otherA, _ := a.(core.QuotaProviderOther)
	otherB, _ := b.(core.QuotaProviderOther)
	return cmp.Compare(otherA.Value, otherB.Value)
}

// PoolPeriod is the period of an allowance. Period identity keeps unrelated
// rolling and long-running allowances apart. Variant values are comparable.
type PoolPeriod interface {
	isPoolPeriod()
	// ordinal is the variant's position in declaration order, for Ord.
	ordinal() int
	// label is the period's name in a pool heading.
	label() string
	// marker is one character naming the period on timelines and in the
	// legend.
	marker() rune
}

type PoolPeriodWeek struct{}
type PoolPeriodMonth struct{}
type PoolPeriodFiveHour struct{}

// PoolPeriodOther is any other window, by its label and duration (Rust's
// `seconds: Option<i64>` is Seconds with HasSeconds).
type PoolPeriodOther struct {
	Label      string
	Seconds    int64
	HasSeconds bool
}

func (PoolPeriodWeek) isPoolPeriod()     {}
func (PoolPeriodMonth) isPoolPeriod()    {}
func (PoolPeriodFiveHour) isPoolPeriod() {}
func (PoolPeriodOther) isPoolPeriod()    {}

func (PoolPeriodWeek) ordinal() int     { return 0 }
func (PoolPeriodMonth) ordinal() int    { return 1 }
func (PoolPeriodFiveHour) ordinal() int { return 2 }
func (PoolPeriodOther) ordinal() int    { return 3 }

func (PoolPeriodWeek) label() string     { return "week" }
func (PoolPeriodMonth) label() string    { return "Month" }
func (PoolPeriodFiveHour) label() string { return "5H" }
func (p PoolPeriodOther) label() string  { return p.Label }

func (PoolPeriodWeek) marker() rune     { return 'W' }
func (PoolPeriodMonth) marker() rune    { return 'M' }
func (PoolPeriodFiveHour) marker() rune { return '5' }
func (p PoolPeriodOther) marker() rune {
	for _, ch := range p.Label {
		if unicode.IsLetter(ch) {
			if ch >= 'a' && ch <= 'z' {
				return ch - ('a' - 'A')
			}
			return ch
		}
	}
	return 'R'
}

// comparePoolPeriods is PoolPeriod's derived Ord.
func comparePoolPeriods(a, b PoolPeriod) int {
	if c := cmp.Compare(a.ordinal(), b.ordinal()); c != 0 {
		return c
	}
	otherA, _ := a.(PoolPeriodOther)
	otherB, _ := b.(PoolPeriodOther)
	if c := cmp.Compare(otherA.Label, otherB.Label); c != 0 {
		return c
	}
	if otherA.HasSeconds != otherB.HasSeconds {
		if otherA.HasSeconds {
			return 1
		}
		return -1
	}
	return cmp.Compare(otherA.Seconds, otherB.Seconds)
}

// poolPeriodOf is PoolPeriod::of: the period a window's allowance belongs
// to.
func poolPeriodOf(window *client.QuotaWindow) PoolPeriod {
	label := quotaPoolsASCIILower(window.Label)
	seconds := window.DurationSeconds
	switch {
	case (label == "week" || label == "weekly" || label == "7d") && (seconds == nil || *seconds == 604800):
		return PoolPeriodWeek{}
	case (label == "month" || label == "monthly") && (seconds == nil || *seconds > 604800):
		return PoolPeriodMonth{}
	case (label == "5h" || label == "5-hour" || label == "5 hour") && (seconds == nil || *seconds == 18000):
		return PoolPeriodFiveHour{}
	}
	other := PoolPeriodOther{Label: window.Label}
	if seconds != nil {
		other.Seconds, other.HasSeconds = *seconds, true
	}
	return other
}

// QuotaRow is one selectable row of the Quota pane. Variant values are
// comparable, so rows compare with ==; a nil QuotaRow is Rust's None.
type QuotaRow interface {
	isQuotaRow()
	// pool is the pool the row belongs to; false for a profile row.
	pool() (PoolKey, bool)
}

// QuotaRowPool is a pool's bar.
type QuotaRowPool struct{ Key PoolKey }

// QuotaRowProfile is a profile shown as text, outside any pool.
type QuotaRowProfile struct{ ID string }

// QuotaRowLane is one member profile's lane in an expanded pool.
type QuotaRowLane struct {
	Key PoolKey
	ID  string
}

func (QuotaRowPool) isQuotaRow()    {}
func (QuotaRowProfile) isQuotaRow() {}
func (QuotaRowLane) isQuotaRow()    {}

func (r QuotaRowPool) pool() (PoolKey, bool)  { return r.Key, true }
func (QuotaRowProfile) pool() (PoolKey, bool) { return PoolKey{}, false }
func (r QuotaRowLane) pool() (PoolKey, bool)  { return r.Key, true }

// selectableRows is the Quota pane's rows in drawing order: each pool, its
// lanes when expanded, then the provider's text rows. expanded is the
// dashboard's set of open pools (Rust's BTreeSet<PoolKey>, a sorted slice in
// DashboardState.quotaExpanded).
func selectableRows(providers []ProviderQuota, expanded []PoolKey) []QuotaRow {
	var rows []QuotaRow
	for _, provider := range providers {
		for _, pool := range provider.Pools {
			rows = append(rows, QuotaRowPool{Key: pool.Key})
			if slices.Contains(expanded, pool.Key) {
				for _, member := range pool.Members {
					rows = append(rows, QuotaRowLane{Key: pool.Key, ID: member.ID})
				}
			}
		}
		for _, text := range provider.Text {
			rows = append(rows, QuotaRowProfile{ID: text.ID})
		}
	}
	return rows
}

// ProviderQuota is one provider's pools, and the profiles it can only
// describe in words.
type ProviderQuota struct {
	Provider core.QuotaProvider
	Pools    []QuotaPool
	Text     []ProfileText
}

// ProfileText is a profile and the status text shown for it (Rust's
// `(String, String)` in ProviderQuota::text).
type ProfileText struct {
	ID   string
	Text string
}

// QuotaPool is one pooled allowance and the profiles that draw on it.
type QuotaPool struct {
	Key        PoolKey
	Label      string
	Members    []PoolMember
	Remaining  uint8
	Unweighted bool
	Missing    int
}

// reportedMembers is the members whose reading is current.
func (p *QuotaPool) reportedMembers() []*PoolMember {
	var members []*PoolMember
	for i := range p.Members {
		if !p.Members[i].AwaitingReading {
			members = append(members, &p.Members[i])
		}
	}
	return members
}

func (p *QuotaPool) reportedWeight() float64 {
	total := 0.0
	for _, member := range p.reportedMembers() {
		total += member.Weight
	}
	return total
}

// availableCapacity is measured in weighted percentage points.
func (p *QuotaPool) availableCapacity() float64 {
	total := 0.0
	for _, member := range p.reportedMembers() {
		total += member.Weight * float64(member.Remaining)
	}
	return total
}

// percentage is the pool's remaining share as the pane prints it: `~` marks
// an unweighted average (plan sizes are unknown), `?` that nothing is known
// to remain, and `*` that some profiles are not reporting.
func (p *QuotaPool) percentage() string {
	// With every reading past its reset, nothing is known to remain.
	known := len(p.reportedMembers()) > 0
	var text strings.Builder
	if p.Unweighted {
		text.WriteString("~")
	}
	if known {
		fmt.Fprintf(&text, "%d", p.Remaining)
	} else {
		text.WriteString("?")
	}
	text.WriteString("%")
	if p.Missing > 0 {
		text.WriteString("*")
	}
	return text.String()
}

// PoolMember is one profile's share of a pool.
type PoolMember struct {
	ID        string
	Weight    float64
	Limit     *int64
	Remaining uint8
	Reset     *int64
	// AwaitingReading: the reset time has passed and no reading taken since
	// has arrived.
	AwaitingReading bool
	// OtherLongBlockedUntil is the latest reset among the profile's other
	// exhausted unscoped long windows (math.MaxInt64 when one reports no
	// reset): the lane stays blocked until then.
	OtherLongBlockedUntil   *int64
	Banked                  *uint64
	FiveHour                *client.QuotaWindow
	FiveHourAwaitingReading bool
	// ResetText is the provider's reset text, for reports without a reset
	// timestamp.
	ResetText    *string
	FiveHourIdle bool
	Fable        *uint8
	Note         *string
}

// providerPools groups the enabled profiles' quota readings into one entry
// per provider, pooling the allowances that share a provider, scope and
// period.
func providerPools(config *core.Config, quotas map[string]client.ProfileQuota, providers *ProviderDiscovery, now uint64) []ProviderQuota {
	panic("unported: mj-tui/src/render/quota_pools.rs provider_pools")
}

// profileProvider is the provider a profile's quota is grouped under; a
// file-backed profile whose provider is still being read is grouped as
// resolving.
func profileProvider(id string, profile *core.HarnessProfile, providers *ProviderDiscovery) core.QuotaProvider {
	if provider, ok := providers.provider(id, profile); ok {
		return provider
	}
	if profile.Kind == core.HarnessKindCodex || profile.Kind == core.HarnessKindOpenCode {
		return core.QuotaProviderOther{Value: fmt.Sprintf("%s (resolving)", profile.Kind.DisplayName())}
	}
	return profile.QuotaProvider()
}

// refillWidths is largest-remainder apportionment for the available quota
// and refill chunks. Give every positive segment a cell when they all fit,
// taking cells only from other refills. On narrower bars, keep positive
// availability visible and let the caller list omitted refills.
func refillWidths(available float64, refills []float64, total float64, cells int) []int {
	panic("unported: mj-tui/src/render/quota_pools.rs refill_widths")
}
