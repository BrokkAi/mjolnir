// Port of mj-import/src/utility_llm.rs.

// Direct, tool-free utility-model selection and structured inference.

package mjimport

import (
	"bytes"
	"cmp"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/url"
	"os"
	"strings"
	"sync"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/accounts"
	"github.com/BrokkAi/mjolnir/go/internal/anvilclient"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const quotaFreshSeconds uint64 = 20 * 60

const maxSummaryBytes = 8 * 1024

// MaxPageBytes is the largest page this pipeline sends, whatever the model
// could accept. Beyond about a megabyte a single request stops being a
// summary and starts being a bet, and the pages are already independent and
// concurrent.
const MaxPageBytes = 1024 * 1024

// UtilityQuotaClass is ordered worst first. A usage-priced (API) profile
// bills each request, so it ranks after every subscription that still has,
// or may have, quota.
type UtilityQuotaClass int

const (
	UtilityQuotaClassUsagePriced UtilityQuotaClass = iota
	UtilityQuotaClassUnknown
	UtilityQuotaClassReserve
	UtilityQuotaClassHealthy
)

// String is the Rust Debug name.
func (c UtilityQuotaClass) String() string {
	switch c {
	case UtilityQuotaClassUsagePriced:
		return "UsagePriced"
	case UtilityQuotaClassUnknown:
		return "Unknown"
	case UtilityQuotaClassReserve:
		return "Reserve"
	case UtilityQuotaClassHealthy:
		return "Healthy"
	}
	return fmt.Sprintf("UtilityQuotaClass(%d)", int(c))
}

type UtilityCandidate struct {
	ProfileID       string
	Harness         core.HarnessKind
	Model           string
	QuotaClass      UtilityQuotaClass
	QuotaScore      uint8
	ReasoningEffort *string
	// PageBytes is how much transcript this model can read in one
	// compaction request.
	PageBytes int
	// family is resolved once when the candidate is built, because a Codex
	// profile with a custom provider serves a family its harness alone does
	// not name.
	family  utilityFamily
	backend anvilclient.LlmBackend
}

// String is Rust's Debug for the candidate, which leaves out the backend.
func (c UtilityCandidate) String() string {
	return fmt.Sprintf("UtilityCandidate { profile_id: %q, harness: %v, model: %q, quota_class: %v, quota_score: %d, page_bytes: %d }",
		c.ProfileID, c.Harness, c.Model, c.QuotaClass, c.QuotaScore, c.PageBytes)
}

// cachedBackend is a built backend and the profile configuration it was
// built from.
type cachedBackend struct {
	profile core.HarnessProfile
	backend anvilclient.LlmBackend
}

// UtilityLlmRuntime resolves utility candidates. Its zero value is ready to
// use.
type UtilityLlmRuntime struct {
	// mu guards backendCache. Backends are reused across resolves so a
	// client that mints a key holds it in memory instead of minting one per
	// compaction.
	mu           sync.Mutex
	backendCache map[string]cachedBackend
}

// UtilityProfile is one profile offered to UtilityLlmRuntime.Quotas (Rust
// passes `(&str, &HarnessProfile)` pairs).
type UtilityProfile struct {
	ID      string
	Profile *core.HarnessProfile
}

// noUtilityModel: no enabled profile offers a direct model with local
// credentials.
type noUtilityModel struct {
	message string
}

func (e *noUtilityModel) Error() string { return e.message }

var sharedUtilityLlmRuntime = sync.OnceValue(func() *UtilityLlmRuntime { return &UtilityLlmRuntime{} })

// UtilityLlmRuntimeShared is the process's one runtime.
func UtilityLlmRuntimeShared() *UtilityLlmRuntime {
	return sharedUtilityLlmRuntime()
}

// Resolve ranks the enabled profiles that can serve utility inference, best
// first. ctx is the discovery's cancellation.
func (r *UtilityLlmRuntime) Resolve(ctx context.Context, config *core.Config) ([]UtilityCandidate, error) {
	panic("unported: mj-import/src/utility_llm.rs UtilityLlmRuntime::resolve")
}

// backend is the cached backend for a profile, rebuilt when its
// configuration changes, or nil when the profile has no credentials. Reuse
// keeps any in-memory credential the client minted.
func (r *UtilityLlmRuntime) backend(profileID string, profile *core.HarnessProfile) (anvilclient.LlmBackend, error) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if cached, ok := r.backendCache[profileID]; ok && cached.profile.Equal(*profile) {
		return cached.backend, nil
	}
	delete(r.backendCache, profileID)
	backend, err := backendForProfile(profile)
	if err != nil {
		return nil, err
	}
	if backend != nil {
		if r.backendCache == nil {
			r.backendCache = map[string]cachedBackend{}
		}
		r.backendCache[profileID] = cachedBackend{profile: *profile, backend: backend}
	}
	return backend, nil
}

// Quotas is each profile's stored quota report that is fresh enough to rank
// by. It reads only the owner's cache and never starts a provider check.
func (r *UtilityLlmRuntime) Quotas(profiles []UtilityProfile) map[string]client.ProfileQuota {
	panic("unported: mj-import/src/utility_llm.rs UtilityLlmRuntime::quotas")
}

func (r *UtilityLlmRuntime) retainEnabled(config *core.Config) {
	enabled := map[string]bool{}
	for id := range config.EnabledProfiles() {
		enabled[id] = true
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	for id := range r.backendCache {
		if !enabled[id] {
			delete(r.backendCache, id)
		}
	}
}

// UtilityCompactionBackend is the compaction backend over resolved utility
// candidates, failing over between them.
type UtilityCompactionBackend struct {
	candidates []UtilityCandidate
	// mu guards disabled, the candidates retired for the rest of the
	// compaction.
	mu       sync.RWMutex
	disabled map[int]struct{}
	// ctx is the compaction's cancellation (Rust: its CancellationToken).
	ctx context.Context
}

func NewUtilityCompactionBackend(ctx context.Context, candidates []UtilityCandidate) *UtilityCompactionBackend {
	return &UtilityCompactionBackend{candidates: candidates, disabled: map[int]struct{}{}, ctx: ctx}
}

// PageBytes is how large a page this backend accepts. Any candidate may
// answer any request once an earlier one fails, so the smallest window
// governs. The floor keeps one small-window candidate from failing the whole
// compaction before a single request is sent; a page that model really
// cannot read comes back as an oversize rejection and is split.
func (b *UtilityCompactionBackend) PageBytes() int {
	smallest := core.DefaultContextBytes
	for index, candidate := range b.candidates {
		if index == 0 || candidate.PageBytes < smallest {
			smallest = candidate.PageBytes
		}
	}
	return max(smallest, MinContextBytes)
}

// InferTextOnce makes one structured text request with an already selected
// utility model. Callers that need a hard operation deadline own the
// timeout and cancellation; this helper only builds and sends the request.
func InferTextOnce(ctx context.Context, candidate *UtilityCandidate, systemPrompt, userPrompt, outputField string) (string, error) {
	profileID, model, output, err := InferStructuredWithFailover(ctx, []UtilityCandidate{*candidate}, systemPrompt, userPrompt, "utility_text", textSchema(outputField))
	if err != nil {
		return "", err
	}
	if text, ok := utilityJSONField(output, outputField).(string); ok {
		return text, nil
	}
	return "", fmt.Errorf("%s model %s returned no %s string", profileID, model, outputField)
}

// utilityJSONField is serde_json's `value.get(key)` on a JSON value: the
// member of an object, or nil.
func utilityJSONField(value any, key string) any {
	if object, ok := value.(jsonvalue.Object); ok {
		member, _ := object.Get(key)
		return member
	}
	return nil
}

func textSchema(field string) any {
	return jsonvalue.Object{
		{Key: "type", Value: "object"},
		{Key: "properties", Value: jsonvalue.Object{{Key: field, Value: jsonvalue.Object{{Key: "type", Value: "string"}}}}},
		{Key: "required", Value: []any{field}},
		{Key: "additionalProperties", Value: false},
	}
}

// InferStructuredWithFailover asks the candidates in order for output
// matching schema and returns the profile and model that answered with the
// output.
func InferStructuredWithFailover(ctx context.Context, candidates []UtilityCandidate, systemPrompt, userPrompt, schemaName string, schema any) (profileID, model string, output any, err error) {
	request := anvilclient.StructuredInferRequest{
		Messages:   []anvilclient.InferMessage{anvilclient.InferMessageSystem(systemPrompt), anvilclient.InferMessageUser(userPrompt)},
		SchemaName: schemaName,
		Schema:     schema,
	}
	candidate, output, err := inferCandidates(ctx, candidates, request,
		func(int) bool { return true }, func(output any) (any, bool) { return output, true }, func(int) {})
	if err != nil {
		return "", "", nil, err
	}
	return candidate.ProfileID, candidate.Model, output, nil
}

// GuardianUtilityOutcome is GuardianUtilityOutcomeReviewed,
// GuardianUtilityOutcomeNoUtilityModel or GuardianUtilityOutcomeFailed,
// serialized internally tagged by "status" (snake_case).
type GuardianUtilityOutcome interface {
	isGuardianUtilityOutcome()
}

type GuardianUtilityOutcomeReviewed struct {
	Assessment core.Assessment `json:"assessment"`
	ProfileID  string          `json:"profile_id"`
	Model      string          `json:"model"`
	ElapsedMs  uint64          `json:"elapsed_ms"`
}

type GuardianUtilityOutcomeNoUtilityModel struct {
	Reason string `json:"reason"`
}

type GuardianUtilityOutcomeFailed struct {
	Error string `json:"error"`
}

func (GuardianUtilityOutcomeReviewed) isGuardianUtilityOutcome()       {}
func (GuardianUtilityOutcomeNoUtilityModel) isGuardianUtilityOutcome() {}
func (GuardianUtilityOutcomeFailed) isGuardianUtilityOutcome()         {}

func (o GuardianUtilityOutcomeReviewed) MarshalJSON() ([]byte, error) {
	type plain GuardianUtilityOutcomeReviewed
	return guardianUtilityOutcomeJSON(struct {
		Status string `json:"status"`
		plain
	}{"reviewed", plain(o)})
}

func (o GuardianUtilityOutcomeNoUtilityModel) MarshalJSON() ([]byte, error) {
	type plain GuardianUtilityOutcomeNoUtilityModel
	return guardianUtilityOutcomeJSON(struct {
		Status string `json:"status"`
		plain
	}{"no_utility_model", plain(o)})
}

func (o GuardianUtilityOutcomeFailed) MarshalJSON() ([]byte, error) {
	type plain GuardianUtilityOutcomeFailed
	return guardianUtilityOutcomeJSON(struct {
		Status string `json:"status"`
		plain
	}{"failed", plain(o)})
}

// guardianUtilityOutcomeJSON encodes as serde_json does: `<`, `>` and `&`
// stay literal and there is no trailing newline.
func guardianUtilityOutcomeJSON(value any) ([]byte, error) {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(value); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buffer.Bytes(), []byte("\n")), nil
}

// ReviewGuardianWithUtility has the utility model review one permission
// request. Provider errors can include fragments of the prompt or reply, so
// a failed inference reports only its kind.
func ReviewGuardianWithUtility(ctx context.Context, config *core.Config, request *core.GuardianReviewRequest) GuardianUtilityOutcome {
	panic("unported: mj-import/src/utility_llm.rs review_guardian_with_utility")
}

// pageBytesFor is how much transcript to send this model in one request.
// Providers publish a context window in tokens; four bytes per token is the
// estimator this codebase already uses, and half the window is left for the
// system prompt and the response. Codex publishes no window at all, and the
// GPT-5 family's is far larger than the cap, so it is trusted with a full
// page.
func pageBytesFor(harness core.HarnessKind, metadata *anvilclient.ModelMetadata) int {
	switch {
	case metadata.ContextLength != nil:
		return min(MaxPageBytes, int(*metadata.ContextLength)*4/2)
	case harness == core.HarnessKindCodex:
		return MaxPageBytes
	}
	return core.DefaultContextBytes
}

type utilityRequestError struct {
	kind   anvilclient.InferErrorKind
	detail string
}

func (e *utilityRequestError) Error() string {
	return "utility inference failed: " + e.detail
}

// inferCandidates tries candidates in priority order. Compaction can reject
// a schema-valid snapshot (validateOutput) and retire an unavailable
// candidate for the rest of its pages (enabled, disable). A failure is a
// *utilityRequestError.
func inferCandidates[T any](
	ctx context.Context,
	candidates []UtilityCandidate,
	request anvilclient.StructuredInferRequest,
	enabled func(index int) bool,
	validateOutput func(output any) (T, bool),
	disable func(index int),
) (*UtilityCandidate, T, error) {
	panic("unported: mj-import/src/utility_llm.rs infer_candidates")
}

var _ CompactionBackend = (*UtilityCompactionBackend)(nil)

// Compact asks the candidates, in order, for a state snapshot of prompt.
// ctx is the caller's request (Rust: dropping the compact future); b.ctx is
// the compaction's own cancellation.
func (b *UtilityCompactionBackend) Compact(ctx context.Context, prompt string) (string, error) {
	panic("unported: mj-import/src/utility_llm.rs UtilityCompactionBackend::compact")
}

// ClassifyFailure: only a context-length rejection earns a smaller page.
func (b *UtilityCompactionBackend) ClassifyFailure(err error) CompactionFailure {
	var requestError *utilityRequestError
	if errors.As(err, &requestError) && requestError.kind == anvilclient.InferErrorKindContextLength {
		return CompactionFailureOversize
	}
	return CompactionFailureFatal
}

func quotaRequest(profileID string, profile *core.HarnessProfile) accounts.QuotaRefreshRequest {
	cwd, err := os.Getwd()
	if err != nil {
		cwd = "."
	}
	return accounts.QuotaRefreshRequestForProfile(profileID, profile, cwd)
}

// ClassifyQuota ranks a quota report, or reports false when its quota is
// exhausted.
func ClassifyQuota(report *client.ProfileQuota) (UtilityQuotaClass, uint8, bool) {
	panic("unported: mj-import/src/utility_llm.rs classify_quota")
}

// utilityFamily is which model family a profile offers Mjolnir for its own
// inference. A profile's harness usually decides this, but a Codex profile
// pointed at a custom provider serves that provider's family instead.
type utilityFamily int

const (
	utilityFamilyCodex utilityFamily = iota
	utilityFamilyMuse
	utilityFamilyGrok
	utilityFamilyKimi
	utilityFamilyDeepSeek
)

// precedence is the preference between families when several have healthy
// quota. A higher number is tried first.
func (f utilityFamily) precedence() uint8 {
	switch f {
	case utilityFamilyCodex:
		return 5
	case utilityFamilyMuse:
		return 4
	case utilityFamilyGrok:
		return 3
	case utilityFamilyKimi:
		return 2
	}
	return 1
}

// matches is whether a catalog model id belongs to this family's small,
// fast model.
func (f utilityFamily) matches(id string) bool {
	id = utilityASCIILower(id)
	switch f {
	case utilityFamilyCodex:
		return strings.HasPrefix(id, "gpt-") && core.IsLunaModel(id)
	case utilityFamilyGrok:
		return strings.HasPrefix(id, "grok-")
	case utilityFamilyKimi:
		tail, ok := strings.CutPrefix(id, "k")
		return strings.HasPrefix(id, "kimi-") || ok && tail != "" && tail[0] >= '0' && tail[0] <= '9'
	case utilityFamilyDeepSeek:
		return strings.HasPrefix(id, "deepseek-") && strings.Contains(id, "flash")
	}
	return museSparkModel(id)
}

// utilityASCIILower is Rust's `to_ascii_lowercase`: only ASCII letters
// change.
func utilityASCIILower(text string) string {
	return strings.Map(func(character rune) rune {
		if character >= 'A' && character <= 'Z' {
			return character + ('a' - 'A')
		}
		return character
	}, text)
}

// profileUtilityFamily is the family this profile offers (Rust
// `utility_family`), or false when it serves no utility work.
//
// Claude exposes no direct inference client, so it never serves. A Codex
// profile that authenticates with an API key against a custom provider
// serves only when that provider is DeepSeek: DeepSeek's `/v1` base URL is
// the same chat-completions endpoint the shared OpenAI client speaks, so
// the client can reach it verbatim. Z.ai's Coding Plan key serves chat
// completions under a different path, and an unknown provider is not known
// to serve them at all, so both stay excluded. Those profiles still run
// sessions; they just never serve Mjolnir's own inference.
func profileUtilityFamily(profile *core.HarnessProfile) (utilityFamily, bool) {
	panic("unported: mj-import/src/utility_llm.rs utility_family")
}

// dshDefaultBaseURL is the base URL dsh-acp uses when `DEEPSEEK_BASE_URL`
// is unset.
const dshDefaultBaseURL = "https://api.deepseek.com"

// dshDeepseekBaseURL is the DeepSeek API a DSH profile's sessions talk to,
// when it is DeepSeek's own service. dsh-acp appends `/chat/completions` to
// its base URL while the shared OpenAI client appends
// `/v1/chat/completions`; DeepSeek's own host serves both, and no other host
// is known to.
func dshDeepseekBaseURL(profile *core.HarnessProfile) (string, bool) {
	base, ok := profile.Environment.Get("DEEPSEEK_BASE_URL")
	if !ok {
		base = dshDefaultBaseURL
	}
	// Rust's url::Url::parse rejects a relative URL, and `host_str()?` a
	// URL without a host.
	parsed, err := url.Parse(base)
	if err != nil || parsed.Scheme == "" || parsed.Hostname() == "" {
		return "", false
	}
	if core.CodexProviderKindFromHost(utilityASCIILower(parsed.Hostname())) != core.CodexProviderKindDeepSeek {
		return "", false
	}
	return base, true
}

// profileServesAsUtility is whether this profile may be ranked as a utility
// model, the backend Mjolnir uses for its own inference such as compacting
// a transcript.
func profileServesAsUtility(profile *core.HarnessProfile) bool {
	_, ok := profileUtilityFamily(profile)
	return ok
}

func candidateOrder(left, right *UtilityCandidate) int {
	if order := cmp.Compare(right.QuotaClass, left.QuotaClass); order != 0 {
		return order
	}
	if order := cmp.Compare(right.family.precedence(), left.family.precedence()); order != 0 {
		return order
	}
	if order := cmp.Compare(right.QuotaScore, left.QuotaScore); order != 0 {
		return order
	}
	return strings.Compare(left.ProfileID, right.ProfileID)
}

// newestFamilyModel is the newest catalog model of family. As Rust's
// `max_by`, the last of equally new models wins.
func newestFamilyModel(family utilityFamily, catalog []anvilclient.ModelMetadata) *anvilclient.ModelMetadata {
	var newest *anvilclient.ModelMetadata
	for index := range catalog {
		model := &catalog[index]
		if family.matches(model.ID) && (newest == nil || core.ModelVersionCmp(model.ID, newest.ID) >= 0) {
			newest = model
		}
	}
	return newest
}

func museSparkModel(id string) bool {
	version, ok := strings.CutPrefix(id, "muse-spark-")
	if !ok || version == "" {
		return false
	}
	for part := range strings.SplitSeq(version, ".") {
		if part == "" || strings.Trim(part, "0123456789") != "" {
			return false
		}
	}
	return true
}

// backendForProfile builds the inference backend of a profile that serves
// utility work, or nil when its credentials are unavailable.
func backendForProfile(profile *core.HarnessProfile) (anvilclient.LlmBackend, error) {
	panic("unported: mj-import/src/utility_llm.rs backend_for_profile")
}

func nowSeconds() uint64 {
	seconds := time.Now().Unix()
	if seconds < 0 {
		return 0
	}
	return uint64(seconds)
}
