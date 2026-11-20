// Port of mj-core/src/review/settings.rs.

// Shared review policy, independent of the UI and of profile names.

package core

import (
	"cmp"
	"fmt"
	"strings"
)

// ReviewModelFamilyKind names a ReviewModelFamily variant, in the Rust
// declaration order that orders families.
type ReviewModelFamilyKind int

const (
	ReviewModelFamilyTerra ReviewModelFamilyKind = iota
	ReviewModelFamilySol
	ReviewModelFamilySonnet
	ReviewModelFamilyDeepSeek
	ReviewModelFamilyGrok
	ReviewModelFamilyKimi
	ReviewModelFamilyOther
)

// ReviewModelFamily is a model family used by reviewer autoconfiguration.
// One comparable struct with a kind, so it can key a map; Other names the
// family for ReviewModelFamilyOther.
type ReviewModelFamily struct {
	Kind  ReviewModelFamilyKind
	Other string
}

// Compare is the derived Rust ordering: by variant, then by the Other name.
func (f ReviewModelFamily) Compare(other ReviewModelFamily) int {
	if c := cmp.Compare(f.Kind, other.Kind); c != 0 {
		return c
	}
	return strings.Compare(f.Other, other.Other)
}

// ReviewModelFamilyForModel is the family a model id belongs to, or false
// for an empty id.
func ReviewModelFamilyForModel(model string) (ReviewModelFamily, bool) {
	model = NormalizeModel(model)
	segments := strings.Split(model, "/")
	// Kimi Code qualifies its version as `kimi-code/k3`, so the provider
	// segment is part of the family identity.
	if len(segments) >= 2 && segments[len(segments)-2] == "kimi-code" {
		return ReviewModelFamily{Kind: ReviewModelFamilyKimi}, true
	}
	model = segments[len(segments)-1]
	if strings.TrimSpace(model) == "" {
		return ReviewModelFamily{}, false
	}
	switch {
	case strings.HasPrefix(model, "gpt-") && modelMatchesFamily(model, "terra"):
		return ReviewModelFamily{Kind: ReviewModelFamilyTerra}, true
	case strings.HasPrefix(model, "gpt-") && modelMatchesFamily(model, "sol"):
		return ReviewModelFamily{Kind: ReviewModelFamilySol}, true
	case strings.HasPrefix(model, "claude-") && modelMatchesFamily(model, "sonnet"):
		return ReviewModelFamily{Kind: ReviewModelFamilySonnet}, true
	case strings.HasPrefix(model, "deepseek-") && modelMatchesFamily(model, "pro"):
		return ReviewModelFamily{Kind: ReviewModelFamilyDeepSeek}, true
	case strings.HasPrefix(model, "grok-") &&
		!strings.Contains(model, "non-reasoning") &&
		!strings.Contains(model, "multi-agent") &&
		!strings.Contains(model, "build"):
		return ReviewModelFamily{Kind: ReviewModelFamilyGrok}, true
	case strings.HasPrefix(model, "kimi-"):
		return ReviewModelFamily{Kind: ReviewModelFamilyKimi}, true
	}
	// Keep versions and reduced variants in one advertised family, including
	// names such as gemini-3-pro, glm-5.3 and qwen3-235b.
	family := model
	if end := strings.IndexFunc(model, func(c rune) bool {
		return !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z')
	}); end > 0 {
		family = model[:end]
	}
	return ReviewModelFamily{Kind: ReviewModelFamilyOther, Other: family}, true
}

type ReviewModelSettings struct {
	Model    *string `json:"model"`
	Effort   *string `json:"effort"`
	FastMode bool    `json:"fast_mode"`
}

type ResolvedReviewSettings struct {
	Profile        string              `json:"profile"`
	Main           ReviewModelSettings `json:"main"`
	Autoconfigured bool                `json:"autoconfigured"`
}

func (s *ResolvedReviewSettings) Description() string {
	prefix := ""
	if s.Autoconfigured {
		prefix = "Autoconfigured: "
	}
	model := "harness default"
	if s.Main.Model != nil {
		model = *s.Main.Model
	}
	effort := "default effort"
	if s.Main.Effort != nil {
		effort = *s.Main.Effort
	}
	return fmt.Sprintf("%s%s: %s / %s", prefix, s.Profile, model, effort)
}

// modelMatchesFamily matches advertised IDs rather than sending family
// shorthand to a harness.
func modelMatchesFamily(id, family string) bool {
	for _, part := range strings.FieldsFunc(strings.ToLower(id), func(r rune) bool {
		return r == '-' || r == '_' || r == '.' || r == '/'
	}) {
		if part == family {
			return true
		}
	}
	return false
}

// PlainDirectoryReviewOff is what every surface says about a session whose
// folder has no Git commits.
const PlainDirectoryReviewOff = "Code review is off for this session: its folder has no Git commits, and review finds changes through Git history."

const NoReviewersReviewOff = "Review is off for this session: no reviewers are configured. Choose reviewers in Settings › Code Review, or pass --reviewer."

// AutomaticReviewOff is why Mjolnir itself, not the user, turns review off
// for a launch.
type AutomaticReviewOff int

const (
	AutomaticReviewOffNoReviewers AutomaticReviewOff = iota
	AutomaticReviewOffPlainDirectory
)

// Notice is what every surface tells the user about this rule.
func (r AutomaticReviewOff) Notice() string {
	if r == AutomaticReviewOffNoReviewers {
		return NoReviewersReviewOff
	}
	return PlainDirectoryReviewOff
}

// AutomaticReviewOffFor is the one place the automatic rules live. (Rust:
// `automatic_review_off`; the name is taken by the type.) They are never
// stored in the session's review choice: every launch, reviewer selection
// and projection evaluates them again against the current `[review]`, so
// reviewers added later take effect on the next launch.
//
//   - An explicit Push choice, or an Inherit choice with its own list, whose
//     reviewer list resolves empty has no reviewers. An omitted list means
//     the global list; an explicitly empty list means none whatever the
//     global list is. Legacy Auto is resolved during review preparation.
//   - A session whose folder has no Git commits cannot be reviewed, because
//     review finds changes through Git history. This applies only when
//     review would have run anyway.
//
// A nil choice is Rust's None.
func AutomaticReviewOffFor(review *ReviewConfig, choice SessionReview, plainDirectory bool) (AutomaticReviewOff, bool) {
	noReviewers := false
	switch choice := choice.(type) {
	case SessionReviewPush:
		if choice.Reviewers != nil {
			noReviewers = len(*choice.Reviewers) == 0
		} else {
			noReviewers = !review.IsLegacy() && len(review.Reviewers) == 0
		}
	case SessionReviewInherit:
		if choice.Reviewers != nil {
			noReviewers = len(*choice.Reviewers) == 0 && review.Trigger() != ReviewTriggerOff
		}
	}
	if noReviewers {
		return AutomaticReviewOffNoReviewers, true
	}
	if plainDirectory && review.ForSession(choice).Trigger() != ReviewTriggerOff {
		return AutomaticReviewOffPlainDirectory, true
	}
	return 0, false
}

// EffectiveReviewConfig is the review configuration that governs a launch:
// the current global `[review]` under the session's stored choice, then the
// automatic rules. Every reader of a session's effective review asks this.
func EffectiveReviewConfig(review *ReviewConfig, choice SessionReview, plainDirectory bool) ReviewConfig {
	effective := review.ForSession(choice)
	if _, off := AutomaticReviewOffFor(review, choice, plainDirectory); off {
		effective.Disabled = true
	}
	return effective
}
