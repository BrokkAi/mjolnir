// Port of mj-core/src/continuation.rs.
//
// Evidence and durable admission for bounded, already-authorized continuation.

package core

import (
	"encoding/json"
	"errors"
	"fmt"
	"strconv"
	"strings"
	"unicode"

	acp "github.com/BrokkAi/acp-go/schema"
)

// RestartRecoveryNote: restart prompts retain their original payload; the
// note is added only when rendered.
const RestartRecoveryNote = "[HARNESS NOTE: interrupted turn recovery] Your previous turn was interrupted because this session's worker or harness restarted, and running tool calls were lost. The original request that began this turn is repeated below for context. Any corrections or redirections given during the turn, visible in the conversation, take precedence over it. If you had already started, continue from where you left off rather than starting over. Before repeating a command whose result you did not see, check whether it already took effect, especially for anything with external side effects."

const restartPrefix = "auto-continue-restart-"

// restartParts splits a restart continuation id into its generation and root.
func restartParts(id string) (string, string, bool) {
	rest, ok := strings.CutPrefix(id, restartPrefix)
	if !ok {
		return "", "", false
	}
	generation, root, ok := strings.Cut(rest, "-")
	if !ok || generation == "" || generation[0] == '0' || root == "" {
		return "", "", false
	}
	for i := 0; i < len(generation); i++ {
		if generation[i] < '0' || generation[i] > '9' {
			return "", "", false
		}
	}
	return generation, root, true
}

// CarriedCommand is the original command carried by a restart
// continuation, across any number of restarts. The ID is the sole carrier;
// no journal lookup is needed.
func CarriedCommand(id string) string {
	for {
		_, root, ok := restartParts(id)
		if !ok {
			return id
		}
		id = root
	}
}

func RestartContinuationID(commandID string) string {
	// Decimal increment avoids imposing a machine-integer retry budget.
	current := "0"
	if generation, _, ok := restartParts(commandID); ok {
		current = generation
	}
	generation := []byte(current)
	carry := true
	for i := len(generation) - 1; i >= 0; i-- {
		if generation[i] == '9' {
			generation[i] = '0'
		} else {
			generation[i]++
			carry = false
			break
		}
	}
	if carry {
		generation = append([]byte{'1'}, generation...)
	}
	return restartPrefix + string(generation) + "-" + CarriedCommand(commandID)
}

// ValidCommandID: ordinary IDs retain their 8–128 byte bound. The
// worker-owned restart envelope adds only its prefix and generation to an
// already valid root.
func ValidCommandID(id string) bool {
	root := id
	if IsRestartContinuation(id) {
		_, carried, ok := restartParts(id)
		if !ok || IsRestartContinuation(carried) {
			return false
		}
		root = carried
	}
	if len(root) < 8 || len(root) > 128 {
		return false
	}
	for i := 0; i < len(root); i++ {
		c := root[i]
		if !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || c == '-' || c == '_') {
			return false
		}
	}
	return true
}

func IsRestartContinuation(commandID string) bool {
	suffix, ok := GeneratedPromptAutoContinue.Suffix(commandID)
	return ok && strings.HasPrefix(suffix, "restart-")
}

// CarriesMailboxWake reports whether commandID is a mailbox wake, or the
// restart continuation that carries one. A wake restored from a checkpoint
// runs as a plain prompt, so its ID is what still says what it is.
func CarriesMailboxWake(commandID string) bool {
	producer, ok := GeneratedPromptOf(CarriedCommand(commandID))
	return ok && producer == GeneratedPromptMailboxWake
}

const UserBytes = 32 * 1024
const AssistantBytes = 16 * 1024
const MaxBodyBytes = 64 * 1024
const Prompt = "Continue the unfinished work already requested by the user, following their latest instructions. This message supplies no new approval or missing information. If a genuine decision, required approval, or external action remains necessary, explain it and stop."

// ParentWaitPromptText is the historical sub-agent reminder, retained to
// exclude old journal prompts from authorization.
const ParentWaitPromptText = "One or more sub-agents finished. Call wait to collect their reports."

type ContinuationState struct {
	// AwaitingChildrenSinceMs is a legacy protocol-39 snapshot field; no
	// longer affects continuation.
	AwaitingChildrenSinceMs *int64 `json:"awaiting_children_since_ms,omitempty"`
	// UserCommandID is the original user request, or an automatic request
	// retained only for quota recovery.
	UserCommandID *string `json:"user_command_id"`
	// Attempts and Suppressed are legacy automatic-continuation counters:
	// still written so older readers decode the state, and still set by
	// replaying old journals.
	Attempts           uint8   `json:"attempts"`
	Suppressed         bool    `json:"suppressed"`
	CompletedCommandID *string `json:"completed_command_id"`
	QuotaSuppressed    bool    `json:"quota_suppressed"`
	// WithdrawnOrdinal is the ordinal of the newest event after which
	// automatic continuation was withdrawn (a control command, a
	// cancellation, lost native continuity, a context command). Written
	// beside every QuotaSuppressed = true and never reset: a consumer
	// compares it with the ordinal of the prompt it asks about, so a
	// withdrawal before that prompt does not count.
	WithdrawnOrdinal *uint64        `json:"withdrawn_ordinal,omitempty"`
	QuotaRecovery    *QuotaRecovery `json:"quota_recovery,omitempty"`
	// AutonomousTurnStarted is a legacy pre-revision-24 snapshot field; only
	// used during snapshot upgrade. The open StoredHarnessTurn now owns this
	// fact. Read but never written.
	AutonomousTurnStarted *uint64 `json:"-"`
	// HarnessTurn is the self-started turn that CompletedCommandID names,
	// once it ends.
	HarnessTurn *HarnessCompletion `json:"harness_turn,omitempty"`
}

func (s *ContinuationState) UnmarshalJSON(data []byte) error {
	type plain ContinuationState
	aux := struct {
		*plain
		AutonomousTurnStarted *uint64 `json:"autonomous_turn_started"`
	}{plain: (*plain)(s)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	s.AutonomousTurnStarted = aux.AutonomousTurnStarted
	return nil
}

// HarnessCompletion is a turn the harness started and ended on its own. It
// stands in for a completed command so the same checks and guards apply to it.
type HarnessCompletion struct {
	// ID is harness-turn-<start ordinal>; see HarnessTurnID.
	ID string `json:"id"`
	// StartPosition is the ordinal of the start event, which is also the
	// transcript position of the turn's marker.
	StartPosition  uint64 `json:"start_position"`
	SettledOrdinal uint64 `json:"settled_ordinal"`
	SettledAtMs    int64  `json:"settled_at_ms"`
}

const harnessTurnIDPrefix = "harness-turn-"

func HarnessTurnID(startOrdinal uint64) string {
	return harnessTurnIDPrefix + strconv.FormatUint(startOrdinal, 10)
}

// QuotaGoalResumePrefix is the id prefix of the goal resume Mjolnir submits
// when a quota recovery comes due for a goal the usage limit stopped. The
// command is an ordinary GoalControl { Resume }; the prefix is what makes
// the relay guard it as a quota recovery and send the goal's identity with
// it.
const QuotaGoalResumePrefix = "quota-goal-resume-"

func IsQuotaGoalResume(commandID string) bool {
	return strings.HasPrefix(commandID, QuotaGoalResumePrefix)
}

// QuotaRecovery is durable recovery for one completed turn; a missing
// deadline records abstention.
type QuotaRecovery struct {
	UserCommandID      string `json:"user_command_id"`
	CompletedCommandID string `json:"completed_command_id"`
	ProfileID          string `json:"profile_id"`
	ResetAtMs          *int64 `json:"reset_at_ms"`
	RetryAtMs          *int64 `json:"retry_at_ms"`
	Notice             string `json:"notice"`
	Submitted          bool   `json:"submitted"`
}

type EvidenceMessage struct {
	ID   string `json:"id"`
	Role string `json:"role"`
	Text string `json:"text"`
}

func (m *EvidenceMessage) UnmarshalJSON(data []byte) error {
	type plain EvidenceMessage
	return stateDecodeStrict(data, (*plain)(m))
}

type ContinuationEvidence struct {
	AssistantHistoryOmitted bool              `json:"assistant_history_omitted"`
	Messages                []EvidenceMessage `json:"messages"`
}

func (e *ContinuationEvidence) UnmarshalJSON(data []byte) error {
	type plain ContinuationEvidence
	return stateDecodeStrict(data, (*plain)(e))
}

func (e *ContinuationEvidence) Validate() error {
	if len(e.Messages) == 0 || len(e.Messages) > 256 {
		return errors.New("invalid continuation messages")
	}
	user, assistant := 0, 0
	for _, message := range e.Messages {
		if message.ID == "" || len(message.ID) > 256 {
			return errors.New("invalid message id")
		}
		if strings.TrimSpace(message.Text) == "" {
			return errors.New("empty continuation message")
		}
		switch message.Role {
		case "user":
			user += len(message.Text)
		case "assistant":
			assistant += len(message.Text)
		default:
			return errors.New("invalid continuation role")
		}
	}
	if !(user > 0 && user <= UserBytes && assistant > 0 && assistant <= AssistantBytes) {
		return errors.New("incomplete or oversized continuation context")
	}
	if e.Messages[len(e.Messages)-1].Role != "assistant" {
		return errors.New("missing final assistant reply")
	}
	encoded, err := json.Marshal(e)
	if err != nil {
		return err
	}
	if len(encoded) > MaxBodyBytes {
		return errors.New("continuation evidence exceeds byte limit")
	}
	return nil
}

func PromptBlocks() []acp.ContentBlock {
	return []acp.ContentBlock{{Text: &acp.TextContent{Text: Prompt}}}
}

// IsControlOriginPrompt reports whether a harness note was authored by
// Mjolnir rather than a person.
func IsControlOriginPrompt(text string) bool {
	return strings.HasPrefix(strings.TrimLeftFunc(text, unicode.IsSpace), "[HARNESS NOTE:")
}

// IsGeneratedPromptText: these established producers author prompts on the
// user's behalf. Their text is never evidence of fresh user authorization
// or a renewed allowance.
func IsGeneratedPromptText(text string) bool {
	return IsControlOriginPrompt(text) || strings.TrimSpace(text) == ParentWaitPromptText
}

// GeneratedPrompt is every producer of user-role prompts that Mjolnir
// submits itself rather than the person typing them. This is the one owner
// of those command ids: producers build their ids here and every reader
// asks IsGeneratedPrompt, so a new producer cannot be added without being
// recognized (one declaration lists every producer).
//
// A prompt that carries the person's own text (plan-review feedback, image
// pastes, startup prompts, prompt-, web-prompt-) is not generated, and
// neither is a command that is not a prompt (auto-steer-, a goal resume).
// The id strings are stored in journals and transcripts; do not change them.
type GeneratedPrompt int

const (
	// GeneratedPromptAutoContinue: automatic continuation of unfinished work.
	GeneratedPromptAutoContinue GeneratedPrompt = iota
	// GeneratedPromptQuotaRetry: the retry a quota recovery submits.
	GeneratedPromptQuotaRetry
	// GeneratedPromptAuthenticationRetry: resume a turn whose rejected
	// Claude access token was replaced.
	GeneratedPromptAuthenticationRetry
	// GeneratedPromptReviewForward: review findings forwarded to the author.
	// No current producer; older transcripts still carry it.
	GeneratedPromptReviewForward
	// GeneratedPromptArchive: restored archive context. No current producer;
	// older transcripts still carry it.
	GeneratedPromptArchive
	// GeneratedPromptSubagentSpawn: a child's original assignment from its parent.
	GeneratedPromptSubagentSpawn
	// GeneratedPromptSubagentInput: a parent's follow-up assignment.
	GeneratedPromptSubagentInput
	// GeneratedPromptSubagent: other generated sub-agent traffic, including
	// older wait notices.
	GeneratedPromptSubagent
	// GeneratedPromptMailboxWake: a mailbox event that wakes an idle session.
	GeneratedPromptMailboxWake
	// GeneratedPromptHandbackReminder: the prompt that reminds a child to
	// hand back its report.
	GeneratedPromptHandbackReminder
	// GeneratedPromptHoldWake: the prompt that tells a held child its
	// background work has exited.
	GeneratedPromptHoldWake
	// GeneratedPromptHoldGiveUp: the prompt that asks a held child to hand
	// back after the hold ended.
	GeneratedPromptHoldGiveUp
	// GeneratedPromptSessionMessageTurn: another session's message delivered
	// as a turn.
	GeneratedPromptSessionMessageTurn
	// GeneratedPromptServerRetry: a worker's retry after a provider failure.
	GeneratedPromptServerRetry
	// GeneratedPromptCapacityRetry: older workers' name for ServerRetry.
	GeneratedPromptCapacityRetry
)

// GeneratedPromptAll is GeneratedPrompt::ALL, in declaration order.
var GeneratedPromptAll = []GeneratedPrompt{
	GeneratedPromptAutoContinue,
	GeneratedPromptQuotaRetry,
	GeneratedPromptAuthenticationRetry,
	GeneratedPromptReviewForward,
	GeneratedPromptArchive,
	GeneratedPromptSubagentSpawn,
	GeneratedPromptSubagentInput,
	GeneratedPromptSubagent,
	GeneratedPromptMailboxWake,
	GeneratedPromptHandbackReminder,
	GeneratedPromptHoldWake,
	GeneratedPromptHoldGiveUp,
	GeneratedPromptSessionMessageTurn,
	GeneratedPromptServerRetry,
	GeneratedPromptCapacityRetry,
}

var generatedPromptPrefixes = [...]string{
	GeneratedPromptAutoContinue:        "auto-continue-",
	GeneratedPromptQuotaRetry:          "quota-retry-",
	GeneratedPromptAuthenticationRetry: "auth-retry-",
	GeneratedPromptReviewForward:       "review-forward-",
	GeneratedPromptArchive:             "archive-",
	GeneratedPromptSubagentSpawn:       "subagent-spawn-",
	GeneratedPromptSubagentInput:       "subagent-input-",
	GeneratedPromptSubagent:            "subagent-",
	GeneratedPromptMailboxWake:         "mailbox-wake-",
	GeneratedPromptHandbackReminder:    "handback-reminder-",
	GeneratedPromptHoldWake:            "hold-wake-",
	GeneratedPromptHoldGiveUp:          "hold-give-up-",
	GeneratedPromptSessionMessageTurn:  "session-message-turn-",
	GeneratedPromptServerRetry:         "server-retry-",
	GeneratedPromptCapacityRetry:       "capacity-retry-",
}

func (p GeneratedPrompt) Prefix() string { return generatedPromptPrefixes[p] }

// CommandID is the command id this producer gives a prompt: its prefix and
// suffix (formatted with %v, as Display).
func (p GeneratedPrompt) CommandID(suffix any) string {
	return p.Prefix() + fmt.Sprint(suffix)
}

// Suffix is what follows this producer's prefix, when commandID carries it.
func (p GeneratedPrompt) Suffix(commandID string) (string, bool) {
	return strings.CutPrefix(commandID, p.Prefix())
}

// RetryOrdinal is the ordinal a retry id ends in: ASCII digits that fit a uint64.
func (p GeneratedPrompt) RetryOrdinal(commandID string) (uint64, bool) {
	ordinal, ok := p.Suffix(commandID)
	if !ok || ordinal == "" {
		return 0, false
	}
	for i := 0; i < len(ordinal); i++ {
		if ordinal[i] < '0' || ordinal[i] > '9' {
			return 0, false
		}
	}
	value, err := strconv.ParseUint(ordinal, 10, 64)
	return value, err == nil
}

// IsRetry: retry ids end in the ordinal of the turn they retry, and nothing
// else counts as one.
func (p GeneratedPrompt) IsRetry() bool {
	return p == GeneratedPromptServerRetry || p == GeneratedPromptCapacityRetry || p == GeneratedPromptAuthenticationRetry
}

func (p GeneratedPrompt) recognizes(commandID string) bool {
	if p.IsRetry() {
		_, ok := p.RetryOrdinal(commandID)
		return ok
	}
	_, ok := p.Suffix(commandID)
	return ok
}

// GeneratedPromptOf is the producer that submitted the prompt with this
// command id.
func GeneratedPromptOf(commandID string) (GeneratedPrompt, bool) {
	for _, producer := range GeneratedPromptAll {
		if producer.recognizes(commandID) {
			return producer, true
		}
	}
	return 0, false
}

// GeneratedPromptIsSubagentAssignment: parent assignments are authorization
// only for the handback check.
func GeneratedPromptIsSubagentAssignment(commandID string) bool {
	producer, ok := GeneratedPromptOf(commandID)
	return ok && (producer == GeneratedPromptSubagentSpawn || producer == GeneratedPromptSubagentInput)
}

// GeneratedPromptSubagentInputID is the id of another session's input to a
// sub-agent, delivered as a turn.
func GeneratedPromptSubagentInputID(requestID string) string {
	return GeneratedPromptSubagentInput.CommandID(requestID)
}

// GeneratedPromptSubagentSpawnID is the id (and startup group id) of a
// child's spawn brief.
func GeneratedPromptSubagentSpawnID(requestKey string) string {
	return GeneratedPromptSubagentSpawn.CommandID(requestKey)
}

func IsGeneratedPrompt(commandID string) bool {
	_, ok := GeneratedPromptOf(commandID)
	return ok
}

// IsUserAuthoredMessage reports whether a user-role transcript item is a
// message the person wrote. This is the one place the id and text rules are
// combined; every reader that asks for "the user's latest message" goes
// through it. stableID is the item's id (user:<command id>, or a bare
// command id), nil for a prompt the controller has not yet projected, which
// can only be the person's own.
func IsUserAuthoredMessage(stableID *string, text string) bool {
	if IsGeneratedPromptText(text) {
		return false
	}
	if stableID == nil {
		return true
	}
	commandID := strings.TrimPrefix(*stableID, "user:")
	return !IsGeneratedPrompt(commandID)
}
