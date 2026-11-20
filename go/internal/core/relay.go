// Port of mj-core/src/relay.rs.
//
// Shared relay messages, deterministic state and framing. Durable execution
// lives in mj-worker.
//
// Rust enums that carry data are sealed interfaces here, one struct per
// variant named <Type><Variant>. Each variant writes its own serde-tagged JSON
// (MarshalJSON), and Unmarshal<Type>JSON decodes the tagged form; structs
// holding such a field decode it through that function in their own
// UnmarshalJSON. Rust methods on those enums are free functions named
// <Type><Method> that take the value.

package core

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"sync/atomic"
	"time"
	"unicode"
)

// NativeSessionStartupTimeout is the observed harness startup allowance:
// first launches can take minutes. The worker owns expiry; controller request
// waits use the same allowance.
const NativeSessionStartupTimeout = 300 * time.Second

const MaxFrameBytes = 8 * 1024 * 1024

// RelayReplayByteBudget is the serialized bytes of observations allowed in
// one attach response, well under MaxFrameBytes to leave room for the
// envelope.
const RelayReplayByteBudget = 4 * 1024 * 1024

// RelayCommandByteBudget: a single durable command must remain comfortably
// smaller than a relay frame. Commands are repeated in the event journal and
// private dispatch state, so admitting a frame-sized command would make later
// attaches impossible to encode.
const RelayCommandByteBudget = 1024 * 1024

// RelayMailboxByteBudget: pending mailbox contents must fit in one relay frame
// after rendering and JSON framing, so a single hook can claim the full
// mailbox atomically.
const RelayMailboxByteBudget = RelayCommandByteBudget

// RelayEventByteBudget: every event must fit by itself in a replay page.
const RelayEventByteBudget = 2 * 1024 * 1024

// RelayStateByteBudget: the public operational state shares an attach frame
// with a replay page.
const RelayStateByteBudget = 2 * 1024 * 1024

// TerminalJournalOutputBytes is the bytes of terminal output one journal
// entry keeps. The agent read the whole stream over terminal/output; the
// journal copy is for the person reading the transcript, so it keeps the tail
// and stays far below the event budget.
const TerminalJournalOutputBytes = 256 * 1024

// RelayEventEnvelopeReserve is the headroom left for an event's envelope —
// ordinals, digests, timestamp and command id — when clamping an observation
// to RelayEventByteBudget.
const RelayEventEnvelopeReserve = 8 * 1024

// RelayTruncationFloor: clamping never shortens a string below this.
// Identifiers, type tags and paths stay whole; only genuinely large payloads
// are candidates.
const RelayTruncationFloor = 4 * 1024

// RelaySnapshotByteBudget: the private snapshot also has a hard ceiling so
// repeated accepted commands cannot grow the durable state file without bound
// between checkpoints.
const RelaySnapshotByteBudget = 16 * 1024 * 1024

// RelayRetryIDRetention is the number of completed command IDs and delivered
// mailbox keys retained for lost-ACK retries.
const RelayRetryIDRetention = 512

// Current durable ACP relay protocol. A worker serves only this version.
//
// A controller still connects to a worker that speaks an older version in
// RelayMinProtocolVersion..=this range, so that it can read the session and
// replace the worker with the current build once it is quiet. Until then, a
// request the older worker cannot decode is refused on the controller side.
// Protocol 0 is the retired pre-relay worker protocol.
//
// 30 removed the reviewer's change-analysis request and added per-file line
// counts to a review capture. 31 added delivery status to sub-agent results
// (older completion responses mean delivered) and compare-and-replace for
// project memory trees. 32 added worker-owned sub-agent mutation admission.
// 33 adds durable mailbox events, their leased hook drain/ack requests, and
// parent mailbox messages in the sub-agent request queue. 34 carries
// structured mailbox bodies; controllers keep the old text representation
// when they submit to a protocol-33 worker.
// 35 records rendered elicitation replies in resolution events.
// 36 adds typed peer-message senders, which older workers receive as
// queued turns. 37 records a Claude steer as part of its acknowledged
// PostToolBatch lease, which older controllers cannot project as a turn.
const (
	RelayLegacyMailboxProtocol           uint32 = 33
	RelayStructuredMailboxProtocol       uint32 = 34
	RelaySessionMessageProtocol          uint32 = 36
	RelayClaudeHookSteeringProtocol      uint32 = 37
	RelaySubagentFeedProtocol            uint32 = 38 // 38 replaces delegation-history snapshots with acknowledged queue changes.
	RelaySubagentWaitProtocol            uint32 = 39 // 39 adds explicit, durable parent waits that leave child workers running.
	RelayTurnControlsProtocol            uint32 = 40 // 40 publishes worker-owned controls and binds Apply queued to a queue head.
	RelaySubagentFinishedProtocol        uint32 = 41 // 41 adds structured child reports and always installs mailbox drain hooks.
	RelayReviewAttributionProtocol       uint32 = 42 // 42 says in a review capture why the worker could not tell the turn's commits from other history, which a protocol-41 controller cannot decode.
	RelayPushReviewProtocol              uint32 = 43 // 43 adds push-scoped review capture, reviewed-tree recording and verdict mailboxes.
	RelayCompressedProjectMemoryProtocol uint32 = 44 // 44 compresses project-memory trees with gzip, retaining plain-tree decoding.
	RelayOperatorAwayProtocol            uint32 = 45 // 45 adds operator-away mode: SetOperatorAway and the OperatorAway mailbox body.
	RelayAwsCredentialsProtocol          uint32 = 46 // 46 adds connection-only AWS credential provisioning and renewal.
	RelaySlopcopLoginProtocol            uint32 = 47 // 47 adds connection-only sync of the host's slopcop login.
	RelayDismissQuestionProtocol         uint32 = 48 // 48 adds durable dismissal of an inferred question for an exact completed turn.
	RelayWorktreeReviewProtocol          uint32 = 49 // 49 adds repository-scoped review capture for linked worktrees.
	RelayMergedReviewWarningProtocol     uint32 = 50 // 50 carries a warning when merged-work review is skipped on old Git.
	// Settlements identify autonomous turns and their stop reasons.
	RelayAutonomousTurnOutcomeProtocol uint32 = 51
	// 52 adds RecordOverridden and SetReviewBase: an override that allows a
	// push without moving the review base, and an explicit move of that base.
	RelayReviewBaseProtocol uint32 = 52
	// 53 removes per-turn capture and baseline advancement; push review remains.
	RelayTurnCaptureRemovedProtocol uint32 = 53
	// 54 renders a child report that is not a finish (a message to a working
	// child failed) under its own heading, and says when a child whose turn
	// failed can still take a message. Older workers get the report as text.
	RelaySubagentReportStateProtocol uint32 = 54
	// 55 adds RemoveAwsCredentials and AwsCredentialsScope, and a scope on
	// InstallAwsCredentials: the controller withdraws a session's AWS
	// credentials when the profile requires role-scoped credentials and none
	// can be issued, and recognises credentials it issued from what the
	// worker recorded. The scope reply also names the AWS profile the session
	// was staged with, which the controller keeps using for the session's life.
	RelayAwsCredentialsRemovalProtocol uint32 = 55
	// 56 adds the observed facts of a sub-agent's finished turn (its processes
	// and working tree, captured by its worker before the outcome is
	// published) to the prompt outcome record, and the finish-report facts
	// that carry them. Older readers ignore the field; older workers get the
	// report as text.
	RelayFinishFactsProtocol uint32 = 56
	// 57 lets an attach name the digest of the operational state the
	// controller already holds. The worker then answers
	// AttachedWithKnownState instead of sending an identical state again, and
	// Attached carries the digest of the state it does send. Older workers
	// reject the unknown field, so controllers send it only to workers that
	// negotiated 57.
	RelayStateDigestProtocol uint32 = 57
	// 58 adds WaitForChange: the worker holds the request until its events,
	// its operational state, or this connection's sub-agent queue differ from
	// what the controller names, so the controller syncs on change instead of
	// on a timer.
	RelayChangeWaitProtocol uint32 = 58
	// 59 adds paged, connection-only project-memory snapshot and replacement
	// transfers so complete trees need not fit in one relay frame.
	RelayProjectMemoryTransferProtocol uint32 = 59
	// 60 makes ReserveIdle treat a saved goal as intent, while still refusing
	// running goals, turns, and other work. Older workers need an
	// observed-idle checkpoint barrier to bootstrap their replacement without
	// changing goals.
	RelayIdleGoalIntentProtocol uint32 = 60
	// 61 journals single-attempt handback checks and parent assignment evidence.
	RelayHandbackCheckProtocol uint32 = 61
	// Additive typed GLiDE completed-turn assessment; old readers retain stored action.
	RelayGlideTurnProtocol uint32 = 62
	// Optional assessment attention hint, independent of scheduling action.
	RelayAttentionProtocol uint32 = 63
	// Worker-owned unanswered prose requests.
	RelayOpenRequestProtocol uint32 = 64
	// Structured lifecycle notices for sessions following a pull request.
	RelayFollowedLifecycleProtocol uint32 = 65
	// 66 gives harness startup expiry to the worker. Older workers require the
	// daemon's legacy deadline; workers at this version have exactly one owner.
	RelayWorkerStartupDeadlineProtocol uint32 = 66
	// 67 lets a worker publish whether a finished child turn's work processes
	// are still alive (work_liveness) and adds HoldFollowUp and the process
	// list on HandbackReminder. Older workers publish no liveness, so the
	// daemon neither holds nor delays the park of their children.
	RelayWorkLivenessProtocol uint32 = 67
	// 68 carries a managed child's work facts on the settlement of a turn the
	// harness started on its own, and sets work_liveness from them as it does
	// for a prompted turn. The settlement's bytes change, so an older reader
	// would fail the event's digest.
	RelayHarnessTurnFactsProtocol uint32 = 68
	// The worker publishes the fingerprint of its held harness login.
	RelayHeldLoginProtocol uint32 = 69
	// The worker publishes the failed login it still owes a retry for
	// (authentication_retry_intent), separate from whether it can submit now.
	// It no longer sets authentication_retry_waiting.
	RelayAuthenticationRetryIntentProtocol uint32 = 70
	// 71 reports the notification stack and accepts exact Clear and read pruning.
	RelayNotificationStackProtocol uint32 = 71
	// 72 atomically transfers an interrupted prompt into restart recovery. The
	// continuation ID is digest-covered, so older readers must not consume it.
	RelayRestartRecoveryProtocol uint32 = 72
	// Operation-owned command retention and acknowledgement-only journal GC.
	RelayCommandRetentionProtocol uint32 = 73
	// 74 reserves the next quiescent boundary for cancellable adoption cuts.
	RelayLiveBaselineProtocol uint32 = 74
	RelayProtocolVersion      uint32 = RelayLiveBaselineProtocol
	// Connection-only worker CPU measurements.
	RelayCpuUsageProtocol uint32 = 29

	RelayMinProtocolVersion uint32 = 1
	// The first protocol whose workers read a gzip-compressed skills archive
	// (HELSKIL2). A controller sends an older worker the uncompressed
	// HELSKIL1 format, collected with that format's raw-size limits.
	RelayGzipSkillsProtocol uint32 = 23
	// The first protocol whose workers support conditional whole-tree
	// replacement of session project memory, including deletion of omitted
	// files.
	RelayProjectMemoryReplaceProtocol uint32 = 31
	// Completion replies at this version distinguish a live caller from a late result.
	RelaySubagentDeliveryProtocol uint32 = 31
)

// RelayEventGenesisDigest is the digest for the empty relay event prefix
// (ordinal zero).
const RelayEventGenesisDigest = EventFrontierGenesisDigest

// RelayEventDigestDomain is the domain separator for a v1 (chained) relay
// event digest. v1 records fold previous_digest into the hash, forming a
// linked chain.
var RelayEventDigestDomain = []byte("hel-relay-event-v1\x00")

// RelayEventDigestDomainV2 is the domain separator for a v2 (self-describing)
// relay event digest. v2 records carry no previous_digest; the digest depends
// only on the record's own content, so a corrupt record can never invalidate
// its neighbours.
var RelayEventDigestDomainV2 = []byte("hel-relay-event-v2\x00")

// RelayStateVersion: snapshots at or below this schema are readable; a newer
// schema is rejected. A v1 snapshot is upgraded in place to the current
// schema on open (its stored frontier digests stay valid, since each is
// recomputed with the formula that matches the record's format).
//
// Revision 15 is breaking: unified assessment state and events must not be lost by an older writer.
// Revision 16 is breaking: mailbox state and journal events must not be lost by an older writer.
// Revision 17 is breaking: revision-16 readers require mailbox text fields and cannot preserve structured bodies.
// Revision 18 is breaking: revision-17 readers cannot preserve session-message sender bodies.
// Revision 19 is breaking: revision-18 readers cannot replay a hook lease carrying a Claude steer.
// Revision 20 is breaking: terminal receipts no longer carry execution payloads.
// Revision 21 is breaking: older readers cannot preserve DismissQuestion commands.
// Revision 22 is breaking: older writers cannot preserve handback check admission.
// Revision 23 is breaking: older writers lose requests and the person-input frontier,
// and cannot replay child ownership or question-target observations.
// Revision 24 is breaking: open turns own their autonomous fact; older readers reject the new field.
// Revision 25 is breaking: older readers reject it. Upgrading seeds withdrawn_ordinal from the legacy flag.
// Revision 26 is breaking: an older writer drops set_aside_through, so a dismissed ask would count again.
// Revision 27 is breaking: older writers cannot preserve notification entries or replay their commands.
// Revision 28 is breaking: older writers lose command pins and native-use evidence.
const RelayStateVersion uint32 = 28

// RelayStateFile is the relay snapshot inside a worker root. Teardown and
// restore name it from here rather than repeating the literal.
const RelayStateFile = "relay-state.json"

// RelayJournalDir is the relay's durable event journal inside a worker root.
const RelayJournalDir = "relay-journal"

// RestoredRelaySeedFile is the seed a checkpoint restore leaves in a worker
// root for the relay that opens next. It carries only what a fresh relay
// cannot derive on its own.
const RestoredRelaySeedFile = "relay-seed.json"

// WorkerPIDFile is the file in which a running worker daemon records its own
// PID, inside its worker root. Session teardown reads it to stop that daemon
// before the root it writes to is removed.
const WorkerPIDFile = "worker.pid"

// WorkerStartupFile is the file in which a starting worker daemon records the
// step it is on, inside its worker root. It exists from the first moment of
// the process, before the control socket, so a controller waiting for that
// socket can tell a worker that is still making progress from one that died
// without a word.
const WorkerStartupFile = "worker-startup.json"

// WorkerStartupStepLoginEnvironment is the startup step a worker records
// while it reads the account's login-shell environment. That probe carries
// its own deadline (LoginEnvironmentProbeTimeout), so the controller watching
// a starting worker defers to the probe's deadline for this step instead of
// applying its stall detection to it.
const WorkerStartupStepLoginEnvironment = "login-environment"

// WorkerExitFile is the file a dying worker daemon writes with the reason it
// stopped.
const WorkerExitFile = "worker-exit.json"

// WorkerHarnessStartupFailure is an ACP failure before this worker first
// recorded a configured session.
type WorkerHarnessStartupFailure struct {
	Message string `json:"message"`
}

func (f WorkerHarnessStartupFailure) Error() string { return f.Message }

// ReviewUntrackedFile is the file in which a worker records which untracked
// paths each repository held when its review baseline was taken, so a restart
// measures new files against the same point the baseline belongs to.
const ReviewUntrackedFile = "review-untracked.json"

const RelayActiveSegment = "active.jsonl"

const RelaySegmentByteLimit uint64 = 1024 * 1024

// RelaySnapshotLagByteLimit is the journal bytes a restart may have to replay
// before the snapshot is rewritten. Transcript observations are reconstructed
// by that replay, so they are journaled without their own snapshot write
// until this much has accumulated.
const RelaySnapshotLagByteLimit = 1024 * 1024

const RelayHotEventCapacity = 32

const RelayReplayCursorCapacity = 32

const NativeSessionIdentityFile = "native-session.json"

// HiddenPromptContextURI is the URI of the embedded resource that carries
// controller-only context to a bridge that receives it that way (Codex).
// codex-acp gives the model such a block as
// `{uri}\n<context ref="{uri}">\n{text}\n</context>`, and Codex keeps that
// text in its own history of the prompt.
const HiddenPromptContextURI = "mj://prompt-context"

// StripHiddenPromptContext removes controller-only context that an ACP
// harness copied into a user-facing prompt or title. Hidden context is
// prepended as reserved XML-like blocks, or as codex-acp's wrapping of the
// HiddenPromptContextURI resource; an unterminated reserved block is treated
// as a truncated hidden value, not as text safe to display.
func StripHiddenPromptContext(text string) string {
	for {
		text = strings.TrimLeftFunc(text, unicode.IsSpace)
		if afterURI, ok := strings.CutPrefix(text, HiddenPromptContextURI); ok {
			if rest, ok := afterHiddenContextResource(afterURI); ok {
				text = rest
				continue
			}
			return text
		}
		afterOpen, ok := strings.CutPrefix(text, "<")
		if !ok {
			return text
		}
		openEnd := strings.IndexByte(afterOpen, '>')
		if openEnd < 0 {
			if reservedHiddenContextPrefix(afterOpen) {
				return ""
			}
			return text
		}
		tag := afterOpen[:openEnd]
		if !reservedHiddenContextTag(tag) {
			return text
		}
		closeTag := "</" + tag + ">"
		afterOpen = afterOpen[openEnd+1:]
		closeStart := strings.LastIndex(afterOpen, closeTag)
		if closeStart < 0 {
			return ""
		}
		text = afterOpen[closeStart+len(closeTag):]
	}
}

// afterHiddenContextResource is what follows codex-acp's wrapping of the
// hidden context resource, given the text after its leading URI: empty when
// the wrapping is cut short, and false when the URI does not open one.
func afterHiddenContextResource(text string) (string, bool) {
	const closeTag = "</context>"
	open := `<context ref="` + HiddenPromptContextURI + `">`
	text = strings.TrimLeftFunc(text, unicode.IsSpace)
	inner, ok := strings.CutPrefix(text, open)
	if !ok {
		return "", strings.HasPrefix(open, text)
	}
	// The context is a run of reserved blocks (project memory, shell
	// results), or a plain hand-off, so the wrapping closes after the reserved
	// blocks or at the first close tag of the hand-off.
	inner = StripHiddenPromptContext(inner)
	if index := strings.Index(inner, closeTag); index >= 0 {
		return inner[index+len(closeTag):], true
	}
	return "", true
}

func reservedHiddenContextPrefix(text string) bool {
	return strings.HasPrefix(text, "hel-") || strings.HasPrefix(text, "mj-") || strings.HasPrefix(text, "user_shell_command")
}

func reservedHiddenContextTag(tag string) bool {
	if strings.HasPrefix(tag, "hel-") || strings.HasPrefix(tag, "mj-") {
		for i := 0; i < len(tag); i++ {
			b := tag[i]
			if !(b >= 'a' && b <= 'z' || b >= '0' && b <= '9' || b == '-') {
				return tag == "user_shell_command"
			}
		}
		return true
	}
	return tag == "user_shell_command"
}

// AcpActivityClock is the process-local clock for inbound ACP traffic. It
// deliberately stays out of the durable relay journal: this is render timing,
// not recoverable session history. Copies share one clock, as Rust's Arc did.
type AcpActivityClock struct {
	millis *atomic.Int64
}

// NewAcpActivityClock returns a clock that has not yet seen ACP traffic.
func NewAcpActivityClock() AcpActivityClock {
	return AcpActivityClock{millis: new(atomic.Int64)}
}

func (c AcpActivityClock) Mark() { c.millis.Store(EpochMillis()) }

// LastAtMs is the last time ACP traffic arrived, if any did.
func (c AcpActivityClock) LastAtMs() (int64, bool) {
	value := c.millis.Load()
	return value, value > 0
}

// RestoredRelaySeed is what a restored relay needs from the checkpoint it
// continues.
//
// A restore used to leave the whole canonical session here, but the relay
// only ever read three fields from it. On a large session the unread
// transcript was tens of megabytes written on the target and parsed again at
// worker start. Unknown fields are refused (serde deny_unknown_fields).
type RestoredRelaySeed struct {
	// Ignored. Archives written by protocol 26 workers carried a command
	// receipt ledger here; it is accepted so those archives still restore.
	CommandLedger json.RawMessage `json:"command_ledger,omitempty"`
	// Versioned worker assessment state, validated by assessment Checkpoint.
	AssessmentState json.RawMessage `json:"assessment_state,omitempty"`
	// Worker-owned state outside the bounded assessment record.
	NotificationStack   *NotificationStack `json:"notification_stack,omitempty"`
	EventFrontier       uint64             `json:"event_frontier"`
	EventFrontierDigest string             `json:"event_frontier_digest"`
	// Commands the archived session still had queued, already filtered by the
	// restore spec's queue disposition.
	QueuedPrompts []CanonicalQueuedPrompt `json:"queued_prompts"`
	// The model and effort the archived session had accepted, when the
	// restore continues its native conversation. The restored worker pins
	// them on its first bridge start, as it does after any restart, so a
	// resume keeps the session's model instead of the harness default.
	AcceptedConfig map[string]string `json:"accepted_config,omitempty"`
	// The archived conversation shows that the native session this restore
	// continues never received a prompt, so its harness wrote no history for
	// it. The restored worker may then replace that session when the
	// harness cannot find it, exactly as it may for a session its own journal
	// opened and never used. Seeds from older releases omit the field, which
	// reads as false: the imported identity counts as used.
	NativeSessionUnused bool `json:"native_session_unused,omitempty"`
	// The archived worker's mail, already selected by the restore spec's
	// queue policy: pending events are absent when the controller admits
	// them after readiness, and claimed prompt mail is absent when the queue
	// is dropped. Seeds from older releases omit it. (Rust commit 39722ace5,
	// after this file's base; ported for mj-worker's relay.)
	Mailbox *CanonicalMailbox `json:"mailbox,omitempty"`
}

// UnmarshalJSON refuses unknown fields, as serde's deny_unknown_fields does.
func (s *RestoredRelaySeed) UnmarshalJSON(data []byte) error {
	type plain RestoredRelaySeed
	if err := relayRejectUnknownFields(data, reflect.TypeFor[plain](), ""); err != nil {
		return err
	}
	return json.Unmarshal(data, (*plain)(s))
}

// MarshalJSON writes absent queued prompts as [], as serde writes the Vec;
// a Rust worker refuses a null queue.
func (s RestoredRelaySeed) MarshalJSON() ([]byte, error) {
	type plain RestoredRelaySeed
	v := plain(s)
	if v.QueuedPrompts == nil {
		v.QueuedPrompts = []CanonicalQueuedPrompt{}
	}
	return transcriptMarshal(v)
}

// Validate applies the same frontier checks a canonical session snapshot
// carries, so a malformed seed is refused before it can become relay state.
func (s *RestoredRelaySeed) Validate() error {
	if s.NotificationStack != nil {
		if err := s.NotificationStack.validateAt(s.EventFrontier); err != nil {
			return err
		}
	}
	if len(s.AssessmentState) > 0 && string(s.AssessmentState) != "null" {
		var value any
		if err := json.Unmarshal(s.AssessmentState, &value); err != nil {
			return err
		}
		if _, err := CheckpointDecode(value); err != nil {
			return err
		}
	}
	if s.Mailbox != nil {
		if err := s.Mailbox.Validate(); err != nil {
			return err
		}
	}
	if err := ValidateRelayDigest(s.EventFrontierDigest, "restored relay event frontier digest"); err != nil {
		return err
	}
	if (s.EventFrontier == 0) != (s.EventFrontierDigest == RelayEventGenesisDigest) {
		return errors.New("restored relay event frontier and genesis digest disagree")
	}
	return nil
}

func RestoredRelaySeedPath(relayRoot string) string {
	return filepath.Join(relayRoot, RestoredRelaySeedFile)
}

func ClearNativeSessionIdentity(root string) error {
	path := filepath.Join(root, NativeSessionIdentityFile)
	if err := os.Remove(path); err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return nil
		}
		return fmt.Errorf("remove %s: %w", path, err)
	}
	return SyncDirectory(root)
}

// --- serde enum encoding shared by the relay modules ---

// relayMarshalJSON encodes v as serde_json does: without Go's HTML escaping
// of <, > and &, and without a trailing newline. Relay digests hash these
// bytes, so they must match the Rust encoding. (Go still escapes U+2028 and
// U+2029, which serde_json writes raw.)
func relayMarshalJSON(v any) ([]byte, error) {
	var b bytes.Buffer
	encoder := json.NewEncoder(&b)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(b.Bytes(), []byte("\n")), nil
}

// relayAdjacentJSON writes a serde adjacently tagged variant:
// {"<tag>":"<name>","<content>":<fields>}, or {"<tag>":"<name>"} for a unit
// variant (fields nil).
func relayAdjacentJSON(tag, content, name string, fields any) ([]byte, error) {
	var b bytes.Buffer
	b.WriteByte('{')
	relayWriteKey(&b, tag)
	relayWriteString(&b, name)
	if fields != nil {
		encoded, err := relayMarshalJSON(fields)
		if err != nil {
			return nil, err
		}
		b.WriteByte(',')
		relayWriteKey(&b, content)
		b.Write(encoded)
	}
	b.WriteByte('}')
	return b.Bytes(), nil
}

// relayInternalJSON writes a serde internally tagged variant: the tag first,
// then the variant's own fields.
func relayInternalJSON(tag, name string, fields any) ([]byte, error) {
	var b bytes.Buffer
	b.WriteByte('{')
	relayWriteKey(&b, tag)
	relayWriteString(&b, name)
	if fields != nil {
		encoded, err := relayMarshalJSON(fields)
		if err != nil {
			return nil, err
		}
		inner := bytes.TrimSpace(encoded)
		if len(inner) < 2 || inner[0] != '{' {
			return nil, fmt.Errorf("internally tagged variant %q is not an object", name)
		}
		if body := bytes.TrimSpace(inner[1 : len(inner)-1]); len(body) > 0 {
			b.WriteByte(',')
			b.Write(body)
		}
	}
	b.WriteByte('}')
	return b.Bytes(), nil
}

func relayWriteKey(b *bytes.Buffer, key string) {
	relayWriteString(b, key)
	b.WriteByte(':')
}

func relayWriteString(b *bytes.Buffer, text string) {
	encoded, _ := relayMarshalJSON(text)
	b.Write(encoded)
}

// relaySplitAdjacent reads the tag and content of a serde adjacently tagged
// value. strict refuses keys other than the two (deny_unknown_fields).
func relaySplitAdjacent(data []byte, tag, content string, strict bool) (string, json.RawMessage, error) {
	var object map[string]json.RawMessage
	if err := json.Unmarshal(data, &object); err != nil {
		return "", nil, err
	}
	rawName, ok := object[tag]
	if !ok {
		return "", nil, fmt.Errorf("missing field %q", tag)
	}
	var name string
	if err := json.Unmarshal(rawName, &name); err != nil {
		return "", nil, fmt.Errorf("field %q: %w", tag, err)
	}
	if strict {
		for key := range object {
			if key != tag && key != content {
				return "", nil, fmt.Errorf("unknown field %q, expected %q or %q", key, tag, content)
			}
		}
	}
	return name, object[content], nil
}

// relaySplitInternal reads the tag of a serde internally tagged value.
func relaySplitInternal(data []byte, tag string) (string, error) {
	var object map[string]json.RawMessage
	if err := json.Unmarshal(data, &object); err != nil {
		return "", err
	}
	rawName, ok := object[tag]
	if !ok {
		return "", fmt.Errorf("missing field %q", tag)
	}
	var name string
	if err := json.Unmarshal(rawName, &name); err != nil {
		return "", fmt.Errorf("field %q: %w", tag, err)
	}
	return name, nil
}

// relayVariant decodes the content of one adjacently tagged variant into T.
func relayVariant[T any, I any](strict bool) func(json.RawMessage) (I, error) {
	return func(body json.RawMessage) (I, error) {
		var value T
		if len(body) > 0 && string(body) != "null" {
			if strict {
				if err := relayRejectUnknownFields(body, reflect.TypeFor[T](), ""); err != nil {
					var zero I
					return zero, err
				}
			}
			if err := json.Unmarshal(body, &value); err != nil {
				var zero I
				return zero, err
			}
		}
		return any(value).(I), nil
	}
}

// relayRequiredVariant is relayVariant(true) that also refuses a body missing
// a required field, as serde does for every field that is not an Option: a
// field is required unless it is a pointer or tagged omitempty.
func relayRequiredVariant[T any, I any]() func(json.RawMessage) (I, error) {
	decode := relayVariant[T, I](true)
	return func(body json.RawMessage) (I, error) {
		if err := relayRequireFields(body, reflect.TypeFor[T]()); err != nil {
			var zero I
			return zero, err
		}
		return decode(body)
	}
}

// relayRequireFields refuses an object (or an absent body, nil or null) that
// lacks a field structType requires.
func relayRequireFields(data []byte, structType reflect.Type) error {
	var object map[string]json.RawMessage
	if len(data) > 0 {
		if err := json.Unmarshal(data, &object); err != nil {
			return err
		}
	}
	for i := 0; i < structType.NumField(); i++ {
		field := structType.Field(i)
		name, options, _ := strings.Cut(field.Tag.Get("json"), ",")
		if name == "-" || field.Type.Kind() == reflect.Pointer || strings.Contains(","+options+",", ",omitempty,") {
			continue
		}
		if name == "" {
			name = field.Name
		}
		if _, ok := object[name]; !ok {
			if object == nil {
				return errors.New("missing field `params`")
			}
			return fmt.Errorf("missing field `%s`", name)
		}
	}
	return nil
}

// relayInternalVariant decodes one internally tagged variant into T; the tag
// key is the only field T does not declare.
func relayInternalVariant[T any, I any](tag string, strict bool) func(json.RawMessage) (I, error) {
	return func(body json.RawMessage) (I, error) {
		var value T
		if strict {
			if err := relayRejectUnknownFields(body, reflect.TypeFor[T](), tag); err != nil {
				var zero I
				return zero, err
			}
		}
		if err := json.Unmarshal(body, &value); err != nil {
			var zero I
			return zero, err
		}
		return any(value).(I), nil
	}
}

// relayRejectUnknownFields refuses object keys that the struct type does not
// declare in its json tags (serde deny_unknown_fields). extra is one more
// permitted key, such as an internal tag; empty for none.
func relayRejectUnknownFields(data []byte, structType reflect.Type, extra string) error {
	var object map[string]json.RawMessage
	if err := json.Unmarshal(data, &object); err != nil {
		return err
	}
	known := map[string]bool{}
	if extra != "" {
		known[extra] = true
	}
	for i := 0; i < structType.NumField(); i++ {
		field := structType.Field(i)
		name, _, _ := strings.Cut(field.Tag.Get("json"), ",")
		if name == "-" {
			continue
		}
		if name == "" {
			name = field.Name
		}
		known[name] = true
	}
	for key := range object {
		if !known[key] {
			return fmt.Errorf("unknown field %q", key)
		}
	}
	return nil
}
