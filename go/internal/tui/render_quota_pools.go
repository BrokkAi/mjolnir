// Port of mj-tui/src/render/quota_pools.rs.
//
// Provider identity is resolved off the UI thread; pooling uses only
// snapshots.
package tui

import (
	"cmp"
	"fmt"
	"maps"
	"math"
	"reflect"
	"slices"
	"strings"
	"unicode"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
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
	var readable []*client.QuotaWindow
	for i := range report.Windows {
		window := &report.Windows[i]
		if _, ok := quotaRemainingPercent(window); window.ModelScope == nil && ok {
			readable = append(readable, window)
		}
	}
	var long []*client.QuotaWindow
	for _, window := range readable {
		switch poolPeriodOf(window).(type) {
		case PoolPeriodWeek, PoolPeriodMonth:
			long = append(long, window)
		}
	}
	if len(long) > 0 {
		// One window per period, in the order the report lists them.
		var seen []PoolPeriod
		var windows []*client.QuotaWindow
		for _, window := range long {
			period := poolPeriodOf(window)
			if slices.Contains(seen, period) {
				continue
			}
			seen = append(seen, period)
			windows = append(windows, window)
		}
		return windows
	}
	// The longest; Rust's max_by_key keeps the last of equal elements.
	var longest *client.QuotaWindow
	for _, window := range readable {
		if longest == nil || periodSeconds(window) >= periodSeconds(longest) {
			longest = window
		}
	}
	if longest == nil {
		return nil
	}
	return []*client.QuotaWindow{longest}
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
	for id, profile := range config.EnabledProfiles() {
		if !readsFiles(&profile) {
			continue
		}
		if _, inFlight := d.tasks[id]; inFlight {
			continue
		}
		if resolved, ok := d.resolved[id]; ok && resolved.Profile.Equal(profile) && resolved.Generation == d.generation {
			continue
		}
		// A goroutine always starts, so Rust's branch for a thread the OS
		// refused (recording the profile in spawnFailures) has no Go
		// counterpart.
		if d.tasks == nil {
			d.tasks = map[string]*ProviderTask{}
		}
		// The goroutine gets a profile of its own, as Rust moves a clone
		// into the thread: a struct copy would still share the
		// environment's tables with the dashboard's configuration.
		owned := profile
		owned.Environment = profile.Environment.Clone()
		recorded := profile
		recorded.Environment = profile.Environment.Clone()
		d.tasks[id] = &ProviderTask{
			profile:    recorded,
			generation: d.generation,
			handle:     spawnProviderTask(owned),
		}
	}
}

// provider is the provider read for profile id, while the reading is for
// this configuration of the profile; false while it is still being read.
func (d *ProviderDiscovery) provider(id string, profile *core.HarnessProfile) (core.QuotaProvider, bool) {
	if !readsFiles(profile) {
		return profile.QuotaProvider(), true
	}
	resolved, ok := d.resolved[id]
	if !ok || !resolved.Profile.Equal(*profile) {
		return nil, false
	}
	return resolved.Provider, true
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
	stale := task.generation != d.generation
	provider, joined := task.handle.join()
	failed := !joined
	if failed {
		provider = providerUnavailable(&task.profile)
	}
	if !stale {
		if d.resolved == nil {
			d.resolved = Providers{}
		}
		d.resolved[id] = ResolvedProvider{Profile: task.profile, Provider: provider, Generation: task.generation}
	}
	return failed
}

// poll publishes finished readings and starts missing ones. It returns a
// notice naming the profiles whose discovery failed, if any did.
func (d *ProviderDiscovery) poll(config *core.Config) (string, bool) {
	if d.refreshRequested {
		d.refreshRequested = false
		// Keep showing the previous reading until the new one arrives.
		d.generation++
	}
	var finished []string
	for id, task := range d.tasks {
		if task.handle.isFinished() {
			finished = append(finished, id)
		}
	}
	slices.Sort(finished)
	failed := d.spawnFailures
	d.spawnFailures = nil
	for _, id := range finished {
		task := d.tasks[id]
		delete(d.tasks, id)
		if d.finish(id, task) {
			failed = append(failed, id)
		}
	}
	d.spawnMissing(config)
	failed = append(failed, d.spawnFailures...)
	d.spawnFailures = nil
	if len(failed) == 0 {
		return "", false
	}
	return fmt.Sprintf("Quota provider discovery failed for %s; refresh profiles to retry.", strings.Join(failed, ", ")), true
}

// settle waits for every task in flight and publishes it. Only tests use
// this.
func (d *ProviderDiscovery) settle() {
	tasks := d.tasks
	d.tasks = nil
	for _, id := range slices.Sorted(maps.Keys(tasks)) {
		if d.finish(id, tasks[id]) {
			panic("quota provider discovery")
		}
	}
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
	ellipsis := chat.ThemeGlyphs().Ellipsis
	// Rust's BTreeMap<QuotaProvider, ProviderQuota>: groups are found by
	// provider and put in provider order at the end.
	var grouped []*ProviderQuota
	groupFor := func(provider core.QuotaProvider) *ProviderQuota {
		for _, group := range grouped {
			if group.Provider == provider {
				return group
			}
		}
		group := &ProviderQuota{Provider: provider}
		grouped = append(grouped, group)
		return group
	}
	// A reset time that has passed with no reading taken since.
	resetPassed := func(reset *int64, refreshed uint64) bool {
		return reset != nil && *reset >= 0 && uint64(*reset) <= now && refreshed < uint64(*reset)
	}
	for id, profile := range config.EnabledProfiles() {
		if profile.Kind == core.HarnessKindCodex || profile.Kind == core.HarnessKindOpenCode {
			if _, known := providers.provider(id, &profile); !known {
				heading := core.QuotaProviderOther{Value: fmt.Sprintf("%s (resolving)", profile.Kind.DisplayName())}
				group := groupFor(heading)
				group.Text = append(group.Text, ProfileText{ID: id, Text: "resolving provider" + ellipsis})
				continue
			}
		}
		group := groupFor(profileProvider(id, &profile, providers))
		quota, ok := quotas[id]
		if !ok {
			group.Text = append(group.Text, ProfileText{ID: id, Text: "refreshing" + ellipsis})
			continue
		}
		label, hasLabel := "", true
		switch {
		case quota.IsUsagePriced():
			label = "pay-per-use"
		case quota.IsNotReported() || (quota.Extra != nil && *quota.Extra == "n/a"):
			label = "not reported"
		case quota.IsIdleWithoutReading():
			label = "idle; usage shows after the next request"
		case quota.Error != nil:
			var known bool
			if label, known = quota.RefusedCheckWithoutReading(now); !known {
				if label, known = quota.ErrorLabel(); !known {
					panic("errored report has a label")
				}
			}
		default:
			hasLabel = false
		}
		if hasLabel {
			if quota.Extra != nil && !quota.IsUsagePriced() && !quota.IsIdleWithoutReading() {
				label = label + " · " + *quota.Extra
			}
			group.Text = append(group.Text, ProfileText{ID: id, Text: label})
			continue
		}
		type scopedReport struct {
			scope  *string
			report client.ProfileQuota
		}
		var reports []scopedReport
		if profile.Kind == core.HarnessKindAntigravity {
			for _, pool := range profile.AntigravityPools() {
				scope := pool.ModelScope()
				reports = append(reports, scopedReport{scope: &scope, report: quota.ForAntigravityPool(pool)})
			}
		} else {
			reports = []scopedReport{{report: quota}}
		}
		added := false
		for r := range reports {
			scope, report := reports[r].scope, &reports[r].report
			for _, window := range readableWindows(report) {
				remaining, ok := quotaRemainingPercent(window)
				if !ok {
					continue
				}
				added = true
				keyPeriod := poolPeriodOf(window)
				key := PoolKey{Provider: group.Provider, Period: keyPeriod}
				if scope != nil {
					key.Scope, key.HasScope = *scope, true
				}
				index := slices.IndexFunc(group.Pools, func(pool QuotaPool) bool { return pool.Key == key })
				if index < 0 {
					poolLabel := ""
					if scope != nil {
						if pool, ok := core.AntigravityQuotaPoolOfScope(*scope); ok {
							poolLabel = pool.Name()
						}
					}
					group.Pools = append(group.Pools, QuotaPool{Key: key, Label: poolLabel})
					index = len(group.Pools) - 1
				}
				member := PoolMember{
					ID:              id,
					Remaining:       remaining,
					Reset:           window.ResetsAtEpochSeconds,
					AwaitingReading: resetPassed(window.ResetsAtEpochSeconds, quota.RefreshedAtEpochSeconds),
					Banked:          report.BankedResetsForWindow(window),
					ResetText:       window.Resets,
					// Claude omits the short window until first use, and
					// Muse while none is open; other adapters do not define
					// an absent window as idle.
					FiveHourIdle: profile.Kind == core.HarnessKindClaude || profile.Kind == core.HarnessKindMuse,
				}
				if quota.MonthlyPriceUsd != nil {
					member.Weight = float64(*quota.MonthlyPriceUsd)
				}
				if window.Limit != nil && *window.Limit > 0 {
					limit := *window.Limit
					member.Limit = &limit
				}
				for i := range report.Windows {
					other := &report.Windows[i]
					if other.ModelScope != nil {
						continue
					}
					switch poolPeriodOf(other).(type) {
					case PoolPeriodWeek, PoolPeriodMonth:
					default:
						continue
					}
					if poolPeriodOf(other) == keyPeriod {
						continue
					}
					if otherRemaining, ok := quotaRemainingPercent(other); !ok || otherRemaining != 0 {
						continue
					}
					until := int64(math.MaxInt64)
					if other.ResetsAtEpochSeconds != nil {
						until = *other.ResetsAtEpochSeconds
					}
					if member.OtherLongBlockedUntil == nil || until > *member.OtherLongBlockedUntil {
						member.OtherLongBlockedUntil = &until
					}
				}
				if five := report.FiveHourWindow(); five != nil {
					if !reflect.DeepEqual(*five, *window) {
						copied := *five
						member.FiveHour = &copied
					}
					member.FiveHourAwaitingReading = resetPassed(five.ResetsAtEpochSeconds, quota.RefreshedAtEpochSeconds)
				}
				if fable := quota.FableWeeklyWindow(); fable != nil {
					if percent, ok := quotaRemainingPercent(fable); ok {
						member.Fable = &percent
					}
				}
				var notes []string
				for i := range report.Windows {
					window := &report.Windows[i]
					if _, ok := quotaRemainingPercent(window); window.ModelScope == nil && !ok {
						notes = append(notes, window.Label+" unknown")
					}
				}
				rate, hasRate := quota.RateLimitLabel(now)
				switch {
				case hasRate && quota.Extra != nil:
					notes = append(notes, rate+" · "+*quota.Extra)
				case hasRate:
					notes = append(notes, rate)
				case quota.Extra != nil:
					notes = append(notes, *quota.Extra)
				}
				if len(notes) > 0 {
					note := strings.Join(notes, " · ")
					member.Note = &note
				}
				group.Pools[index].Members = append(group.Pools[index].Members, member)
			}
		}
		if !added {
			text := "unavailable"
			if quota.Extra != nil {
				text = "unavailable · " + *quota.Extra
			}
			group.Text = append(group.Text, ProfileText{ID: id, Text: text})
		}
	}
	for _, group := range grouped {
		// Gemini first, matching the existing independent-pool presentation.
		// A Month pool follows its Week pool whatever order the report lists
		// them.
		sortKey := func(pool *QuotaPool) [2]bool {
			_, month := pool.Key.Period.(PoolPeriodMonth)
			return [2]bool{!(pool.Key.HasScope && pool.Key.Scope == "gemini"), month}
		}
		slices.SortStableFunc(group.Pools, func(a, b QuotaPool) int {
			keyA, keyB := sortKey(&a), sortKey(&b)
			if c := compareBool(keyA[0], keyB[0]); c != 0 {
				return c
			}
			return compareBool(keyA[1], keyB[1])
		})
		multiplePeriods := false
		for _, pool := range group.Pools {
			for _, other := range group.Pools {
				if other.Key.HasScope == pool.Key.HasScope && other.Key.Scope == pool.Key.Scope && other.Key.Period != pool.Key.Period {
					multiplePeriods = true
				}
			}
		}
		for p := range group.Pools {
			pool := &group.Pools[p]
			if _, week := pool.Key.Period.(PoolPeriodWeek); multiplePeriods || !week {
				pool.Label = strings.TrimSpace(pool.Label + " " + pool.Key.Period.label())
			}
			slices.SortStableFunc(pool.Members, func(a, b PoolMember) int {
				resetA, resetB := int64(math.MaxInt64), int64(math.MaxInt64)
				if a.Reset != nil {
					resetA = *a.Reset
				}
				if b.Reset != nil {
					resetB = *b.Reset
				}
				if c := cmp.Compare(resetA, resetB); c != 0 {
					return c
				}
				return cmp.Compare(a.ID, b.ID)
			})
			missing := 0
			for id, profile := range config.EnabledProfiles() {
				provider, known := providers.provider(id, &profile)
				if !known || provider != group.Provider ||
					slices.ContainsFunc(pool.Members, func(member PoolMember) bool { return member.ID == id }) {
					continue
				}
				// An Antigravity profile belongs only to the pools it uses.
				eligible := !pool.Key.HasScope || slices.ContainsFunc(profile.AntigravityPools(),
					func(candidate core.AntigravityQuotaPool) bool { return candidate.ModelScope() == pool.Key.Scope })
				report, ok := quotas[id]
				if !ok {
					if eligible {
						missing++
					}
					continue
				}
				if report.IsUsagePriced() || report.IsNotReported() || (report.Extra != nil && *report.Extra == "n/a") {
					continue
				}
				// A failed probe cannot tell us which allowances the
				// profile holds.
				if report.Error != nil || len(report.Windows) == 0 {
					if eligible {
						missing++
					}
					continue
				}
				var scoped *client.ProfileQuota
				if profile.Kind == core.HarnessKindAntigravity {
					if pool.Key.HasScope {
						for _, candidate := range profile.AntigravityPools() {
							if candidate.ModelScope() == pool.Key.Scope {
								forPool := report.ForAntigravityPool(candidate)
								scoped = &forPool
								break
							}
						}
					}
				} else if !pool.Key.HasScope {
					scoped = &report
				}
				if scoped == nil {
					continue
				}
				for i := range scoped.Windows {
					window := &scoped.Windows[i]
					if poolPeriodOf(window) != pool.Key.Period {
						continue
					}
					if _, readable := quotaRemainingPercent(window); !readable ||
						resetPassed(window.ResetsAtEpochSeconds, scoped.RefreshedAtEpochSeconds) {
						missing++
						break
					}
				}
			}
			// A pooled reading older than its own reset is missing too.
			for _, member := range pool.Members {
				if member.AwaitingReading {
					missing++
				}
			}
			pool.Missing = missing
			// The provider, scope and period identify the allowance's unit.
			// Prefer its absolute capacity over a subscription's dollar
			// price.
			allLimited := !slices.ContainsFunc(pool.Members, func(member PoolMember) bool { return member.Limit == nil })
			if allLimited {
				for m := range pool.Members {
					pool.Members[m].Weight = float64(*pool.Members[m].Limit)
				}
			} else if slices.ContainsFunc(pool.Members, func(member PoolMember) bool { return member.Weight <= 0 }) {
				pool.Unweighted = true
				for m := range pool.Members {
					pool.Members[m].Weight = 1
				}
			}
			if total := pool.reportedWeight(); total > 0 {
				pool.Remaining = saturatingUint8(math.Round(pool.availableCapacity() / total))
			} else {
				pool.Remaining = 0
			}
		}
	}
	slices.SortFunc(grouped, func(a, b *ProviderQuota) int { return compareQuotaProviders(a.Provider, b.Provider) })
	providerQuotas := make([]ProviderQuota, len(grouped))
	for i, group := range grouped {
		providerQuotas[i] = *group
	}
	return providerQuotas
}

// compareBool is bool's Ord: false before true.
func compareBool(a, b bool) int {
	switch {
	case a == b:
		return 0
	case !a:
		return -1
	default:
		return 1
	}
}

// saturatingUint8 is Rust's `f64 as u8`: saturating at both ends, NaN as 0.
func saturatingUint8(value float64) uint8 {
	switch {
	case math.IsNaN(value) || value <= 0:
		return 0
	case value >= math.MaxUint8:
		return math.MaxUint8
	default:
		return uint8(value)
	}
}

// saturatingInt is Rust's `f64 as usize`: saturating at both ends, NaN as 0.
func saturatingInt(value float64) int {
	switch {
	case math.IsNaN(value) || value <= 0:
		return 0
	case value >= math.MaxInt:
		return math.MaxInt
	default:
		return int(value)
	}
}

// totalCmp is f64::total_cmp: IEEE 754 totalOrder.
func totalCmp(a, b float64) int {
	x, y := int64(math.Float64bits(a)), int64(math.Float64bits(b))
	x ^= int64(uint64(x>>63) >> 1)
	y ^= int64(uint64(y>>63) >> 1)
	return cmp.Compare(x, y)
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
	capacities := append([]float64{available}, refills...)
	type fraction struct {
		index int
		value float64
	}
	widths := make([]int, 0, len(capacities))
	fractions := make([]fraction, 0, len(capacities))
	for index, capacity := range capacities {
		exact := float64(cells) * capacity / total
		widths = append(widths, saturatingInt(math.Floor(exact)))
		fractions = append(fractions, fraction{index, exact - math.Trunc(exact)})
	}
	slices.SortStableFunc(fractions, func(a, b fraction) int {
		if c := totalCmp(b.value, a.value); c != 0 {
			return c
		}
		return cmp.Compare(a.index, b.index)
	})
	used := 0
	for _, width := range widths {
		used += width
	}
	leftover := max(cells-used, 0)
	for _, fraction := range fractions[:min(leftover, len(fractions))] {
		widths[fraction.index]++
	}
	positive := 0
	for _, capacity := range capacities {
		if capacity > 0 {
			positive++
		}
	}
	allFit := positive <= cells
	// Minimum cells come only from refill chunks, never from the available
	// fill, so the fill stays proportional to the percentage printed in it.
	// A refill that cannot get a cell is listed under the bar instead.
	for index, capacity := range capacities {
		if capacity > 0 && widths[index] == 0 && (allFit || index == 0) {
			// A refill only takes a cell from a refill that keeps one.
			spare := 1
			if index == 0 {
				spare = 0
			}
			// The widest donor; Rust's max_by_key keeps the last of equals.
			donor := -1
			for candidate := 1; candidate < len(widths); candidate++ {
				if widths[candidate] > spare && (donor < 0 || widths[candidate] >= widths[donor]) {
					donor = candidate
				}
			}
			if donor >= 0 {
				widths[donor]--
				widths[index] = 1
			}
		}
	}
	return widths
}
