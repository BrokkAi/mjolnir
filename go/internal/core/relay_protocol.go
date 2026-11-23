// Port of mj-core/src/relay/protocol.rs.
//
// Wire protocol for the durable ACP relay: request/response envelopes, error
// shapes, and newline-delimited JSON framing. This module is pure serde plus
// byte-oriented framing; it has no filesystem or state-machine concerns of its
// own.

package core

import (
	"bufio"
	"bytes"
	"compress/gzip"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"reflect"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"
)

// RelayVersionRange refuses unknown fields.
type RelayVersionRange struct {
	Min uint32 `json:"min"`
	Max uint32 `json:"max"`
}

// UnmarshalJSON refuses unknown fields, as serde's deny_unknown_fields does.
func (r *RelayVersionRange) UnmarshalJSON(data []byte) error {
	type plain RelayVersionRange
	if err := relayRejectUnknownFields(data, reflect.TypeFor[plain](), ""); err != nil {
		return err
	}
	return json.Unmarshal(data, (*plain)(r))
}

// RelayVersionRangeCurrent is the range this build speaks.
var RelayVersionRangeCurrent = RelayVersionRange{Min: RelayMinProtocolVersion, Max: RelayProtocolVersion}

func (r RelayVersionRange) Contains(version uint32) bool {
	return r.Min <= version && version <= r.Max
}

// Negotiate is the highest version both ranges contain, if any.
func (r RelayVersionRange) Negotiate(peer RelayVersionRange) (uint32, bool) {
	minimum := max(r.Min, peer.Min)
	maximum := min(r.Max, peer.Max)
	return maximum, minimum <= maximum
}

// ProjectMemoryWireTree is a connection-only memory tree. Plain trees retain
// the pre-44 wire shape. Compression and expansion must run on a blocking
// thread; expansion has no size cap because both endpoints are trusted mj
// processes. Serialized untagged; unknown fields are refused.
type ProjectMemoryWireTree interface {
	json.Marshaler
	isProjectMemoryWireTree()
}

type ProjectMemoryWireTreePlain struct {
	Snapshot ProjectMemorySnapshot
}

func (ProjectMemoryWireTreePlain) isProjectMemoryWireTree() {}

// MarshalJSON writes the plain tree exactly as the snapshot.
func (t ProjectMemoryWireTreePlain) MarshalJSON() ([]byte, error) {
	return relayMarshalJSON(t.Snapshot)
}

type ProjectMemoryWireTreeGzip struct {
	Gzip string `json:"gzip"`
}

func (ProjectMemoryWireTreeGzip) isProjectMemoryWireTree() {}

// MarshalJSON writes {"gzip": ...}.
func (t ProjectMemoryWireTreeGzip) MarshalJSON() ([]byte, error) {
	type plain ProjectMemoryWireTreeGzip
	return relayMarshalJSON(plain(t))
}

// UnmarshalProjectMemoryWireTreeJSON decodes the untagged form: the first
// variant whose fields match, plain before gzip.
func UnmarshalProjectMemoryWireTreeJSON(data []byte) (ProjectMemoryWireTree, error) {
	if relayRejectUnknownFields(data, reflect.TypeFor[ProjectMemorySnapshot](), "") == nil {
		var plain ProjectMemoryWireTreePlain
		if err := json.Unmarshal(data, &plain.Snapshot); err == nil {
			return plain, nil
		}
	}
	type gzipFields ProjectMemoryWireTreeGzip
	if relayRejectUnknownFields(data, reflect.TypeFor[gzipFields](), "") == nil {
		var compressed gzipFields
		if err := json.Unmarshal(data, &compressed); err == nil {
			return ProjectMemoryWireTreeGzip(compressed), nil
		}
	}
	return nil, errors.New("data did not match any variant of untagged enum ProjectMemoryWireTree")
}

func ProjectMemoryWireTreeEncode(tree ProjectMemorySnapshot, protocol uint32) (ProjectMemoryWireTree, error) {
	if protocol < RelayCompressedProjectMemoryProtocol {
		return ProjectMemoryWireTreePlain{Snapshot: tree}, nil
	}
	var compressed bytes.Buffer
	encoder := gzip.NewWriter(&compressed)
	files, err := relayMarshalJSON(tree.Files)
	if err == nil {
		_, err = encoder.Write(files)
	}
	if err != nil {
		return nil, fmt.Errorf("compress project memory files: %w", err)
	}
	if err := encoder.Close(); err != nil {
		return nil, fmt.Errorf("finish project memory gzip: %w", err)
	}
	return ProjectMemoryWireTreeGzip{Gzip: base64.StdEncoding.EncodeToString(compressed.Bytes())}, nil
}

func ProjectMemoryWireTreeDecode(tree ProjectMemoryWireTree) (ProjectMemorySnapshot, error) {
	switch tree := tree.(type) {
	case ProjectMemoryWireTreePlain:
		return tree.Snapshot, nil
	case ProjectMemoryWireTreeGzip:
		compressed, err := base64.StdEncoding.DecodeString(tree.Gzip)
		if err != nil {
			return ProjectMemorySnapshot{}, fmt.Errorf("decode project memory base64: %w", err)
		}
		var snapshot ProjectMemorySnapshot
		decoder, err := gzip.NewReader(bytes.NewReader(compressed))
		if err == nil {
			err = json.NewDecoder(decoder).Decode(&snapshot.Files)
		}
		if err != nil {
			return ProjectMemorySnapshot{}, fmt.Errorf("decompress project memory files: %w", err)
		}
		return snapshot, nil
	}
	return ProjectMemorySnapshot{}, fmt.Errorf("unknown project memory wire tree %T", tree)
}

// ProjectMemoryWireSnapshot is the complete connection-only project-memory
// snapshot transferred in protocol 59. Serializing this value once gives
// every page the same frozen baseline and replica, even if either tree
// changes while pages are read. Unknown fields are refused.
type ProjectMemoryWireSnapshot struct {
	Baseline ProjectMemoryWireTree `json:"baseline"`
	Replica  ProjectMemoryWireTree `json:"replica"`
}

// UnmarshalJSON refuses unknown fields and decodes both trees.
func (s *ProjectMemoryWireSnapshot) UnmarshalJSON(data []byte) error {
	var wire struct {
		Baseline json.RawMessage `json:"baseline"`
		Replica  json.RawMessage `json:"replica"`
	}
	if err := relayRejectUnknownFields(data, reflect.TypeOf(wire), ""); err != nil {
		return err
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	baseline, err := UnmarshalProjectMemoryWireTreeJSON(wire.Baseline)
	if err != nil {
		return fmt.Errorf("baseline: %w", err)
	}
	replica, err := UnmarshalProjectMemoryWireTreeJSON(wire.Replica)
	if err != nil {
		return fmt.Errorf("replica: %w", err)
	}
	s.Baseline, s.Replica = baseline, replica
	return nil
}

func ProjectMemoryWireSnapshotEncode(baseline, replica ProjectMemorySnapshot, protocol uint32) ([]byte, error) {
	encodedBaseline, err := ProjectMemoryWireTreeEncode(baseline, protocol)
	if err != nil {
		return nil, err
	}
	encodedReplica, err := ProjectMemoryWireTreeEncode(replica, protocol)
	if err != nil {
		return nil, err
	}
	encoded, err := relayMarshalJSON(ProjectMemoryWireSnapshot{Baseline: encodedBaseline, Replica: encodedReplica})
	if err != nil {
		return nil, fmt.Errorf("serialize project memory transfer snapshot: %w", err)
	}
	return encoded, nil
}

func ProjectMemoryWireSnapshotDecode(data []byte) (ProjectMemorySnapshot, ProjectMemorySnapshot, error) {
	var snapshot ProjectMemoryWireSnapshot
	if err := json.Unmarshal(data, &snapshot); err != nil {
		return ProjectMemorySnapshot{}, ProjectMemorySnapshot{}, fmt.Errorf("parse project memory transfer snapshot: %w", err)
	}
	baseline, err := ProjectMemoryWireTreeDecode(snapshot.Baseline)
	if err != nil {
		return ProjectMemorySnapshot{}, ProjectMemorySnapshot{}, err
	}
	replica, err := ProjectMemoryWireTreeDecode(snapshot.Replica)
	return baseline, replica, err
}

// ProjectMemoryTransferRequestChunkBytes is the largest raw byte slice that
// fits in a protocol-59 append request, using the exact serialized envelope
// overhead and base64 expansion.
func ProjectMemoryTransferRequestChunkBytes(protocolVersion uint32, requestID string) (int, error) {
	return RelayTransferChunkCapacity(RelayRequestEnvelope{
		RequestID:       requestID,
		ProtocolVersion: protocolVersion,
		Request: RelayRequestProjectMemoryTransfer{Action: ProjectMemoryTransferActionAppendUpload{
			TransferID: ^uint64(0),
			Offset:     ^uint64(0),
		}},
	})
}

// ProjectMemoryTransferResponseChunkBytes is the largest raw byte slice that
// fits in a protocol-59 snapshot-page response, using the exact serialized
// envelope overhead and base64 expansion.
func ProjectMemoryTransferResponseChunkBytes(protocolVersion uint32, requestID string) (int, error) {
	return RelayTransferChunkCapacity(RelayResponseEnvelope{
		RequestID:       requestID,
		ProtocolVersion: protocolVersion,
		Body: RelayResponseBodyOk{Payload: RelayResponsePayloadProjectMemoryTransfer{
			Result: ProjectMemoryTransferResultSnapshotPage{
				TransferID: ^uint64(0),
				Offset:     ^uint64(0),
				NextOffset: ^uint64(0),
			},
		}},
	})
}

func RelayTransferChunkCapacity(envelope any) (int, error) {
	// The base64 field is empty in the serialized template, so its encoded
	// length can be added without JSON escaping. Include the newline because
	// the worker's bounded reader counts it against the frame budget.
	encoded, err := relayMarshalJSON(envelope)
	if err != nil {
		return 0, err
	}
	overhead := len(encoded) + 1
	if overhead > MaxFrameBytes {
		return 0, errors.New("snapshot transfer envelope exceeds relay frame budget")
	}
	capacity := ((MaxFrameBytes - overhead) / 4) * 3
	if capacity <= 0 {
		return 0, errors.New("snapshot transfer envelope leaves no room for a chunk")
	}
	return capacity, nil
}

type CaptureScope string

const CaptureScopePushableCommits CaptureScope = "pushable_commits"

// RepoDelta is what one repository contributed to a cumulative review delta.
// Unknown fields are refused.
type RepoDelta struct {
	Root       string  `json:"root"`
	BaseCommit *string `json:"base_commit,omitempty"`
	HeadCommit *string `json:"head_commit,omitempty"`
	BaseKind   *string `json:"base_kind,omitempty"`
	// The tree-ish Patch is diffed from: the baseline, with every change
	// the turn did not write already in it (see MergedIn). Nil when
	// there is no usable baseline and coverage starts at this capture.
	BaselineTree *string `json:"baseline_tree"`
	// The capture, which a completed review records as the next baseline: a
	// Git tag naming the captured tree and the HEAD it was taken at, or a
	// bare tree id from an earlier worker. Either is a tree-ish.
	CurrentTree string `json:"current_tree"`
	// Unified diff, bounded worker-side; an empty patch means this repository
	// has nothing to review.
	Patch string `json:"patch"`
	// Human-readable file and line totals, computed from the untruncated
	// patch so bounding cannot make a change look smaller than it is.
	Diffstat     string `json:"diffstat"`
	ChangedLines int    `json:"changed_lines"`
	// Lines added and removed in each changed file, from git diff --numstat
	// between the baseline and the captured tree. Like Diffstat it is
	// computed from the whole change, so it lists every file even when
	// Patch was cut short. Empty when there is no usable baseline, and from
	// a worker that predates it.
	Files []FileLineChange `json:"files,omitempty"`
	// Commits of other history the turn merged, pulled or switched to. Their
	// changes are part of BaselineTree, so Patch holds only the turn's
	// own commits, how it resolved its merges, and its uncommitted edits.
	// Empty from a worker that predates it.
	MergedIn []string `json:"merged_in,omitempty"`
	// Why the worker could not tell the turn's commits from other history,
	// so Patch may hold changes the turn pulled, merged or switched to.
	// Nil when it could, and from a worker that predates it.
	Unattributed *string `json:"unattributed,omitempty"`
	// Reason a push was recorded without review because Git cannot construct
	// the merged review point. Older workers omit this field.
	PushReviewWarning *string `json:"push_review_warning,omitempty"`
}

// UnmarshalJSON refuses unknown fields, as serde's deny_unknown_fields does.
func (d *RepoDelta) UnmarshalJSON(data []byte) error {
	type plain RepoDelta
	if err := relayRejectUnknownFields(data, reflect.TypeFor[plain](), ""); err != nil {
		return err
	}
	return json.Unmarshal(data, (*plain)(d))
}

// FileLineChange is one changed file's line counts in a review capture.
// Unknown fields are refused.
type FileLineChange struct {
	// The file's path in the captured tree, relative to the repository root.
	Path string `json:"path"`
	// The path it was renamed or copied from, when Git detected one.
	OldPath    *string `json:"old_path,omitempty"`
	Insertions int     `json:"insertions"`
	Deletions  int     `json:"deletions"`
	// Git reports no line counts for a binary file.
	Binary bool `json:"binary,omitempty"`
}

// UnmarshalJSON refuses unknown fields, as serde's deny_unknown_fields does.
func (c *FileLineChange) UnmarshalJSON(data []byte) error {
	type plain FileLineChange
	if err := relayRejectUnknownFields(data, reflect.TypeFor[plain](), ""); err != nil {
		return err
	}
	return json.Unmarshal(data, (*plain)(c))
}

func ReviewerRequestActionName(request ReviewerRequest) string {
	switch request.(type) {
	case ReviewerRequestCaptureDelta:
		return "reviewer_capture_delta"
	case ReviewerRequestRecordReviewed:
		return "reviewer_record_reviewed"
	case ReviewerRequestRecordOverridden:
		return "reviewer_record_overridden"
	case ReviewerRequestSetReviewBase:
		return "reviewer_set_review_base"
	}
	panic(fmt.Sprintf("unknown reviewer request %T", request))
}

func RelayRequestMethodName(request RelayRequest) string {
	switch request := request.(type) {
	case RelayRequestHello:
		return "hello"
	case RelayRequestAttach:
		return "attach"
	case RelayRequestAcknowledge:
		return "acknowledge"
	case RelayRequestSubmit:
		return "submit"
	case RelayRequestReadPendingElicitation:
		return "read_pending_elicitation"
	case RelayRequestReadQueuedCommand:
		return "read_queued_command"
	case RelayRequestReadCheckpointMailbox:
		return "read_checkpoint_mailbox"
	case RelayRequestBeginLiveBaseline:
		return "begin_live_baseline"
	case RelayRequestReadLiveBaselinePage:
		return "read_live_baseline_page"
	case RelayRequestAbortLiveBaseline:
		return "abort_live_baseline"
	case RelayRequestCommandRetentionStatus:
		return "command_retention_status"
	case RelayRequestUpdateCommandRetention:
		return "update_command_retention"
	case RelayRequestStatus:
		return "status"
	case RelayRequestDrainMailbox:
		return "drain_mailbox"
	case RelayRequestAckMailbox:
		return "ack_mailbox"
	case RelayRequestReserveIdle:
		return "reserve_idle"
	case RelayRequestInstallPromptContext:
		return "install_prompt_context"
	case RelayRequestProjectMemorySnapshot:
		return "project_memory_snapshot"
	case RelayRequestInstallProjectMemorySnapshot:
		return "install_project_memory_snapshot"
	case RelayRequestReplaceProjectMemoryTree:
		return "replace_project_memory_tree"
	case RelayRequestProjectMemoryTransfer:
		return "project_memory_transfer"
	case RelayRequestAttachmentPresent:
		return "attachment_present"
	case RelayRequestInstallAttachment:
		return "install_attachment"
	case RelayRequestReadAttachment:
		return "read_attachment"
	case RelayRequestCpuUsage:
		return "cpu_usage"
	case RelayRequestCredentialState:
		return "credential_state"
	case RelayRequestReadCredentials:
		return "read_credentials"
	case RelayRequestInstallCredentials:
		return "install_credentials"
	case RelayRequestSkillsState:
		return "skills_state"
	case RelayRequestInstallSkills:
		return "install_skills"
	case RelayRequestGithubTokenState:
		return "github_token_state"
	case RelayRequestInstallGithubToken:
		return "install_github_token"
	case RelayRequestRemoveGithubToken:
		return "remove_github_token"
	case RelayRequestAwsCredentialsState:
		return "aws_credentials_state"
	case RelayRequestInstallAwsCredentials:
		return "install_aws_credentials"
	case RelayRequestRemoveAwsCredentials:
		return "remove_aws_credentials"
	case RelayRequestAwsCredentialsScope:
		return "aws_credentials_scope"
	case RelayRequestSlopcopLoginState:
		return "slopcop_login_state"
	case RelayRequestInstallSlopcopLogin:
		return "install_slopcop_login"
	case RelayRequestRemoveSlopcopLogin:
		return "remove_slopcop_login"
	case RelayRequestRespondElicitation:
		return "respond_elicitation"
	case RelayRequestStopBackgroundTask:
		return "stop_background_task"
	case RelayRequestSubagentRequests:
		return "subagent_requests"
	case RelayRequestPollSubagentQueue:
		return "poll_subagent_queue"
	case RelayRequestWaitForChange:
		return "wait_for_change"
	case RelayRequestSetSubagentAdmission:
		return "set_subagent_admission"
	case RelayRequestSetOperatorAway:
		return "set_operator_away"
	case RelayRequestHistoryQuery:
		return "history_query"
	case RelayRequestHistoryRequests:
		return "history_requests"
	case RelayRequestCompleteHistoryRequest:
		return "complete_history_request"
	case RelayRequestCompleteSubagentRequest:
		return "complete_subagent_request"
	case RelayRequestReviewer:
		return ReviewerRequestActionName(request.Request)
	}
	panic(fmt.Sprintf("unknown relay request %T", request))
}

// RelayRequestMinimumProtocol is the oldest protocol that understands this
// method or command payload. Form answers landed in protocol 2, hidden
// context in 3, project-memory sync in 4, user shell commands in 5, the
// reviewer sidecar in 6, the non-steering turn cancellation in 7, and
// project-memory replacement in protocol 31.
func RelayRequestMinimumProtocol(request RelayRequest) uint32 {
	switch request := request.(type) {
	case RelayRequestAwsCredentialsState, RelayRequestInstallAwsCredentials:
		return RelayAwsCredentialsProtocol
	case RelayRequestRemoveAwsCredentials, RelayRequestAwsCredentialsScope:
		return RelayAwsCredentialsRemovalProtocol
	case RelayRequestSlopcopLoginState, RelayRequestInstallSlopcopLogin, RelayRequestRemoveSlopcopLogin:
		return RelaySlopcopLoginProtocol
	case RelayRequestCpuUsage:
		return RelayCpuUsageProtocol
	case RelayRequestDrainMailbox, RelayRequestAckMailbox:
		return 33
	case RelayRequestReserveIdle:
		return 21
	case RelayRequestHistoryQuery, RelayRequestHistoryRequests, RelayRequestCompleteHistoryRequest:
		return 16
	case RelayRequestAttachmentPresent, RelayRequestInstallAttachment, RelayRequestReadAttachment:
		return 8
	case RelayRequestStopBackgroundTask:
		return 9
	case RelayRequestSubagentRequests, RelayRequestCompleteSubagentRequest:
		return 12
	case RelayRequestPollSubagentQueue:
		return RelaySubagentFeedProtocol
	case RelayRequestWaitForChange:
		return RelayChangeWaitProtocol
	case RelayRequestSetSubagentAdmission:
		return 32
	case RelayRequestSetOperatorAway:
		return RelayOperatorAwayProtocol
	case RelayRequestRespondElicitation:
		return 2
	case RelayRequestInstallPromptContext:
		return 3
	case RelayRequestProjectMemorySnapshot, RelayRequestInstallProjectMemorySnapshot:
		return 4
	case RelayRequestReplaceProjectMemoryTree:
		return RelayProjectMemoryReplaceProtocol
	case RelayRequestProjectMemoryTransfer:
		return RelayProjectMemoryTransferProtocol
	case RelayRequestBeginLiveBaseline, RelayRequestReadLiveBaselinePage, RelayRequestAbortLiveBaseline:
		return RelayLiveBaselineProtocol
	case RelayRequestReadPendingElicitation, RelayRequestReadQueuedCommand, RelayRequestCommandRetentionStatus,
		RelayRequestUpdateCommandRetention:
		return RelayCommandRetentionProtocol
	case RelayRequestReadCheckpointMailbox:
		return RelayCheckpointMailboxProtocol
	case RelayRequestSubmit:
		return RelayCommandMinimumProtocol(request.Command)
	case RelayRequestReviewer:
		switch reviewer := request.Request.(type) {
		case ReviewerRequestCaptureDelta:
			if reviewer.Repositories != nil {
				return RelayWorktreeReviewProtocol
			}
			if reviewer.Scope == CaptureScopePushableCommits {
				return RelayPushReviewProtocol
			}
		case ReviewerRequestRecordOverridden, ReviewerRequestSetReviewBase:
			return RelayReviewBaseProtocol
		case ReviewerRequestRecordReviewed:
			return RelayPushReviewProtocol
		}
	}
	return RelayMinProtocolVersion
}

func RelayRequestSupportedAt(request RelayRequest, protocolVersion uint32) bool {
	return RelayVersionRangeCurrent.Contains(protocolVersion) && protocolVersion >= RelayRequestMinimumProtocol(request)
}

// RelayProtocolRejection is the worker-side protocol rule, applied once where
// requests arrive: a relay serves only RelayProtocolVersion. Hello is exempt
// because it is how the two sides discover each other's versions. Nil means
// the request is accepted.
func RelayProtocolRejection(envelope *RelayRequestEnvelope) RelayResponseBody {
	if _, hello := envelope.Request.(RelayRequestHello); hello || envelope.ProtocolVersion == RelayProtocolVersion {
		return nil
	}
	return IncompatibleRequestProtocol(envelope.ProtocolVersion)
}

func IncompatibleRequestProtocol(protocolVersion uint32) RelayResponseBody {
	return RelayError(
		RelayErrorCodeIncompatibleProtocol,
		fmt.Sprintf("request uses protocol %d, relay supports protocol %d", protocolVersion, RelayProtocolVersion),
		false,
		nil,
	)
}

// RelayRequestEnvelope refuses unknown fields. A protocol-33 envelope is
// written with mailbox commands in their legacy text representation.
type RelayRequestEnvelope struct {
	RequestID       string       `json:"request_id"`
	ProtocolVersion uint32       `json:"protocol_version"`
	Request         RelayRequest `json:"request"`
}

// MarshalJSON writes the envelope, rewriting mailbox commands for a
// protocol-33 peer.
func (e RelayRequestEnvelope) MarshalJSON() ([]byte, error) {
	request, err := relayMarshalJSON(e.Request)
	if err != nil {
		return nil, err
	}
	if e.ProtocolVersion == RelayLegacyMailboxProtocol {
		if request, err = rewriteProtocol33MailboxCommands(request); err != nil {
			return nil, err
		}
	}
	return relayMarshalJSON(struct {
		RequestID       string          `json:"request_id"`
		ProtocolVersion uint32          `json:"protocol_version"`
		Request         json.RawMessage `json:"request"`
	}{e.RequestID, e.ProtocolVersion, request})
}

// UnmarshalJSON refuses unknown fields and decodes the request by its method.
func (e *RelayRequestEnvelope) UnmarshalJSON(data []byte) error {
	var wire struct {
		RequestID       *string         `json:"request_id"`
		ProtocolVersion *uint32         `json:"protocol_version"`
		Request         json.RawMessage `json:"request"`
	}
	if err := relayRejectUnknownFields(data, reflect.TypeOf(wire), ""); err != nil {
		return err
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	switch {
	case wire.RequestID == nil:
		return errors.New("missing field `request_id`")
	case wire.ProtocolVersion == nil:
		return errors.New("missing field `protocol_version`")
	case wire.Request == nil:
		return errors.New("missing field `request`")
	}
	request, err := UnmarshalRelayRequestJSON(wire.Request)
	if err != nil {
		return err
	}
	*e = RelayRequestEnvelope{RequestID: *wire.RequestID, ProtocolVersion: *wire.ProtocolVersion, Request: request}
	return nil
}

// rewriteProtocol33MailboxCommands rewrites a serialized request's mailbox
// commands into the protocol-33 legacy event representation, keeping every
// other byte and key order as written.
func rewriteProtocol33MailboxCommands(request []byte) ([]byte, error) {
	method, err := relayObjectPath(request, "method")
	if err != nil {
		return nil, err
	}
	var commandPath []string
	switch relayJSONString(method) {
	case "submit":
		commandPath = []string{"params", "command"}
	case "reviewer":
		action, err := relayObjectPath(request, "params", "request", "action")
		if err != nil {
			return nil, err
		}
		if relayJSONString(action) == "submit" {
			commandPath = []string{"params", "request", "params", "command"}
		}
	}
	if commandPath == nil {
		return request, nil
	}
	return relayRewritePath(request, commandPath, func(command json.RawMessage) (json.RawMessage, error) {
		commandType, err := relayObjectPath(command, "type")
		if err != nil {
			return nil, err
		}
		switch relayJSONString(commandType) {
		case "deliver_mailbox_event":
			return relayRewritePath(command, []string{"data", "event"}, rewriteProtocol33Event)
		case "mailbox_wake":
			return relayRewritePath(command, []string{"data", "events"}, func(events json.RawMessage) (json.RawMessage, error) {
				var items []json.RawMessage
				if err := json.Unmarshal(events, &items); err != nil || items == nil {
					return events, nil
				}
				for i, event := range items {
					rewritten, err := rewriteProtocol33Event(event)
					if err != nil {
						return nil, err
					}
					items[i] = rewritten
				}
				return relayMarshalJSON(items)
			})
		}
		return command, nil
	})
}

// relayJSONString is the string a raw JSON value holds; empty when it holds
// another kind of value or nothing.
func relayJSONString(value json.RawMessage) string {
	var text string
	if json.Unmarshal(value, &text) != nil {
		return ""
	}
	return text
}

// relayObjectPath is the raw value at path through nested objects; nil when a
// step is absent or not an object.
func relayObjectPath(value json.RawMessage, path ...string) (json.RawMessage, error) {
	for _, key := range path {
		members, err := relayOrderedObject(value)
		if err != nil || members == nil {
			return nil, err
		}
		value = nil
		for _, member := range members {
			if member.key == key {
				value = member.value
			}
		}
		if value == nil {
			return nil, nil
		}
	}
	return value, nil
}

type relayObjectMember struct {
	key   string
	value json.RawMessage
}

// relayOrderedObject reads an object's members in written order, as
// serde_json's order-preserving Value does; nil without error when value is
// not an object.
func relayOrderedObject(value json.RawMessage) ([]relayObjectMember, error) {
	trimmed := bytes.TrimSpace(value)
	if len(trimmed) == 0 || trimmed[0] != '{' {
		return nil, nil
	}
	decoder := json.NewDecoder(bytes.NewReader(trimmed))
	if _, err := decoder.Token(); err != nil {
		return nil, err
	}
	members := []relayObjectMember{}
	for decoder.More() {
		token, err := decoder.Token()
		if err != nil {
			return nil, err
		}
		key, _ := token.(string)
		var member json.RawMessage
		if err := decoder.Decode(&member); err != nil {
			return nil, err
		}
		members = append(members, relayObjectMember{key: key, value: member})
	}
	return members, nil
}

// relayRewritePath replaces the value at path through nested objects with
// rewrite's result, keeping every other member and its order. A missing step
// leaves value unchanged.
func relayRewritePath(value json.RawMessage, path []string, rewrite func(json.RawMessage) (json.RawMessage, error)) (json.RawMessage, error) {
	if len(path) == 0 {
		return rewrite(value)
	}
	members, err := relayOrderedObject(value)
	if err != nil || members == nil {
		return value, err
	}
	for i, member := range members {
		if member.key != path[0] {
			continue
		}
		rewritten, err := relayRewritePath(member.value, path[1:], rewrite)
		if err != nil {
			return nil, err
		}
		members[i].value = rewritten
		var b bytes.Buffer
		b.WriteByte('{')
		for j, member := range members {
			if j > 0 {
				b.WriteByte(',')
			}
			relayWriteKey(&b, member.key)
			b.Write(member.value)
		}
		b.WriteByte('}')
		return b.Bytes(), nil
	}
	return value, nil
}

func rewriteProtocol33Event(value json.RawMessage) (json.RawMessage, error) {
	var event MailboxEvent
	if err := json.Unmarshal(value, &event); err != nil {
		return nil, err
	}
	return relayMarshalJSON(event.legacyRepresentation())
}

// RelayChangeWaitBound is how long a worker holds RelayRequestWaitForChange
// when nothing changes. It matches the controller's 10 s worker CPU reading,
// so a quiet session's waiting connection is proven alive as often as its
// main one.
const RelayChangeWaitBound = 10 * time.Second

// RelayResponseEnvelope is serialized with Body flattened into it.
type RelayResponseEnvelope struct {
	RequestID       string            `json:"request_id"`
	ProtocolVersion uint32            `json:"protocol_version"`
	Body            RelayResponseBody `json:"-"`
}

// MarshalJSON writes request_id and protocol_version, then the body's
// result tag and its payload or error.
func (e RelayResponseEnvelope) MarshalJSON() ([]byte, error) {
	head, err := relayMarshalJSON(struct {
		RequestID       string `json:"request_id"`
		ProtocolVersion uint32 `json:"protocol_version"`
	}{e.RequestID, e.ProtocolVersion})
	if err != nil {
		return nil, err
	}
	body, err := relayMarshalJSON(e.Body)
	if err != nil {
		return nil, err
	}
	body = bytes.TrimSpace(body)
	if len(body) < 2 || body[0] != '{' {
		return nil, errors.New("relay response body is not an object")
	}
	out := append(head[:len(head)-1:len(head)-1], ',')
	return append(out, body[1:]...), nil
}

// UnmarshalJSON decodes the flattened wire form directly. Payload can only
// belong to Ok and error only to Error, so each decodes straight into its
// type in whatever order the fields arrive; result must agree. Unknown fields
// are ignored.
func (e *RelayResponseEnvelope) UnmarshalJSON(data []byte) error {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return err
	}
	payload, hasPayload := fields["payload"]
	failure, hasError := fields["error"]
	if hasPayload && hasError {
		return errors.New("relay response has more than one payload or error")
	}
	var body RelayResponseBody
	decoded := ""
	switch {
	case hasPayload:
		value, err := UnmarshalRelayResponsePayloadJSON(payload)
		if err != nil {
			return err
		}
		body, decoded = RelayResponseBodyOk{Payload: value}, "ok"
	case hasError:
		var value RelayProtocolError
		if err := json.Unmarshal(failure, &value); err != nil {
			return err
		}
		body, decoded = RelayResponseBodyError{Error: value}, "error"
	default:
		return errors.New("missing field `payload`")
	}
	rawResult, ok := fields["result"]
	if !ok {
		return errors.New("missing field `result`")
	}
	var result string
	if err := json.Unmarshal(rawResult, &result); err != nil || (result != "ok" && result != "error") {
		return fmt.Errorf("unknown relay response result %s", rawResult)
	}
	if result != decoded {
		return errors.New("relay response result does not match its body")
	}
	var envelope struct {
		RequestID       *string `json:"request_id"`
		ProtocolVersion *uint32 `json:"protocol_version"`
	}
	if err := json.Unmarshal(data, &envelope); err != nil {
		return err
	}
	if envelope.RequestID == nil {
		return errors.New("missing field `request_id`")
	}
	if envelope.ProtocolVersion == nil {
		return errors.New("missing field `protocol_version`")
	}
	*e = RelayResponseEnvelope{RequestID: *envelope.RequestID, ProtocolVersion: *envelope.ProtocolVersion, Body: body}
	return nil
}

type MailboxHookContext struct {
	Key  string `json:"key"`
	Text string `json:"text"`
}

type RelayProtocolError struct {
	Code      RelayErrorCode   `json:"code"`
	Message   string           `json:"message"`
	Retryable bool             `json:"retryable"`
	Detail    RelayErrorDetail `json:"detail,omitempty"`
}

// UnmarshalJSON decodes Detail through its variant table.
func (e *RelayProtocolError) UnmarshalJSON(data []byte) error {
	type plain RelayProtocolError
	aux := struct {
		*plain
		Detail json.RawMessage `json:"detail"`
	}{plain: (*plain)(e)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	e.Detail = nil
	if len(aux.Detail) > 0 && string(aux.Detail) != "null" {
		detail, err := UnmarshalRelayErrorDetailJSON(aux.Detail)
		if err != nil {
			return err
		}
		e.Detail = detail
	}
	return nil
}

type RelayErrorCode string

const (
	RelayErrorCodeIncompatibleProtocol RelayErrorCode = "incompatible_protocol"
	RelayErrorCodeInvalidRequest       RelayErrorCode = "invalid_request"
	RelayErrorCodeInvalidState         RelayErrorCode = "invalid_state"
	RelayErrorCodeDesynchronized       RelayErrorCode = "desynchronized"
	RelayErrorCodeInternal             RelayErrorCode = "internal"
)

// NewRelayProtocolError is Rust's relay_protocol_error, renamed because the
// Go name would collide with the RelayProtocolError type.
func NewRelayProtocolError(code RelayErrorCode, message string, retryable bool, detail RelayErrorDetail) RelayProtocolError {
	return RelayProtocolError{Code: code, Message: message, Retryable: retryable, Detail: detail}
}

func RelayError(code RelayErrorCode, message string, retryable bool, detail RelayErrorDetail) RelayResponseBody {
	return RelayResponseBodyError{Error: NewRelayProtocolError(code, message, retryable, detail)}
}

func UnsupportedRelayMethodResponse(requestID string, protocolVersion uint32, method string) RelayResponseEnvelope {
	return RelayResponseEnvelope{
		RequestID:       requestID,
		ProtocolVersion: protocolVersion,
		Body:            RelayError(RelayErrorCodeInvalidRequest, fmt.Sprintf("relay does not support method %s", rustDebugString(method)), false, nil),
	}
}

func InvalidRelayRequestResponse(requestID string, protocolVersion uint32, message string) RelayResponseEnvelope {
	return RelayResponseEnvelope{
		RequestID:       requestID,
		ProtocolVersion: protocolVersion,
		Body:            RelayError(RelayErrorCodeInvalidRequest, message, false, nil),
	}
}

// RustDebugString renders text as Rust's {:?} does for a str.
func RustDebugString(text string) string { return rustDebugString(text) }

// rustDebugString renders text as Rust's {:?} does for a str.
func rustDebugString(text string) string {
	var b strings.Builder
	b.WriteByte('"')
	for i, r := range text {
		switch {
		case r == '\t':
			b.WriteString(`\t`)
		case r == '\r':
			b.WriteString(`\r`)
		case r == '\n':
			b.WriteString(`\n`)
		case r == '\\':
			b.WriteString(`\\`)
		case r == '"':
			b.WriteString(`\"`)
		case r == 0:
			b.WriteString(`\0`)
		case r == utf8.RuneError && !strings.HasPrefix(text[i:], "�"):
			// Invalid UTF-8 cannot occur in a Rust str; keep the byte visible.
			fmt.Fprintf(&b, `\u{%x}`, text[i])
		case !unicode.IsGraphic(r) || unicode.In(r, unicode.Mn, unicode.Me):
			// Rust escapes non-printable and grapheme-extending characters.
			fmt.Fprintf(&b, `\u{%x}`, r)
		default:
			b.WriteRune(r)
		}
	}
	b.WriteByte('"')
	return b.String()
}

// ReadRelayFrame reads one request frame; nil at a clean end of stream.
func ReadRelayFrame(reader *bufio.Reader) (*RelayRequestEnvelope, error) {
	var line []byte
	read, _, err := ReadBoundedLine(reader, &line, MaxFrameBytes)
	if err != nil {
		return nil, fmt.Errorf("read relay protocol frame: %w", err)
	}
	if read == 0 {
		return nil, nil
	}
	line = bytes.TrimSuffix(line, []byte("\r"))
	if len(line) == 0 {
		return nil, errors.New("empty relay protocol frame")
	}
	var envelope RelayRequestEnvelope
	if err := json.Unmarshal(line, &envelope); err != nil {
		return nil, fmt.Errorf("parse relay protocol request: %w", err)
	}
	return &envelope, nil
}

func WriteRelayFrame(writer io.Writer, response RelayResponseEnvelope) error {
	encoded, err := relayMarshalJSON(response)
	if err != nil {
		return err
	}
	if _, err := writer.Write(append(encoded, '\n')); err != nil {
		return err
	}
	if flusher, ok := writer.(interface{ Flush() error }); ok {
		return flusher.Flush()
	}
	return nil
}

// ReadBoundedLine reads one line into *line, without its newline, refusing to
// buffer more than maximumBytes. It returns the bytes consumed (newline
// included) and whether a newline ended the line.
func ReadBoundedLine(reader *bufio.Reader, line *[]byte, maximumBytes int) (int, bool, error) {
	*line = (*line)[:0]
	consumed := 0
	for {
		if _, err := reader.Peek(1); err != nil {
			if errors.Is(err, io.EOF) {
				return consumed, false, nil
			}
			return consumed, false, err
		}
		available, _ := reader.Peek(reader.Buffered())
		newline := bytes.IndexByte(available, '\n')
		content := len(available)
		if newline >= 0 {
			content = newline
		}
		if err := EnsureByteBudget(len(*line)+content, maximumBytes, "relay journal event"); err != nil {
			return consumed, false, err
		}
		*line = append(*line, available[:content]...)
		step := content
		if newline >= 0 {
			step++
		}
		if _, err := reader.Discard(step); err != nil {
			return consumed, false, err
		}
		consumed += step
		if newline >= 0 {
			return consumed, true, nil
		}
	}
}

func reviewBaseChangedDefault() bool {
	return true
}

// UnmarshalJSON defaults Changed to true when absent.
func (v *RelayResponsePayloadReviewBaseSet) UnmarshalJSON(data []byte) error {
	type plain RelayResponsePayloadReviewBaseSet
	decoded := plain{Changed: reviewBaseChangedDefault()}
	if err := json.Unmarshal(data, &decoded); err != nil {
		return err
	}
	*v = RelayResponsePayloadReviewBaseSet(decoded)
	return nil
}

// ProjectMemoryTransferAction is an action carried by the protocol-59
// connection-only project-memory transfer method. Chunk fields contain
// standard-base64 encoded serialized bytes.
type ProjectMemoryTransferAction interface {
	json.Marshaler
	isProjectMemoryTransferAction()
}

type ProjectMemoryTransferActionBeginSnapshot struct{}

func (ProjectMemoryTransferActionBeginSnapshot) isProjectMemoryTransferAction() {}

// MarshalJSON writes the serde-tagged form of ProjectMemoryTransferActionBeginSnapshot.
func (v ProjectMemoryTransferActionBeginSnapshot) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("action", "params", "begin_snapshot", nil)
}

type ProjectMemoryTransferActionReadSnapshotPage struct {
	TransferID uint64 `json:"transfer_id"`
	Offset     uint64 `json:"offset"`
}

func (ProjectMemoryTransferActionReadSnapshotPage) isProjectMemoryTransferAction() {}

// MarshalJSON writes the serde-tagged form of ProjectMemoryTransferActionReadSnapshotPage.
func (v ProjectMemoryTransferActionReadSnapshotPage) MarshalJSON() ([]byte, error) {
	type plain ProjectMemoryTransferActionReadSnapshotPage
	return relayAdjacentJSON("action", "params", "read_snapshot_page", plain(v))
}

type ProjectMemoryTransferActionBeginUpload struct {
	Kind       ProjectMemoryUploadKind `json:"kind"`
	TotalBytes uint64                  `json:"total_bytes"`
}

func (ProjectMemoryTransferActionBeginUpload) isProjectMemoryTransferAction() {}

// MarshalJSON writes the serde-tagged form of ProjectMemoryTransferActionBeginUpload.
func (v ProjectMemoryTransferActionBeginUpload) MarshalJSON() ([]byte, error) {
	type plain ProjectMemoryTransferActionBeginUpload
	return relayAdjacentJSON("action", "params", "begin_upload", plain(v))
}

// UnmarshalJSON decodes the interface-typed fields through their variant tables.
func (v *ProjectMemoryTransferActionBeginUpload) UnmarshalJSON(data []byte) error {
	type plain ProjectMemoryTransferActionBeginUpload
	aux := struct {
		*plain
		Kind json.RawMessage `json:"kind"`
	}{plain: (*plain)(v)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	if len(aux.Kind) > 0 && string(aux.Kind) != "null" {
		decoded, err := UnmarshalProjectMemoryUploadKindJSON(aux.Kind)
		if err != nil {
			return err
		}
		v.Kind = decoded
	}
	return nil
}

type ProjectMemoryTransferActionAppendUpload struct {
	TransferID uint64 `json:"transfer_id"`
	Offset     uint64 `json:"offset"`
	Chunk      string `json:"chunk"`
}

func (ProjectMemoryTransferActionAppendUpload) isProjectMemoryTransferAction() {}

// MarshalJSON writes the serde-tagged form of ProjectMemoryTransferActionAppendUpload.
func (v ProjectMemoryTransferActionAppendUpload) MarshalJSON() ([]byte, error) {
	type plain ProjectMemoryTransferActionAppendUpload
	return relayAdjacentJSON("action", "params", "append_upload", plain(v))
}

type ProjectMemoryTransferActionCommitUpload struct {
	TransferID uint64 `json:"transfer_id"`
}

func (ProjectMemoryTransferActionCommitUpload) isProjectMemoryTransferAction() {}

// MarshalJSON writes the serde-tagged form of ProjectMemoryTransferActionCommitUpload.
func (v ProjectMemoryTransferActionCommitUpload) MarshalJSON() ([]byte, error) {
	type plain ProjectMemoryTransferActionCommitUpload
	return relayAdjacentJSON("action", "params", "commit_upload", plain(v))
}

type ProjectMemoryTransferActionAbort struct {
	TransferID uint64 `json:"transfer_id"`
}

func (ProjectMemoryTransferActionAbort) isProjectMemoryTransferAction() {}

// MarshalJSON writes the serde-tagged form of ProjectMemoryTransferActionAbort.
func (v ProjectMemoryTransferActionAbort) MarshalJSON() ([]byte, error) {
	type plain ProjectMemoryTransferActionAbort
	return relayAdjacentJSON("action", "params", "abort", plain(v))
}

// projectMemoryTransferActionVariants decodes each ProjectMemoryTransferAction variant by its wire name.
var projectMemoryTransferActionVariants = map[string]func(json.RawMessage) (ProjectMemoryTransferAction, error){
	"begin_snapshot":     relayVariant[ProjectMemoryTransferActionBeginSnapshot, ProjectMemoryTransferAction](true),
	"read_snapshot_page": relayVariant[ProjectMemoryTransferActionReadSnapshotPage, ProjectMemoryTransferAction](true),
	"begin_upload":       relayVariant[ProjectMemoryTransferActionBeginUpload, ProjectMemoryTransferAction](true),
	"append_upload":      relayVariant[ProjectMemoryTransferActionAppendUpload, ProjectMemoryTransferAction](true),
	"commit_upload":      relayVariant[ProjectMemoryTransferActionCommitUpload, ProjectMemoryTransferAction](true),
	"abort":              relayVariant[ProjectMemoryTransferActionAbort, ProjectMemoryTransferAction](true),
}

// UnmarshalProjectMemoryTransferActionJSON decodes the serde-tagged form of ProjectMemoryTransferAction.
func UnmarshalProjectMemoryTransferActionJSON(data []byte) (ProjectMemoryTransferAction, error) {
	name, body, err := relaySplitAdjacent(data, "action", "params", true)
	if err != nil {
		return nil, fmt.Errorf("decode ProjectMemoryTransferAction: %w", err)
	}
	decode, ok := projectMemoryTransferActionVariants[name]
	if !ok {
		return nil, fmt.Errorf("decode ProjectMemoryTransferAction: unknown variant %q", name)
	}
	return decode(body)
}

// ProjectMemoryUploadKind is the semantics applied when a complete
// project-memory upload is committed.
type ProjectMemoryUploadKind interface {
	json.Marshaler
	isProjectMemoryUploadKind()
}

type ProjectMemoryUploadKindInstallSnapshot struct{}

func (ProjectMemoryUploadKindInstallSnapshot) isProjectMemoryUploadKind() {}

// MarshalJSON writes the serde-tagged form of ProjectMemoryUploadKindInstallSnapshot.
func (v ProjectMemoryUploadKindInstallSnapshot) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("kind", "params", "install_snapshot", nil)
}

type ProjectMemoryUploadKindReplaceTree struct {
	ExpectedReplica TreeVersion `json:"expected_replica"`
}

func (ProjectMemoryUploadKindReplaceTree) isProjectMemoryUploadKind() {}

// MarshalJSON writes the serde-tagged form of ProjectMemoryUploadKindReplaceTree.
func (v ProjectMemoryUploadKindReplaceTree) MarshalJSON() ([]byte, error) {
	type plain ProjectMemoryUploadKindReplaceTree
	return relayAdjacentJSON("kind", "params", "replace_tree", plain(v))
}

// projectMemoryUploadKindVariants decodes each ProjectMemoryUploadKind variant by its wire name.
var projectMemoryUploadKindVariants = map[string]func(json.RawMessage) (ProjectMemoryUploadKind, error){
	"install_snapshot": relayVariant[ProjectMemoryUploadKindInstallSnapshot, ProjectMemoryUploadKind](true),
	"replace_tree":     relayVariant[ProjectMemoryUploadKindReplaceTree, ProjectMemoryUploadKind](true),
}

// UnmarshalProjectMemoryUploadKindJSON decodes the serde-tagged form of ProjectMemoryUploadKind.
func UnmarshalProjectMemoryUploadKindJSON(data []byte) (ProjectMemoryUploadKind, error) {
	name, body, err := relaySplitAdjacent(data, "kind", "params", true)
	if err != nil {
		return nil, fmt.Errorf("decode ProjectMemoryUploadKind: %w", err)
	}
	decode, ok := projectMemoryUploadKindVariants[name]
	if !ok {
		return nil, fmt.Errorf("decode ProjectMemoryUploadKind: unknown variant %q", name)
	}
	return decode(body)
}

// ProjectMemoryTransferResult is a result returned by protocol-59
// project-memory transfer actions.
type ProjectMemoryTransferResult interface {
	json.Marshaler
	isProjectMemoryTransferResult()
}

type ProjectMemoryTransferResultSnapshotStarted struct {
	TransferID uint64 `json:"transfer_id"`
	TotalBytes uint64 `json:"total_bytes"`
}

func (ProjectMemoryTransferResultSnapshotStarted) isProjectMemoryTransferResult() {}

// MarshalJSON writes the serde-tagged form of ProjectMemoryTransferResultSnapshotStarted.
func (v ProjectMemoryTransferResultSnapshotStarted) MarshalJSON() ([]byte, error) {
	type plain ProjectMemoryTransferResultSnapshotStarted
	return relayAdjacentJSON("action", "data", "snapshot_started", plain(v))
}

type ProjectMemoryTransferResultSnapshotPage struct {
	TransferID uint64 `json:"transfer_id"`
	Offset     uint64 `json:"offset"`
	NextOffset uint64 `json:"next_offset"`
	Complete   bool   `json:"complete"`
	Chunk      string `json:"chunk"`
}

func (ProjectMemoryTransferResultSnapshotPage) isProjectMemoryTransferResult() {}

// MarshalJSON writes the serde-tagged form of ProjectMemoryTransferResultSnapshotPage.
func (v ProjectMemoryTransferResultSnapshotPage) MarshalJSON() ([]byte, error) {
	type plain ProjectMemoryTransferResultSnapshotPage
	return relayAdjacentJSON("action", "data", "snapshot_page", plain(v))
}

type ProjectMemoryTransferResultUploadStarted struct {
	TransferID uint64 `json:"transfer_id"`
}

func (ProjectMemoryTransferResultUploadStarted) isProjectMemoryTransferResult() {}

// MarshalJSON writes the serde-tagged form of ProjectMemoryTransferResultUploadStarted.
func (v ProjectMemoryTransferResultUploadStarted) MarshalJSON() ([]byte, error) {
	type plain ProjectMemoryTransferResultUploadStarted
	return relayAdjacentJSON("action", "data", "upload_started", plain(v))
}

type ProjectMemoryTransferResultUploadProgress struct {
	TransferID uint64 `json:"transfer_id"`
	NextOffset uint64 `json:"next_offset"`
}

func (ProjectMemoryTransferResultUploadProgress) isProjectMemoryTransferResult() {}

// MarshalJSON writes the serde-tagged form of ProjectMemoryTransferResultUploadProgress.
func (v ProjectMemoryTransferResultUploadProgress) MarshalJSON() ([]byte, error) {
	type plain ProjectMemoryTransferResultUploadProgress
	return relayAdjacentJSON("action", "data", "upload_progress", plain(v))
}

type ProjectMemoryTransferResultInstalled struct{}

func (ProjectMemoryTransferResultInstalled) isProjectMemoryTransferResult() {}

// MarshalJSON writes the serde-tagged form of ProjectMemoryTransferResultInstalled.
func (v ProjectMemoryTransferResultInstalled) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("action", "data", "installed", nil)
}

type ProjectMemoryTransferResultReplaced struct {
	Outcome ReplicaReplaceOutcome `json:"outcome"`
}

func (ProjectMemoryTransferResultReplaced) isProjectMemoryTransferResult() {}

// MarshalJSON writes the serde-tagged form of ProjectMemoryTransferResultReplaced.
func (v ProjectMemoryTransferResultReplaced) MarshalJSON() ([]byte, error) {
	type plain ProjectMemoryTransferResultReplaced
	return relayAdjacentJSON("action", "data", "replaced", plain(v))
}

type ProjectMemoryTransferResultAborted struct {
	TransferID uint64 `json:"transfer_id"`
}

func (ProjectMemoryTransferResultAborted) isProjectMemoryTransferResult() {}

// MarshalJSON writes the serde-tagged form of ProjectMemoryTransferResultAborted.
func (v ProjectMemoryTransferResultAborted) MarshalJSON() ([]byte, error) {
	type plain ProjectMemoryTransferResultAborted
	return relayAdjacentJSON("action", "data", "aborted", plain(v))
}

// projectMemoryTransferResultVariants decodes each ProjectMemoryTransferResult variant by its wire name.
var projectMemoryTransferResultVariants = map[string]func(json.RawMessage) (ProjectMemoryTransferResult, error){
	"snapshot_started": relayVariant[ProjectMemoryTransferResultSnapshotStarted, ProjectMemoryTransferResult](true),
	"snapshot_page":    relayVariant[ProjectMemoryTransferResultSnapshotPage, ProjectMemoryTransferResult](true),
	"upload_started":   relayVariant[ProjectMemoryTransferResultUploadStarted, ProjectMemoryTransferResult](true),
	"upload_progress":  relayVariant[ProjectMemoryTransferResultUploadProgress, ProjectMemoryTransferResult](true),
	"installed":        relayVariant[ProjectMemoryTransferResultInstalled, ProjectMemoryTransferResult](true),
	"replaced":         relayVariant[ProjectMemoryTransferResultReplaced, ProjectMemoryTransferResult](true),
	"aborted":          relayVariant[ProjectMemoryTransferResultAborted, ProjectMemoryTransferResult](true),
}

// UnmarshalProjectMemoryTransferResultJSON decodes the serde-tagged form of ProjectMemoryTransferResult.
func UnmarshalProjectMemoryTransferResultJSON(data []byte) (ProjectMemoryTransferResult, error) {
	name, body, err := relaySplitAdjacent(data, "action", "data", true)
	if err != nil {
		return nil, fmt.Errorf("decode ProjectMemoryTransferResult: %w", err)
	}
	decode, ok := projectMemoryTransferResultVariants[name]
	if !ok {
		return nil, fmt.Errorf("decode ProjectMemoryTransferResult: unknown variant %q", name)
	}
	return decode(body)
}

// RelayRequest is a request on the new controller-to-relay boundary. ACP
// payloads remain ACP payloads; only durability and queue-control operations
// are Hel-specific.
type RelayRequest interface {
	json.Marshaler
	isRelayRequest()
}

type RelayRequestHello struct {
	ControllerVersion string            `json:"controller_version"`
	Supported         RelayVersionRange `json:"supported"`
}

func (RelayRequestHello) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestHello.
func (v RelayRequestHello) MarshalJSON() ([]byte, error) {
	type plain RelayRequestHello
	return relayAdjacentJSON("method", "params", "hello", plain(v))
}

type RelayRequestAttach struct {
	AfterOrdinal uint64 `json:"after_ordinal"`
	AfterDigest  string `json:"after_digest"`
	// The digest of the operational state the controller holds, from
	// RelayResponsePayloadAttached. Protocol 57.
	KnownStateDigest *string `json:"known_state_digest,omitempty"`
}

func (RelayRequestAttach) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestAttach.
func (v RelayRequestAttach) MarshalJSON() ([]byte, error) {
	type plain RelayRequestAttach
	return relayAdjacentJSON("method", "params", "attach", plain(v))
}

type RelayRequestAcknowledge struct {
	ThroughOrdinal uint64 `json:"through_ordinal"`
	ThroughDigest  string `json:"through_digest"`
}

func (RelayRequestAcknowledge) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestAcknowledge.
func (v RelayRequestAcknowledge) MarshalJSON() ([]byte, error) {
	type plain RelayRequestAcknowledge
	return relayAdjacentJSON("method", "params", "acknowledge", plain(v))
}

type RelayRequestSubmit struct {
	CommandID string       `json:"command_id"`
	Command   RelayCommand `json:"command"`
}

func (RelayRequestSubmit) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestSubmit.
func (v RelayRequestSubmit) MarshalJSON() ([]byte, error) {
	type plain RelayRequestSubmit
	return relayAdjacentJSON("method", "params", "submit", plain(v))
}

// UnmarshalJSON decodes the interface-typed fields through their variant tables.
func (v *RelayRequestSubmit) UnmarshalJSON(data []byte) error {
	type plain RelayRequestSubmit
	aux := struct {
		*plain
		Command json.RawMessage `json:"command"`
	}{plain: (*plain)(v)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	if len(aux.Command) > 0 && string(aux.Command) != "null" {
		decoded, err := UnmarshalRelayCommandJSON(aux.Command)
		if err != nil {
			return err
		}
		v.Command = decoded
	}
	return nil
}

type RelayRequestCommandRetentionStatus struct{}

func (RelayRequestCommandRetentionStatus) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestCommandRetentionStatus.
func (v RelayRequestCommandRetentionStatus) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("method", "params", "command_retention_status", nil)
}

// Freeze a coherent current-state recovery cut on this connection.
type RelayRequestBeginLiveBaseline struct{}

func (RelayRequestBeginLiveBaseline) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestBeginLiveBaseline.
func (v RelayRequestBeginLiveBaseline) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("method", "params", "begin_live_baseline", nil)
}

type RelayRequestReadLiveBaselinePage struct {
	Offset uint64 `json:"offset"`
}

func (RelayRequestReadLiveBaselinePage) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestReadLiveBaselinePage.
func (v RelayRequestReadLiveBaselinePage) MarshalJSON() ([]byte, error) {
	type plain RelayRequestReadLiveBaselinePage
	return relayAdjacentJSON("method", "params", "read_live_baseline_page", plain(v))
}

type RelayRequestAbortLiveBaseline struct{}

func (RelayRequestAbortLiveBaseline) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestAbortLiveBaseline.
func (v RelayRequestAbortLiveBaseline) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("method", "params", "abort_live_baseline", nil)
}

type RelayRequestReadQueuedCommand struct {
	CommandID string `json:"command_id"`
}

func (RelayRequestReadQueuedCommand) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestReadQueuedCommand.
func (v RelayRequestReadQueuedCommand) MarshalJSON() ([]byte, error) {
	type plain RelayRequestReadQueuedCommand
	return relayAdjacentJSON("method", "params", "read_queued_command", plain(v))
}

// RelayRequestReadCheckpointMailbox asks for the mail the relay holds at the
// cut of a ready checkpoint barrier. Refused unless that barrier is ready at
// exactly Cut and nothing has been journaled since, so the answer describes
// the archived cut. Protocol 75.
type RelayRequestReadCheckpointMailbox struct {
	BarrierCommandID string      `json:"barrier_command_id"`
	Cut              RelayCursor `json:"cut"`
}

func (RelayRequestReadCheckpointMailbox) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestReadCheckpointMailbox.
func (v RelayRequestReadCheckpointMailbox) MarshalJSON() ([]byte, error) {
	type plain RelayRequestReadCheckpointMailbox
	return relayAdjacentJSON("method", "params", "read_checkpoint_mailbox", plain(v))
}

type RelayRequestReadPendingElicitation struct {
	ElicitationID string `json:"elicitation_id"`
}

func (RelayRequestReadPendingElicitation) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestReadPendingElicitation.
func (v RelayRequestReadPendingElicitation) MarshalJSON() ([]byte, error) {
	type plain RelayRequestReadPendingElicitation
	return relayAdjacentJSON("method", "params", "read_pending_elicitation", plain(v))
}

// Register operation-owned retry receipts before submitting any of its commands.
type RelayRequestUpdateCommandRetention struct {
	OperationID *string `json:"operation_id"`
	// Sorted and unique (a Rust BTreeSet).
	CommandIds []string `json:"command_ids"`
	// Consume upgrade protection once; a late reconciliation cannot undo newer pins.
	ReconcileLegacy bool `json:"reconcile_legacy"`
}

func (RelayRequestUpdateCommandRetention) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestUpdateCommandRetention.
func (v RelayRequestUpdateCommandRetention) MarshalJSON() ([]byte, error) {
	type plain RelayRequestUpdateCommandRetention
	return relayAdjacentJSON("method", "params", "update_command_retention", plain(v))
}

type RelayRequestStatus struct{}

func (RelayRequestStatus) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestStatus.
func (v RelayRequestStatus) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("method", "params", "status", nil)
}

// Atomically lease pending mailbox events to a harness tool hook.
type RelayRequestDrainMailbox struct {
	HookEvent string `json:"hook_event"`
}

func (RelayRequestDrainMailbox) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestDrainMailbox.
func (v RelayRequestDrainMailbox) MarshalJSON() ([]byte, error) {
	type plain RelayRequestDrainMailbox
	return relayAdjacentJSON("method", "params", "drain_mailbox", plain(v))
}

// Confirm that a hook wrote and flushed the leased events to its output.
type RelayRequestAckMailbox struct {
	LeaseID string `json:"lease_id"`
}

func (RelayRequestAckMailbox) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestAckMailbox.
func (v RelayRequestAckMailbox) MarshalJSON() ([]byte, error) {
	type plain RelayRequestAckMailbox
	return relayAdjacentJSON("method", "params", "ack_mailbox", plain(v))
}

// Atomically admit a checkpoint barrier only when no work would be lost.
// Uses the existing barrier journal and connection-disconnect cleanup.
type RelayRequestReserveIdle struct {
	CommandID string `json:"command_id"`
}

func (RelayRequestReserveIdle) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestReserveIdle.
func (v RelayRequestReserveIdle) MarshalJSON() ([]byte, error) {
	type plain RelayRequestReserveIdle
	return relayAdjacentJSON("method", "params", "reserve_idle", plain(v))
}

type RelayRequestAttachmentPresent struct {
	Reference AttachmentRef `json:"reference"`
}

func (RelayRequestAttachmentPresent) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestAttachmentPresent.
func (v RelayRequestAttachmentPresent) MarshalJSON() ([]byte, error) {
	type plain RelayRequestAttachmentPresent
	return relayAdjacentJSON("method", "params", "attachment_present", plain(v))
}

type RelayRequestInstallAttachment struct {
	Reference AttachmentRef `json:"reference"`
	Data      string        `json:"data"`
}

func (RelayRequestInstallAttachment) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestInstallAttachment.
func (v RelayRequestInstallAttachment) MarshalJSON() ([]byte, error) {
	type plain RelayRequestInstallAttachment
	return relayAdjacentJSON("method", "params", "install_attachment", plain(v))
}

type RelayRequestReadAttachment struct {
	Reference AttachmentRef `json:"reference"`
}

func (RelayRequestReadAttachment) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestReadAttachment.
func (v RelayRequestReadAttachment) MarshalJSON() ([]byte, error) {
	type plain RelayRequestReadAttachment
	return relayAdjacentJSON("method", "params", "read_attachment", plain(v))
}

// Add hidden background context attached to the next real prompt.
// This mutates only the relay-private snapshot and is never projected as
// conversation history.
type RelayRequestInstallPromptContext struct {
	Text string `json:"text"`
}

func (RelayRequestInstallPromptContext) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestInstallPromptContext.
func (v RelayRequestInstallPromptContext) MarshalJSON() ([]byte, error) {
	type plain RelayRequestInstallPromptContext
	return relayAdjacentJSON("method", "params", "install_prompt_context", plain(v))
}

// Read the session-private memory replica and the baseline it was seeded
// from. Connection-only: memory content never enters the relay journal.
type RelayRequestProjectMemorySnapshot struct{}

func (RelayRequestProjectMemorySnapshot) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestProjectMemorySnapshot.
func (v RelayRequestProjectMemorySnapshot) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("method", "params", "project_memory_snapshot", nil)
}

// Install a controller-reconciled tree into both the replica and its
// baseline for the next three-way synchronization.
type RelayRequestInstallProjectMemorySnapshot struct {
	Snapshot ProjectMemoryWireTree `json:"snapshot"`
}

func (RelayRequestInstallProjectMemorySnapshot) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestInstallProjectMemorySnapshot.
func (v RelayRequestInstallProjectMemorySnapshot) MarshalJSON() ([]byte, error) {
	type plain RelayRequestInstallProjectMemorySnapshot
	return relayAdjacentJSON("method", "params", "install_project_memory_snapshot", plain(v))
}

// UnmarshalJSON decodes the interface-typed fields through their variant tables.
func (v *RelayRequestInstallProjectMemorySnapshot) UnmarshalJSON(data []byte) error {
	type plain RelayRequestInstallProjectMemorySnapshot
	aux := struct {
		*plain
		Snapshot json.RawMessage `json:"snapshot"`
	}{plain: (*plain)(v)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	if len(aux.Snapshot) > 0 && string(aux.Snapshot) != "null" {
		decoded, err := UnmarshalProjectMemoryWireTreeJSON(aux.Snapshot)
		if err != nil {
			return err
		}
		v.Snapshot = decoded
	}
	return nil
}

// Replace the complete session replica and baseline only if the replica
// still matches the tree the controller read.
type RelayRequestReplaceProjectMemoryTree struct {
	ExpectedReplica TreeVersion           `json:"expected_replica"`
	Tree            ProjectMemoryWireTree `json:"tree"`
}

func (RelayRequestReplaceProjectMemoryTree) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestReplaceProjectMemoryTree.
func (v RelayRequestReplaceProjectMemoryTree) MarshalJSON() ([]byte, error) {
	type plain RelayRequestReplaceProjectMemoryTree
	return relayAdjacentJSON("method", "params", "replace_project_memory_tree", plain(v))
}

// UnmarshalJSON decodes the interface-typed fields through their variant tables.
func (v *RelayRequestReplaceProjectMemoryTree) UnmarshalJSON(data []byte) error {
	type plain RelayRequestReplaceProjectMemoryTree
	aux := struct {
		*plain
		Tree json.RawMessage `json:"tree"`
	}{plain: (*plain)(v)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	if len(aux.Tree) > 0 && string(aux.Tree) != "null" {
		decoded, err := UnmarshalProjectMemoryWireTreeJSON(aux.Tree)
		if err != nil {
			return err
		}
		v.Tree = decoded
	}
	return nil
}

// Transfer a frozen project-memory snapshot or a compare-and-replace tree
// over bounded connection-only pages. Protocol 59.
type RelayRequestProjectMemoryTransfer struct {
	Action ProjectMemoryTransferAction `json:"action"`
}

func (RelayRequestProjectMemoryTransfer) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestProjectMemoryTransfer.
func (v RelayRequestProjectMemoryTransfer) MarshalJSON() ([]byte, error) {
	type plain RelayRequestProjectMemoryTransfer
	return relayAdjacentJSON("method", "params", "project_memory_transfer", plain(v))
}

// UnmarshalJSON decodes the interface-typed fields through their variant tables.
func (v *RelayRequestProjectMemoryTransfer) UnmarshalJSON(data []byte) error {
	type plain RelayRequestProjectMemoryTransfer
	aux := struct {
		*plain
		Action json.RawMessage `json:"action"`
	}{plain: (*plain)(v)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	if len(aux.Action) > 0 && string(aux.Action) != "null" {
		decoded, err := UnmarshalProjectMemoryTransferActionJSON(aux.Action)
		if err != nil {
			return err
		}
		v.Action = decoded
	}
	return nil
}

// Connection-only CPU measurement; never journaled.
type RelayRequestCpuUsage struct{}

func (RelayRequestCpuUsage) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestCpuUsage.
func (v RelayRequestCpuUsage) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("method", "params", "cpu_usage", nil)
}

// Report non-secret metadata for this session's harness credentials.
// The runtime handles credential requests on the connection and never
// passes them through the durable relay.
type RelayRequestCredentialState struct{}

func (RelayRequestCredentialState) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestCredentialState.
func (v RelayRequestCredentialState) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("method", "params", "credential_state", nil)
}

// Read this session's harness credential file as base64. The payload is
// connection-only and must never enter relay state or observations.
type RelayRequestReadCredentials struct{}

func (RelayRequestReadCredentials) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestReadCredentials.
func (v RelayRequestReadCredentials) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("method", "params", "read_credentials", nil)
}

// Install a base64-encoded credential file into this session's harness
// home. The destination path is fixed by the worker launch config.
type RelayRequestInstallCredentials struct {
	Data string `json:"data"`
}

func (RelayRequestInstallCredentials) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestInstallCredentials.
func (v RelayRequestInstallCredentials) MarshalJSON() ([]byte, error) {
	type plain RelayRequestInstallCredentials
	return relayAdjacentJSON("method", "params", "install_credentials", plain(v))
}

// Report non-secret metadata for this session's synced skills trees.
// Handled on the connection like credential requests; the durable relay
// never sees them.
type RelayRequestSkillsState struct{}

func (RelayRequestSkillsState) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestSkillsState.
func (v RelayRequestSkillsState) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("method", "params", "skills_state", nil)
}

// Replace this session's synced skills trees with a base64-encoded
// skills archive. The destination directories are fixed by the
// worker launch config and the harness skills whitelist.
type RelayRequestInstallSkills struct {
	Data string `json:"data"`
}

func (RelayRequestInstallSkills) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestInstallSkills.
func (v RelayRequestInstallSkills) MarshalJSON() ([]byte, error) {
	type plain RelayRequestInstallSkills
	return relayAdjacentJSON("method", "params", "install_skills", plain(v))
}

// Report whether this worker has a synchronized GitHub CLI token and its
// non-secret fingerprint. This request is connection-only.
type RelayRequestGithubTokenState struct{}

func (RelayRequestGithubTokenState) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestGithubTokenState.
func (v RelayRequestGithubTokenState) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("method", "params", "github_token_state", nil)
}

// Install the controller's current GitHub CLI token into worker-private
// runtime storage. The token never enters durable relay state.
type RelayRequestInstallGithubToken struct {
	Data string `json:"data"`
}

func (RelayRequestInstallGithubToken) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestInstallGithubToken.
func (v RelayRequestInstallGithubToken) MarshalJSON() ([]byte, error) {
	type plain RelayRequestInstallGithubToken
	return relayAdjacentJSON("method", "params", "install_github_token", plain(v))
}

// Remove the worker's synchronized GitHub CLI token.
type RelayRequestRemoveGithubToken struct{}

func (RelayRequestRemoveGithubToken) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestRemoveGithubToken.
func (v RelayRequestRemoveGithubToken) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("method", "params", "remove_github_token", nil)
}

// AWS credentials stay on the control connection, outside the journal.
type RelayRequestAwsCredentialsState struct{}

func (RelayRequestAwsCredentialsState) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestAwsCredentialsState.
func (v RelayRequestAwsCredentialsState) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("method", "params", "aws_credentials_state", nil)
}

type RelayRequestInstallAwsCredentials struct {
	Data string `json:"data"`
	// Which role and session Mjolnir issued these credentials for, kept
	// beside the secret. Sent only to workers at protocol 55 or later.
	Scope *AwsCredentialScope `json:"scope,omitempty"`
}

func (RelayRequestInstallAwsCredentials) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestInstallAwsCredentials.
func (v RelayRequestInstallAwsCredentials) MarshalJSON() ([]byte, error) {
	type plain RelayRequestInstallAwsCredentials
	return relayAdjacentJSON("method", "params", "install_aws_credentials", plain(v))
}

// Delete the session's AWS credentials.
type RelayRequestRemoveAwsCredentials struct{}

func (RelayRequestRemoveAwsCredentials) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestRemoveAwsCredentials.
func (v RelayRequestRemoveAwsCredentials) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("method", "params", "remove_aws_credentials", nil)
}

// The scope recorded beside the session's AWS credentials, if any, and
// the AWS profile the session was staged with.
type RelayRequestAwsCredentialsScope struct{}

func (RelayRequestAwsCredentialsScope) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestAwsCredentialsScope.
func (v RelayRequestAwsCredentialsScope) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("method", "params", "aws_credentials_scope", nil)
}

// The host's slopcop login, like the GitHub token, stays on the control
// connection, outside the journal.
type RelayRequestSlopcopLoginState struct{}

func (RelayRequestSlopcopLoginState) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestSlopcopLoginState.
func (v RelayRequestSlopcopLoginState) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("method", "params", "slopcop_login_state", nil)
}

type RelayRequestInstallSlopcopLogin struct {
	Data string `json:"data"`
}

func (RelayRequestInstallSlopcopLogin) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestInstallSlopcopLogin.
func (v RelayRequestInstallSlopcopLogin) MarshalJSON() ([]byte, error) {
	type plain RelayRequestInstallSlopcopLogin
	return relayAdjacentJSON("method", "params", "install_slopcop_login", plain(v))
}

type RelayRequestRemoveSlopcopLogin struct{}

func (RelayRequestRemoveSlopcopLogin) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestRemoveSlopcopLogin.
func (v RelayRequestRemoveSlopcopLogin) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("method", "params", "remove_slopcop_login", nil)
}

// Resolve one in-flight form. Only its rendered, masked reply is journaled.
type RelayRequestRespondElicitation struct {
	ElicitationID string              `json:"elicitation_id"`
	Response      ElicitationResponse `json:"response"`
}

func (RelayRequestRespondElicitation) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestRespondElicitation.
func (v RelayRequestRespondElicitation) MarshalJSON() ([]byte, error) {
	type plain RelayRequestRespondElicitation
	return relayAdjacentJSON("method", "params", "respond_elicitation", plain(v))
}

// Stop one task from the current process-local background-work level.
// The opaque id must come from RelayOperationalState.BackgroundCommands.
type RelayRequestStopBackgroundTask struct {
	BackgroundTaskID string `json:"background_task_id"`
}

func (RelayRequestStopBackgroundTask) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestStopBackgroundTask.
func (v RelayRequestStopBackgroundTask) MarshalJSON() ([]byte, error) {
	type plain RelayRequestStopBackgroundTask
	return relayAdjacentJSON("method", "params", "stop_background_task", plain(v))
}

// Fetch controller work queued by the parent session's private MCP
// socket. Connection-only: request payloads do not enter chat history.
type RelayRequestSubagentRequests struct{}

func (RelayRequestSubagentRequests) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestSubagentRequests.
func (v RelayRequestSubagentRequests) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("method", "params", "subagent_requests", nil)
}

// Acknowledge the previous page and fetch bounded changes on this connection.
type RelayRequestPollSubagentQueue struct {
	Acknowledged *uint64 `json:"acknowledged"`
}

func (RelayRequestPollSubagentQueue) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestPollSubagentQueue.
func (v RelayRequestPollSubagentQueue) MarshalJSON() ([]byte, error) {
	type plain RelayRequestPollSubagentQueue
	return relayAdjacentJSON("method", "params", "poll_subagent_queue", plain(v))
}

// Answer once the worker's latest event ordinal or operational state
// digest differs from these, or this connection's sub-agent queue has
// changes it has not polled, or after RelayChangeWaitBound with
// none of them. Connection-only. Protocol 58.
type RelayRequestWaitForChange struct {
	AfterOrdinal uint64  `json:"after_ordinal"`
	StateDigest  *string `json:"state_digest"`
}

func (RelayRequestWaitForChange) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestWaitForChange.
func (v RelayRequestWaitForChange) MarshalJSON() ([]byte, error) {
	type plain RelayRequestWaitForChange
	return relayAdjacentJSON("method", "params", "wait_for_change", plain(v))
}

// Connection-only operator-away control; never journaled as a command.
type RelayRequestSetOperatorAway struct {
	Away     bool   `json:"away"`
	Revision uint64 `json:"revision"`
}

func (RelayRequestSetOperatorAway) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestSetOperatorAway.
func (v RelayRequestSetOperatorAway) MarshalJSON() ([]byte, error) {
	type plain RelayRequestSetOperatorAway
	return relayAdjacentJSON("method", "params", "set_operator_away", plain(v))
}

// Open or close worker-owned admission for requests that change child
// state. Closing is serialized with the private MCP queue's enqueue lock.
type RelayRequestSetSubagentAdmission struct {
	Open bool `json:"open"`
}

func (RelayRequestSetSubagentAdmission) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestSetSubagentAdmission.
func (v RelayRequestSetSubagentAdmission) MarshalJSON() ([]byte, error) {
	type plain RelayRequestSetSubagentAdmission
	return relayAdjacentJSON("method", "params", "set_subagent_admission", plain(v))
}

// Connection-only history queries; never part of the durable transcript.
type RelayRequestHistoryQuery struct {
	Query HistoryQuery `json:"query"`
}

func (RelayRequestHistoryQuery) isRelayRequest() {}

// UnmarshalJSON reads the variant's fields; Query is a sealed interface
// decoded by DecodeHistoryQuery.
func (v *RelayRequestHistoryQuery) UnmarshalJSON(data []byte) error {
	var wire struct {
		Query json.RawMessage `json:"query"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	if wire.Query == nil {
		return fmt.Errorf("missing field `query`")
	}
	query, err := DecodeHistoryQuery(wire.Query)
	if err != nil {
		return err
	}
	v.Query = query
	return nil
}

// MarshalJSON writes the serde-tagged form of RelayRequestHistoryQuery.
func (v RelayRequestHistoryQuery) MarshalJSON() ([]byte, error) {
	type plain RelayRequestHistoryQuery
	return relayAdjacentJSON("method", "params", "history_query", plain(v))
}

type RelayRequestHistoryRequests struct{}

func (RelayRequestHistoryRequests) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestHistoryRequests.
func (v RelayRequestHistoryRequests) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("method", "params", "history_requests", nil)
}

type RelayRequestCompleteHistoryRequest struct {
	Result HistoryResult `json:"result"`
}

func (RelayRequestCompleteHistoryRequest) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestCompleteHistoryRequest.
func (v RelayRequestCompleteHistoryRequest) MarshalJSON() ([]byte, error) {
	type plain RelayRequestCompleteHistoryRequest
	return relayAdjacentJSON("method", "params", "complete_history_request", plain(v))
}

// Acknowledge a completed MCP request and cache its bounded answer for
// subsequent tool calls and restart recovery.
type RelayRequestCompleteSubagentRequest struct {
	Result SubagentToolResult `json:"result"`
}

func (RelayRequestCompleteSubagentRequest) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestCompleteSubagentRequest.
func (v RelayRequestCompleteSubagentRequest) MarshalJSON() ([]byte, error) {
	type plain RelayRequestCompleteSubagentRequest
	return relayAdjacentJSON("method", "params", "complete_subagent_request", plain(v))
}

// Capture repository changes or advance their review baseline.
type RelayRequestReviewer struct {
	// Legacy routing field retained for older workers; capture uses no role.
	Role    *string         `json:"role,omitempty"`
	Request ReviewerRequest `json:"request"`
}

func (RelayRequestReviewer) isRelayRequest() {}

// MarshalJSON writes the serde-tagged form of RelayRequestReviewer.
func (v RelayRequestReviewer) MarshalJSON() ([]byte, error) {
	type plain RelayRequestReviewer
	return relayAdjacentJSON("method", "params", "reviewer", plain(v))
}

// UnmarshalJSON decodes the interface-typed fields through their variant tables.
func (v *RelayRequestReviewer) UnmarshalJSON(data []byte) error {
	type plain RelayRequestReviewer
	aux := struct {
		*plain
		Request json.RawMessage `json:"request"`
	}{plain: (*plain)(v)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	if len(aux.Request) > 0 && string(aux.Request) != "null" {
		decoded, err := UnmarshalReviewerRequestJSON(aux.Request)
		if err != nil {
			return err
		}
		v.Request = decoded
	}
	return nil
}

// relayRequestVariants decodes each RelayRequest variant by its wire name.
var relayRequestVariants = map[string]func(json.RawMessage) (RelayRequest, error){
	"hello":                           relayRequiredVariant[RelayRequestHello, RelayRequest](),
	"attach":                          relayRequiredVariant[RelayRequestAttach, RelayRequest](),
	"acknowledge":                     relayRequiredVariant[RelayRequestAcknowledge, RelayRequest](),
	"submit":                          relayRequiredVariant[RelayRequestSubmit, RelayRequest](),
	"command_retention_status":        relayRequiredVariant[RelayRequestCommandRetentionStatus, RelayRequest](),
	"begin_live_baseline":             relayRequiredVariant[RelayRequestBeginLiveBaseline, RelayRequest](),
	"read_live_baseline_page":         relayRequiredVariant[RelayRequestReadLiveBaselinePage, RelayRequest](),
	"abort_live_baseline":             relayRequiredVariant[RelayRequestAbortLiveBaseline, RelayRequest](),
	"read_queued_command":             relayRequiredVariant[RelayRequestReadQueuedCommand, RelayRequest](),
	"read_pending_elicitation":        relayRequiredVariant[RelayRequestReadPendingElicitation, RelayRequest](),
	"read_checkpoint_mailbox":         relayRequiredVariant[RelayRequestReadCheckpointMailbox, RelayRequest](),
	"update_command_retention":        relayRequiredVariant[RelayRequestUpdateCommandRetention, RelayRequest](),
	"status":                          relayRequiredVariant[RelayRequestStatus, RelayRequest](),
	"drain_mailbox":                   relayRequiredVariant[RelayRequestDrainMailbox, RelayRequest](),
	"ack_mailbox":                     relayRequiredVariant[RelayRequestAckMailbox, RelayRequest](),
	"reserve_idle":                    relayRequiredVariant[RelayRequestReserveIdle, RelayRequest](),
	"attachment_present":              relayRequiredVariant[RelayRequestAttachmentPresent, RelayRequest](),
	"install_attachment":              relayRequiredVariant[RelayRequestInstallAttachment, RelayRequest](),
	"read_attachment":                 relayRequiredVariant[RelayRequestReadAttachment, RelayRequest](),
	"install_prompt_context":          relayRequiredVariant[RelayRequestInstallPromptContext, RelayRequest](),
	"project_memory_snapshot":         relayRequiredVariant[RelayRequestProjectMemorySnapshot, RelayRequest](),
	"install_project_memory_snapshot": relayRequiredVariant[RelayRequestInstallProjectMemorySnapshot, RelayRequest](),
	"replace_project_memory_tree":     relayRequiredVariant[RelayRequestReplaceProjectMemoryTree, RelayRequest](),
	"project_memory_transfer":         relayRequiredVariant[RelayRequestProjectMemoryTransfer, RelayRequest](),
	"cpu_usage":                       relayRequiredVariant[RelayRequestCpuUsage, RelayRequest](),
	"credential_state":                relayRequiredVariant[RelayRequestCredentialState, RelayRequest](),
	"read_credentials":                relayRequiredVariant[RelayRequestReadCredentials, RelayRequest](),
	"install_credentials":             relayRequiredVariant[RelayRequestInstallCredentials, RelayRequest](),
	"skills_state":                    relayRequiredVariant[RelayRequestSkillsState, RelayRequest](),
	"install_skills":                  relayRequiredVariant[RelayRequestInstallSkills, RelayRequest](),
	"github_token_state":              relayRequiredVariant[RelayRequestGithubTokenState, RelayRequest](),
	"install_github_token":            relayRequiredVariant[RelayRequestInstallGithubToken, RelayRequest](),
	"remove_github_token":             relayRequiredVariant[RelayRequestRemoveGithubToken, RelayRequest](),
	"aws_credentials_state":           relayRequiredVariant[RelayRequestAwsCredentialsState, RelayRequest](),
	"install_aws_credentials":         relayRequiredVariant[RelayRequestInstallAwsCredentials, RelayRequest](),
	"remove_aws_credentials":          relayRequiredVariant[RelayRequestRemoveAwsCredentials, RelayRequest](),
	"aws_credentials_scope":           relayRequiredVariant[RelayRequestAwsCredentialsScope, RelayRequest](),
	"slopcop_login_state":             relayRequiredVariant[RelayRequestSlopcopLoginState, RelayRequest](),
	"install_slopcop_login":           relayRequiredVariant[RelayRequestInstallSlopcopLogin, RelayRequest](),
	"remove_slopcop_login":            relayRequiredVariant[RelayRequestRemoveSlopcopLogin, RelayRequest](),
	"respond_elicitation":             relayRequiredVariant[RelayRequestRespondElicitation, RelayRequest](),
	"stop_background_task":            relayRequiredVariant[RelayRequestStopBackgroundTask, RelayRequest](),
	"subagent_requests":               relayRequiredVariant[RelayRequestSubagentRequests, RelayRequest](),
	"poll_subagent_queue":             relayRequiredVariant[RelayRequestPollSubagentQueue, RelayRequest](),
	"wait_for_change":                 relayRequiredVariant[RelayRequestWaitForChange, RelayRequest](),
	"set_operator_away":               relayRequiredVariant[RelayRequestSetOperatorAway, RelayRequest](),
	"set_subagent_admission":          relayRequiredVariant[RelayRequestSetSubagentAdmission, RelayRequest](),
	"history_query":                   relayRequiredVariant[RelayRequestHistoryQuery, RelayRequest](),
	"history_requests":                relayRequiredVariant[RelayRequestHistoryRequests, RelayRequest](),
	"complete_history_request":        relayRequiredVariant[RelayRequestCompleteHistoryRequest, RelayRequest](),
	"complete_subagent_request":       relayRequiredVariant[RelayRequestCompleteSubagentRequest, RelayRequest](),
	"reviewer":                        relayRequiredVariant[RelayRequestReviewer, RelayRequest](),
}

// UnmarshalRelayRequestJSON decodes the serde-tagged form of RelayRequest.
func UnmarshalRelayRequestJSON(data []byte) (RelayRequest, error) {
	name, body, err := relaySplitAdjacent(data, "method", "params", true)
	if err != nil {
		return nil, fmt.Errorf("decode RelayRequest: %w", err)
	}
	decode, ok := relayRequestVariants[name]
	if !ok {
		return nil, fmt.Errorf("decode RelayRequest: unknown variant %q", name)
	}
	return decode(body)
}

// ReviewerRequest is a worker-owned repository operation used by the review host.
type ReviewerRequest interface {
	json.Marshaler
	isReviewerRequest()
}

// Capture committed changes since the push review base. Scope and
// Baselines remain on the wire for older workers.
type ReviewerRequestCaptureDelta struct {
	Repositories *[]string         `json:"repositories,omitempty"`
	Scope        CaptureScope      `json:"scope"`
	Baselines    map[string]string `json:"baselines"`
}

func (ReviewerRequestCaptureDelta) isReviewerRequest() {}

// MarshalJSON writes the serde-tagged form of ReviewerRequestCaptureDelta.
func (v ReviewerRequestCaptureDelta) MarshalJSON() ([]byte, error) {
	type plain ReviewerRequestCaptureDelta
	return relayAdjacentJSON("action", "params", "capture_delta", plain(v))
}

// Record Trees, the current_tree ids of a reviewed capture, as the new
// review baselines, pinning each so a later git gc cannot collect it.
type ReviewerRequestRecordReviewed struct {
	Trees map[string][2]string `json:"trees"`
}

func (ReviewerRequestRecordReviewed) isReviewerRequest() {}

// MarshalJSON writes the serde-tagged form of ReviewerRequestRecordReviewed.
func (v ReviewerRequestRecordReviewed) MarshalJSON() ([]byte, error) {
	type plain ReviewerRequestRecordReviewed
	return relayAdjacentJSON("action", "params", "record_reviewed", plain(v))
}

// Record Trees as overridden: a push of that tree is allowed, but unlike
// RecordReviewed it does not become the base of the next push review.
type ReviewerRequestRecordOverridden struct {
	Trees map[string][2]string `json:"trees"`
}

func (ReviewerRequestRecordOverridden) isReviewerRequest() {}

// MarshalJSON writes the serde-tagged form of ReviewerRequestRecordOverridden.
func (v ReviewerRequestRecordOverridden) MarshalJSON() ([]byte, error) {
	type plain ReviewerRequestRecordOverridden
	return relayAdjacentJSON("action", "params", "record_overridden", plain(v))
}

// Make Commit the base of the next push review of one repository. The
// worker resolves Path like a scoped capture (the working directory when
// absent) and Commit as any revision, and refuses unless the commit's
// tree was recorded as reviewed or overridden.
type ReviewerRequestSetReviewBase struct {
	Path   *string `json:"path,omitempty"`
	Commit string  `json:"commit"`
}

func (ReviewerRequestSetReviewBase) isReviewerRequest() {}

// MarshalJSON writes the serde-tagged form of ReviewerRequestSetReviewBase.
func (v ReviewerRequestSetReviewBase) MarshalJSON() ([]byte, error) {
	type plain ReviewerRequestSetReviewBase
	return relayAdjacentJSON("action", "params", "set_review_base", plain(v))
}

// reviewerRequestVariants decodes each ReviewerRequest variant by its wire name.
var reviewerRequestVariants = map[string]func(json.RawMessage) (ReviewerRequest, error){
	"capture_delta":     relayVariant[ReviewerRequestCaptureDelta, ReviewerRequest](false),
	"record_reviewed":   relayVariant[ReviewerRequestRecordReviewed, ReviewerRequest](false),
	"record_overridden": relayVariant[ReviewerRequestRecordOverridden, ReviewerRequest](false),
	"set_review_base":   relayVariant[ReviewerRequestSetReviewBase, ReviewerRequest](false),
}

// UnmarshalReviewerRequestJSON decodes the serde-tagged form of ReviewerRequest.
func UnmarshalReviewerRequestJSON(data []byte) (ReviewerRequest, error) {
	name, body, err := relaySplitAdjacent(data, "action", "params", false)
	if err != nil {
		return nil, fmt.Errorf("decode ReviewerRequest: %w", err)
	}
	decode, ok := reviewerRequestVariants[name]
	if !ok {
		return nil, fmt.Errorf("decode ReviewerRequest: unknown variant %q", name)
	}
	return decode(body)
}

// RelayResponseBody is the flattened outcome of a relay response.
type RelayResponseBody interface {
	json.Marshaler
	isRelayResponseBody()
}

type RelayResponseBodyOk struct {
	Payload RelayResponsePayload `json:"payload"`
}

func (RelayResponseBodyOk) isRelayResponseBody() {}

// MarshalJSON writes the serde-tagged form of RelayResponseBodyOk.
func (v RelayResponseBodyOk) MarshalJSON() ([]byte, error) {
	type plain RelayResponseBodyOk
	return relayInternalJSON("result", "ok", plain(v))
}

// UnmarshalJSON decodes the interface-typed fields through their variant tables.
func (v *RelayResponseBodyOk) UnmarshalJSON(data []byte) error {
	type plain RelayResponseBodyOk
	aux := struct {
		*plain
		Payload json.RawMessage `json:"payload"`
	}{plain: (*plain)(v)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	if len(aux.Payload) > 0 && string(aux.Payload) != "null" {
		decoded, err := UnmarshalRelayResponsePayloadJSON(aux.Payload)
		if err != nil {
			return err
		}
		v.Payload = decoded
	}
	return nil
}

type RelayResponseBodyError struct {
	Error RelayProtocolError `json:"error"`
}

func (RelayResponseBodyError) isRelayResponseBody() {}

// MarshalJSON writes the serde-tagged form of RelayResponseBodyError.
func (v RelayResponseBodyError) MarshalJSON() ([]byte, error) {
	type plain RelayResponseBodyError
	return relayInternalJSON("result", "error", plain(v))
}

// relayResponseBodyVariants decodes each RelayResponseBody variant by its wire name.
var relayResponseBodyVariants = map[string]func(json.RawMessage) (RelayResponseBody, error){
	"ok":    relayInternalVariant[RelayResponseBodyOk, RelayResponseBody]("result", false),
	"error": relayInternalVariant[RelayResponseBodyError, RelayResponseBody]("result", false),
}

// UnmarshalRelayResponseBodyJSON decodes the serde-tagged form of RelayResponseBody.
func UnmarshalRelayResponseBodyJSON(data []byte) (RelayResponseBody, error) {
	name, err := relaySplitInternal(data, "result")
	body := json.RawMessage(data)
	if err != nil {
		return nil, fmt.Errorf("decode RelayResponseBody: %w", err)
	}
	decode, ok := relayResponseBodyVariants[name]
	if !ok {
		return nil, fmt.Errorf("decode RelayResponseBody: unknown variant %q", name)
	}
	return decode(body)
}

// RelayResponsePayload is the successful answer to a RelayRequest.
type RelayResponsePayload interface {
	json.Marshaler
	isRelayResponsePayload()
}

type RelayResponsePayloadCommandRetentionUpdated struct{}

func (RelayResponsePayloadCommandRetentionUpdated) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadCommandRetentionUpdated.
func (v RelayResponsePayloadCommandRetentionUpdated) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "command_retention_updated", nil)
}

type RelayResponsePayloadCommandRetentionState struct {
	LegacyPending bool `json:"legacy_pending"`
	// Sorted and unique (a Rust BTreeSet).
	OperationIds []string `json:"operation_ids"`
}

func (RelayResponsePayloadCommandRetentionState) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadCommandRetentionState.
func (v RelayResponsePayloadCommandRetentionState) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadCommandRetentionState
	return relayAdjacentJSON("type", "data", "command_retention_state", plain(v))
}

// Ordinal nil means work won the race; no barrier or other mutation occurred.
type RelayResponsePayloadIdleReservation struct {
	Ordinal *uint64 `json:"ordinal"`
}

func (RelayResponsePayloadIdleReservation) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadIdleReservation.
func (v RelayResponsePayloadIdleReservation) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadIdleReservation
	return relayAdjacentJSON("type", "data", "idle_reservation", plain(v))
}

type RelayResponsePayloadHello struct {
	Negotiated   uint32 `json:"negotiated"`
	RelayVersion string `json:"relay_version"`
	SessionID    string `json:"session_id"`
	// Worker-input compatibility identity of the worker that answered.
	// Absent from a worker built before the field existed, which counts
	// as outdated.
	WorkerBuild *string `json:"worker_build,omitempty"`
}

func (RelayResponsePayloadHello) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadHello.
func (v RelayResponsePayloadHello) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadHello
	return relayAdjacentJSON("type", "data", "hello", plain(v))
}

type RelayResponsePayloadAttached struct {
	State          RelayOperationalState `json:"state"`
	Events         []RelayEvent          `json:"events"`
	ThroughOrdinal uint64                `json:"through_ordinal"`
	ThroughDigest  string                `json:"through_digest"`
	// RelayStateDigest of State, for a controller that negotiated protocol 57
	// to name in its next attach.
	StateDigest *string `json:"state_digest,omitempty"`
}

func (RelayResponsePayloadAttached) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadAttached.
func (v RelayResponsePayloadAttached) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadAttached
	return relayAdjacentJSON("type", "data", "attached", plain(v))
}

// The answer to RelayRequestWaitForChange: the worker's latest event
// ordinal and state digest now, and whether this connection's sub-agent
// queue has changes to poll. Equal values mean the wait ran out.
type RelayResponsePayloadChangeObserved struct {
	LatestOrdinal        uint64 `json:"latest_ordinal"`
	StateDigest          string `json:"state_digest"`
	SubagentQueueChanged bool   `json:"subagent_queue_changed"`
}

func (RelayResponsePayloadChangeObserved) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadChangeObserved.
func (v RelayResponsePayloadChangeObserved) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadChangeObserved
	return relayAdjacentJSON("type", "data", "change_observed", plain(v))
}

// An attach whose known_state_digest still matches: the state is the
// one the controller holds, so only the events are sent. Protocol 57.
type RelayResponsePayloadAttachedWithKnownState struct {
	Events         []RelayEvent `json:"events"`
	ThroughOrdinal uint64       `json:"through_ordinal"`
	ThroughDigest  string       `json:"through_digest"`
}

func (RelayResponsePayloadAttachedWithKnownState) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadAttachedWithKnownState.
func (v RelayResponsePayloadAttachedWithKnownState) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadAttachedWithKnownState
	return relayAdjacentJSON("type", "data", "attached_with_known_state", plain(v))
}

type RelayResponsePayloadAcknowledged struct {
	ThroughOrdinal uint64 `json:"through_ordinal"`
	ThroughDigest  string `json:"through_digest"`
}

func (RelayResponsePayloadAcknowledged) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadAcknowledged.
func (v RelayResponsePayloadAcknowledged) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadAcknowledged
	return relayAdjacentJSON("type", "data", "acknowledged", plain(v))
}

type RelayResponsePayloadAccepted struct {
	CommandID string `json:"command_id"`
	Ordinal   uint64 `json:"ordinal"`
}

func (RelayResponsePayloadAccepted) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadAccepted.
func (v RelayResponsePayloadAccepted) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadAccepted
	return relayAdjacentJSON("type", "data", "accepted", plain(v))
}

type RelayResponsePayloadStatus struct {
	State RelayOperationalState
}

func (RelayResponsePayloadStatus) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadStatus.
func (v RelayResponsePayloadStatus) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "status", v.State)
}

type RelayResponsePayloadLiveBaselineWaiting struct {
	Blockers []string `json:"blockers"`
}

func (RelayResponsePayloadLiveBaselineWaiting) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadLiveBaselineWaiting.
func (v RelayResponsePayloadLiveBaselineWaiting) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadLiveBaselineWaiting
	return relayAdjacentJSON("type", "data", "live_baseline_waiting", plain(v))
}

type RelayResponsePayloadLiveBaselineStarted struct {
	TotalBytes uint64 `json:"total_bytes"`
}

func (RelayResponsePayloadLiveBaselineStarted) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadLiveBaselineStarted.
func (v RelayResponsePayloadLiveBaselineStarted) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadLiveBaselineStarted
	return relayAdjacentJSON("type", "data", "live_baseline_started", plain(v))
}

type RelayResponsePayloadLiveBaselinePage struct {
	Page FrozenSnapshotPage `json:"page"`
}

func (RelayResponsePayloadLiveBaselinePage) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadLiveBaselinePage.
func (v RelayResponsePayloadLiveBaselinePage) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadLiveBaselinePage
	return relayAdjacentJSON("type", "data", "live_baseline_page", plain(v))
}

type RelayResponsePayloadLiveBaselineAborted struct{}

func (RelayResponsePayloadLiveBaselineAborted) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadLiveBaselineAborted.
func (v RelayResponsePayloadLiveBaselineAborted) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "live_baseline_aborted", nil)
}

type RelayResponsePayloadQueuedCommand struct {
	Command         RelayCommand `json:"command"`
	AcceptedOrdinal *uint64      `json:"accepted_ordinal"`
}

func (RelayResponsePayloadQueuedCommand) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadQueuedCommand.
func (v RelayResponsePayloadQueuedCommand) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadQueuedCommand
	return relayAdjacentJSON("type", "data", "queued_command", plain(v))
}

// UnmarshalJSON decodes the interface-typed fields through their variant tables.
func (v *RelayResponsePayloadQueuedCommand) UnmarshalJSON(data []byte) error {
	type plain RelayResponsePayloadQueuedCommand
	aux := struct {
		*plain
		Command json.RawMessage `json:"command"`
	}{plain: (*plain)(v)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	if len(aux.Command) > 0 && string(aux.Command) != "null" {
		decoded, err := UnmarshalRelayCommandJSON(aux.Command)
		if err != nil {
			return err
		}
		v.Command = decoded
	}
	return nil
}

// RelayResponsePayloadCheckpointMailbox is the mail at a ready checkpoint
// cut (protocol 75).
type RelayResponsePayloadCheckpointMailbox struct {
	Mailbox CanonicalMailbox `json:"mailbox"`
}

func (RelayResponsePayloadCheckpointMailbox) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadCheckpointMailbox.
func (v RelayResponsePayloadCheckpointMailbox) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadCheckpointMailbox
	return relayAdjacentJSON("type", "data", "checkpoint_mailbox", plain(v))
}

type RelayResponsePayloadPendingElicitation struct {
	Request *ElicitationRequest `json:"request"`
}

func (RelayResponsePayloadPendingElicitation) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadPendingElicitation.
func (v RelayResponsePayloadPendingElicitation) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadPendingElicitation
	return relayAdjacentJSON("type", "data", "pending_elicitation", plain(v))
}

type RelayResponsePayloadMailboxDrained struct {
	// Present when mailbox events or a Claude boundary steer are leased.
	LeaseID *string `json:"lease_id,omitempty"`
	Text    *string `json:"text"`
	// Number of context items leased to the hook.
	Count int `json:"count"`
	// Per-event identities and rendering for durable harness inboxes.
	// Additive response data; older hook clients still use Text.
	Contexts []MailboxHookContext `json:"contexts,omitempty"`
}

func (RelayResponsePayloadMailboxDrained) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadMailboxDrained.
func (v RelayResponsePayloadMailboxDrained) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadMailboxDrained
	return relayAdjacentJSON("type", "data", "mailbox_drained", plain(v))
}

type RelayResponsePayloadMailboxAcknowledged struct {
	Acknowledged bool `json:"acknowledged"`
}

func (RelayResponsePayloadMailboxAcknowledged) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadMailboxAcknowledged.
func (v RelayResponsePayloadMailboxAcknowledged) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadMailboxAcknowledged
	return relayAdjacentJSON("type", "data", "mailbox_acknowledged", plain(v))
}

type RelayResponsePayloadAttachmentPresent struct {
	Present bool `json:"present"`
}

func (RelayResponsePayloadAttachmentPresent) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadAttachmentPresent.
func (v RelayResponsePayloadAttachmentPresent) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadAttachmentPresent
	return relayAdjacentJSON("type", "data", "attachment_present", plain(v))
}

type RelayResponsePayloadAttachmentInstalled struct{}

func (RelayResponsePayloadAttachmentInstalled) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadAttachmentInstalled.
func (v RelayResponsePayloadAttachmentInstalled) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "attachment_installed", nil)
}

type RelayResponsePayloadAttachmentData struct {
	Data string `json:"data"`
}

func (RelayResponsePayloadAttachmentData) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadAttachmentData.
func (v RelayResponsePayloadAttachmentData) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadAttachmentData
	return relayAdjacentJSON("type", "data", "attachment_data", plain(v))
}

type RelayResponsePayloadPromptContextInstalled struct{}

func (RelayResponsePayloadPromptContextInstalled) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadPromptContextInstalled.
func (v RelayResponsePayloadPromptContextInstalled) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "prompt_context_installed", nil)
}

type RelayResponsePayloadProjectMemorySnapshot struct {
	Baseline ProjectMemoryWireTree `json:"baseline"`
	Replica  ProjectMemoryWireTree `json:"replica"`
}

func (RelayResponsePayloadProjectMemorySnapshot) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadProjectMemorySnapshot.
func (v RelayResponsePayloadProjectMemorySnapshot) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadProjectMemorySnapshot
	return relayAdjacentJSON("type", "data", "project_memory_snapshot", plain(v))
}

// UnmarshalJSON decodes the interface-typed fields through their variant tables.
func (v *RelayResponsePayloadProjectMemorySnapshot) UnmarshalJSON(data []byte) error {
	type plain RelayResponsePayloadProjectMemorySnapshot
	aux := struct {
		*plain
		Baseline json.RawMessage `json:"baseline"`
		Replica  json.RawMessage `json:"replica"`
	}{plain: (*plain)(v)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	if len(aux.Baseline) > 0 && string(aux.Baseline) != "null" {
		decoded, err := UnmarshalProjectMemoryWireTreeJSON(aux.Baseline)
		if err != nil {
			return err
		}
		v.Baseline = decoded
	}
	if len(aux.Replica) > 0 && string(aux.Replica) != "null" {
		decoded, err := UnmarshalProjectMemoryWireTreeJSON(aux.Replica)
		if err != nil {
			return err
		}
		v.Replica = decoded
	}
	return nil
}

type RelayResponsePayloadProjectMemorySnapshotInstalled struct{}

func (RelayResponsePayloadProjectMemorySnapshotInstalled) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadProjectMemorySnapshotInstalled.
func (v RelayResponsePayloadProjectMemorySnapshotInstalled) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "project_memory_snapshot_installed", nil)
}

type RelayResponsePayloadProjectMemoryTreeReplaced struct {
	Outcome ReplicaReplaceOutcome `json:"outcome"`
}

func (RelayResponsePayloadProjectMemoryTreeReplaced) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadProjectMemoryTreeReplaced.
func (v RelayResponsePayloadProjectMemoryTreeReplaced) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadProjectMemoryTreeReplaced
	return relayAdjacentJSON("type", "data", "project_memory_tree_replaced", plain(v))
}

type RelayResponsePayloadProjectMemoryTransfer struct {
	Result ProjectMemoryTransferResult `json:"result"`
}

func (RelayResponsePayloadProjectMemoryTransfer) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadProjectMemoryTransfer.
func (v RelayResponsePayloadProjectMemoryTransfer) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadProjectMemoryTransfer
	return relayAdjacentJSON("type", "data", "project_memory_transfer", plain(v))
}

// UnmarshalJSON decodes the interface-typed fields through their variant tables.
func (v *RelayResponsePayloadProjectMemoryTransfer) UnmarshalJSON(data []byte) error {
	type plain RelayResponsePayloadProjectMemoryTransfer
	aux := struct {
		*plain
		Result json.RawMessage `json:"result"`
	}{plain: (*plain)(v)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	if len(aux.Result) > 0 && string(aux.Result) != "null" {
		decoded, err := UnmarshalProjectMemoryTransferResultJSON(aux.Result)
		if err != nil {
			return err
		}
		v.Result = decoded
	}
	return nil
}

// Fingerprint and freshness of a session's harness credentials. Neither
// value is secret.
type RelayResponsePayloadCpuUsage struct {
	Usage *SessionCpuUsage `json:"usage"`
}

func (RelayResponsePayloadCpuUsage) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadCpuUsage.
func (v RelayResponsePayloadCpuUsage) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadCpuUsage
	return relayAdjacentJSON("type", "data", "cpu_usage", plain(v))
}

type RelayResponsePayloadCredentialState struct {
	Present          bool   `json:"present"`
	Fingerprint      string `json:"fingerprint"`
	FreshnessEpochMs *int64 `json:"freshness_epoch_ms"`
}

func (RelayResponsePayloadCredentialState) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadCredentialState.
func (v RelayResponsePayloadCredentialState) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadCredentialState
	return relayAdjacentJSON("type", "data", "credential_state", plain(v))
}

// The scope recorded beside a session's AWS credentials, and the AWS
// profile the session was staged with (the section name in its AWS
// configuration, which survives the credentials being withdrawn). Not
// secret.
type RelayResponsePayloadAwsCredentialsScope struct {
	Scope   *AwsCredentialScope `json:"scope"`
	Profile *string             `json:"profile,omitempty"`
}

func (RelayResponsePayloadAwsCredentialsScope) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadAwsCredentialsScope.
func (v RelayResponsePayloadAwsCredentialsScope) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadAwsCredentialsScope
	return relayAdjacentJSON("type", "data", "aws_credentials_scope", plain(v))
}

// Base64 of a session's credential file. Sent only on the connection
// socket, never recorded.
type RelayResponsePayloadCredentials struct {
	Data string `json:"data"`
}

func (RelayResponsePayloadCredentials) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadCredentials.
func (v RelayResponsePayloadCredentials) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadCredentials
	return relayAdjacentJSON("type", "data", "credentials", plain(v))
}

// Fingerprint of a session's synced skills trees. Not secret.
type RelayResponsePayloadSkillsState struct {
	Present     bool   `json:"present"`
	Fingerprint string `json:"fingerprint"`
}

func (RelayResponsePayloadSkillsState) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadSkillsState.
func (v RelayResponsePayloadSkillsState) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadSkillsState
	return relayAdjacentJSON("type", "data", "skills_state", plain(v))
}

// Presence and fingerprint of the worker-private GitHub CLI token.
type RelayResponsePayloadGithubTokenState struct {
	Present     bool   `json:"present"`
	Fingerprint string `json:"fingerprint"`
}

func (RelayResponsePayloadGithubTokenState) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadGithubTokenState.
func (v RelayResponsePayloadGithubTokenState) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadGithubTokenState
	return relayAdjacentJSON("type", "data", "github_token_state", plain(v))
}

type RelayResponsePayloadElicitationResolved struct {
	ElicitationID string `json:"elicitation_id"`
}

func (RelayResponsePayloadElicitationResolved) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadElicitationResolved.
func (v RelayResponsePayloadElicitationResolved) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadElicitationResolved
	return relayAdjacentJSON("type", "data", "elicitation_resolved", plain(v))
}

type RelayResponsePayloadBackgroundTaskStopRequested struct {
	BackgroundTaskID string `json:"background_task_id"`
}

func (RelayResponsePayloadBackgroundTaskStopRequested) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadBackgroundTaskStopRequested.
func (v RelayResponsePayloadBackgroundTaskStopRequested) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadBackgroundTaskStopRequested
	return relayAdjacentJSON("type", "data", "background_task_stop_requested", plain(v))
}

type RelayResponsePayloadSubagentRequests struct {
	Requests []SubagentToolRequest `json:"requests"`
	Results  []SubagentToolResult  `json:"results"`
}

func (RelayResponsePayloadSubagentRequests) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadSubagentRequests.
func (v RelayResponsePayloadSubagentRequests) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadSubagentRequests
	return relayAdjacentJSON("type", "data", "subagent_requests", plain(v))
}

type RelayResponsePayloadSubagentQueue struct {
	Page SubagentQueuePage `json:"page"`
}

func (RelayResponsePayloadSubagentQueue) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadSubagentQueue.
func (v RelayResponsePayloadSubagentQueue) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadSubagentQueue
	return relayAdjacentJSON("type", "data", "subagent_queue", plain(v))
}

type RelayResponsePayloadSubagentAdmissionChanged struct {
	Open bool `json:"open"`
}

func (RelayResponsePayloadSubagentAdmissionChanged) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadSubagentAdmissionChanged.
func (v RelayResponsePayloadSubagentAdmissionChanged) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadSubagentAdmissionChanged
	return relayAdjacentJSON("type", "data", "subagent_admission_changed", plain(v))
}

type RelayResponsePayloadSubagentRequestCompleted struct{}

func (RelayResponsePayloadSubagentRequestCompleted) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadSubagentRequestCompleted.
func (v RelayResponsePayloadSubagentRequestCompleted) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "subagent_request_completed", nil)
}

// Newer peers report whether the result reached a live tool caller. The
// original unit response remains valid and is treated as delivered.
type RelayResponsePayloadSubagentRequestCompletedWithDelivery struct {
	DeliveredToWaiter bool `json:"delivered_to_waiter"`
}

func (RelayResponsePayloadSubagentRequestCompletedWithDelivery) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadSubagentRequestCompletedWithDelivery.
func (v RelayResponsePayloadSubagentRequestCompletedWithDelivery) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadSubagentRequestCompletedWithDelivery
	return relayAdjacentJSON("type", "data", "subagent_request_completed_with_delivery", plain(v))
}

type RelayResponsePayloadHistoryRequests struct {
	Requests []HistoryRequest `json:"requests"`
}

func (RelayResponsePayloadHistoryRequests) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadHistoryRequests.
func (v RelayResponsePayloadHistoryRequests) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadHistoryRequests
	return relayAdjacentJSON("type", "data", "history_requests", plain(v))
}

type RelayResponsePayloadHistoryResult struct {
	Result HistoryResult `json:"result"`
}

func (RelayResponsePayloadHistoryResult) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadHistoryResult.
func (v RelayResponsePayloadHistoryResult) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadHistoryResult
	return relayAdjacentJSON("type", "data", "history_result", plain(v))
}

type RelayResponsePayloadHistoryRequestCompleted struct{}

func (RelayResponsePayloadHistoryRequestCompleted) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadHistoryRequestCompleted.
func (v RelayResponsePayloadHistoryRequestCompleted) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "history_request_completed", nil)
}

// What every workspace repository changed since the stored baselines.
type RelayResponsePayloadReviewDelta struct {
	Repositories []RepoDelta `json:"repositories"`
}

func (RelayResponsePayloadReviewDelta) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadReviewDelta.
func (v RelayResponsePayloadReviewDelta) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadReviewDelta
	return relayAdjacentJSON("type", "data", "review_delta", plain(v))
}

// The review baselines now name the trees the controller sent.
// Acknowledges RecordReviewed and RecordOverridden.
type RelayResponsePayloadReviewBaselineAdvanced struct{}

func (RelayResponsePayloadReviewBaselineAdvanced) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadReviewBaselineAdvanced.
func (v RelayResponsePayloadReviewBaselineAdvanced) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "review_baseline_advanced", nil)
}

// The repository, commit and tree SetReviewBase made the review base.
type RelayResponsePayloadReviewBaseSet struct {
	Root   string `json:"root"`
	Commit string `json:"commit"`
	Tree   string `json:"tree"`
	// Absent on the wire means true (serde default review_base_changed_default).
	Changed bool `json:"changed"`
}

func (RelayResponsePayloadReviewBaseSet) isRelayResponsePayload() {}

// MarshalJSON writes the serde-tagged form of RelayResponsePayloadReviewBaseSet.
func (v RelayResponsePayloadReviewBaseSet) MarshalJSON() ([]byte, error) {
	type plain RelayResponsePayloadReviewBaseSet
	return relayAdjacentJSON("type", "data", "review_base_set", plain(v))
}

// relayResponsePayloadVariants decodes each RelayResponsePayload variant by its wire name.
var relayResponsePayloadVariants = map[string]func(json.RawMessage) (RelayResponsePayload, error){
	"command_retention_updated": relayVariant[RelayResponsePayloadCommandRetentionUpdated, RelayResponsePayload](false),
	"command_retention_state":   relayVariant[RelayResponsePayloadCommandRetentionState, RelayResponsePayload](false),
	"idle_reservation":          relayVariant[RelayResponsePayloadIdleReservation, RelayResponsePayload](false),
	"hello":                     relayVariant[RelayResponsePayloadHello, RelayResponsePayload](false),
	"attached":                  relayVariant[RelayResponsePayloadAttached, RelayResponsePayload](false),
	"change_observed":           relayVariant[RelayResponsePayloadChangeObserved, RelayResponsePayload](false),
	"attached_with_known_state": relayVariant[RelayResponsePayloadAttachedWithKnownState, RelayResponsePayload](false),
	"acknowledged":              relayVariant[RelayResponsePayloadAcknowledged, RelayResponsePayload](false),
	"accepted":                  relayVariant[RelayResponsePayloadAccepted, RelayResponsePayload](false),
	"status": func(body json.RawMessage) (RelayResponsePayload, error) {
		var v RelayResponsePayloadStatus
		err := json.Unmarshal(body, &v.State)
		return v, err
	},
	"live_baseline_waiting":                    relayVariant[RelayResponsePayloadLiveBaselineWaiting, RelayResponsePayload](false),
	"live_baseline_started":                    relayVariant[RelayResponsePayloadLiveBaselineStarted, RelayResponsePayload](false),
	"live_baseline_page":                       relayVariant[RelayResponsePayloadLiveBaselinePage, RelayResponsePayload](false),
	"live_baseline_aborted":                    relayVariant[RelayResponsePayloadLiveBaselineAborted, RelayResponsePayload](false),
	"queued_command":                           relayVariant[RelayResponsePayloadQueuedCommand, RelayResponsePayload](false),
	"pending_elicitation":                      relayVariant[RelayResponsePayloadPendingElicitation, RelayResponsePayload](false),
	"checkpoint_mailbox":                       relayVariant[RelayResponsePayloadCheckpointMailbox, RelayResponsePayload](false),
	"mailbox_drained":                          relayVariant[RelayResponsePayloadMailboxDrained, RelayResponsePayload](false),
	"mailbox_acknowledged":                     relayVariant[RelayResponsePayloadMailboxAcknowledged, RelayResponsePayload](false),
	"attachment_present":                       relayVariant[RelayResponsePayloadAttachmentPresent, RelayResponsePayload](false),
	"attachment_installed":                     relayVariant[RelayResponsePayloadAttachmentInstalled, RelayResponsePayload](false),
	"attachment_data":                          relayVariant[RelayResponsePayloadAttachmentData, RelayResponsePayload](false),
	"prompt_context_installed":                 relayVariant[RelayResponsePayloadPromptContextInstalled, RelayResponsePayload](false),
	"project_memory_snapshot":                  relayVariant[RelayResponsePayloadProjectMemorySnapshot, RelayResponsePayload](false),
	"project_memory_snapshot_installed":        relayVariant[RelayResponsePayloadProjectMemorySnapshotInstalled, RelayResponsePayload](false),
	"project_memory_tree_replaced":             relayVariant[RelayResponsePayloadProjectMemoryTreeReplaced, RelayResponsePayload](false),
	"project_memory_transfer":                  relayVariant[RelayResponsePayloadProjectMemoryTransfer, RelayResponsePayload](false),
	"cpu_usage":                                relayVariant[RelayResponsePayloadCpuUsage, RelayResponsePayload](false),
	"credential_state":                         relayVariant[RelayResponsePayloadCredentialState, RelayResponsePayload](false),
	"aws_credentials_scope":                    relayVariant[RelayResponsePayloadAwsCredentialsScope, RelayResponsePayload](false),
	"credentials":                              relayVariant[RelayResponsePayloadCredentials, RelayResponsePayload](false),
	"skills_state":                             relayVariant[RelayResponsePayloadSkillsState, RelayResponsePayload](false),
	"github_token_state":                       relayVariant[RelayResponsePayloadGithubTokenState, RelayResponsePayload](false),
	"elicitation_resolved":                     relayVariant[RelayResponsePayloadElicitationResolved, RelayResponsePayload](false),
	"background_task_stop_requested":           relayVariant[RelayResponsePayloadBackgroundTaskStopRequested, RelayResponsePayload](false),
	"subagent_requests":                        relayVariant[RelayResponsePayloadSubagentRequests, RelayResponsePayload](false),
	"subagent_queue":                           relayVariant[RelayResponsePayloadSubagentQueue, RelayResponsePayload](false),
	"subagent_admission_changed":               relayVariant[RelayResponsePayloadSubagentAdmissionChanged, RelayResponsePayload](false),
	"subagent_request_completed":               relayVariant[RelayResponsePayloadSubagentRequestCompleted, RelayResponsePayload](false),
	"subagent_request_completed_with_delivery": relayVariant[RelayResponsePayloadSubagentRequestCompletedWithDelivery, RelayResponsePayload](false),
	"history_requests":                         relayVariant[RelayResponsePayloadHistoryRequests, RelayResponsePayload](false),
	"history_result":                           relayVariant[RelayResponsePayloadHistoryResult, RelayResponsePayload](false),
	"history_request_completed":                relayVariant[RelayResponsePayloadHistoryRequestCompleted, RelayResponsePayload](false),
	"review_delta":                             relayVariant[RelayResponsePayloadReviewDelta, RelayResponsePayload](false),
	"review_baseline_advanced":                 relayVariant[RelayResponsePayloadReviewBaselineAdvanced, RelayResponsePayload](false),
	"review_base_set":                          relayVariant[RelayResponsePayloadReviewBaseSet, RelayResponsePayload](false),
}

// UnmarshalRelayResponsePayloadJSON decodes the serde-tagged form of RelayResponsePayload.
func UnmarshalRelayResponsePayloadJSON(data []byte) (RelayResponsePayload, error) {
	name, body, err := relaySplitAdjacent(data, "type", "data", false)
	if err != nil {
		return nil, fmt.Errorf("decode RelayResponsePayload: %w", err)
	}
	decode, ok := relayResponsePayloadVariants[name]
	if !ok {
		return nil, fmt.Errorf("decode RelayResponsePayload: unknown variant %q", name)
	}
	return decode(body)
}

// RelayErrorDetail is the structured detail of a relay protocol error.
type RelayErrorDetail interface {
	json.Marshaler
	isRelayErrorDetail()
}

type RelayErrorDetailDesynchronized struct {
	RequestedAfter    uint64 `json:"requested_after"`
	RequestedDigest   string `json:"requested_digest"`
	EarliestAvailable uint64 `json:"earliest_available"`
	EarliestDigest    string `json:"earliest_digest"`
	Latest            uint64 `json:"latest"`
	LatestDigest      string `json:"latest_digest"`
}

func (RelayErrorDetailDesynchronized) isRelayErrorDetail() {}

// MarshalJSON writes the serde-tagged form of RelayErrorDetailDesynchronized.
func (v RelayErrorDetailDesynchronized) MarshalJSON() ([]byte, error) {
	type plain RelayErrorDetailDesynchronized
	return relayInternalJSON("kind", "desynchronized", plain(v))
}

// relayErrorDetailVariants decodes each RelayErrorDetail variant by its wire name.
var relayErrorDetailVariants = map[string]func(json.RawMessage) (RelayErrorDetail, error){
	"desynchronized": relayInternalVariant[RelayErrorDetailDesynchronized, RelayErrorDetail]("kind", false),
}

// UnmarshalRelayErrorDetailJSON decodes the serde-tagged form of RelayErrorDetail.
func UnmarshalRelayErrorDetailJSON(data []byte) (RelayErrorDetail, error) {
	name, err := relaySplitInternal(data, "kind")
	body := json.RawMessage(data)
	if err != nil {
		return nil, fmt.Errorf("decode RelayErrorDetail: %w", err)
	}
	decode, ok := relayErrorDetailVariants[name]
	if !ok {
		return nil, fmt.Errorf("decode RelayErrorDetail: unknown variant %q", name)
	}
	return decode(body)
}
