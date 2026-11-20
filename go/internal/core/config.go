// Port of mj-core/src/config.rs.
//
// Hel's versioned user configuration and domain model.
//
// This is intentionally a clean namespace. Nothing here reads or migrates the
// legacy `mj` configuration tree.
//
// This file holds Config itself and the sections it owns directly. The rest
// is split by responsibility into the config_*.go files:
//
//   - config_harness.go: HarnessKind, HarnessProfile and execution policy.
//   - config_keys.go: the [keys] section: the prefix key and every bindable action.
//   - config_machines.go: hosts, the stored runtime shape, and the conversion
//     between them and TargetTemplate.
//   - config_targets.go: projects, containers and TargetTemplate.
//   - config_ui.go: terminal appearance settings.
//   - config_loading.go: instance names, directories, and atomic file writes.
//   - config_secrets.go: environment entries that name a secret held elsewhere.
//
// Serialization: every configuration type has one serde-compatible JSON form
// (MarshalJSON/UnmarshalJSON where serde attributes need code). TOML goes
// through that same form: a TOML document is decoded into generic values and
// read as JSON (tomlToJSON), and a value is written by converting its JSON form
// into TOML in serde's field order (marshalTOMLPretty), so the two formats can
// never disagree about defaults, aliases or skipped fields.

package core

import (
	"bytes"
	"crypto/sha256"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"iter"
	"strings"
	"sync"
	"unicode"
)

// RawProjectContextID is the stable context identity for a bare project at
// the serialized path boundary.
func RawProjectContextID(projectDirectory string) string {
	digest := sha256.Sum256([]byte(strings.TrimSpace(projectDirectory)))
	return "remote-project-" + LowerHex(digest[:8])
}

// InstanceViewerPortsStart and InstanceViewerPortsEnd bound (inclusively) the
// ports a named instance's viewer may default to. The default instance keeps
// 3765; each named instance hashes its name into this range so a test build
// beside the live one does not collide with it, and so its viewer URL (and the
// bookmarked QR login) stays the same across daemon restarts.
const (
	InstanceViewerPortsStart uint16 = 38000
	InstanceViewerPortsEnd   uint16 = 38999
)

func defaultPhoneBind() string {
	if name, ok := InstanceName(); ok {
		return DefaultPhoneBindFor(&name)
	}
	return DefaultPhoneBindFor(nil)
}

// DefaultPhoneBindFor is the default [phone] bind for the default instance
// (nil) or a named one.
func DefaultPhoneBindFor(instance *string) string {
	if instance == nil {
		return "127.0.0.1:3765"
	}
	digest := sha256.Sum256([]byte(*instance))
	span := uint32(InstanceViewerPortsEnd-InstanceViewerPortsStart) + 1
	offset := binary.BigEndian.Uint32(digest[:4]) % span
	return fmt.Sprintf("127.0.0.1:%d", uint32(InstanceViewerPortsStart)+offset)
}

func defaultTrue() bool { return true }

func isTrue(value bool) bool { return value }

// isFalse is shared by every module of this package that skips a false flag.
func isFalse(value bool) bool { return !value }

type PhoneConfig struct {
	Enabled         bool    `json:"enabled" toml:"enabled"`
	Bind            string  `json:"bind" toml:"bind"`
	TailscaleDetect bool    `json:"tailscale_detect" toml:"tailscale_detect"`
	TLSCert         *string `json:"tls_cert,omitempty" toml:"tls_cert,omitempty"`
	TLSKey          *string `json:"tls_key,omitempty" toml:"tls_key,omitempty"`
}

// DefaultPhoneConfig is PhoneConfig's Default.
func DefaultPhoneConfig() PhoneConfig {
	return PhoneConfig{Enabled: true, Bind: defaultPhoneBind(), TailscaleDetect: true}
}

func (p PhoneConfig) MarshalJSON() ([]byte, error) {
	var object serdeObject
	object.fieldUnless(isTrue(p.Enabled), "enabled", p.Enabled)
	object.field("bind", p.Bind)
	object.fieldUnless(isTrue(p.TailscaleDetect), "tailscale_detect", p.TailscaleDetect)
	object.fieldUnless(p.TLSCert == nil, "tls_cert", p.TLSCert)
	object.fieldUnless(p.TLSKey == nil, "tls_key", p.TLSKey)
	return object.finish()
}

func (p *PhoneConfig) UnmarshalJSON(data []byte) error {
	type plain PhoneConfig
	value := plain{Enabled: true, TailscaleDetect: true}
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	fields, err := decodeObject(data)
	if err != nil {
		return err
	}
	if _, ok := fields["bind"]; !ok {
		value.Bind = defaultPhoneBind()
	}
	*p = PhoneConfig(value)
	return nil
}

func (p PhoneConfig) validate() error {
	panic("unported: mj-core/src/config.rs PhoneConfig::validate")
}

func (p PhoneConfig) isDefault() bool {
	return p == DefaultPhoneConfig()
}

// ReviewConfig is the ordered reviewer choices. Review runs before push
// exactly when reviewers are configured; there is no separate switch. See
// ReviewConfig.Trigger.
type ReviewConfig struct {
	// Legacy is a configuration written before reviewer lists that asked for
	// review but named no model. Its reviewers are chosen automatically once,
	// after which this is cleared. It counts as configured while it is set.
	Legacy    *LegacyReview
	Reviewers []SubagentChoice
	// Disabled is set only by ReviewConfig.ForSession for a session that
	// chose Off. Never read from or written to a file.
	Disabled bool
	// BriefWriter is an optional session that writes a relation brief before
	// the reviewers start. Global only: a session's own review choice does
	// not change it.
	BriefWriter *BriefWriterConfig
}

// MarshalJSON writes what a save writes (reviewConfigOutput).
func (r ReviewConfig) MarshalJSON() ([]byte, error) {
	return json.Marshal(reviewConfigOutputFrom(r))
}

// UnmarshalJSON reads any shape older files used (reviewConfigInput).
func (r *ReviewConfig) UnmarshalJSON(data []byte) error {
	var input reviewConfigInput
	if err := json.Unmarshal(data, &input); err != nil {
		return err
	}
	*r = reviewConfigFromInput(input)
	return nil
}

// BriefWriterConfig is the [review] brief_writer table: a writer session reads
// the change and hands back relations between it and the rest of the
// repository, which every reviewer's prompt then carries.
type BriefWriterConfig struct {
	Enabled bool `json:"enabled" toml:"enabled"`
	// Model is required while Enabled. Resolved to a profile like a reviewer's.
	Model  string  `json:"model" toml:"model"`
	Effort *string `json:"effort,omitempty" toml:"effort,omitempty"`
}

// Choice is the writer's model choice when the table turns the stage on.
func (b BriefWriterConfig) Choice() *SubagentChoice {
	if !b.Enabled {
		return nil
	}
	return &SubagentChoice{Model: b.Model, Effort: cloneStringPointer(b.Effort)}
}

const MaxReviewers = 3

type LegacyReview struct {
	// Count is a NonZeroUsize in Rust: never zero when set.
	Count   *int
	Profile *string
	Effort  *string
}

func (r ReviewConfig) IsLegacy() bool { return r.Legacy != nil }

func (r *ReviewConfig) ClearLegacy() { r.Legacy = nil }

// ReviewConfigLegacyDefault is review as an older configuration that asked
// for it without naming a model loads: reviewers still to be chosen
// automatically.
func ReviewConfigLegacyDefault() ReviewConfig {
	return ReviewConfig{Legacy: &LegacyReview{}}
}

// reviewConfigOutput is what a save writes. The removed `trigger` key appears
// only while reviewers still have to be chosen automatically, so a restart
// before that finishes does not forget that review was wanted. Once reviewers
// are configured the next save drops it.
type reviewConfigOutput struct {
	Trigger     *ReviewTrigger     `json:"trigger,omitempty"`
	Reviewers   *[]SubagentChoice  `json:"reviewers,omitempty"`
	Count       *int               `json:"count,omitempty"`
	Profile     *string            `json:"profile,omitempty"`
	Effort      *string            `json:"effort,omitempty"`
	BriefWriter *BriefWriterConfig `json:"brief_writer,omitempty"`
}

func reviewConfigOutputFrom(value ReviewConfig) reviewConfigOutput {
	output := reviewConfigOutput{BriefWriter: value.BriefWriter}
	if legacy := value.Legacy; legacy != nil {
		push := ReviewTriggerPush
		output.Trigger = &push
		output.Count = legacy.Count
		output.Profile = legacy.Profile
		output.Effort = legacy.Effort
	} else {
		reviewers := value.Reviewers
		if reviewers == nil {
			reviewers = []SubagentChoice{}
		}
		output.Reviewers = &reviewers
	}
	return output
}

// ReviewTrigger is whether review runs before push for a session. It is
// derived, never chosen globally: see ReviewConfig.Trigger. Per-turn review was
// removed; a stored `turn` reads as `push`.
type ReviewTrigger string

const (
	ReviewTriggerOff  ReviewTrigger = "off"
	ReviewTriggerPush ReviewTrigger = "push"
)

func (t ReviewTrigger) String() string { return string(t) }

func (t *ReviewTrigger) UnmarshalText(text []byte) error {
	switch string(text) {
	case "off":
		*t = ReviewTriggerOff
	case "push", "turn":
		*t = ReviewTriggerPush
	default:
		return serdeUnknownVariant(string(text), "off", "push")
	}
	return nil
}

// removedTrigger is the value of the removed [review] trigger key. Any value
// loads: only `push` and `turn` mean the user wanted review.
type removedTrigger struct {
	// name is set when the value was a string.
	name *string
}

func (r *removedTrigger) UnmarshalJSON(data []byte) error {
	var name string
	if json.Unmarshal(data, &name) == nil {
		r.name = &name
	} else {
		r.name = nil
	}
	return nil
}

func (r removedTrigger) wantedReview() bool {
	return r.name != nil && (*r.name == "push" || *r.name == "turn")
}

type reviewConfigInput struct {
	Trigger   *removedTrigger   `json:"trigger"`
	Reviewers *[]SubagentChoice `json:"reviewers"`
	Count     *nonZeroCount     `json:"count"`
	Enabled   *bool             `json:"enabled"`
	// Tier is accepted only so that old files still load.
	Tier        *string            `json:"tier"`
	Profile     *string            `json:"profile"`
	Model       *string            `json:"model"`
	Effort      *string            `json:"effort"`
	BriefWriter *BriefWriterConfig `json:"brief_writer"`
}

func reviewConfigFromInput(input reviewConfigInput) ReviewConfig {
	// The old profile pin and tier are dropped: reviewers are load-balanced
	// across enabled profiles. Review is on exactly when reviewers are
	// configured. A table that names none but asked for review (the removed
	// `trigger` or `enabled` keys) keeps asking for automatic reviewers; one
	// that did not has nothing to configure and review stays off.
	wanted := input.Enabled != nil && *input.Enabled
	if input.Trigger != nil {
		wanted = input.Trigger.wantedReview()
	}
	count := input.Count.value()
	var legacy *LegacyReview
	if input.Reviewers == nil && input.Model == nil && wanted {
		legacy = &LegacyReview{Count: count, Profile: input.Profile, Effort: cloneStringPointer(input.Effort)}
	}
	var reviewers []SubagentChoice
	if found := legacyReviewers(input.Reviewers, count, input.Model, input.Effort); found != nil {
		reviewers = *found
	}
	return ReviewConfig{Legacy: legacy, Reviewers: reviewers, BriefWriter: input.BriefWriter}
}

// nonZeroCount reads a serde NonZeroUsize.
type nonZeroCount int

func (n *nonZeroCount) UnmarshalJSON(data []byte) error {
	var value uint64
	if err := json.Unmarshal(data, &value); err != nil {
		return fmt.Errorf("invalid type: %s, expected a nonzero usize", strings.TrimSpace(string(data)))
	}
	if value == 0 {
		return errors.New("invalid value: integer `0`, expected a nonzero usize")
	}
	*n = nonZeroCount(value)
	return nil
}

func (n *nonZeroCount) value() *int {
	if n == nil {
		return nil
	}
	value := int(*n)
	return &value
}

// SessionReview is one session's review choice, made when the session is
// created. It overrides [review] for that session only; a session without one
// (a nil SessionReview) follows [review]. A stored `turn` (or `on`) reads as
// `push`, with the same reviewers, and is never written.
//
// Decode with UnmarshalSessionReviewJSON.
type SessionReview interface {
	isSessionReview()
	Trigger() (ReviewTrigger, bool)
	MarshalJSON() ([]byte, error)
}

type SessionReviewLegacy struct {
	ReviewTrigger *ReviewTrigger
	Count         *int
	Model         *string
	Effort        *string
}

type SessionReviewInherit struct {
	Reviewers *[]SubagentChoice
}

type SessionReviewOff struct{}

type SessionReviewPush struct {
	Reviewers *[]SubagentChoice
}

func (SessionReviewLegacy) isSessionReview()  {}
func (SessionReviewInherit) isSessionReview() {}
func (SessionReviewOff) isSessionReview()     {}
func (SessionReviewPush) isSessionReview()    {}

func (s SessionReviewLegacy) Trigger() (ReviewTrigger, bool) {
	if s.ReviewTrigger == nil {
		return "", false
	}
	return *s.ReviewTrigger, true
}
func (SessionReviewInherit) Trigger() (ReviewTrigger, bool) { return "", false }
func (SessionReviewOff) Trigger() (ReviewTrigger, bool)     { return ReviewTriggerOff, true }
func (SessionReviewPush) Trigger() (ReviewTrigger, bool)    { return ReviewTriggerPush, true }

// MarshalJSON writes SessionReviewOutput's LegacyInherit or LegacyPush.
func (s SessionReviewLegacy) MarshalJSON() ([]byte, error) {
	mode := "inherit"
	if s.ReviewTrigger != nil && *s.ReviewTrigger == ReviewTriggerPush {
		mode = "push"
	}
	var object serdeObject
	object.field("mode", mode)
	object.fieldUnless(s.Count == nil, "count", s.Count)
	object.fieldUnless(s.Model == nil, "model", s.Model)
	object.fieldUnless(s.Effort == nil, "effort", s.Effort)
	return object.finish()
}

func (s SessionReviewInherit) MarshalJSON() ([]byte, error) {
	var object serdeObject
	object.field("mode", "inherit")
	object.fieldUnless(s.Reviewers == nil, "reviewers", s.Reviewers)
	return object.finish()
}

func (SessionReviewOff) MarshalJSON() ([]byte, error) { return []byte(`{"mode":"off"}`), nil }

func (s SessionReviewPush) MarshalJSON() ([]byte, error) {
	var object serdeObject
	object.field("mode", "push")
	object.fieldUnless(s.Reviewers == nil, "reviewers", s.Reviewers)
	return object.finish()
}

// sessionReviewInput is the wire form read for every variant
// (tag = "mode", kebab-case; `turn` and `on` are aliases of `push`).
type sessionReviewInput struct {
	Mode      string            `json:"mode"`
	Reviewers *[]SubagentChoice `json:"reviewers"`
	Count     *nonZeroCount     `json:"count"`
	Model     *string           `json:"model"`
	Effort    *string           `json:"effort"`
	// Tier is accepted only so that old files still load.
	Tier *string `json:"tier"`
}

// UnmarshalSessionReviewJSON reads a SessionReview (From<SessionReviewInput>).
func UnmarshalSessionReviewJSON(data []byte) (SessionReview, error) {
	fields, err := decodeObject(data)
	if err != nil {
		return nil, err
	}
	if _, ok := fields["mode"]; !ok {
		return nil, missingField("mode")
	}
	var input sessionReviewInput
	if err := json.Unmarshal(data, &input); err != nil {
		return nil, err
	}
	count := input.Count.value()
	legacy := input.Reviewers == nil && (count != nil || input.Model != nil || input.Effort != nil)
	switch input.Mode {
	case "inherit":
		if legacy {
			return SessionReviewLegacy{Count: count, Model: input.Model, Effort: input.Effort}, nil
		}
		return SessionReviewInherit{Reviewers: input.Reviewers}, nil
	case "off":
		return SessionReviewOff{}, nil
	case "push", "turn", "on":
		if legacy {
			push := ReviewTriggerPush
			return SessionReviewLegacy{ReviewTrigger: &push, Count: count, Model: input.Model, Effort: input.Effort}, nil
		}
		return SessionReviewPush{Reviewers: input.Reviewers}, nil
	default:
		return nil, serdeUnknownVariant(input.Mode, "inherit", "off", "push")
	}
}

// legacyReviewers is the reviewer list, or the one a configuration written
// before reviewer lists implies: `count` copies of its model (two when `count`
// is absent, the old default), and none when it named no model (old Auto).
func legacyReviewers(reviewers *[]SubagentChoice, count *int, model, effort *string) *[]SubagentChoice {
	if reviewers != nil {
		return reviewers
	}
	if model == nil {
		return nil
	}
	n := 2
	if count != nil {
		n = *count
	}
	n = min(n, MaxReviewers)
	list := make([]SubagentChoice, n)
	for i := range list {
		list[i] = SubagentChoice{Model: *model, Effort: cloneStringPointer(effort)}
	}
	return &list
}

// Trigger reports whether review runs before push. This is the only answer:
// review is on exactly when reviewers are configured (or still to be chosen
// automatically), unless the session chose Off.
func (r ReviewConfig) Trigger() ReviewTrigger {
	if !r.Disabled && (r.Legacy != nil || len(r.Reviewers) > 0) {
		return ReviewTriggerPush
	}
	return ReviewTriggerOff
}

func (r ReviewConfig) IsPush() bool { return r.Trigger() == ReviewTriggerPush }

// ForSession is this configuration as it applies to a session with session's
// choice (nil for none). The effective reviewer list enables review unless
// the session chose Off.
func (r ReviewConfig) ForSession(session SessionReview) ReviewConfig {
	panic("unported: mj-core/src/config.rs ReviewConfig::for_session")
}

func ApplyLegacyOverride(list []SubagentChoice, count *int, model, effort *string, global ReviewConfig) []SubagentChoice {
	panic("unported: mj-core/src/config.rs apply_legacy_override")
}

// SessionWikiConfig controls indexing every session into the user's
// SessionWiki index.
//
// SessionWiki is a separate tool that keeps one searchable index of AI coding
// sessions across every tool the user runs. The daemon always writes
// Mjolnir's own sessions into it under the tool name `mjolnir`, so one search
// covers every harness. Only archiving is a setting, because only archiving
// deletes data.
type SessionWikiConfig struct {
	// Enabled is a deprecated switch retained for read compatibility with
	// configurations written while indexing was optional. It is ignored and
	// omitted from newly written configurations.
	Enabled bool `json:"enabled" toml:"enabled"`
	// ArchiveAfterDays is the age after which a stopped session Mjolnir has
	// indexed is removed from Mjolnir's own storage. Nil keeps every session.
	ArchiveAfterDays *uint32 `json:"archive_after_days,omitempty" toml:"archive_after_days,omitempty"`
}

func (s SessionWikiConfig) MarshalJSON() ([]byte, error) {
	var object serdeObject
	object.fieldUnless(s.ArchiveAfterDays == nil, "archive_after_days", s.ArchiveAfterDays)
	return object.finish()
}

// isDefault: the deprecated `enabled` key is never written, so a section
// holding only that key is still a default section and stays out of the file.
func (s SessionWikiConfig) isDefault() bool { return s.ArchiveAfterDays == nil }

// validate rejects an age that would archive a session the moment it stops.
func (s SessionWikiConfig) validate() error {
	if s.ArchiveAfterDays != nil && *s.ArchiveAfterDays == 0 {
		return errors.New("[sessionwiki] archive_after_days = 0 would remove a session as soon as it stops; " +
			"use 1 or more, or leave it empty to keep every session")
	}
	return nil
}

func (r ReviewConfig) validate(profiles map[string]HarnessProfile) error {
	if len(r.Reviewers) > MaxReviewers {
		return fmt.Errorf("[review] supports at most %d reviewers", MaxReviewers)
	}
	for _, reviewer := range r.Reviewers {
		if strings.TrimSpace(reviewer.Model) == "" {
			return errors.New("[review] reviewer model must not be empty")
		}
	}
	if writer := r.BriefWriter; writer != nil && writer.Enabled && strings.TrimSpace(writer.Model) == "" {
		return errors.New("[review] brief_writer needs a model while it is enabled")
	}
	return nil
}

func defaultSubagentLimit() int { return 6 }

func isDefaultSubagentLimit(value int) bool { return value == defaultSubagentLimit() }

// SubagentConfig is the global policy for Mjolnir-managed child agents.
//
// Sessions choose Native or Mjolnir; task model and effort choices are global,
// except that a target whose harness runs without approvals (a container, or
// `permissions = "yolo"`) can have its own choices.
type SubagentConfig struct {
	Straightforward *SubagentChoice `json:"straightforward,omitempty" toml:"straightforward,omitempty"`
	Subtle          *SubagentChoice `json:"subtle,omitempty" toml:"subtle,omitempty"`
	// Oracle is the read-only advisor behind `ask_oracle`. Optional, and
	// independent of the other two.
	Oracle *SubagentChoice `json:"oracle,omitempty" toml:"oracle,omitempty"`
	// Enabled is a deprecated global switch retained for read compatibility
	// with configurations written before the per-session choice existed. It
	// is ignored and omitted from newly written configurations.
	Enabled       bool `json:"enabled" toml:"enabled"`
	MaxConcurrent int  `json:"max_concurrent" toml:"max_concurrent"`
	// LegacyEligibleProfiles is the old allowlist, accepted for
	// compatibility and dropped when saved.
	LegacyEligibleProfiles map[string]bool `json:"eligible_profiles" toml:"eligible_profiles"`
	// Yolo holds the choices for targets whose permissions are `yolo`.
	// Ignored while UseSameOnAllTargets is true, but retained so re-checking
	// and unchecking the box restores them.
	Yolo SubagentChoices `json:"yolo" toml:"yolo"`
	// UseSameOnAllTargets is whether every target uses the choices above.
	// MaxConcurrent is always shared, whichever set a target uses.
	UseSameOnAllTargets bool `json:"use_same_on_all_targets" toml:"use_same_on_all_targets"`
}

func (s SubagentConfig) MarshalJSON() ([]byte, error) {
	var object serdeObject
	object.fieldUnless(s.Straightforward == nil, "straightforward", s.Straightforward)
	object.fieldUnless(s.Subtle == nil, "subtle", s.Subtle)
	object.fieldUnless(s.Oracle == nil, "oracle", s.Oracle)
	object.fieldUnless(isDefaultSubagentLimit(s.MaxConcurrent), "max_concurrent", s.MaxConcurrent)
	object.fieldUnless(s.Yolo.isDefault(), "yolo", s.Yolo)
	object.fieldUnless(isTrue(s.UseSameOnAllTargets), "use_same_on_all_targets", s.UseSameOnAllTargets)
	return object.finish()
}

func (s *SubagentConfig) UnmarshalJSON(data []byte) error {
	type plain SubagentConfig
	value := plain(DefaultSubagentConfig())
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	*s = SubagentConfig(value)
	return nil
}

// SubagentChoices is the task choices one target mode uses.
type SubagentChoices struct {
	Straightforward *SubagentChoice `json:"straightforward,omitempty" toml:"straightforward,omitempty"`
	Subtle          *SubagentChoice `json:"subtle,omitempty" toml:"subtle,omitempty"`
	Oracle          *SubagentChoice `json:"oracle,omitempty" toml:"oracle,omitempty"`
}

func (s SubagentChoices) Choice(taskType SubagentTaskType) *SubagentChoice {
	panic("unported: mj-core/src/config.rs SubagentChoices::choice")
}

func (s SubagentChoices) isDefault() bool {
	return s.Straightforward == nil && s.Subtle == nil && s.Oracle == nil
}

func (s SubagentChoices) validate(section string) error {
	return validateSubagentChoices(section, s.Straightforward, s.Subtle, s.Oracle)
}

func validateSubagentChoices(section string, straightforward, subtle, oracle *SubagentChoice) error {
	for _, task := range []struct {
		name   string
		choice *SubagentChoice
	}{{"straightforward", straightforward}, {"subtle", subtle}, {"oracle", oracle}} {
		if task.choice != nil && task.choice.Effort != nil && *task.choice.Effort == "adaptive" {
			return fmt.Errorf("[%s.%s] adaptive effort is no longer supported", section, task.name)
		}
	}
	if subtle != nil && straightforward == nil {
		return fmt.Errorf("[%s.subtle] requires [%s.straightforward]; configure straightforward alone or both", section, section)
	}
	return nil
}

// SubagentChoice is one model choice. Unlike the other configuration types it
// refuses a key it does not read, wherever it is read: API requests carry it
// inside tagged policies (SubagentPolicy, SessionReview) that serde reads from
// a buffered copy, where only the type itself can refuse a misspelled `effort`
// rather than run at the default. A field added here therefore needs a daemon
// socket protocol version, as before #1557.
type SubagentChoice struct {
	Model  string  `json:"model" toml:"model"`
	Effort *string `json:"effort,omitempty" toml:"effort,omitempty"`
}

// UnmarshalJSON applies serde's deny_unknown_fields and the required `model`.
func (s *SubagentChoice) UnmarshalJSON(data []byte) error {
	fields, err := decodeObject(data)
	if err != nil {
		return err
	}
	for key := range fields {
		if key != "model" && key != "effort" {
			return fmt.Errorf("unknown field `%s`, expected `model` or `effort`", key)
		}
	}
	if _, ok := fields["model"]; !ok {
		return missingField("model")
	}
	type plain SubagentChoice
	var value plain
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	*s = SubagentChoice(value)
	return nil
}

// DefaultSubagentConfig is SubagentConfig's Default.
func DefaultSubagentConfig() SubagentConfig {
	return SubagentConfig{MaxConcurrent: defaultSubagentLimit(), LegacyEligibleProfiles: map[string]bool{}, UseSameOnAllTargets: true}
}

// Choice is the shared task choice, ignoring the YOLO split.
func (s SubagentConfig) Choice(taskType SubagentTaskType) *SubagentChoice {
	panic("unported: mj-core/src/config.rs SubagentConfig::choice")
}

// ChoiceFor is the task choice a target with `yolo` permissions uses: its own
// set when the split is enabled, else the shared set.
func (s SubagentConfig) ChoiceFor(taskType SubagentTaskType, yolo bool) *SubagentChoice {
	if yolo && !s.UseSameOnAllTargets {
		return s.Yolo.Choice(taskType)
	}
	return s.Choice(taskType)
}

// EffectiveChoiceFor is a session's choice for taskType: its own override,
// else the choice for this target mode.
func (s SubagentConfig) EffectiveChoiceFor(policy SubagentPolicy, taskType SubagentTaskType, yolo bool) *SubagentChoice {
	panic("unported: mj-core/src/config.rs SubagentConfig::effective_choice_for")
}

// OffersSubtleFor reports whether a parent with policy may spawn subtle
// children. Without a subtle choice, every child uses the straightforward
// choice.
func (s SubagentConfig) OffersSubtleFor(policy SubagentPolicy, yolo bool) bool {
	panic("unported: mj-core/src/config.rs SubagentConfig::offers_subtle_for")
}

// OracleConfiguredFor reports whether sessions on this target mode can be
// offered an oracle.
func (s SubagentConfig) OracleConfiguredFor(yolo bool) bool {
	panic("unported: mj-core/src/config.rs SubagentConfig::oracle_configured_for")
}

// IsConfiguredFor: straightforward is required; subtle is optional.
func (s SubagentConfig) IsConfiguredFor(yolo bool) bool {
	panic("unported: mj-core/src/config.rs SubagentConfig::is_configured_for")
}

// isDefault: the deprecated `enabled` key is never written, so a section
// holding only that key is still a default section and stays out of the file.
func (s SubagentConfig) isDefault() bool {
	return s.MaxConcurrent == defaultSubagentLimit() && s.Straightforward == nil && s.Subtle == nil &&
		s.Oracle == nil && s.Yolo.isDefault() && s.UseSameOnAllTargets
}

func (s SubagentConfig) validate() error {
	panic("unported: mj-core/src/config.rs SubagentConfig::validate")
}

// legacyBuildCacheConfig is the read-only legacy policy, folded into machine
// settings when loading.
type legacyBuildCacheConfig struct {
	Enabled bool `json:"enabled" toml:"enabled"`
}

func defaultLegacyBuildCacheConfig() legacyBuildCacheConfig {
	return legacyBuildCacheConfig{Enabled: true}
}

func (c *legacyBuildCacheConfig) UnmarshalJSON(data []byte) error {
	type plain legacyBuildCacheConfig
	value := plain(defaultLegacyBuildCacheConfig())
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	*c = legacyBuildCacheConfig(value)
	return nil
}

// JevConfig is the one switch for the hosted Jev service. When it is off,
// Mjolnir sends nothing to Jev: no turn or GitHub-item classification and no
// quota recovery. `[jev] enabled = false`.
type JevConfig struct {
	Enabled bool `json:"enabled" toml:"enabled"`
}

// DefaultJevConfig is JevConfig's Default.
func DefaultJevConfig() JevConfig { return JevConfig{Enabled: true} }

func (j JevConfig) MarshalJSON() ([]byte, error) {
	var object serdeObject
	object.fieldUnless(isTrue(j.Enabled), "enabled", j.Enabled)
	return object.finish()
}

func (j *JevConfig) UnmarshalJSON(data []byte) error {
	type plain JevConfig
	value := plain(DefaultJevConfig())
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	*j = JevConfig(value)
	return nil
}

func (j JevConfig) isDefault() bool { return j == DefaultJevConfig() }

// QuotaConfig holds quota display and profile preferences.
type QuotaConfig struct {
	// Runtime is the retired runtime selector, accepted from older clients and
	// files but ignored.
	Runtime struct{} `json:"-" toml:"-"`
	// Prefer lists the profiles chosen in the dashboard to win load balancing
	// when their quota allows, most preferred first. `[quota] prefer = [...]`.
	Prefer []string `json:"prefer,omitempty" toml:"prefer,omitempty"`
}

func (q QuotaConfig) isDefault() bool { return len(q.Prefer) == 0 }

// legacyContinuationConfig is the legacy automatic-continuation switch, read
// but never written. The feature was removed (#1532); an existing
// [continuation] section is still accepted.
type legacyContinuationConfig struct {
	Enabled bool `json:"enabled" toml:"enabled"`
}

// legacyMailboxConfig is the legacy GitHub event source switch, read but never
// written.
type legacyMailboxConfig struct {
	Enabled bool `json:"enabled" toml:"enabled"`
}

func defaultLegacyMailboxConfig() legacyMailboxConfig { return legacyMailboxConfig{Enabled: true} }

func (c *legacyMailboxConfig) UnmarshalJSON(data []byte) error {
	type plain legacyMailboxConfig
	value := plain(defaultLegacyMailboxConfig())
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	*c = legacyMailboxConfig(value)
	return nil
}

// ProfilesDiscoveryKey is the identity of the configured profile list and the
// managed harness builds its probes would run, excluding policies and UI
// defaults.
func (c Config) ProfilesDiscoveryKey() string {
	panic("unported: mj-core/src/config.rs Config::profiles_discovery_key")
}

// SelectedModels is the models this configuration selects on profile id,
// whose efforts its settings offer, sorted: the sub-agent choices, which any
// enabled profile may serve, and the reviewer's model on the review profile.
// Only these and each profile's current model have their efforts discovered
// ahead of a request; aggregating harnesses advertise hundreds of models.
func (c Config) SelectedModels(id string) []string {
	panic("unported: mj-core/src/config.rs Config::selected_models")
}

// CapabilityDemandKey is the identity of what capability hydration covers for
// this configuration: its profile definitions and the models each enabled
// profile is selected for.
func (c Config) CapabilityDemandKey() string {
	panic("unported: mj-core/src/config.rs Config::capability_demand_key")
}

// SubagentDiscoveryKey is an opaque identity safe to send to control surfaces
// without exposing profile environments. Policies and unrelated preferences
// are absent.
func (c Config) SubagentDiscoveryKey(parent string, model *string) string {
	panic("unported: mj-core/src/config.rs Config::subagent_discovery_key")
}

// SubagentDiscoveryInputs: enabled installations determine sub-agent choices.
func (c Config) SubagentDiscoveryInputs() map[string]ProfileDiscoveryInputs {
	inputs := map[string]ProfileDiscoveryInputs{}
	for id, profile := range c.Profiles {
		if profile.Enabled {
			inputs[id] = profile.DiscoveryInputs()
		}
	}
	return inputs
}

// QuotaRecoveryEnabled: quota recovery resumes a session after a provider
// quota resets. It depends on the turn classifier, so the Privacy switch owns
// it.
func (c Config) QuotaRecoveryEnabled() bool { return c.Jev.Enabled }

// GithubEventsEnabled: the GitHub watcher uses Jev to classify activity, so
// both switches must be enabled. Neither switch disables mailbox delivery.
func (c Config) GithubEventsEnabled() bool { return c.Github.Watch.Enabled && c.Jev.Enabled }

// LegacyEC2TargetNotice: historical per-session EC2 records remain readable,
// but new work uses a persistent SSH machine with a container runtime.
//
// Version 20 adds daemon placement; 21 adds review counts; 22 adds the session
// GitHub credential default. Older strict configuration readers reject the new
// fields instead of dropping them.
const LegacyEC2TargetNotice = "Per-session EC2 targets were removed. Choose a persistent Podman target, or create one with `mj ec2 create <instance-type> --zone <zone>`."

// ConfigurationDocumentationURL is the page that documents config.toml,
// schema version included.
const ConfigurationDocumentationURL = "https://mjolnir.brokk.ai/configuration/"

// ContainerImageRepository is the registry repository for the release image.
const ContainerImageRepository = "ghcr.io/brokkai/mjolnir/agent-dev"

// ReleaseContainerImage is the image for daemon-container mode.
//
// A release build names the immutable image published for its own version; a
// development build names the floating image published from master. The
// release build sets it with -ldflags "-X ...ReleaseContainerImage=..." to
// ContainerImageFor(version, true), as mj-core/build.rs bakes it in for Rust.
var ReleaseContainerImage = ContainerImageRepository + ":latest"

// DefaultSessionImage is the session image used when neither its project nor
// target selects one.
const DefaultSessionImage = "docker.io/library/buildpack-deps:noble"

// LegacyDefaultContainerImage is the image name every configuration up to
// legacyDefaultImageLastVersion wrote into a container target a user had
// touched.
//
// Loading such a file replaces this literal with DefaultSessionImage, so an
// upgraded install follows the current default session image instead of
// pinning whatever was current when the file was written.
const LegacyDefaultContainerImage = "ghcr.io/brokkai/mjolnir/agent-dev:latest"

// legacyDefaultImageLastVersion is the last configuration version that wrote
// the default image into a file.
const legacyDefaultImageLastVersion = 14

// ContainerImageFor is the rule ReleaseContainerImage is resolved from.
//
// It is a function so one test build can exercise both channels.
func ContainerImageFor(version string, release bool) string {
	if release {
		return ContainerImageRepository + ":" + version
	}
	return ContainerImageRepository + ":latest"
}

// migrateLegacyDefaultImage moves a container target still carrying the
// historical default image onto defaultImage.
//
// A target the user customized -- any other image -- is left alone. Split from
// configFromStored to exercise legacy config migration directly.
func migrateLegacyDefaultImage(target *TargetTemplate, defaultImage string) {
	container := (*target).Container()
	if container == nil || container.Image != LegacyDefaultContainerImage {
		return
	}
	updated := *container
	updated.Image = defaultImage
	*target = (*target).withContainer(updated)
}

// refuseUnreadKeys refuses a configuration file that carries keys no setting
// reads, naming each one as the file spells it.
func refuseUnreadKeys(path string, unread []string) error {
	if len(unread) == 0 {
		return nil
	}
	quoted := make([]string, len(unread))
	for i, key := range unread {
		quoted[i] = "`" + key + "`"
	}
	return fmt.Errorf("%s: no Mjolnir setting is named %s; correct or remove it (see %s)",
		path, strings.Join(quoted, ", "), ConfigurationDocumentationURL)
}

// settingPath is a key path as the configuration file spells it, one segment
// per table or array level, such as ["profiles", "codex", "models"]. Filters
// compare segments, so a quoted key that contains a dot is still one key.
// (Rust flattens a serde_ignored::Path; the Go unread-key walk already yields
// segments.)
func settingPath(path []string) []string { return path }

// unreadBuildCacheKeys is the `build_cache` keys that no setting reads, under
// a machine or runtime whose kind reads that table.
//
// Machines and runtimes are tagged tables, which serde reads from a buffered
// copy out of sight of the file loader's unread-key check. Their `build_cache`
// tables have always refused a key they do not read, so each is read again
// here on its own. Under a kind without the setting, such as a bare runtime or
// an EC2 machine, the table was never read and still loads.
func unreadBuildCacheKeys(document map[string]any) []string {
	panic("unported: mj-core/src/config.rs unread_build_cache_keys")
}

// discardLegacySetting reads a retired setting whole without retaining or
// writing it. Unlike IgnoredAny, this does not make the file loader flag a key
// as unread.
func discardLegacySetting(data []byte) error {
	var value any
	return json.Unmarshal(data, &value)
}

// CanonicalImagePlatform is the OCI spelling for the architecture aliases
// accepted by container engines. Variant suffixes share the project's
// architecture-specific assignment.
func CanonicalImagePlatform(platform string) string {
	architecture, _, _ := strings.Cut(strings.TrimPrefix(platform, "linux/"), "/")
	switch architecture {
	case "x86_64", "amd64":
		return "linux/amd64"
	case "aarch64", "arm64":
		return "linux/arm64"
	}
	return platform
}

// ImagesConfig is the registry namespace used for published project session
// images.
type ImagesConfig struct {
	Repository *string                      `json:"repository,omitempty" toml:"repository,omitempty"`
	Bundles    map[string]map[string]string `json:"bundles,omitempty" toml:"bundles,omitempty"`
}

// ConsolidateBundle transfers assignments when project discovery consolidates
// two aliases. Existing canonical assignments decide each platform.
func (i *ImagesConfig) ConsolidateBundle(alias, canonical string) {
	if alias == canonical {
		return
	}
	images, ok := i.Bundles[alias]
	if !ok {
		return
	}
	delete(i.Bundles, alias)
	target := i.Bundles[canonical]
	if target == nil {
		target = map[string]string{}
		i.Bundles[canonical] = target
	}
	for platform, image := range images {
		if _, taken := target[platform]; !taken {
			target[platform] = image
		}
	}
}

// imagesReportedUnknownBundles holds the bundle ids whose ignored image
// assignments this process already warned about.
var imagesReportedUnknownBundles = struct {
	sync.Mutex
	ids map[string]struct{}
}{ids: map[string]struct{}{}}

func (i ImagesConfig) validate(bundles map[string]ProjectBundle) error {
	panic("unported: mj-core/src/config.rs ImagesConfig::validate")
}

func (i ImagesConfig) isDefault() bool { return i.Repository == nil && len(i.Bundles) == 0 }

// Config is the resolved configuration every part of Mjolnir works with.
//
// Each entry of Targets is a TargetTemplate: a host and a runtime fused
// together. The file stores the two apart, so this type is read and written
// through storedConfig, which is what [machines.<id>] and [targets.<id>]
// actually look like. Machines is carried through from the file so the
// Settings screen can edit hosts directly.
type Config struct {
	SessionsSide SessionsSide
	// SidebarSize is the shared Sessions sidebar size, independent of the
	// selected workspace.
	SidebarSize PaneSize
	Advanced    AdvancedConfig
	Notify      NotifyConfig
	Version     uint32
	// Spinner is the client-side activity animation; omitted configurations
	// retain the classic scan.
	Spinner SpinnerStyle
	Theme   ThemeChoice
	Phone   PhoneConfig
	// Daemon is where the daemon runs: on this computer or in a container.
	Daemon        DaemonConfig
	Github        GithubConfig
	Review        ReviewConfig
	Sessionwiki   SessionWikiConfig
	Subagents     SubagentConfig
	Jev           JevConfig
	Quota         QuotaConfig
	Keys          KeysConfig
	LegacyStartup struct{}
	Profiles      map[string]HarnessProfile
	Bundles       map[string]ProjectBundle
	Images        ImagesConfig
	// Machines is the hosts runtimes run on. `local` is implied even when it
	// is absent.
	Machines map[string]Machine
	Targets  map[string]TargetTemplate
	// DefaultTargets is the ids in Targets that Config.WithLocalTargets
	// inserted because the file names no target of that id. They are
	// candidates, not choices the user made, and they are never written to
	// the file. The file is the only source of an origin: this set is not
	// serialized.
	DefaultTargets map[string]struct{}
	// DisabledTargets is the ids in Targets the user switched off with
	// `enabled = false`. A disabled runtime stays configured, so detection
	// never adds its kind again, but nothing new is launched on it.
	DisabledTargets map[string]struct{}
}

// MarshalJSON writes the file's own shape (serde into = "StoredConfig").
func (c Config) MarshalJSON() ([]byte, error) {
	return json.Marshal(storedConfigFrom(c))
}

// UnmarshalJSON reads the file's own shape (serde try_from = "StoredConfig").
func (c *Config) UnmarshalJSON(data []byte) error {
	var stored storedConfig
	if err := json.Unmarshal(data, &stored); err != nil {
		return err
	}
	config, err := configFromStored(stored)
	if err != nil {
		return err
	}
	*c = config
	return nil
}

// MarshalTOML is toml::to_string_pretty(&config).
func (c Config) MarshalTOML() ([]byte, error) {
	text, err := marshalTOMLPretty(c)
	return []byte(text), err
}

// UnmarshalTOML reads a decoded TOML document through the JSON form.
func (c *Config) UnmarshalTOML(value any) error {
	data, err := tomlToJSON(value)
	if err != nil {
		return err
	}
	return c.UnmarshalJSON(data)
}

// storedConfig is the configuration file's own shape: hosts under
// [machines.<id>] and runtimes under [targets.<id>], each runtime naming its
// machine.
type storedConfig struct {
	// ShowStoppedSessions is a retired session filter. Files written before
	// version 7 may still carry it, so it is accepted and dropped.
	ShowStoppedSessions bool           `json:"show_stopped_sessions" toml:"show_stopped_sessions"`
	SessionsSide        SessionsSide   `json:"sessions_side" toml:"sessions_side"`
	SidebarSize         PaneSize       `json:"sidebar_size" toml:"sidebar_size"`
	Advanced            AdvancedConfig `json:"advanced" toml:"advanced"`
	Notify              NotifyConfig   `json:"notify" toml:"notify"`
	Version             uint32         `json:"version" toml:"version"`
	Spinner             SpinnerStyle   `json:"spinner" toml:"spinner"`
	Theme               ThemeChoice    `json:"theme" toml:"theme"`
	Phone               PhoneConfig    `json:"phone" toml:"phone"`
	Daemon              DaemonConfig   `json:"daemon" toml:"daemon"`
	Github              GithubConfig   `json:"github" toml:"github"`
	Review              ReviewConfig   `json:"review" toml:"review"`
	// Continuation is the legacy [continuation] section, read and ignored,
	// never written.
	Continuation legacyContinuationConfig `json:"continuation" toml:"continuation"`
	Mailbox      legacyMailboxConfig      `json:"mailbox" toml:"mailbox"`
	Sessionwiki  SessionWikiConfig        `json:"sessionwiki" toml:"sessionwiki"`
	Subagents    SubagentConfig           `json:"subagents" toml:"subagents"`
	BuildCache   legacyBuildCacheConfig   `json:"build_cache" toml:"build_cache"`
	Jev          JevConfig                `json:"jev" toml:"jev"`
	Quota        QuotaConfig              `json:"quota" toml:"quota"`
	Keys         KeysConfig               `json:"keys" toml:"keys"`
	// LegacyStartup is the retired [startup] section, read whole and dropped.
	LegacyStartup struct{}                  `json:"-" toml:"-"`
	Profiles      map[string]HarnessProfile `json:"profiles" toml:"profiles"`
	Bundles       map[string]ProjectBundle  `json:"bundles" toml:"bundles"`
	Images        ImagesConfig              `json:"images" toml:"images"`
	Machines      map[string]Machine        `json:"machines" toml:"machines"`
	Targets       map[string]targetEntry    `json:"targets" toml:"targets"`
}

// MarshalJSON applies StoredConfig's skip_serializing_if attributes in field
// order.
func (s storedConfig) MarshalJSON() ([]byte, error) {
	var object serdeObject
	object.fieldUnless(s.SessionsSide.isDefault(), "sessions_side", s.SessionsSide)
	object.fieldUnless(s.SidebarSize.IsDefault(), "sidebar_size", s.SidebarSize)
	object.fieldUnless(s.Advanced.isDefault(), "advanced", s.Advanced)
	object.fieldUnless(s.Notify.isDefault(), "notify", s.Notify)
	object.field("version", s.Version)
	object.fieldUnless(s.Spinner.IsDefault(), "spinner", s.Spinner)
	object.fieldUnless(themeChoiceIsDefault(s.Theme), "theme", s.Theme)
	object.fieldUnless(s.Phone.isDefault(), "phone", s.Phone)
	object.fieldUnless(s.Daemon.IsDefault(), "daemon", s.Daemon)
	object.fieldUnless(s.Github.IsDefault(), "github", s.Github)
	object.field("review", s.Review)
	object.fieldUnless(s.Sessionwiki.isDefault(), "sessionwiki", s.Sessionwiki)
	object.fieldUnless(s.Subagents.isDefault(), "subagents", s.Subagents)
	object.fieldUnless(s.Jev.isDefault(), "jev", s.Jev)
	object.fieldUnless(s.Quota.isDefault(), "quota", s.Quota)
	object.fieldUnless(s.Keys.IsDefault(), "keys", s.Keys)
	object.fieldUnless(len(s.Profiles) == 0, "profiles", s.Profiles)
	object.fieldUnless(len(s.Bundles) == 0, "bundles", s.Bundles)
	object.fieldUnless(s.Images.isDefault(), "images", s.Images)
	object.fieldUnless(len(s.Machines) == 0, "machines", s.Machines)
	object.fieldUnless(len(s.Targets) == 0, "targets", s.Targets)
	return object.finish()
}

// UnmarshalJSON applies StoredConfig's defaults: every section but `version`
// may be absent, and the polymorphic tables are read by their own readers.
func (s *storedConfig) UnmarshalJSON(data []byte) error {
	panic("unported: mj-core/src/config.rs StoredConfig deserialize")
}

// targetEntry is one [targets.<id>] table. Reading keeps it raw, because
// whether an old `kind` is allowed depends on the file's `version`, which is
// a sibling key; writing always produces the current shape (serde untagged).
type targetEntry interface {
	isTargetEntry()
}

type targetEntryStored struct{ Target StoredTarget }

type targetEntryDisabled struct{ Value disabledTarget }

// targetEntryRaw holds the table as decoded generic JSON (map[string]any
// for a table).
type targetEntryRaw struct{ Value any }

func (targetEntryStored) isTargetEntry()   {}
func (targetEntryDisabled) isTargetEntry() {}
func (targetEntryRaw) isTargetEntry()      {}

func (t targetEntryStored) MarshalJSON() ([]byte, error)   { return json.Marshal(t.Target) }
func (t targetEntryDisabled) MarshalJSON() ([]byte, error) { return json.Marshal(t.Value) }
func (t targetEntryRaw) MarshalJSON() ([]byte, error)      { return json.Marshal(t.Value) }

// unmarshalTargetEntryJSON always reads a table raw.
func unmarshalTargetEntryJSON(data []byte) (targetEntry, error) {
	var value any
	if err := json.Unmarshal(data, &value); err != nil {
		return nil, err
	}
	return targetEntryRaw{Value: value}, nil
}

// disabledTarget is a runtime written with `enabled = false`. Only a disabled
// runtime carries the key; an enabled one omits it, as profiles do.
type disabledTarget struct {
	Target  StoredTarget
	Enabled bool
}

// MarshalJSON flattens the runtime's keys and appends `enabled`.
func (d disabledTarget) MarshalJSON() ([]byte, error) {
	var object serdeObject
	object.flatten(d.Target)
	object.field("enabled", d.Enabled)
	return object.finish()
}

// takeTargetEnabled takes `enabled` out of a raw [targets.<id>] table, so the
// runtime's own keys are interpreted without it. Absent means enabled.
func takeTargetEnabled(id string, value any) (bool, error) {
	table, ok := value.(map[string]any)
	if !ok {
		return true, nil
	}
	enabled, present := table["enabled"]
	if !present {
		return true, nil
	}
	delete(table, "enabled")
	if flag, ok := enabled.(bool); ok {
		return flag, nil
	}
	rendered, _ := json.Marshal(enabled)
	return false, fmt.Errorf("target %s has enabled = %s; write true or false", debugString(id), rendered)
}

// interpretedTarget is a [targets.<id>] table read from a file, once its
// `kind` has been judged against the file's version.
type interpretedTarget interface {
	isInterpretedTarget()
}

type interpretedTargetStored struct{ Target StoredTarget }

// interpretedTargetLegacy is a pre-version-12 table, which fuses a host and a
// runtime and is migrated into a machine plus a runtime.
type interpretedTargetLegacy struct{ Target TargetTemplate }

func (interpretedTargetStored) isInterpretedTarget() {}
func (interpretedTargetLegacy) isInterpretedTarget() {}

// newlyUnknownTargetKeysReported holds the (target id, key) pairs this process
// has already warned about.
var newlyUnknownTargetKeysReported = struct {
	sync.Mutex
	pairs map[[2]string]struct{}
}{pairs: map[[2]string]struct{}{}}

// newlyUnknownTargetKeys is the keys of a [targets.<id>] table that its
// runtime kind does not read, leaving out any already reported by this
// process.
//
// Serde cannot refuse them: a container runtime's settings are flattened into
// the tagged StoredTarget, out of sight of the file loader's unread-key check.
// A misspelled or invented setting is therefore ignored, and saying so once is
// what stops a person from believing it applies (R4-6). The configuration is
// read many times in one process, so each key is reported only the first time.
func newlyUnknownTargetKeys(id, kind string, table map[string]any) []string {
	panic("unported: mj-core/src/config.rs newly_unknown_target_keys")
}

func interpretTarget(id string, value any, version uint32) (interpretedTarget, error) {
	panic("unported: mj-core/src/config.rs interpret_target")
}

// configFromStored is TryFrom<StoredConfig> for Config.
func configFromStored(stored storedConfig) (Config, error) {
	panic("unported: mj-core/src/config.rs TryFrom<StoredConfig> for Config")
}

// storedConfigFrom is From<Config> for StoredConfig.
func storedConfigFrom(config Config) storedConfig {
	panic("unported: mj-core/src/config.rs From<Config> for StoredConfig")
}

// DefaultConfig is Config's Default.
func DefaultConfig() Config {
	panic("unported: mj-core/src/config.rs Config::default")
}

// SessionImage is the image for a session on a container target. Bundle
// assignments take precedence only for the target's platform; bare targets
// have no image.
func (c Config) SessionImage(bundleID *string, targetID, platform string) (string, bool) {
	panic("unported: mj-core/src/config.rs Config::session_image")
}

// SessionTarget applies the same resolution to a configured target without
// changing it.
func (c Config) SessionTarget(bundleID *string, targetID, platform string) (TargetTemplate, error) {
	panic("unported: mj-core/src/config.rs Config::session_target")
}

// SetupAdditions suggests additions for setup without replacing existing
// entries. Both setup surfaces use the same identities and collision-free
// names.
func (c Config) SetupAdditions(discovered Config) Config {
	panic("unported: mj-core/src/config.rs Config::setup_additions")
}

// MachineRuntimes is the runtimes that run on machine machineID, in id order.
func (c Config) MachineRuntimes(machineID string) iter.Seq2[string, TargetTemplate] {
	panic("unported: mj-core/src/config.rs Config::machine_runtimes")
}

// MachinesWithoutRuntimes is the SSH and EC2 machines no runtime runs on,
// which no session can use until one does, in id order.
func (c Config) MachinesWithoutRuntimes() iter.Seq2[string, Machine] {
	panic("unported: mj-core/src/config.rs Config::machines_without_runtimes")
}

// MachineRuntimeAdditions is the runtimes from found to add for machine
// machineID, under collision-free ids: each kind (bare, podman, docker) the
// machine does not run yet. probed is the machine as it was when found was
// learned; when the machine has since changed or gone, nothing is added,
// because what was found describes a host this configuration no longer names.
// Every surface that adds a machine's runtimes, from the file under its lock
// to the Settings draft, decides here.
func (c Config) MachineRuntimeAdditions(machineID string, probed Machine, found iter.Seq2[string, TargetTemplate]) map[string]TargetTemplate {
	panic("unported: mj-core/src/config.rs Config::machine_runtime_additions")
}

func (c Config) IsUnconfigured() bool {
	return len(c.Profiles) == 0 && len(c.Bundles) == 0 && len(c.Targets) == 0
}

// EnabledProfiles is the profiles Mjolnir may choose or probe automatically,
// in id order.
func (c Config) EnabledProfiles() iter.Seq2[string, HarnessProfile] {
	panic("unported: mj-core/src/config.rs Config::enabled_profiles")
}

// NamedProfile looks up a profile explicitly named by a user or recorded on a
// session.
func (c Config) NamedProfile(id string) (HarnessProfile, error) {
	profile, ok := c.Profiles[id]
	if !ok {
		return HarnessProfile{}, fmt.Errorf("unknown profile %s", debugString(id))
	}
	return profile, nil
}

// EnabledProfile is one profile eligible for automatic selection or probing.
func (c Config) EnabledProfile(id string) (HarnessProfile, bool) {
	profile, ok := c.Profiles[id]
	if !ok || !profile.Enabled {
		return HarnessProfile{}, false
	}
	return profile, true
}

// EnabledTargets is the runtimes available for new work, in id order: every
// one not switched off.
func (c Config) EnabledTargets() iter.Seq2[string, TargetTemplate] {
	panic("unported: mj-core/src/config.rs Config::enabled_targets")
}

// EnabledTarget is one runtime when it exists and is available for new work.
func (c Config) EnabledTarget(id string) (TargetTemplate, bool) {
	target, ok := c.Targets[id]
	if !ok {
		return nil, false
	}
	if _, refused := target.LaunchRefusal(); refused {
		return nil, false
	}
	if _, disabled := c.DisabledTargets[id]; disabled {
		return nil, false
	}
	return target, true
}

// EnsureTargetEnabled refuses new work on runtime id when it is not
// configured or is switched off. Every launch, resume and move asks this.
func (c Config) EnsureTargetEnabled(id string) (TargetTemplate, error) {
	panic("unported: mj-core/src/config.rs Config::ensure_target_enabled")
}

// NativeContinuity reports whether a session recorded under sourceProfileID,
// running sourceKind, keeps its native conversation when it continues under
// destinationProfileID. When it does not, a resume or move gives a fresh
// native session the transcript as compacted text.
//
// The harness must match, and so must the model service, because two Codex
// profiles can point at different providers. Continuity needs evidence: when
// the source profile is gone or now runs another harness, only a harness
// whose service does not depend on the profile continues.
func (c Config) NativeContinuity(sourceKind HarnessKind, sourceProfileID, destinationProfileID string) (bool, error) {
	panic("unported: mj-core/src/config.rs Config::native_continuity")
}

// Validate checks what this file itself declares. Whether a profile or target
// can start depends on harness homes, secrets and variables outside it, and is
// asked of HarnessProfile.EnsureReady and TargetTemplate.EnsureReady where
// they are used, so one unusable profile never stops the configuration, and
// the daemon, from loading.
func (c Config) Validate() error {
	panic("unported: mj-core/src/config.rs Config::validate")
}

// Keybinds is the key bindings in force.
//
// A Config in hand has already passed Validate, so the fallback to the
// defaults here can only be reached by an unvalidated value.
func (c Config) Keybinds() Keybinds {
	keybinds, err := c.Keys.Resolve()
	if err != nil {
		return DefaultKeybinds()
	}
	return keybinds
}

// ConfigLoad is Config::load.
func ConfigLoad() (Config, error) {
	config, err := ConfigLoadFrom(ConfigPath())
	if err != nil {
		return Config{}, err
	}
	return config.WithLocalTargets(), nil
}

// ConfigLoadLocked reads current configuration while excluding cooperating
// config edits. Hold the opaque guard through the commit that relies on this
// snapshot, and Close it after.
func ConfigLoadLocked() (Config, *ConfigReadGuard, error) {
	panic("unported: mj-core/src/config.rs Config::load_locked")
}

// WithLocalTargets supplies standard local choices without requiring setup or
// writing a file. These are candidates: callers must check availability
// before offering launch. Explicit entries with the same name override the
// standard defaults.
func (c Config) WithLocalTargets() Config {
	panic("unported: mj-core/src/config.rs Config::with_local_targets")
}

// IsDefaultTarget reports whether target id is a default candidate that
// WithLocalTargets supplied, rather than an entry the user wrote in the config
// file. Read this, do not compare names: a user who writes [targets.docker]
// configured it.
func (c Config) IsDefaultTarget(id string) bool {
	_, ok := c.DefaultTargets[id]
	return ok
}

// ConfiguresTarget reports whether the user configured target id: the file
// names it and the entry differs from the standard local target of the same
// name that WithLocalTargets supplies. An entry that repeats a standard target
// word for word adds nothing the user chose, so callers that treat built-in
// targets gently treat it as built-in too.
func (c Config) ConfiguresTarget(id string) bool {
	panic("unported: mj-core/src/config.rs Config::configures_target")
}

// ConfigLoadFrom reads the config from path, returning DefaultConfig when the
// file is missing and an error when it is malformed or was written by a newer
// Mjolnir.
func ConfigLoadFrom(path string) (Config, error) {
	panic("unported: mj-core/src/config.rs Config::load_from")
}

func (c Config) Save() error { return c.SaveTo(ConfigPath()) }

// ConfigUpdate loads the latest config, applies one edit, validates it, and
// saves it while holding the config lock for the complete transaction.
//
// The returned config includes runtime local defaults, and the second value
// is whatever the edit returned. Keeping the load and edit under the same lock
// is what lets independent processes update disjoint sections without one
// stale full-config save erasing the other.
func ConfigUpdate[T any](edit func(*Config) (T, error)) (Config, T, error) {
	return ConfigUpdateWithLocalTargetsAt(ConfigPath(), edit)
}

// ConfigUpdateWithLocalTargetsAt is ConfigUpdate using an explicit config path.
func ConfigUpdateWithLocalTargetsAt[T any](path string, edit func(*Config) (T, error)) (Config, T, error) {
	panic("unported: mj-core/src/config.rs Config::update_with_local_targets_at")
}

// ConfigUpdateLocked edits and saves while an existing opaque owner retains
// the config lock. The owner can span a subsequent reference migration or
// rollback.
func ConfigUpdateLocked[T any](guard *ConfigReadGuard, edit func(*Config) (T, error)) (Config, T, error) {
	panic("unported: mj-core/src/config.rs Config::update_locked")
}

// ConfigUpdateTo is ConfigUpdate using an explicit config path, without local
// target candidates.
func ConfigUpdateTo[T any](path string, edit func(*Config) (T, error)) (Config, T, error) {
	lock, err := configLockAcquire(path)
	if err != nil {
		var zero T
		return Config{}, zero, err
	}
	defer lock.Close()
	return configUpdateToLocked(path, edit)
}

func configUpdateToLocked[T any](path string, edit func(*Config) (T, error)) (Config, T, error) {
	var zero T
	config, err := ConfigLoadFrom(path)
	if err != nil {
		return Config{}, zero, err
	}
	value, err := edit(&config)
	if err != nil {
		return Config{}, zero, err
	}
	if err := config.saveToLocked(path); err != nil {
		return Config{}, zero, err
	}
	return config, value, nil
}

// ConfigSaveReview loads the current file, replaces only its global review
// section, and writes it atomically. Callers use this for the dashboard editor
// so a stale dashboard snapshot cannot overwrite profiles, bundles, targets,
// or phone settings changed concurrently by another client.
func ConfigSaveReview(review ReviewConfig) (Config, error) {
	config, err := ConfigSaveReviewTo(ConfigPath(), review)
	if err != nil {
		return Config{}, err
	}
	return config.WithLocalTargets(), nil
}

// ConfigSaveReviewTo is ConfigSaveReview using an explicit path for tests and
// tools.
func ConfigSaveReviewTo(path string, review ReviewConfig) (Config, error) {
	config, _, err := ConfigUpdateTo(path, func(config *Config) (struct{}, error) {
		config.Review = review
		return struct{}{}, nil
	})
	return config, err
}

// SaveTo refuses when the file belongs to a newer Mjolnir -- judged by the
// marker this config loaded with *and* a fresh look at the file, since a
// newer build may have written it since. Overwriting would silently drop
// settings this build cannot represent.
func (c Config) SaveTo(path string) error {
	lock, err := configLockAcquire(path)
	if err != nil {
		return err
	}
	defer lock.Close()
	return c.saveToLocked(path)
}

// saveToLocked saves while the caller already owns ConfigLock. This is
// separate from SaveTo so a transaction does not try to lock the same sibling
// file recursively.
func (c Config) saveToLocked(path string) error {
	panic("unported: mj-core/src/config.rs Config::save_to_locked")
}

// configEditedFile is the file at path edited in place to hold body, keeping
// its comments and layout. False when there is no file to keep, or it cannot
// be edited in place.
func configEditedFile(path, body string) (string, bool) {
	panic("unported: mj-core/src/config.rs Config::edited_file")
}

// configEnsureWritable refuses to overwrite a file that a newer Mjolnir wrote
// after this config was loaded.
func configEnsureWritable(path string) error {
	if found, ok := NewerVersionOnDisk(path); ok {
		return fmt.Errorf("%s was written by a newer Mjolnir (config version %d; this build writes "+
			"%d). Update Mjolnir, or change settings with the newer build", path, found, ConfigVersion)
	}
	return nil
}

// ConfigReadGuard is the opaque owner of the configuration snapshot's
// cross-process lock. It exposes no lock internals; Close releases ownership.
type ConfigReadGuard struct {
	lock *ConfigLock
	path string
}

// Close releases the lock (Rust's Drop).
func (g *ConfigReadGuard) Close() error { return g.lock.Close() }

// ConfigLock is the cross-process lock for one config path. The lock has a
// stable inode beside the config because the config itself is replaced
// atomically after the lock is acquired.
type ConfigLock struct {
	file *FileLock
}

func configLockAcquire(configPath string) (*ConfigLock, error) {
	panic("unported: mj-core/src/config.rs ConfigLock::acquire")
}

// Close releases the lock (Rust's Drop).
func (l *ConfigLock) Close() error {
	panic("unported: mj-core/src/config.rs ConfigLock drop")
}

func configLockPath(path string) string {
	panic("unported: mj-core/src/config.rs config_lock_path")
}

// --- serde helpers shared by the config_*.go files ---

// serdeObject writes a JSON object member by member, in the order serde
// writes struct fields, so a type with skip_serializing_if attributes or
// flattened members serializes exactly as serde_json does. The first error
// sticks and is returned by finish.
type serdeObject struct {
	buffer bytes.Buffer
	count  int
	err    error
}

// field writes one member.
func (o *serdeObject) field(name string, value any) {
	if o.err != nil {
		return
	}
	encoded, err := json.Marshal(value)
	if err != nil {
		o.err = err
		return
	}
	key, _ := json.Marshal(name)
	o.member(key, encoded)
}

// fieldUnless writes one member unless skip is true.
func (o *serdeObject) fieldUnless(skip bool, name string, value any) {
	if !skip {
		o.field(name, value)
	}
}

// flatten writes every member of value's JSON object form (serde flatten).
func (o *serdeObject) flatten(value any) {
	if o.err != nil {
		return
	}
	encoded, err := json.Marshal(value)
	if err != nil {
		o.err = err
		return
	}
	encoded = bytes.TrimSpace(encoded)
	if len(encoded) < 2 || encoded[0] != '{' {
		o.err = fmt.Errorf("can only flatten structs and maps (got %s)", encoded)
		return
	}
	inner := bytes.TrimSpace(encoded[1 : len(encoded)-1])
	if len(inner) == 0 {
		return
	}
	if o.count > 0 {
		o.buffer.WriteByte(',')
	}
	o.buffer.Write(inner)
	o.count++
}

func (o *serdeObject) member(key, value []byte) {
	if o.count > 0 {
		o.buffer.WriteByte(',')
	}
	o.buffer.Write(key)
	o.buffer.WriteByte(':')
	o.buffer.Write(value)
	o.count++
}

func (o *serdeObject) finish() ([]byte, error) {
	if o.err != nil {
		return nil, o.err
	}
	return append(append([]byte{'{'}, o.buffer.Bytes()...), '}'), nil
}

// decodeObject reads a JSON object's members without interpreting them, with
// serde's message for any other JSON value.
func decodeObject(data []byte) (map[string]json.RawMessage, error) {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil || fields == nil {
		return nil, fmt.Errorf("invalid type: %s, expected a map", serdeUnexpected(data))
	}
	return fields, nil
}

// serdeUnexpected names a JSON value as serde's "invalid type" errors do.
func serdeUnexpected(data []byte) string {
	trimmed := bytes.TrimSpace(data)
	if len(trimmed) == 0 {
		return "unit value"
	}
	switch trimmed[0] {
	case '"':
		var text string
		_ = json.Unmarshal(trimmed, &text)
		return fmt.Sprintf("string %q", text)
	case '[':
		return "sequence"
	case '{':
		return "map"
	case 't', 'f':
		return "boolean `" + string(trimmed) + "`"
	case 'n':
		return "unit value"
	}
	if bytes.ContainsAny(trimmed, ".eE") {
		return "floating point `" + string(trimmed) + "`"
	}
	return "integer `" + string(trimmed) + "`"
}

// missingField is serde's error for an absent required field.
func missingField(name string) error { return fmt.Errorf("missing field `%s`", name) }

// serdeUnknownVariant is serde's error for an enum value it does not know.
func serdeUnknownVariant(variant string, expected ...string) error {
	quoted := make([]string, len(expected))
	for i, name := range expected {
		quoted[i] = "`" + name + "`"
	}
	switch len(quoted) {
	case 0:
		return fmt.Errorf("unknown variant `%s`, there are no variants", variant)
	case 1:
		return fmt.Errorf("unknown variant `%s`, expected %s", variant, quoted[0])
	case 2:
		return fmt.Errorf("unknown variant `%s`, expected %s or %s", variant, quoted[0], quoted[1])
	}
	return fmt.Errorf("unknown variant `%s`, expected one of %s", variant, strings.Join(quoted, ", "))
}

// debugString formats a string as Rust's `{:?}` does.
func debugString(value string) string {
	var builder strings.Builder
	builder.WriteByte('"')
	for _, r := range value {
		switch r {
		case '"':
			builder.WriteString(`\"`)
		case '\\':
			builder.WriteString(`\\`)
		case '\n':
			builder.WriteString(`\n`)
		case '\r':
			builder.WriteString(`\r`)
		case '\t':
			builder.WriteString(`\t`)
		case 0:
			builder.WriteString(`\0`)
		default:
			if unicode.IsPrint(r) || r == ' ' {
				builder.WriteRune(r)
			} else {
				fmt.Fprintf(&builder, `\u{%x}`, r)
			}
		}
	}
	builder.WriteByte('"')
	return builder.String()
}

func cloneStringPointer(value *string) *string {
	if value == nil {
		return nil
	}
	copied := *value
	return &copied
}

// tomlToJSON converts a decoded TOML document or value (as BurntSushi/toml
// decodes into any) into JSON for the serde-compatible readers.
func tomlToJSON(value any) ([]byte, error) {
	panic("unported: TOML to JSON conversion for config reading")
}

// marshalTOMLPretty is toml::to_string_pretty: value's JSON form written as a
// TOML document in serde field order, with nulls (serde None) left out.
func marshalTOMLPretty(value any) (string, error) {
	panic("unported: toml::to_string_pretty")
}

// unmarshalTOML is toml::from_str: text decoded with BurntSushi/toml and read
// through value's JSON form.
func unmarshalTOML(text string, value any) error {
	panic("unported: toml::from_str")
}
