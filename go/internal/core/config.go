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
	"log/slog"
	"math"
	"net/netip"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode"

	"github.com/BurntSushi/toml"
)

// MbxVersion is the mbx release Mjolnir installs for host setup and Nitro
// guests.
const MbxVersion = "1.30.0"

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

// GuardianConfig is the global settings for Mjolnir's permission guardian.
// (Ported from the Rust source after this package's base, for mj-client's
// settings.)
type GuardianConfig struct {
	// Enabled is whether Mjolnir's guardian reviews tool permission requests
	// that reach Mjolnir under configured approvals across all profiles.
	Enabled bool `json:"enabled" toml:"enabled"`
	// LocalClassifier is whether the daemon runs the local zero-shot Wolf
	// Defender Jev ONNX approve-only Stage-1 classifier before escalating to
	// Stage 2.
	LocalClassifier bool `json:"local_classifier" toml:"local_classifier"`
	// UseGPU is whether the local Wolf classifier may use a GPU execution
	// provider when available; false keeps Stage-1 inference on CPU.
	UseGPU bool `json:"use_gpu" toml:"use_gpu"`
}

// DefaultGuardianConfig is GuardianConfig's Default.
func DefaultGuardianConfig() GuardianConfig {
	return GuardianConfig{Enabled: true, LocalClassifier: true, UseGPU: true}
}

func (g GuardianConfig) IsDefault() bool { return g == DefaultGuardianConfig() }

// MarshalJSON skips each switch while it is on (skip_serializing_if is_true).
func (g GuardianConfig) MarshalJSON() ([]byte, error) {
	var object serdeObject
	object.fieldUnless(isTrue(g.Enabled), "enabled", g.Enabled)
	object.fieldUnless(isTrue(g.LocalClassifier), "local_classifier", g.LocalClassifier)
	object.fieldUnless(isTrue(g.UseGPU), "use_gpu", g.UseGPU)
	return object.finish()
}

// UnmarshalJSON defaults every switch to on (serde default = default_true).
func (g *GuardianConfig) UnmarshalJSON(data []byte) error {
	type plain GuardianConfig
	value := plain(DefaultGuardianConfig())
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	*g = GuardianConfig(value)
	return nil
}

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
	bind, err := netip.ParseAddrPort(p.Bind)
	if err != nil {
		return fmt.Errorf("parse phone bind address %s: invalid socket address syntax", debugString(p.Bind))
	}
	if (p.TLSCert != nil) != (p.TLSKey != nil) {
		return errors.New("phone TLS requires both `tls_cert` and `tls_key`")
	}
	if !bind.Addr().IsLoopback() && p.TLSCert == nil {
		return errors.New("a non-loopback phone bind requires TLS")
	}
	return nil
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
	config := r
	switch choice := session.(type) {
	case nil:
	case SessionReviewOff:
		config.Disabled = true
	case SessionReviewLegacy:
		config.Disabled = false
		if choice.Model != nil {
			config.Reviewers = ApplyLegacyOverride(config.Reviewers, choice.Count, choice.Model, choice.Effort, r)
			config.ClearLegacy()
			return config
		}
		config.Reviewers = ApplyLegacyOverride(config.Reviewers, choice.Count, nil, choice.Effort, r)
		return config
	case SessionReviewInherit:
		config.Disabled = false
		if choice.Reviewers != nil {
			config.Reviewers = append([]SubagentChoice{}, *choice.Reviewers...)
			config.ClearLegacy()
		}
	case SessionReviewPush:
		config.Disabled = false
		if choice.Reviewers != nil {
			config.Reviewers = append([]SubagentChoice{}, *choice.Reviewers...)
			config.ClearLegacy()
		}
	}
	return config
}

func ApplyLegacyOverride(list []SubagentChoice, count *int, model, effort *string, global ReviewConfig) []SubagentChoice {
	if model != nil {
		var uniform *SubagentChoice
		if len(global.Reviewers) > 0 {
			first := global.Reviewers[0]
			uniform = &first
			for _, slot := range global.Reviewers {
				if !slot.equal(first) {
					uniform = nil
					break
				}
			}
		}
		n := 2
		switch {
		case count != nil:
			n = *count
		case uniform != nil:
			n = len(global.Reviewers)
		case global.Legacy != nil && global.Legacy.Count != nil:
			n = *global.Legacy.Count
		}
		n = min(max(n, 1), MaxReviewers)
		inheritedEffort := cloneStringPointer(effort)
		if inheritedEffort == nil && uniform != nil {
			inheritedEffort = cloneStringPointer(uniform.Effort)
		}
		if inheritedEffort == nil && global.Legacy != nil {
			inheritedEffort = cloneStringPointer(global.Legacy.Effort)
		}
		result := make([]SubagentChoice, n)
		for i := range result {
			result[i] = SubagentChoice{Model: *model, Effort: cloneStringPointer(inheritedEffort)}
		}
		return result
	}
	result := append([]SubagentChoice{}, list...)
	if effort != nil {
		for i := range result {
			result[i].Effort = cloneStringPointer(effort)
		}
	}
	if count != nil && len(result) > 0 {
		last := result[len(result)-1]
		size := min(max(*count, 1), MaxReviewers)
		for len(result) < size {
			result = append(result, SubagentChoice{Model: last.Model, Effort: cloneStringPointer(last.Effort)})
		}
		result = result[:size]
	}
	return result
}

// equal is SubagentChoice's PartialEq.
func (s SubagentChoice) equal(other SubagentChoice) bool {
	return s.Model == other.Model && equalStringPointers(s.Effort, other.Effort)
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

func defaultSubagentLimit() int { return 8 }

func isDefaultSubagentLimit(value int) bool { return value == defaultSubagentLimit() }

// SubagentConfig is the global policy for Mjolnir-managed child agents.
// Sessions choose Native or Mjolnir; task model and effort choices are
// global lists of equal-peer `(model, effort)` pairs.
//
// (Ported from the Rust source after this package's base: the categories
// became lists and the YOLO split was dropped.)
type SubagentConfig struct {
	// Primary is the choices for top-level primary auto-launch and
	// orchestrators.
	Primary         []SubagentChoice `json:"primary,omitempty" toml:"primary,omitempty"`
	Straightforward []SubagentChoice `json:"straightforward,omitempty" toml:"straightforward,omitempty"`
	Subtle          []SubagentChoice `json:"subtle,omitempty" toml:"subtle,omitempty"`
	// Oracle is the read-only advisor behind `ask_oracle`. Optional, and
	// independent of the other categories.
	Oracle        []SubagentChoice `json:"oracle,omitempty" toml:"oracle,omitempty"`
	MaxConcurrent int              `json:"max_concurrent" toml:"max_concurrent"`
}

func (s SubagentConfig) MarshalJSON() ([]byte, error) {
	var object serdeObject
	object.fieldUnless(len(s.Primary) == 0, "primary", s.Primary)
	object.fieldUnless(len(s.Straightforward) == 0, "straightforward", s.Straightforward)
	object.fieldUnless(len(s.Subtle) == 0, "subtle", s.Subtle)
	object.fieldUnless(len(s.Oracle) == 0, "oracle", s.Oracle)
	object.fieldUnless(isDefaultSubagentLimit(s.MaxConcurrent), "max_concurrent", s.MaxConcurrent)
	return object.finish()
}

// UnmarshalJSON reads serde's SubagentConfigInput: each category is one
// choice or a list of them, and the retired keys (`enabled`,
// `eligible_profiles`, `use_same_on_all_targets`, `yolo`) are accepted and
// dropped so that old files still load.
func (s *SubagentConfig) UnmarshalJSON(data []byte) error {
	var input subagentConfigInput
	if err := json.Unmarshal(data, &input); err != nil {
		return err
	}
	config := DefaultSubagentConfig()
	config.Primary = input.Primary.choices
	config.Straightforward = input.Straightforward.choices
	config.Subtle = input.Subtle.choices
	config.Oracle = input.Oracle.choices
	if input.MaxConcurrent != nil {
		config.MaxConcurrent = *input.MaxConcurrent
	}
	*s = config
	return nil
}

// retiredKeys is the keys this build reads only so that old files load. A
// save removes them by name: they are absent from both serializations it
// compares, as a default the user wrote out is too. "*" matches any key.
var retiredKeys = [][]string{
	{"show_stopped_sessions"},
	{"continuation"},
	{"mailbox"},
	{"build_cache"},
	{"startup"},
	{"sessionwiki", "enabled"},
	{"subagents", "enabled"},
	{"subagents", "eligible_profiles"},
	{"subagents", "use_same_on_all_targets"},
	{"subagents", "yolo"},
	{"quota", "runtime"},
	{"profiles", "*", "muse_auto_review"},
	{"profiles", "*", "subagents"},
}

// subagentConfigInput is serde's SubagentConfigInput, the shape
// SubagentConfig is read through (`#[serde(from = ...)]`).
type subagentConfigInput struct {
	Primary         oneOrManyChoices `json:"primary"`
	Straightforward oneOrManyChoices `json:"straightforward"`
	Subtle          oneOrManyChoices `json:"subtle"`
	Oracle          oneOrManyChoices `json:"oracle"`
	MaxConcurrent   *int             `json:"max_concurrent"`
	// The rest are accepted only so that old files still load.
	Enabled             *bool            `json:"enabled"`
	EligibleProfiles    *map[string]bool `json:"eligible_profiles"`
	UseSameOnAllTargets *bool            `json:"use_same_on_all_targets"`
	Yolo                *json.RawMessage `json:"yolo"`
}

// oneOrManyChoices is serde's untagged OneOrManyChoices: one choice table, or
// a list of them. Null or absent is none.
type oneOrManyChoices struct {
	choices []SubagentChoice
}

func (o *oneOrManyChoices) UnmarshalJSON(data []byte) error {
	trimmed := bytes.TrimSpace(data)
	if bytes.Equal(trimmed, []byte("null")) {
		o.choices = nil
		return nil
	}
	var one SubagentChoice
	if err := json.Unmarshal(data, &one); err == nil {
		o.choices = []SubagentChoice{one}
		return nil
	}
	var many []SubagentChoice
	if err := json.Unmarshal(data, &many); err == nil {
		o.choices = many
		return nil
	}
	return errors.New("data did not match any variant of untagged enum OneOrManyChoices")
}

func validateSubagentCategory(task string, choices []SubagentChoice) error {
	type pair struct {
		model  string
		effort string
		has    bool
	}
	seenPairs := map[pair]bool{}
	seenProviders := map[string]string{}
	for _, choice := range choices {
		if strings.TrimSpace(choice.Model) == "" {
			return fmt.Errorf("[subagents.%s] model cannot be empty", task)
		}
		if choice.Effort != nil && *choice.Effort == "adaptive" {
			return fmt.Errorf("[subagents.%s] adaptive effort is no longer supported", task)
		}
		key := pair{model: NormalizeModel(strings.TrimSpace(choice.Model))}
		if choice.Effort != nil {
			key.effort, key.has = *choice.Effort, true
		}
		if seenPairs[key] {
			return fmt.Errorf("[subagents.%s] duplicate model/effort entry (%s)", task, choice.Model)
		}
		seenPairs[key] = true
		if provider := ModelProvider(choice.Model); provider != nil {
			identity := fmt.Sprintf("%d/%s", QuotaProviderOrdinal(provider), provider.DisplayName())
			if previous, ok := seenProviders[identity]; ok {
				return fmt.Errorf("[subagents.%s] may configure at most one model per provider (both %s and %s belong to %s)",
					task, previous, choice.Model, provider.DisplayName())
			}
			seenProviders[identity] = choice.Model
		}
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
	return SubagentConfig{MaxConcurrent: defaultSubagentLimit()}
}

// PrimaryChoices is the choices configured for top-level primary
// auto-launch and orchestrators.
func (s SubagentConfig) PrimaryChoices() []SubagentChoice { return s.Primary }

// PrimaryConfigured is whether at least one primary choice is configured.
func (s SubagentConfig) PrimaryConfigured() bool { return anyChoiceNamesAModel(s.Primary) }

func anyChoiceNamesAModel(choices []SubagentChoice) bool {
	for _, choice := range choices {
		if strings.TrimSpace(choice.Model) != "" {
			return true
		}
	}
	return false
}

// Choices is the configured choices for taskType.
func (s SubagentConfig) Choices(taskType SubagentTaskType) []SubagentChoice {
	switch taskType {
	case SubagentTaskTypeSubtle:
		return s.Subtle
	case SubagentTaskTypeOracle:
		return s.Oracle
	case SubagentTaskTypeOrchestrator:
		return s.Primary
	}
	return s.Straightforward
}

// EffectiveChoices is a session's effective choices for taskType: its own
// override when set, else the global choices for that category.
func (s SubagentConfig) EffectiveChoices(policy SubagentPolicy, taskType SubagentTaskType) []SubagentChoice {
	if choice := policy.OverrideChoice(taskType); choice != nil {
		return []SubagentChoice{*choice}
	}
	return s.Choices(taskType)
}

// OffersSubtle reports whether a parent with policy may spawn subtle
// children. Without a subtle choice, every child uses the straightforward
// choices.
func (s SubagentConfig) OffersSubtle(policy SubagentPolicy) bool {
	return len(s.EffectiveChoices(policy, SubagentTaskTypeSubtle)) > 0
}

// OracleConfigured reports whether sessions can be offered an oracle.
func (s SubagentConfig) OracleConfigured() bool { return anyChoiceNamesAModel(s.Oracle) }

// IsConfigured: straightforward is required; subtle is optional.
func (s SubagentConfig) IsConfigured() bool { return anyChoiceNamesAModel(s.Straightforward) }

func (s SubagentConfig) isDefault() bool {
	return s.MaxConcurrent == defaultSubagentLimit() && len(s.Primary) == 0 &&
		len(s.Straightforward) == 0 && len(s.Subtle) == 0 && len(s.Oracle) == 0
}

// Validate checks every category's choices and the concurrency limit.
func (s SubagentConfig) Validate() error {
	for _, category := range []struct {
		task    string
		choices []SubagentChoice
	}{
		{"primary", s.Primary},
		{"straightforward", s.Straightforward},
		{"subtle", s.Subtle},
		{"oracle", s.Oracle},
	} {
		if err := validateSubagentCategory(category.task, category.choices); err != nil {
			return err
		}
	}
	if len(s.Subtle) > 0 && len(s.Straightforward) == 0 {
		return errors.New("[subagents.subtle] requires [subagents.straightforward]; configure straightforward alone or both")
	}
	if s.MaxConcurrent < 1 || s.MaxConcurrent > 64 {
		return errors.New("[subagents] `max_concurrent` must be between 1 and 64")
	}
	return nil
}

func (s SubagentConfig) validate() error { return s.Validate() }

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

// JevConfig is the settings for the hosted Jev service. When it is off,
// Mjolnir sends nothing to Jev: no turn or GitHub-item classification and no
// quota recovery. `[jev] enabled = false`.
type JevConfig struct {
	Enabled bool `json:"enabled" toml:"enabled"`
	// FastinoKey is the user's own Fastino key, used in place of the shared
	// Jev limits. Nil when absent.
	FastinoKey EnvironmentValue `json:"fastino_key,omitempty" toml:"fastino_key,omitempty"`
}

// DefaultJevConfig is JevConfig's Default.
func DefaultJevConfig() JevConfig { return JevConfig{Enabled: true} }

// String is Rust's Debug for JevConfig, which never shows the key.
func (j JevConfig) String() string {
	return fmt.Sprintf("JevConfig { enabled: %t, fastino_key_set: %t }", j.Enabled, j.FastinoKey != nil)
}

func (j JevConfig) MarshalJSON() ([]byte, error) {
	var object serdeObject
	object.fieldUnless(isTrue(j.Enabled), "enabled", j.Enabled)
	object.fieldUnless(j.FastinoKey == nil, "fastino_key", j.FastinoKey)
	return object.finish()
}

func (j *JevConfig) UnmarshalJSON(data []byte) error {
	type plain JevConfig
	value := plain(DefaultJevConfig())
	aux := struct {
		*plain
		FastinoKey json.RawMessage `json:"fastino_key"`
	}{plain: &value}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	value.FastinoKey = nil
	if len(aux.FastinoKey) > 0 && string(bytes.TrimSpace(aux.FastinoKey)) != "null" {
		key, err := UnmarshalEnvironmentValueJSON(aux.FastinoKey)
		if err != nil {
			return err
		}
		value.FastinoKey = key
	}
	*j = JevConfig(value)
	return nil
}

func (j JevConfig) isDefault() bool { return j == DefaultJevConfig() }

// JevConfigValidateFastinoKey is Rust's JevConfig::validate_fastino_key.
func JevConfigValidateFastinoKey(key string) error {
	if len(key) > 4096 {
		return errors.New("A Fastino key can be at most 4096 bytes.")
	}
	if strings.IndexFunc(key, unicode.IsSpace) >= 0 {
		return errors.New("A Fastino key can contain no whitespace.")
	}
	for i := 0; i < len(key); i++ {
		if key[i] < 0x21 || key[i] > 0x7e {
			return errors.New("A Fastino key must use printable ASCII.")
		}
	}
	return nil
}

// ResolveFastinoKey resolves the optional key on the host, before creating
// the caller. Nil when Jev is off, no key is set, or the key is empty.
func (j JevConfig) ResolveFastinoKey() (*string, error) {
	if j.FastinoKey == nil || !j.Enabled {
		return nil, nil
	}
	key, err := resolveActive("jev.fastino_key", j.FastinoKey)
	if err != nil {
		return nil, err
	}
	if err := JevConfigValidateFastinoKey(key); err != nil {
		return nil, err
	}
	if key == "" {
		return nil, nil
	}
	return &key, nil
}

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

// ProfileApprovalChoices is profile's approval choices under this
// configuration: the global [guardian] switch can turn the Mjolnir guardian
// off for every profile.
func (c Config) ProfileApprovalChoices(profile HarnessProfile) ApprovalChoices {
	choices := profile.ApprovalChoices()
	choices.MjolnirGuardian = choices.MjolnirGuardian && c.Guardian.Enabled
	return choices
}

// ProfilesDiscoveryKey is the identity of the configured profile list and the
// managed harness builds its probes would run, excluding policies and UI
// defaults.
func (c Config) ProfilesDiscoveryKey() string {
	inputs := []string{}
	for _, id := range sortedKeys(c.Profiles) {
		inputs = append(inputs, c.Profiles[id].CapabilitiesKey(id))
	}
	return sha256JSONHex(inputs, "profile identities serialize")
}

// OrchestratorsConfigured is whether the orchestrator pool is configured.
func (c Config) OrchestratorsConfigured() bool { return len(c.Subagents.Primary) > 0 }

// sha256JSONHex is the lowercase hex SHA-256 of value's serde_json bytes.
func sha256JSONHex(value any, what string) string {
	data, err := serdeJSON(value)
	if err != nil {
		panic(fmt.Sprintf("%s: %v", what, err))
	}
	digest := sha256.Sum256(data)
	return LowerHex(digest[:])
}

// SelectedModels is the models this configuration selects on profile id,
// whose efforts its settings offer, sorted: the sub-agent choices, which any
// enabled profile may serve, and the reviewer's model on the review profile.
// Only these and each profile's current model have their efforts discovered
// ahead of a request; aggregating harnesses advertise hundreds of models.
func (c Config) SelectedModels(id string) []string {
	models := map[string]struct{}{}
	for _, choices := range [][]SubagentChoice{
		c.Subagents.Primary, c.Subagents.Straightforward, c.Subagents.Subtle, c.Subagents.Oracle,
	} {
		for _, choice := range choices {
			models[choice.Model] = struct{}{}
		}
	}
	_ = id
	for _, choice := range c.Review.Reviewers {
		models[choice.Model] = struct{}{}
	}
	if writer := c.Review.BriefWriter; writer != nil {
		if choice := writer.Choice(); choice != nil {
			models[choice.Model] = struct{}{}
		}
	}
	return sortedKeys(models)
}

// CapabilityDemandKey is the identity of what capability hydration covers for
// this configuration: its profile definitions and the models each enabled
// profile is selected for.
func (c Config) CapabilityDemandKey() string {
	selected := map[string][]string{}
	for id := range c.EnabledProfiles() {
		selected[id] = c.SelectedModels(id)
	}
	return sha256JSONHex([]any{c.ProfilesDiscoveryKey(), selected}, "capability demand serializes")
}

// SubagentDiscoveryKey is an opaque identity safe to send to control surfaces
// without exposing profile environments. Policies and unrelated preferences
// are absent.
func (c Config) SubagentDiscoveryKey(parent string, model *string) string {
	return sha256JSONHex([]any{parent, model, c.SubagentDiscoveryInputs()}, "discovery inputs serialize")
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
const LegacyEC2TargetNotice = "Per-session EC2 targets were removed. Choose a persistent Podman target, or create one with `mj ec2 create <instance-type> --zone <zone> --cpus <count> --memory-gib <GiB>`."

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
	readsBuildCache := func(section, kind string) bool {
		if section == "machines" {
			return kind == "local" || kind == "ssh"
		}
		// Container runtimes, under their current kinds and the fused kinds
		// of files before version 12.
		switch kind {
		case "podman", "docker", "apple-container", "nitro", "local-podman", "local-docker",
			"ssh-podman", "ssh-docker", "ssh-apple-container", "ssh-nitro":
			return true
		}
		return false
	}
	unread := []string{}
	for _, section := range []string{"machines", "targets"} {
		tables, ok := document[section].(map[string]any)
		if !ok {
			continue
		}
		for _, id := range sortedKeys(tables) {
			table, _ := tables[id].(map[string]any)
			kind, _ := table["kind"].(string)
			buildCache, ok := table["build_cache"]
			if !ok || !readsBuildCache(section, kind) {
				continue
			}
			// The whole file has already been read, this table with it, so
			// only the names this read gathers matter.
			walkUnreadSettings(reflect.TypeFor[TargetBuildCache](), buildCache, nil, func(key []string) {
				unread = append(unread, section+"."+id+".build_cache."+strings.Join(key, "."))
			})
		}
	}
	return unread
}

// settingInputs maps a configuration type to the type serde actually reads
// it through (`#[serde(from = ...)]`), so the unread-key walk sees the keys
// its reader knows.
var settingInputs = map[reflect.Type]reflect.Type{
	reflect.TypeFor[Config]():           reflect.TypeFor[storedConfig](),
	reflect.TypeFor[ReviewConfig]():     reflect.TypeFor[reviewConfigInput](),
	reflect.TypeFor[TargetBuildCache](): reflect.TypeFor[buildCacheSettings](),
	reflect.TypeFor[SubagentConfig]():   reflect.TypeFor[subagentConfigInput](),
}

// consumedSettings is the keys a type reads whole without a field of its own
// (discard_legacy_setting), which therefore count as read.
var consumedSettings = map[reflect.Type][]string{
	reflect.TypeFor[storedConfig](): {"startup"},
	reflect.TypeFor[QuotaConfig]():  {"runtime"},
}

// walkUnreadSettings reports, as key paths, every key of value (a decoded
// TOML or JSON document) that the reader of type t does not read: what
// serde_ignored reports for a serde reader. Readers whose input serde
// buffers (tagged and untagged enums, flattened members, generic values) read
// everything, as interfaces, raw messages and types without exported fields
// do here.
func walkUnreadSettings(t reflect.Type, value any, path []string, report func([]string)) {
	for t.Kind() == reflect.Pointer {
		t = t.Elem()
	}
	if input, ok := settingInputs[t]; ok {
		t = input
	}
	child := func(key string) []string {
		return append(append([]string{}, path...), key)
	}
	switch t.Kind() {
	case reflect.Map:
		object, ok := value.(map[string]any)
		if !ok {
			return
		}
		for _, key := range sortedKeys(object) {
			walkUnreadSettings(t.Elem(), object[key], child(key), report)
		}
	case reflect.Slice, reflect.Array:
		if t.Elem().Kind() == reflect.Uint8 {
			return
		}
		items, ok := value.([]any)
		if !ok {
			return
		}
		for index, item := range items {
			walkUnreadSettings(t.Elem(), item, child(strconv.Itoa(index)), report)
		}
	case reflect.Struct:
		object, ok := value.(map[string]any)
		if !ok {
			return
		}
		fields := settingFields(t)
		if fields == nil {
			return
		}
		for _, key := range sortedKeys(object) {
			field, known := fields[key]
			if !known {
				report(child(key))
				continue
			}
			if field != nil {
				walkUnreadSettings(field, object[key], child(key), report)
			}
		}
	}
}

// settingFields is the keys a struct type reads, with the type each is read
// as (nil for a key read whole). Nil for a type without exported fields,
// which reads its input some other way.
func settingFields(t reflect.Type) map[string]reflect.Type {
	fields := map[string]reflect.Type{}
	exported := false
	var collect func(t reflect.Type)
	collect = func(t reflect.Type) {
		for i := 0; i < t.NumField(); i++ {
			field := t.Field(i)
			if field.Anonymous && field.Type.Kind() == reflect.Struct {
				collect(field.Type)
				continue
			}
			if !field.IsExported() {
				continue
			}
			exported = true
			name, _, _ := strings.Cut(field.Tag.Get("json"), ",")
			if name == "-" {
				continue
			}
			if name == "" {
				name = field.Name
			}
			fields[name] = field.Type
		}
	}
	collect(t)
	if !exported {
		return nil
	}
	for _, key := range consumedSettings[t] {
		fields[key] = nil
	}
	return fields
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
	for _, bundleID := range sortedKeys(i.Bundles) {
		images := i.Bundles[bundleID]
		if _, known := bundles[bundleID]; !known {
			imagesReportedUnknownBundles.Lock()
			_, reported := imagesReportedUnknownBundles.ids[bundleID]
			imagesReportedUnknownBundles.ids[bundleID] = struct{}{}
			imagesReportedUnknownBundles.Unlock()
			if !reported {
				slog.Warn(fmt.Sprintf("image assignments for unknown bundle %s are ignored; the entry is retained in config.toml",
					debugString(bundleID)), "bundle", bundleID)
			}
			continue
		}
		for _, platform := range sortedKeys(images) {
			image := images[platform]
			architecture, ok := strings.CutPrefix(platform, "linux/")
			valid := ok && architecture != "" && !strings.Contains(architecture, "/")
			for j := 0; valid && j < len(architecture); j++ {
				c := architecture[j]
				valid = c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || c == '_' || c == '-'
			}
			if !valid {
				return fmt.Errorf("bundle %s image platform %s must be linux/<arch>", debugString(bundleID), debugString(platform))
			}
			if strings.TrimSpace(image) == "" || strings.IndexFunc(image, unicode.IsSpace) >= 0 {
				return fmt.Errorf("bundle %s image for %s must be a non-empty image reference without whitespace",
					debugString(bundleID), debugString(platform))
			}
		}
	}
	return nil
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
	Guardian      GuardianConfig
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
	Guardian            GuardianConfig `json:"guardian" toml:"guardian"`
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
	object.fieldUnless(s.Guardian.IsDefault(), "guardian", s.Guardian)
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
	fields, err := decodeObject(data)
	if err != nil {
		return err
	}
	value := storedConfig{
		SessionsSide: DefaultSessionsSide(),
		SidebarSize:  PaneSizeStandard,
		Advanced:     DefaultAdvancedConfig(),
		Notify:       DefaultNotifyConfig(),
		Spinner:      DefaultSpinnerStyle(),
		Theme:        DefaultThemeChoice(),
		Phone:        DefaultPhoneConfig(),
		Daemon:       DefaultDaemonConfig(),
		Github:       DefaultGithubConfig(),
		Guardian:     DefaultGuardianConfig(),
		Mailbox:      defaultLegacyMailboxConfig(),
		Subagents:    DefaultSubagentConfig(),
		BuildCache:   defaultLegacyBuildCacheConfig(),
		Jev:          DefaultJevConfig(),
		Keys:         DefaultKeysConfig(),
		Profiles:     map[string]HarnessProfile{},
		Bundles:      map[string]ProjectBundle{},
		Machines:     map[string]Machine{},
		Targets:      map[string]targetEntry{},
	}
	if _, ok := fields["version"]; !ok {
		return missingField("version")
	}
	plain := map[string]any{
		"show_stopped_sessions": &value.ShowStoppedSessions,
		"sessions_side":         &value.SessionsSide,
		"sidebar_size":          &value.SidebarSize,
		"advanced":              &value.Advanced,
		"notify":                &value.Notify,
		"version":               &value.Version,
		"spinner":               &value.Spinner,
		"phone":                 &value.Phone,
		"daemon":                &value.Daemon,
		"github":                &value.Github,
		"guardian":              &value.Guardian,
		"review":                &value.Review,
		"continuation":          &value.Continuation,
		"mailbox":               &value.Mailbox,
		"sessionwiki":           &value.Sessionwiki,
		"subagents":             &value.Subagents,
		"build_cache":           &value.BuildCache,
		"jev":                   &value.Jev,
		"quota":                 &value.Quota,
		"keys":                  &value.Keys,
		"profiles":              &value.Profiles,
		"bundles":               &value.Bundles,
		"images":                &value.Images,
	}
	for _, key := range sortedKeys(fields) {
		raw := fields[key]
		var err error
		switch key {
		case "theme":
			value.Theme, err = UnmarshalThemeChoiceJSON(raw)
		case "startup":
			err = discardLegacySetting(raw)
		case "machines":
			var machines map[string]json.RawMessage
			if err = json.Unmarshal(raw, &machines); err != nil {
				break
			}
			for id, encoded := range machines {
				machine, machineErr := UnmarshalMachineJSON(encoded)
				if machineErr != nil {
					err = machineErr
					break
				}
				value.Machines[id] = machine
			}
		case "targets":
			var targets map[string]json.RawMessage
			if err = json.Unmarshal(raw, &targets); err != nil {
				break
			}
			for id, encoded := range targets {
				entry, entryErr := unmarshalTargetEntryJSON(encoded)
				if entryErr != nil {
					err = entryErr
					break
				}
				value.Targets[id] = entry
			}
		default:
			if target, ok := plain[key]; ok {
				err = json.Unmarshal(raw, target)
			}
		}
		if err != nil {
			return err
		}
	}
	if value.Profiles == nil {
		value.Profiles = map[string]HarnessProfile{}
	}
	if value.Bundles == nil {
		value.Bundles = map[string]ProjectBundle{}
	}
	*s = value
	return nil
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
	known := func(key string) bool {
		if key == "kind" || key == "machine" {
			return true
		}
		switch kind {
		case "bare":
			return key == "permissions"
		case "podman", "docker", "apple-container", "nitro":
			for _, setting := range containerTemplateKeys {
				if key == setting {
					return true
				}
			}
			return false
		}
		return true
	}
	newlyUnknownTargetKeysReported.Lock()
	defer newlyUnknownTargetKeysReported.Unlock()
	unknown := []string{}
	for _, key := range sortedKeys(table) {
		if known(key) {
			continue
		}
		pair := [2]string{id, key}
		if _, reported := newlyUnknownTargetKeysReported.pairs[pair]; reported {
			continue
		}
		newlyUnknownTargetKeysReported.pairs[pair] = struct{}{}
		unknown = append(unknown, key)
	}
	return unknown
}

func interpretTarget(id string, value any, version uint32) (interpretedTarget, error) {
	table, ok := value.(map[string]any)
	if !ok {
		return nil, fmt.Errorf("target %s must be a table", debugString(id))
	}
	kind, ok := table["kind"].(string)
	if !ok {
		return nil, fmt.Errorf("target %s needs a kind: one of %s", debugString(id), strings.Join(storedTargetKinds[:], ", "))
	}
	if _, ok := table["permissions"]; ok && kind != "bare" && kind != "ssh-bare" {
		return nil, fmt.Errorf("target %s sets `permissions`, which only applies to a bare runtime", debugString(id))
	}
	// The message must stand alone: a failure here becomes a plain string
	// inside the TOML or JSON deserializer's own error, which keeps no source
	// chain.
	data, err := json.Marshal(value)
	if err != nil {
		return nil, err
	}
	stored := false
	for _, candidate := range storedTargetKinds {
		stored = stored || candidate == kind
	}
	if stored {
		// `apple-container` is spelled the same before and after the split.
		// In an old file without a `machine` key it is the fused kind.
		_, hasMachine := table["machine"]
		fused := kind == "apple-container" && !hasMachine && version <= 11
		if !fused {
			for _, key := range newlyUnknownTargetKeys(id, kind, table) {
				slog.Warn(fmt.Sprintf("target %s (%s) has a key Mjolnir does not use, %s; it is ignored",
					debugString(id), kind, debugString(key)), "target", id, "key", key)
			}
			target, err := UnmarshalStoredTargetJSON(data)
			if err != nil {
				return nil, fmt.Errorf("target %s (%s): %s", debugString(id), kind, err)
			}
			return interpretedTargetStored{Target: target}, nil
		}
	} else if newKind, machine, ok := legacyKindAdvice(kind); !ok {
		return nil, fmt.Errorf("target %s has unknown kind %s; use one of %s",
			debugString(id), debugString(kind), strings.Join(storedTargetKinds[:], ", "))
	} else if version > 11 {
		return nil, fmt.Errorf("target %s uses the old kind %s; write kind = %s and machine = %s",
			debugString(id), debugString(kind), debugString(newKind), debugString(machine))
	}
	target, err := UnmarshalTargetTemplateJSON(data)
	if err != nil {
		return nil, fmt.Errorf("target %s (%s): %s", debugString(id), kind, err)
	}
	return interpretedTargetLegacy{Target: target}, nil
}

// configFromStored is TryFrom<StoredConfig> for Config.
func configFromStored(stored storedConfig) (Config, error) {
	stripProjectCredentials(stored.Bundles)
	github := stored.Github
	github.Watch.Enabled = github.Watch.Enabled && stored.Mailbox.Enabled
	if err := github.Validate(); err != nil {
		return Config{}, err
	}
	profiles := make(map[string]HarnessProfile, len(stored.Profiles))
	for id, profile := range stored.Profiles {
		profile.InheritProviderKey()
		profiles[id] = profile
	}
	machines := make(map[string]Machine, len(stored.Machines))
	for id, machine := range stored.Machines {
		machines[id] = machine
	}
	runtimes := map[string]StoredTarget{}
	disabledTargets := map[string]struct{}{}
	for _, id := range sortedKeys(stored.Targets) {
		var runtime StoredTarget
		switch entry := stored.Targets[id].(type) {
		case targetEntryStored:
			runtime = entry.Target
		case targetEntryDisabled:
			if !entry.Value.Enabled {
				disabledTargets[id] = struct{}{}
			}
			runtime = entry.Value.Target
		case targetEntryRaw:
			enabled, err := takeTargetEnabled(id, entry.Value)
			if err != nil {
				return Config{}, err
			}
			if !enabled {
				disabledTargets[id] = struct{}{}
			}
			interpreted, err := interpretTarget(id, entry.Value, stored.Version)
			if err != nil {
				return Config{}, err
			}
			switch interpreted := interpreted.(type) {
			case interpretedTargetStored:
				runtime = interpreted.Target
			case interpretedTargetLegacy:
				// Migration and saving are the same operation, so an old
				// file becomes exactly what the next save would write.
				runtime = StoredTargetOf(id, interpreted.Target, machines)
			}
		}
		runtimes[id] = runtime
	}
	// Fold the old global veto into each machine before resolving runtimes.
	// Local is implicit even in files that only name remote machines.
	if !stored.BuildCache.Enabled {
		if _, ok := machines[LocalMachineID]; !ok {
			machines[LocalMachineID] = MachineLocal{}
		}
		disabled := func(cache *TargetBuildCache) *TargetBuildCache {
			var updated TargetBuildCache
			if cache != nil {
				updated = *cache
			}
			off := false
			updated.Enabled = &off
			return &updated
		}
		for id, machine := range machines {
			switch machine := machine.(type) {
			case MachineLocal:
				machine.TargetBuildCache = disabled(machine.TargetBuildCache)
				machines[id] = machine
			case MachineSsh:
				machine.TargetBuildCache = disabled(machine.TargetBuildCache)
				machines[id] = machine
			}
		}
	}
	resolved := make(map[string]TargetTemplate, len(runtimes))
	for _, id := range sortedKeys(runtimes) {
		target, err := ResolveTarget(id, runtimes[id], machines)
		if err != nil {
			return Config{}, err
		}
		resolved[id] = target
	}
	// Up to version 14, a container target the user had touched carried the
	// then-current default image in the file. Move that literal back onto the
	// current session default; a customized image is left alone.
	if stored.Version <= legacyDefaultImageLastVersion {
		for id, target := range resolved {
			migrateLegacyDefaultImage(&target, DefaultSessionImage)
			resolved[id] = target
		}
	}
	// Versions 1 through 23 acquire this build's defaults in memory and
	// upgrade on the next ordinary save. Version 12 splits machines from
	// runtimes; version 13 added automatic continuation (since removed);
	// version 14 moves the global cache opt-out onto machines; version 15
	// stops writing the default image into a container target; 16 adds
	// native tool cache storage; 17 replaces per-profile delegation
	// selectors with global task choices, without migrating them; 18 moves
	// the GitHub event switch to github.watch.enabled; 19 adds `enabled` to
	// runtimes; 20 adds `[daemon]`; 21 adds review counts; 22 adds the
	// session GitHub credential default; 23 adds target-specific sub-agent
	// choices; 24 generalizes [subagents] into multi-pair category lists and
	// drops [subagents.yolo.*].
	version := stored.Version
	if version >= 1 && version <= 23 {
		version = ConfigVersion
	}
	bundles := stored.Bundles
	if bundles == nil {
		bundles = map[string]ProjectBundle{}
	}
	return Config{
		SessionsSide:    stored.SessionsSide,
		SidebarSize:     stored.SidebarSize,
		Advanced:        stored.Advanced,
		Notify:          stored.Notify,
		Version:         version,
		Spinner:         stored.Spinner,
		Theme:           stored.Theme,
		Phone:           stored.Phone,
		Daemon:          stored.Daemon,
		Github:          github,
		Guardian:        stored.Guardian,
		Review:          stored.Review,
		Sessionwiki:     stored.Sessionwiki,
		Subagents:       stored.Subagents,
		Jev:             stored.Jev,
		Quota:           stored.Quota,
		Keys:            stored.Keys,
		Profiles:        profiles,
		Bundles:         bundles,
		Images:          stored.Images,
		Machines:        machines,
		Targets:         resolved,
		DefaultTargets:  map[string]struct{}{},
		DisabledTargets: disabledTargets,
	}, nil
}

// cloneProjectBundles copies bundles deeply enough that stripping the copy's
// credentials leaves the original alone.
func cloneProjectBundles(bundles map[string]ProjectBundle) map[string]ProjectBundle {
	if bundles == nil {
		return nil
	}
	cloned := make(map[string]ProjectBundle, len(bundles))
	for id, bundle := range bundles {
		bundle.Repositories = slices.Clone(bundle.Repositories)
		cloned[id] = bundle
	}
	return cloned
}

// stripProjectCredentials removes HTTP userinfo from every bundle
// repository's source, in place.
func stripProjectCredentials(bundles map[string]ProjectBundle) {
	for _, bundle := range bundles {
		for index := range bundle.Repositories {
			bundle.Repositories[index].stripHTTPUserinfo()
		}
	}
}

// storedConfigFrom is From<Config> for StoredConfig.
func storedConfigFrom(config Config) storedConfig {
	config.Bundles = cloneProjectBundles(config.Bundles)
	stripProjectCredentials(config.Bundles)
	machines := make(map[string]Machine, len(config.Machines))
	for id, machine := range config.Machines {
		machines[id] = machine
	}
	targets := make(map[string]targetEntry, len(config.Targets))
	for _, id := range sortedKeys(config.Targets) {
		runtime := StoredTargetOf(id, config.Targets[id], machines)
		if _, disabled := config.DisabledTargets[id]; disabled {
			targets[id] = targetEntryDisabled{Value: disabledTarget{Target: runtime, Enabled: false}}
		} else {
			targets[id] = targetEntryStored{Target: runtime}
		}
	}
	// This machine is implied, so a file only names it when it carries
	// settings of its own.
	if local, ok := machines[LocalMachineID].(MachineLocal); ok && local.TargetBuildCache == nil {
		delete(machines, LocalMachineID)
	}
	return storedConfig{
		SessionsSide: config.SessionsSide,
		SidebarSize:  config.SidebarSize,
		Advanced:     config.Advanced,
		Notify:       config.Notify,
		Version:      config.Version,
		Spinner:      config.Spinner,
		Theme:        config.Theme,
		Phone:        config.Phone,
		Daemon:       config.Daemon,
		Github:       config.Github,
		Guardian:     config.Guardian,
		Review:       config.Review,
		Mailbox:      defaultLegacyMailboxConfig(),
		Sessionwiki:  config.Sessionwiki,
		Subagents:    config.Subagents,
		BuildCache:   defaultLegacyBuildCacheConfig(),
		Jev:          config.Jev,
		Quota:        config.Quota,
		Keys:         config.Keys,
		Profiles:     config.Profiles,
		Bundles:      config.Bundles,
		Images:       config.Images,
		Machines:     machines,
		Targets:      targets,
	}
}

// DefaultConfig is Config's Default.
func DefaultConfig() Config {
	return Config{
		SessionsSide:    DefaultSessionsSide(),
		SidebarSize:     PaneSizeStandard,
		Advanced:        DefaultAdvancedConfig(),
		Notify:          DefaultNotifyConfig(),
		Version:         ConfigVersion,
		Spinner:         DefaultSpinnerStyle(),
		Theme:           DefaultThemeChoice(),
		Phone:           DefaultPhoneConfig(),
		Daemon:          DefaultDaemonConfig(),
		Github:          DefaultGithubConfig(),
		Guardian:        DefaultGuardianConfig(),
		Subagents:       DefaultSubagentConfig(),
		Jev:             DefaultJevConfig(),
		Keys:            DefaultKeysConfig(),
		Profiles:        map[string]HarnessProfile{},
		Bundles:         map[string]ProjectBundle{},
		Machines:        map[string]Machine{},
		Targets:         map[string]TargetTemplate{},
		DefaultTargets:  map[string]struct{}{},
		DisabledTargets: map[string]struct{}{},
	}
}

// SessionImage is the image for a session on a container target. Bundle
// assignments take precedence only for the target's platform; bare targets
// have no image.
func (c Config) SessionImage(bundleID *string, targetID, platform string) (string, bool) {
	target, ok := c.Targets[targetID]
	if !ok {
		return "", false
	}
	container := target.Container()
	if container == nil {
		return "", false
	}
	platform = CanonicalImagePlatform(platform)
	if bundleID != nil {
		if _, known := c.Bundles[*bundleID]; known {
			if image, ok := c.Images.Bundles[*bundleID][platform]; ok {
				return image, true
			}
		}
	}
	return container.Image, true
}

// SessionTarget applies the same resolution to a configured target without
// changing it.
func (c Config) SessionTarget(bundleID *string, targetID, platform string) (TargetTemplate, error) {
	target, ok := c.Targets[targetID]
	if !ok {
		return nil, fmt.Errorf("unknown target %s", debugString(targetID))
	}
	if image, ok := c.SessionImage(bundleID, targetID, platform); ok {
		container := target.Container()
		if container == nil {
			panic("session image requires a container")
		}
		container.Image = image
		target = target.withContainer(*container)
	}
	return target, nil
}

// configAdditions is the entries of discovered that existing does not hold
// yet (by same), under collision-free ids derived from base.
func configAdditions[T any](existing, discovered map[string]T, same func(T, T) bool, base func(string, T) string) map[string]T {
	known := make(map[string]T, len(existing))
	for id, value := range existing {
		known[id] = value
	}
	added := map[string]T{}
	for _, id := range sortedKeys(discovered) {
		value := discovered[id]
		duplicate := false
		for _, entry := range known {
			if same(entry, value) {
				duplicate = true
				break
			}
		}
		if duplicate {
			continue
		}
		unique := UniqueConfigID(known, base(id, value))
		known[unique] = value
		added[unique] = value
	}
	return added
}

// SetupAdditions suggests additions for setup without replacing existing
// entries. Both setup surfaces use the same identities and collision-free
// names.
func (c Config) SetupAdditions(discovered Config) Config {
	additions := DefaultConfig()
	sameID := func(id string, _ any) string { return id }
	additions.Profiles = configAdditions(c.Profiles, discovered.Profiles, HarnessProfile.SameInstallation,
		func(id string, _ HarnessProfile) string { return id })
	additions.Bundles = configAdditions(c.Bundles, discovered.Bundles, func(a, b ProjectBundle) bool { return serdeEqual(a, b) },
		func(_ string, bundle ProjectBundle) string { return bundle.PrimaryRepo })
	additions.Machines = configAdditions(c.Machines, discovered.Machines, func(a, b Machine) bool { return serdeEqual(a, b) },
		func(id string, machine Machine) string { return sameID(id, machine) })
	additions.Targets = configAdditions(c.Targets, discovered.Targets, func(a, b TargetTemplate) bool { return serdeEqual(a, b) },
		func(id string, target TargetTemplate) string { return sameID(id, target) })
	return additions
}

// serdeEqual compares two configuration values by their serialized form,
// which is how they are written and read, so it stands in for Rust's
// derived PartialEq without tripping over nil and empty collections.
func serdeEqual(a, b any) bool {
	left, errLeft := serdeJSON(a)
	right, errRight := serdeJSON(b)
	return errLeft == nil && errRight == nil && bytes.Equal(left, right)
}

// MachineRuntimes is the runtimes that run on machine machineID, in id order.
func (c Config) MachineRuntimes(machineID string) iter.Seq2[string, TargetTemplate] {
	return func(yield func(string, TargetTemplate) bool) {
		for _, id := range sortedKeys(c.Targets) {
			target := c.Targets[id]
			if found, ok := TargetMachineID(target, c.Machines); !ok || found != machineID {
				continue
			}
			if !yield(id, target) {
				return
			}
		}
	}
}

// MachinesWithoutRuntimes is the SSH and EC2 machines no runtime runs on,
// which no session can use until one does, in id order.
func (c Config) MachinesWithoutRuntimes() iter.Seq2[string, Machine] {
	return func(yield func(string, Machine) bool) {
		for _, id := range sortedKeys(c.Machines) {
			machine := c.Machines[id]
			if _, local := machine.(MachineLocal); local {
				continue
			}
			used := false
			for range c.MachineRuntimes(id) {
				used = true
				break
			}
			if used {
				continue
			}
			if !yield(id, machine) {
				return
			}
		}
	}
}

// MachineRuntimeAdditions is the runtimes from found to add for machine
// machineID, under collision-free ids: each kind (bare, podman, docker) the
// machine does not run yet. probed is the machine as it was when found was
// learned; when the machine has since changed or gone, nothing is added,
// because what was found describes a host this configuration no longer names.
// Every surface that adds a machine's runtimes, from the file under its lock
// to the Settings draft, decides here.
func (c Config) MachineRuntimeAdditions(machineID string, probed Machine, found iter.Seq2[string, TargetTemplate]) map[string]TargetTemplate {
	added := map[string]TargetTemplate{}
	current, ok := c.Machines[machineID]
	if !ok || !serdeEqual(current, probed) {
		return added
	}
	kinds := map[string]struct{}{}
	for _, target := range c.MachineRuntimes(machineID) {
		kinds[RuntimeKindName(target)] = struct{}{}
	}
	known := make(map[string]TargetTemplate, len(c.Targets))
	for id, target := range c.Targets {
		known[id] = target
	}
	for base, target := range found {
		if runsOn, ok := TargetMachineID(target, c.Machines); !ok || runsOn != machineID {
			continue
		}
		kind := RuntimeKindName(target)
		if _, taken := kinds[kind]; taken {
			continue
		}
		kinds[kind] = struct{}{}
		id := UniqueConfigID(known, base)
		known[id] = target
		added[id] = target
	}
	return added
}

func (c Config) IsUnconfigured() bool {
	return len(c.Profiles) == 0 && len(c.Bundles) == 0 && len(c.Targets) == 0
}

// EnabledProfiles is the profiles Mjolnir may choose or probe automatically,
// in id order.
func (c Config) EnabledProfiles() iter.Seq2[string, HarnessProfile] {
	return func(yield func(string, HarnessProfile) bool) {
		for _, id := range sortedKeys(c.Profiles) {
			profile := c.Profiles[id]
			if profile.Enabled && !yield(id, profile) {
				return
			}
		}
	}
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
	return func(yield func(string, TargetTemplate) bool) {
		for _, id := range sortedKeys(c.Targets) {
			if target, ok := c.EnabledTarget(id); ok && !yield(id, target) {
				return
			}
		}
	}
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
	target, ok := c.Targets[id]
	if !ok {
		return nil, fmt.Errorf("unknown target template %s", debugString(id))
	}
	if notice, refused := target.LaunchRefusal(); refused {
		return nil, errors.New(notice)
	}
	if _, disabled := c.DisabledTargets[id]; disabled {
		return nil, RefusalPrecondition(fmt.Sprintf(
			"runtime %s is turned off; turn it on in Settings › Runtimes or choose another", debugString(id)))
	}
	return target, nil
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
	destination, ok := c.Profiles[destinationProfileID]
	if !ok {
		return false, fmt.Errorf("unknown profile %s", debugString(destinationProfileID))
	}
	if destination.Kind != sourceKind {
		return false, nil
	}
	if destinationProfileID == sourceProfileID {
		return true, nil
	}
	var sourceService ModelService
	if source, ok := c.Profiles[sourceProfileID]; ok && source.Kind == sourceKind {
		service, err := source.ModelService()
		if err != nil {
			return false, err
		}
		sourceService = service
	} else if service, ok := ModelServiceOfHarness(sourceKind); ok {
		sourceService = service
	}
	if sourceService == nil {
		return false, nil
	}
	destinationService, err := destination.ModelService()
	if err != nil {
		return false, err
	}
	return sourceService == destinationService, nil
}

// Validate checks what this file itself declares. Whether a profile or target
// can start depends on harness homes, secrets and variables outside it, and is
// asked of HarnessProfile.EnsureReady and TargetTemplate.EnsureReady where
// they are used, so one unusable profile never stops the configuration, and
// the daemon, from loading.
func (c Config) Validate() error {
	if c.Version != ConfigVersion {
		return fmt.Errorf("unsupported Mjolnir config version %d; expected %d", c.Version, ConfigVersion)
	}
	if err := c.Phone.validate(); err != nil {
		return err
	}
	for _, id := range sortedKeys(c.Profiles) {
		if err := c.Profiles[id].validate(id); err != nil {
			return err
		}
	}
	// Checked after the profiles, so a review pointing at a malformed profile
	// reports the profile's own error first.
	if err := c.Review.validate(c.Profiles); err != nil {
		return err
	}
	if err := c.Sessionwiki.validate(); err != nil {
		return err
	}
	if err := c.Subagents.validate(); err != nil {
		return err
	}
	for _, id := range sortedKeys(c.Bundles) {
		if err := c.Bundles[id].validate(id); err != nil {
			return err
		}
	}
	if err := c.Images.validate(c.Bundles); err != nil {
		return err
	}
	if err := validateMachines(c.Machines); err != nil {
		return err
	}
	for _, id := range sortedKeys(c.Targets) {
		if err := c.Targets[id].validate(id); err != nil {
			return err
		}
	}
	_, err := c.Keys.Resolve()
	return err
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
	path := ConfigPath()
	lock, err := configLockAcquire(path)
	if err != nil {
		return Config{}, nil, err
	}
	guard := &ConfigReadGuard{lock: lock, path: path}
	config, err := ConfigLoadFrom(guard.path)
	if err != nil {
		_ = guard.Close()
		return Config{}, nil, err
	}
	return config.WithLocalTargets(), guard, nil
}

// WithLocalTargets supplies standard local choices without requiring setup or
// writing a file. These are candidates: callers must check availability
// before offering launch. Explicit entries with the same name override the
// standard defaults.
func (c Config) WithLocalTargets() Config {
	container := ContainerTemplate{
		Image:            DefaultSessionImage,
		PullPolicy:       ImagePullPolicyAuto,
		WorkspaceStorage: PodmanWorkspaceStoragePodmanVolume{},
	}
	type candidate struct {
		id       string
		template TargetTemplate
	}
	var defaults []candidate
	if HostRunsWorkers {
		defaults = append(defaults, candidate{"localhost", TargetTemplateLocalBare{}})
	}
	defaults = append(defaults,
		candidate{"podman", TargetTemplateLocalPodman{ContainerTemplate: container}},
		candidate{"docker", TargetTemplateLocalDocker{ContainerTemplate: container}})
	if runtime.GOOS == "darwin" {
		defaults = append(defaults, candidate{"apple-container", TargetTemplateAppleContainer{ContainerTemplate: container}})
	}
	// The value is a copy, so the receiver's maps stay as they were.
	targets := make(map[string]TargetTemplate, len(c.Targets)+len(defaults))
	for id, target := range c.Targets {
		targets[id] = target
	}
	defaultTargets := make(map[string]struct{}, len(c.DefaultTargets)+len(defaults))
	for id := range c.DefaultTargets {
		defaultTargets[id] = struct{}{}
	}
	for _, entry := range defaults {
		if _, taken := targets[entry.id]; !taken {
			targets[entry.id] = entry.template
			defaultTargets[entry.id] = struct{}{}
		}
	}
	c.Targets = targets
	c.DefaultTargets = defaultTargets
	return c
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
	target, ok := c.Targets[id]
	if !ok {
		return false
	}
	standard, isStandard := DefaultConfig().WithLocalTargets().Targets[id]
	return !isStandard || !serdeEqual(standard, target)
}

// warnedYoloPaths is the config files already warned about for removed
// `[subagents.yolo.*]` settings, so each is named once per process.
var warnedYoloPaths = struct {
	sync.Mutex
	paths map[string]struct{}
}{paths: map[string]struct{}{}}

// ConfigLoadFrom reads the config from path, returning DefaultConfig when the
// file is missing and an error when it is malformed or was written by a newer
// Mjolnir.
func ConfigLoadFrom(path string) (Config, error) {
	if _, err := os.Stat(path); err != nil {
		return DefaultConfig(), nil
	}
	contents, err := os.ReadFile(path)
	if err != nil {
		return Config{}, fmt.Errorf("read Mjolnir config %s: %w", path, err)
	}
	if strings.TrimSpace(string(contents)) == "" {
		return DefaultConfig(), nil
	}
	document, err := parseTOMLDocument(string(contents))
	if err != nil {
		return Config{}, configTOMLError(path, string(contents), err)
	}
	if version, ok := newerVersion(document); ok {
		return Config{}, &ConfigLoadErrorNewerSchema{Path: path, Version: version}
	}
	// Every file Mjolnir writes starts with this line, so only a hand-written
	// one lacks it. Serde's "missing field `version`" does not say which
	// value to add (R14-4).
	if _, ok := document["version"]; !ok {
		return Config{}, fmt.Errorf("%s: config.toml needs a `version = %d` line at the top (the current configuration schema); see %s",
			path, ConfigVersion, ConfigurationDocumentationURL)
	}
	if err := rejectRemovedProfileOverrides(string(contents)); err != nil {
		return Config{}, err
	}
	if hasLegacySubagentYoloChoices(document) {
		warnedYoloPaths.Lock()
		_, warned := warnedYoloPaths.paths[path]
		if !warned {
			warnedYoloPaths.paths[path] = struct{}{}
		}
		warnedYoloPaths.Unlock()
		if !warned {
			slog.Warn("ignoring removed [subagents.yolo.*] settings; Mjolnir now uses unified [[subagents.*]] lists across all targets, and the next save removes them",
				"config", path)
		}
	}
	if removed := dropRemovedHarnessProfiles(document); len(removed) > 0 {
		slog.Warn("ignoring profiles for a harness Mjolnir no longer supports; the next save removes them",
			"config", path, "profiles", removed)
	}
	// Earlier configs acquire defaults in memory, and their fused target
	// kinds become machines plus runtimes, on the next ordinary save. The
	// version bump itself happens in configFromStored, which is also what
	// decides whether an old `kind` is still accepted.
	//
	// Environment references resolve against the secrets file beside this
	// configuration and the process environment. One that does not resolve
	// makes its profile or target unusable, not this file unreadable; see
	// HarnessProfile.EnsureReady.
	data, err := json.Marshal(document)
	if err != nil {
		return Config{}, configTOMLError(path, string(contents), err)
	}
	type read struct {
		config Config
		err    error
	}
	result := WithSecretResolver(SecretResolverBeside(path), func() read {
		var config Config
		err := json.Unmarshal(data, &config)
		return read{config, err}
	})
	if result.err != nil {
		return Config{}, configTOMLError(path, string(contents), result.err)
	}
	// The configuration types read past keys they have no field for, because
	// the daemon serves this configuration to clients that may predate a
	// field (#1557). A person writes the file, so here such a key is a
	// mistake, refused by name wherever a type used to refuse it.
	unread := []string{}
	walkUnreadSettings(reflect.TypeFor[Config](), document, nil, func(key []string) {
		// These sections never refused a key, and files still carry retired
		// ones such as `advanced.show_stopped_sessions`. Refusing them now
		// would stop an upgraded daemon from starting.
		if len(key) > 1 && (key[0] == "advanced" || key[0] == "notify") {
			return
		}
		unread = append(unread, strings.Join(key, "."))
	})
	unread = append(unread, unreadBuildCacheKeys(document)...)
	if err := refuseUnreadKeys(path, unread); err != nil {
		return Config{}, err
	}
	if err := result.config.Validate(); err != nil {
		return Config{}, err
	}
	return result.config, nil
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
	lock, err := configLockAcquire(path)
	if err != nil {
		var zero T
		return Config{}, zero, err
	}
	guard := &ConfigReadGuard{lock: lock, path: path}
	defer guard.Close()
	return ConfigUpdateLocked(guard, edit)
}

// ConfigUpdateLocked edits and saves while an existing opaque owner retains
// the config lock. The owner can span a subsequent reference migration or
// rollback.
func ConfigUpdateLocked[T any](guard *ConfigReadGuard, edit func(*Config) (T, error)) (Config, T, error) {
	config, value, err := configUpdateToLocked(guard.path, func(config *Config) (T, error) {
		// Edits see the same local candidates as reads, but unrelated
		// changes must not write implicit defaults into the user's file.
		runtime := config.WithLocalTargets()
		implicit := map[string]TargetTemplate{}
		for id, target := range runtime.Targets {
			if _, written := config.Targets[id]; !written {
				implicit[id] = target
			}
		}
		value, err := edit(&runtime)
		if err != nil {
			return value, err
		}
		for _, id := range sortedKeys(implicit) {
			// A standard runtime switched off is the user's choice and is
			// written with it.
			_, disabled := runtime.DisabledTargets[id]
			if current, ok := runtime.Targets[id]; ok && serdeEqual(current, implicit[id]) && !disabled {
				delete(runtime.Targets, id)
			}
		}
		runtime.DefaultTargets = map[string]struct{}{}
		*config = runtime
		return value, nil
	})
	if err != nil {
		return Config{}, value, err
	}
	return config.WithLocalTargets(), value, nil
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

// ConfigUpdateToCancellable is ConfigUpdateTo with a cancellable lock wait
// and edit.
func ConfigUpdateToCancellable[T any](path string, cancelled func() bool, edit func(*Config) (T, error)) (Config, T, error) {
	var zero T
	file, err := configLockOpen(path)
	if err != nil {
		return Config{}, zero, err
	}
	lock, err := AcquireFileLockCancellable(file, cancelled)
	if err != nil {
		_ = file.Close()
		return Config{}, zero, fmt.Errorf("lock config %s: %w", path, err)
	}
	defer func() {
		lock.Drop()
		_ = file.Close()
	}()
	return configUpdateToLocked(path, func(config *Config) (T, error) {
		if cancelled() {
			return zero, errors.New("configuration edit cancelled")
		}
		value, err := edit(config)
		if err != nil {
			return value, err
		}
		if cancelled() {
			return zero, errors.New("configuration edit cancelled")
		}
		return value, nil
	})
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
	stripProjectCredentials(config.Bundles)
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
	if err := configEnsureWritable(path); err != nil {
		return err
	}
	if err := c.Validate(); err != nil {
		return err
	}
	body, err := marshalTOMLPretty(c)
	if err != nil {
		return fmt.Errorf("serialize Mjolnir config: %w", err)
	}
	if edited, ok := configEditedFile(path, body); ok {
		body = edited
	}
	return AtomicWrite(path, []byte(body))
}

// configEditedFile is the file at path edited in place to hold body, keeping
// its comments and layout. False when there is no file to keep, or it cannot
// be edited in place.
func configEditedFile(path, body string) (string, bool) {
	existing, err := os.ReadFile(path)
	if err != nil {
		return "", false
	}
	loadedConfig, err := ConfigLoadFrom(path)
	if err != nil {
		return "", false
	}
	loaded, err := marshalTOMLPretty(loadedConfig)
	if err != nil {
		return "", false
	}
	return editInPlace(string(existing), loaded, body)
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
	file, err := configLockOpen(configPath)
	if err != nil {
		return nil, err
	}
	if err := configLockExclusive(file); err != nil {
		_ = file.Close()
		return nil, fmt.Errorf("lock config %s: %w", configPath, err)
	}
	return &ConfigLock{file: NewFileLock(file)}, nil
}

func configLockOpen(configPath string) (*os.File, error) {
	lockPath := configLockPath(configPath)
	parent := filepath.Dir(lockPath)
	if err := os.MkdirAll(parent, 0o777); err != nil {
		return nil, fmt.Errorf("create config lock directory %s: %w", parent, err)
	}
	file, err := os.OpenFile(lockPath, os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		return nil, fmt.Errorf("open config lock %s: %w", lockPath, err)
	}
	return file, nil
}

// Close releases the lock (Rust's Drop), then closes the lock file.
func (l *ConfigLock) Close() error {
	if l == nil || l.file == nil {
		return nil
	}
	file := l.file.File()
	err := l.file.Release()
	l.file = nil
	if closeErr := file.Close(); err == nil {
		err = closeErr
	}
	return err
}

func configLockPath(path string) string {
	name, ok := pathFileName(path)
	if !ok {
		return path + ".lock"
	}
	return filepath.Join(filepath.Dir(path), name+".lock")
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
	return json.Marshal(normalizeTOML(value))
}

// normalizeTOML turns what BurntSushi/toml decodes into the generic values
// encoding/json writes: tables as map[string]any, arrays (of tables too) as
// []any, integers as int64 and dates as their TOML text.
func normalizeTOML(value any) any {
	switch value := value.(type) {
	case map[string]any:
		normalized := make(map[string]any, len(value))
		for key, member := range value {
			normalized[key] = normalizeTOML(member)
		}
		return normalized
	case []map[string]any:
		normalized := make([]any, len(value))
		for i, member := range value {
			normalized[i] = normalizeTOML(member)
		}
		return normalized
	case []any:
		normalized := make([]any, len(value))
		for i, member := range value {
			normalized[i] = normalizeTOML(member)
		}
		return normalized
	case time.Time:
		return value.Format(time.RFC3339Nano)
	case fmt.Stringer:
		return value.String()
	}
	return value
}

// tomlDocumentError is a TOML syntax error in toml's wording, keeping the
// byte offset the parser points at.
type tomlDocumentError struct {
	message string
	start   int
}

func (e *tomlDocumentError) Error() string { return e.message }

// tomlErrorOffset is the byte offset a TOML syntax error points at, if any.
func tomlErrorOffset(err error) (int, bool) {
	var document *tomlDocumentError
	if errors.As(err, &document) && document.start >= 0 {
		return document.start, true
	}
	return 0, false
}

// parseTOMLDocument is `text.parse::<toml::Value>()`: the document as
// generic values (normalizeTOML), with toml's "TOML parse error" wording.
func parseTOMLDocument(text string) (map[string]any, error) {
	var document map[string]any
	if _, err := toml.Decode(text, &document); err != nil {
		var parseError toml.ParseError
		if errors.As(err, &parseError) {
			return nil, &tomlDocumentError{
				message: fmt.Sprintf("TOML parse error at line %d, column %d\n%s",
					parseError.Position.Line, parseError.Position.Col, parseError.Message),
				start: parseError.Position.Start,
			}
		}
		return nil, &tomlDocumentError{message: "TOML parse error: " + err.Error(), start: -1}
	}
	if document == nil {
		document = map[string]any{}
	}
	return normalizeTOML(document).(map[string]any), nil
}

// marshalTOMLPretty is toml::to_string_pretty: value's JSON form written as a
// TOML document in serde field order, with nulls (serde None) left out.
func marshalTOMLPretty(value any) (string, error) {
	return marshalTOMLWith(value, true)
}

// marshalTOML is toml::to_string: as marshalTOMLPretty, with every array on
// one line.
func marshalTOML(value any) (string, error) {
	return marshalTOMLWith(value, false)
}

// TOMLTableFromStr is `toml::from_str::<toml::Table>`, for crates above
// core that edit a TOML document as generic values: tables are
// map[string]any, arrays []any, integers int64, floats float64, and dates
// their TOML text (see normalizeTOML).
func TOMLTableFromStr(text string) (map[string]any, error) {
	return parseTOMLDocument(text)
}

// TOMLTableToString is `toml::to_string` of a `toml::Table` (a BTreeMap, so
// keys are written sorted): values first, then tables and arrays of tables.
// A float64 keeps its fraction, as toml writes an f64. A date read by
// TOMLTableFromStr is written back as a string.
func TOMLTableToString(table map[string]any) (string, error) {
	return marshalTOML(tomlKeepFloats(table))
}

// tomlKeepFloats marks every float64 so its JSON form keeps a fraction or an
// exponent, which marshalTOML reads as a float rather than an integer.
func tomlKeepFloats(value any) any {
	switch value := value.(type) {
	case map[string]any:
		marked := make(map[string]any, len(value))
		for key, member := range value {
			marked[key] = tomlKeepFloats(member)
		}
		return marked
	case []any:
		marked := make([]any, len(value))
		for i, item := range value {
			marked[i] = tomlKeepFloats(item)
		}
		return marked
	case []map[string]any:
		marked := make([]any, len(value))
		for i, item := range value {
			marked[i] = tomlKeepFloats(item)
		}
		return marked
	case float64:
		if math.IsNaN(value) || math.IsInf(value, 0) {
			return value
		}
		text := strconv.FormatFloat(value, 'g', -1, 64)
		if !strings.ContainsAny(text, ".eE") {
			text += ".0"
		}
		return json.Number(text)
	}
	return value
}

// unmarshalTOML is toml::from_str: text decoded with BurntSushi/toml and read
// through value's JSON form.
func unmarshalTOML(text string, value any) error {
	document, err := parseTOMLDocument(text)
	if err != nil {
		return err
	}
	data, err := json.Marshal(document)
	if err != nil {
		return err
	}
	if err := json.Unmarshal(data, value); err != nil {
		return fmt.Errorf("TOML parse error: %w", err)
	}
	return nil
}

// orderedJSON is a JSON value with its object members in written order,
// which serde's field order makes significant for TOML output.
type orderedJSON struct {
	// kind is one of 'o' (object), 'a' (array), 's' (string), 'n' (number),
	// 'b' (boolean) or 'z' (null).
	kind    byte
	keys    []string
	members []*orderedJSON
	items   []*orderedJSON
	text    string
	boolean bool
}

func decodeOrderedJSON(decoder *json.Decoder) (*orderedJSON, error) {
	token, err := decoder.Token()
	if err != nil {
		return nil, err
	}
	switch token := token.(type) {
	case json.Delim:
		switch token {
		case '{':
			value := &orderedJSON{kind: 'o'}
			for decoder.More() {
				key, err := decoder.Token()
				if err != nil {
					return nil, err
				}
				member, err := decodeOrderedJSON(decoder)
				if err != nil {
					return nil, err
				}
				value.keys = append(value.keys, key.(string))
				value.members = append(value.members, member)
			}
			_, err := decoder.Token()
			return value, err
		case '[':
			value := &orderedJSON{kind: 'a'}
			for decoder.More() {
				item, err := decodeOrderedJSON(decoder)
				if err != nil {
					return nil, err
				}
				value.items = append(value.items, item)
			}
			_, err := decoder.Token()
			return value, err
		}
		return nil, fmt.Errorf("unexpected delimiter %v", token)
	case string:
		return &orderedJSON{kind: 's', text: token}, nil
	case json.Number:
		return &orderedJSON{kind: 'n', text: token.String()}, nil
	case bool:
		return &orderedJSON{kind: 'b', boolean: token}, nil
	case nil:
		return &orderedJSON{kind: 'z'}, nil
	}
	return nil, fmt.Errorf("unexpected JSON token %v", token)
}

// tomlTable is a toml_edit table as toml's serializer leaves it: plain
// values in field order, then tables and arrays of tables.
type tomlTable struct {
	entries []tomlEntry
	// implicit is set on a table with entries, which is then written only
	// when it holds values of its own (toml's DocumentFormatter).
	implicit bool
}

type tomlEntry struct {
	key   string
	value *orderedJSON
	table *tomlTable
	array []*tomlTable
}

func tomlTableFrom(object *orderedJSON) *tomlTable {
	table := &tomlTable{implicit: len(object.members) > 0}
	for i, member := range object.members {
		key := object.keys[i]
		switch {
		case member.kind == 'z':
			// serde None: the field is skipped.
		case member.kind == 'o':
			table.entries = append(table.entries, tomlEntry{key: key, table: tomlTableFrom(member)})
		case member.kind == 'a' && len(member.items) > 0 && allObjects(member.items):
			tables := make([]*tomlTable, len(member.items))
			for j, item := range member.items {
				tables[j] = tomlTableFrom(item)
			}
			table.entries = append(table.entries, tomlEntry{key: key, array: tables})
		default:
			table.entries = append(table.entries, tomlEntry{key: key, value: member})
		}
	}
	if len(table.entries) == 0 {
		table.implicit = false
	}
	return table
}

func allObjects(items []*orderedJSON) bool {
	for _, item := range items {
		if item.kind != 'o' {
			return false
		}
	}
	return true
}

func marshalTOMLWith(value any, pretty bool) (string, error) {
	data, err := json.Marshal(value)
	if err != nil {
		return "", err
	}
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.UseNumber()
	root, err := decodeOrderedJSON(decoder)
	if err != nil {
		return "", err
	}
	if root.kind != 'o' {
		return "", errors.New("unsupported rust type")
	}
	var output strings.Builder
	first := true
	var visit func(table *tomlTable, path []string, isArray bool) error
	visit = func(table *tomlTable, path []string, isArray bool) error {
		var values []tomlEntry
		for _, entry := range table.entries {
			if entry.value != nil {
				values = append(values, entry)
			}
		}
		header := func(open, close string) {
			if first {
				first = false
			} else {
				output.WriteString("\n")
			}
			output.WriteString(open)
			for i, key := range path {
				if i > 0 {
					output.WriteString(".")
				}
				output.WriteString(tomlKey(key))
			}
			output.WriteString(close + "\n")
		}
		switch {
		case len(path) == 0:
			if len(values) > 0 {
				first = false
			}
		case isArray:
			header("[[", "]]")
		case !(table.implicit && len(values) == 0):
			header("[", "]")
		}
		for _, entry := range values {
			text, err := tomlValue(entry.value, pretty)
			if err != nil {
				return err
			}
			output.WriteString(tomlKey(entry.key) + " = " + text + "\n")
		}
		for _, entry := range table.entries {
			child := append(append([]string{}, path...), entry.key)
			switch {
			case entry.table != nil:
				if err := visit(entry.table, child, false); err != nil {
					return err
				}
			case entry.array != nil:
				for _, item := range entry.array {
					if err := visit(item, child, true); err != nil {
						return err
					}
				}
			}
		}
		return nil
	}
	if err := visit(tomlTableFrom(root), nil, false); err != nil {
		return "", err
	}
	return output.String(), nil
}

// tomlKey is a key as toml_edit writes it: bare when it can be, else a basic
// string.
func tomlKey(key string) string {
	bare := key != ""
	for i := 0; bare && i < len(key); i++ {
		c := key[i]
		bare = c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || c == '-' || c == '_'
	}
	if bare {
		return key
	}
	return tomlBasicString(key, false)
}

// tomlValue is one value as toml_edit writes it inline.
func tomlValue(value *orderedJSON, pretty bool) (string, error) {
	switch value.kind {
	case 's':
		return tomlString(value.text), nil
	case 'b':
		if value.boolean {
			return "true", nil
		}
		return "false", nil
	case 'n':
		if !strings.ContainsAny(value.text, ".eE") {
			if _, err := strconv.ParseInt(value.text, 10, 64); err != nil {
				return "", fmt.Errorf("out-of-range value for i64 type")
			}
			return value.text, nil
		}
		number, err := strconv.ParseFloat(value.text, 64)
		if err != nil {
			return "", err
		}
		return tomlFloat(number), nil
	case 'a':
		var items []string
		for _, item := range value.items {
			if item.kind == 'z' {
				return "", errors.New("unsupported None value")
			}
			text, err := tomlValue(item, pretty)
			if err != nil {
				return "", err
			}
			items = append(items, text)
		}
		if pretty && len(items) > 1 {
			return "[\n    " + strings.Join(items, ",\n    ") + ",\n]", nil
		}
		return "[" + strings.Join(items, ", ") + "]", nil
	case 'o':
		var members []string
		for i, member := range value.members {
			if member.kind == 'z' {
				continue
			}
			text, err := tomlValue(member, pretty)
			if err != nil {
				return "", err
			}
			members = append(members, tomlKey(value.keys[i])+" = "+text)
		}
		if len(members) == 0 {
			return "{}", nil
		}
		return "{ " + strings.Join(members, ", ") + " }", nil
	}
	return "", errors.New("unsupported None value")
}

// tomlFloat is toml_edit's float spelling: Rust's shortest decimal, with
// `.0` on a whole number.
func tomlFloat(number float64) string {
	switch {
	case math.IsNaN(number):
		if math.Signbit(number) {
			return "-nan"
		}
		return "nan"
	case math.IsInf(number, 1):
		return "inf"
	case math.IsInf(number, -1):
		return "-inf"
	case number == 0:
		if math.Signbit(number) {
			return "-0.0"
		}
		return "0.0"
	}
	text := strconv.FormatFloat(number, 'f', -1, 64)
	if !strings.Contains(text, ".") {
		text += ".0"
	}
	return text
}

// tomlString is a string value as toml_edit writes it by default: a literal
// string when it holds a backslash and a literal can carry it, else a basic
// string, either multi-line when it holds a newline.
func tomlString(value string) string {
	multiline := strings.Contains(value, "\n")
	maxSingles, singles := 0, 0
	preferLiteral, canBeLiteral := false, true
	for _, ch := range value {
		if !canBeLiteral {
			break
		}
		if ch == '\'' {
			singles++
			if singles >= 3 {
				canBeLiteral = false
			}
		} else {
			maxSingles = max(maxSingles, singles)
			singles = 0
		}
		switch {
		case ch == '\t':
		case ch == '\\':
			preferLiteral = true
		case ch == '\n':
		case ch <= 0x1f || ch == 0x7f:
			canBeLiteral = false
		}
	}
	if singles > 0 && strings.HasSuffix(value, "'") {
		// The ending quote cannot be escaped, so a basic string it is.
		canBeLiteral = false
	}
	if !preferLiteral || !canBeLiteral {
		return tomlBasicString(value, multiline)
	}
	maxSingles = max(maxSingles, singles)
	switch {
	case multiline:
		return "'''\n" + value + "'''"
	case maxSingles >= 1:
		return "'''" + value + "'''"
	}
	return "'" + value + "'"
}

// tomlBasicString is a basic string, multi-line when asked.
func tomlBasicString(value string, multiline bool) string {
	var builder strings.Builder
	if multiline {
		builder.WriteString("\"\"\"\n")
	} else {
		builder.WriteString("\"")
	}
	for _, ch := range value {
		switch {
		case ch == '\b':
			builder.WriteString(`\b`)
		case ch == '\t':
			builder.WriteString(`\t`)
		case ch == '\n':
			if multiline {
				builder.WriteString("\n")
			} else {
				builder.WriteString(`\n`)
			}
		case ch == '\f':
			builder.WriteString(`\f`)
		case ch == '\r':
			builder.WriteString(`\r`)
		case ch == '"':
			builder.WriteString(`\"`)
		case ch == '\\':
			builder.WriteString(`\\`)
		case ch <= 0x1f || ch == 0x7f:
			fmt.Fprintf(&builder, `\u%04X`, ch)
		default:
			builder.WriteRune(ch)
		}
	}
	if multiline {
		builder.WriteString("\"\"\"")
	} else {
		builder.WriteString("\"")
	}
	return builder.String()
}

// serdeJSON is serde_json::to_vec: encoding/json's output without its HTML
// escapes (`<`, `>`, `&`, U+2028 and U+2029 stay as written), so a digest of
// it matches the one Rust takes.
func serdeJSON(value any) ([]byte, error) {
	data, err := json.Marshal(value)
	if err != nil {
		return nil, err
	}
	if !bytes.Contains(data, []byte(`\u`)) {
		return data, nil
	}
	output := make([]byte, 0, len(data))
	inString := false
	for i := 0; i < len(data); i++ {
		c := data[i]
		if !inString {
			if c == '"' {
				inString = true
			}
			output = append(output, c)
			continue
		}
		switch c {
		case '"':
			inString = false
			output = append(output, c)
		case '\\':
			if i+5 < len(data) && data[i+1] == 'u' {
				switch string(data[i+2 : i+6]) {
				case "003c":
					output = append(output, '<')
					i += 5
					continue
				case "003e":
					output = append(output, '>')
					i += 5
					continue
				case "0026":
					output = append(output, '&')
					i += 5
					continue
				case "2028":
					output = append(output, " "...)
					i += 5
					continue
				case "2029":
					output = append(output, " "...)
					i += 5
					continue
				}
			}
			output = append(output, c, data[i+1])
			i++
		default:
			output = append(output, c)
		}
	}
	return output, nil
}
