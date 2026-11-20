// Port of mj-core/src/mailbox.rs.
//
// Session mailbox events shared by the daemon, relay, and worker.

package core

import (
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"unicode"
)

// MailboxHookRequestTimeoutSecs is the per-request deadline for a hook's
// drain and acknowledgement calls.
const MailboxHookRequestTimeoutSecs uint64 = 5

// MailboxHookTimeoutSecs is the harness timeout for the complete hook
// invocation, including output writes.
const MailboxHookTimeoutSecs uint64 = 20

// MailboxHookLeaseTimeoutMs is the time an unacknowledged hook delivery
// remains leased before it is retried.
const MailboxHookLeaseTimeoutMs int64 = 60_000

// MailboxCommentBodyLimit is the maximum GitHub comment or review body
// retained in an event.
const MailboxCommentBodyLimit = 8 * 1024

// Compile-time checks of the Rust const assertions: converting a negative
// constant to uint64 does not compile.
const (
	_ = uint64(MailboxHookTimeoutSecs - 2*MailboxHookRequestTimeoutSecs - 1)
	_ = uint64(MailboxHookLeaseTimeoutMs - int64(MailboxHookTimeoutSecs)*1_000 - 1)
)

// MailboxEvent is an event addressed to one session. ParentMessage and
// user-originated SessionMessage content are trusted. Peer SessionMessage
// content is explicitly labelled as another agent's work and carries no
// user authority.
//
// Key is the producer's stable deduplication identity. The event body is
// untrusted content and must only be shown to the agent through
// RenderMailboxEvents. The decoder also accepts the protocol-33
// {key, source, wake, text, created_at_ms} representation as plain text.
type MailboxEvent struct {
	Key         string           `json:"key"`
	Source      string           `json:"source"`
	Wake        bool             `json:"wake"`
	CreatedAtMs uint64           `json:"created_at_ms"`
	Body        MailboxEventBody `json:"body"`
}

// legacyMailboxEvent is the protocol-33 and relay revision-16
// representation retained for wire compatibility and historical digest
// validation.
type legacyMailboxEvent struct {
	Key         string `json:"key"`
	Source      string `json:"source"`
	Wake        bool   `json:"wake"`
	Text        string `json:"text"`
	CreatedAtMs uint64 `json:"created_at_ms"`
}

// UnmarshalJSON accepts the structured form or the protocol-33 text form,
// as serde's untagged representation does.
func (e *MailboxEvent) UnmarshalJSON(data []byte) error {
	var structured struct {
		Key         string          `json:"key"`
		Source      string          `json:"source"`
		Wake        bool            `json:"wake"`
		CreatedAtMs uint64          `json:"created_at_ms"`
		Body        json.RawMessage `json:"body"`
	}
	if transcriptStrictDecode(data, &structured, true) == nil {
		if body, err := DecodeMailboxEventBody(structured.Body); err == nil {
			*e = MailboxEvent{structured.Key, structured.Source, structured.Wake, structured.CreatedAtMs, body}
			return nil
		}
	}
	var legacy legacyMailboxEvent
	if transcriptStrictDecode(data, &legacy, true) == nil {
		*e = MailboxEvent{legacy.Key, legacy.Source, legacy.Wake, legacy.CreatedAtMs,
			MailboxEventBodyPlainText{Text: legacy.Text}}
		return nil
	}
	return errors.New("data did not match any variant of untagged enum Representation")
}

// HandbackAssignment: parent instructions are assignments only for the
// child's handback check. It returns the assignment id and text.
func (e *MailboxEvent) HandbackAssignment() (string, string, bool) {
	if body, ok := e.Body.(MailboxEventBodyParentMessage); ok {
		return "subagent-input-mailbox-" + e.Key, body.Text, true
	}
	return "", "", false
}

// MessageText is the full message text, as distinct from an informational
// mailbox event.
func (e *MailboxEvent) MessageText() (string, bool) {
	switch body := e.Body.(type) {
	case MailboxEventBodyParentMessage:
		return body.Text, true
	case MailboxEventBodySessionMessage:
		return body.Text, true
	}
	return "", false
}

// MessageLabel is the human sender label; the event retains the complete
// session identity.
func (e *MailboxEvent) MessageLabel() string {
	panic("unported: mj-core/src/mailbox.rs MailboxEvent::message_label")
}

func (e *MailboxEvent) legacyRepresentation() legacyMailboxEvent {
	text := ""
	if body, ok := e.Body.(MailboxEventBodyPlainText); ok {
		text = body.Text
	} else {
		text = RenderMailboxEvent(e)
	}
	return legacyMailboxEvent{Key: e.Key, Source: e.Source, Wake: e.Wake, Text: text, CreatedAtMs: e.CreatedAtMs}
}

func (e *MailboxEvent) HasRenderableContent() bool {
	return strings.TrimSpace(RenderMailboxEvent(e)) != ""
}

// SenderKind is the "type" tag of a Sender.
type SenderKind string

const (
	SenderSession SenderKind = "session"
	// SenderUser is no longer produced: a message from the user is now
	// submitted as the user's prompt. Kept because stored mailbox rows,
	// relay journals and transcripts written before that change still hold it.
	SenderUser SenderKind = "user"
)

// Sender is the author of a session message. A session sender carries the
// durable ID as well as its display title, so recipients can reply
// unambiguously. One struct with its "type" tag: ID and Title are set only
// for a session.
type Sender struct {
	Type  SenderKind
	ID    string
	Title string
}

func (s Sender) MarshalJSON() ([]byte, error) {
	if s.Type == SenderSession {
		return transcriptMarshalTagged("type", string(s.Type), struct {
			ID    string `json:"id"`
			Title string `json:"title"`
		}{s.ID, s.Title})
	}
	return transcriptMarshalTagged("type", string(s.Type), nil)
}

func (s *Sender) UnmarshalJSON(data []byte) error {
	tag, err := transcriptTag(data, "type", "Sender")
	if err != nil {
		return err
	}
	switch SenderKind(tag) {
	case SenderSession:
		var v struct {
			ID    string `json:"id"`
			Title string `json:"title"`
		}
		if err := transcriptStrictDecode(data, &v, true, "type"); err != nil {
			return err
		}
		*s = Sender{Type: SenderSession, ID: v.ID, Title: v.Title}
		return nil
	case SenderUser:
		if err := transcriptStrictDecode(data, &struct{}{}, true, "type"); err != nil {
			return err
		}
		*s = Sender{Type: SenderUser}
		return nil
	}
	return transcriptUnknownVariant(tag, "session", "user")
}

// SubagentFinishedReport is the state and bounded report data for one
// finished child.
type SubagentFinishedReport struct {
	// Oracle: the report is the oracle's answer, which names no session.
	Oracle         bool   `json:"oracle,omitempty"`
	ChildSessionID string `json:"child_session_id"`
	TaskName       string `json:"task_name"`
	// ChildName is the child's name as the session list shows it when the
	// report was built, bounded to one line. A person may have renamed the
	// child since it was spawned, so this can differ from TaskName. Absent in
	// reports stored before the field existed.
	ChildName *string `json:"child_name,omitempty"`
	State     string  `json:"state"`
	Finished  bool    `json:"finished"`
	Parked    *bool   `json:"parked,omitempty"`
	// Promptable: the child's last turn failed or was interrupted, but the
	// child itself can still be given work: send_message runs it again.
	// Absent in reports stored before the field existed, and for any other
	// child.
	Promptable        *bool             `json:"promptable,omitempty"`
	Failure           json.RawMessage   `json:"failure,omitempty"`
	PendingMessages   []string          `json:"pending_messages,omitempty"`
	MessageDeliveries []json.RawMessage `json:"message_deliveries,omitempty"`
	Output            *string           `json:"output"`
	ReportDir         *string           `json:"report_dir"`
	ReportSource      *string           `json:"report_source,omitempty"`
	Truncated         bool              `json:"truncated,omitempty"`
	// Facts is what the child's worker observed when the report was built:
	// the processes its agent left running and its working tree. Absent in
	// reports stored before the field existed, in the oracle's answer, and
	// in reports that are not a finish; absent means nothing was collected,
	// which is not the same as nothing running.
	Facts *WorkFacts `json:"facts,omitempty"`
}

func (r *SubagentFinishedReport) UnmarshalJSON(data []byte) error {
	type plain SubagentFinishedReport
	return transcriptStrictDecode(data, (*plain)(r), false)
}

// DisplayName is what a reader calls the child: its live name, else the
// task name it was spawned with.
func (r *SubagentFinishedReport) DisplayName() string {
	if r.ChildName != nil && strings.TrimSpace(*r.ChildName) != "" {
		return *r.ChildName
	}
	return r.TaskName
}

// MailboxEventBody is structured content for mailbox events, tagged by
// "type" with unknown fields denied. GitHub bodies contain no URL; the
// project already identifies the repository unless Repo is present for an
// ambiguous multi-repository session. Decode with DecodeMailboxEventBody.
type MailboxEventBody interface {
	isMailboxEventBody()
	// MinimumRelayProtocol is the earliest relay protocol that understands
	// this body's wire shape. Protocol 33 can translate the previously
	// shipped event bodies to legacy text, but it cannot preserve peer
	// sender identity.
	MinimumRelayProtocol() uint32
}

type MailboxEventBodyOperatorAway struct {
	Request string `json:"request"`
}

type MailboxEventBodyOperatorBack struct {
	Requests []string `json:"requests"`
}

type MailboxEventBodyReviewVerdict struct {
	ReviewID   string        `json:"review_id"`
	Repository string        `json:"repository"`
	BaseCommit string        `json:"base_commit"`
	HeadCommit string        `json:"head_commit"`
	Tree       string        `json:"tree"`
	Verdict    ReviewVerdict `json:"verdict"`
}

type MailboxEventBodySubagentFinished struct {
	Report *SubagentFinishedReport `json:"report"`
}

type MailboxEventBodyNewGithubItem struct {
	Kind   GithubItemKind `json:"kind"`
	Number uint64         `json:"number"`
	Title  string         `json:"title"`
	Repo   *string        `json:"repo,omitempty"`
}

type MailboxEventBodyGithubComment struct {
	ItemKind GithubItemKind `json:"item_kind"`
	Number   uint64         `json:"number"`
	Title    string         `json:"title"`
	Author   string         `json:"author"`
	Body     string         `json:"body"`
	Repo     *string        `json:"repo,omitempty"`
}

type MailboxEventBodyGithubReview struct {
	ItemKind    GithubItemKind `json:"item_kind"`
	Number      uint64         `json:"number"`
	Title       string         `json:"title"`
	Author      string         `json:"author"`
	Body        string         `json:"body"`
	ReviewState *string        `json:"review_state,omitempty"`
	Repo        *string        `json:"repo,omitempty"`
}

type MailboxEventBodyGithubReviewComment struct {
	ItemKind GithubItemKind `json:"item_kind"`
	Number   uint64         `json:"number"`
	Title    string         `json:"title"`
	Author   string         `json:"author"`
	Body     string         `json:"body"`
	Repo     *string        `json:"repo,omitempty"`
}

// MailboxEventBodyGithubPullRequestLifecycle omits Recipient when it is the
// creator.
type MailboxEventBodyGithubPullRequestLifecycle struct {
	Change    MailboxPullRequestChange `json:"change"`
	Number    uint64                   `json:"number"`
	Title     string                   `json:"title"`
	Actor     string                   `json:"actor"`
	Recipient LifecycleRecipient       `json:"recipient,omitempty"`
	Repo      *string                  `json:"repo,omitempty"`
}

type MailboxEventBodyParentMessage struct {
	Text string `json:"text"`
}

type MailboxEventBodySessionMessage struct {
	From Sender `json:"from"`
	Text string `json:"text"`
}

type MailboxEventBodyPlainText struct {
	Text string `json:"text"`
}

func (MailboxEventBodyOperatorAway) isMailboxEventBody()               {}
func (MailboxEventBodyOperatorBack) isMailboxEventBody()               {}
func (MailboxEventBodyReviewVerdict) isMailboxEventBody()              {}
func (MailboxEventBodySubagentFinished) isMailboxEventBody()           {}
func (MailboxEventBodyNewGithubItem) isMailboxEventBody()              {}
func (MailboxEventBodyGithubComment) isMailboxEventBody()              {}
func (MailboxEventBodyGithubReview) isMailboxEventBody()               {}
func (MailboxEventBodyGithubReviewComment) isMailboxEventBody()        {}
func (MailboxEventBodyGithubPullRequestLifecycle) isMailboxEventBody() {}
func (MailboxEventBodyParentMessage) isMailboxEventBody()              {}
func (MailboxEventBodySessionMessage) isMailboxEventBody()             {}
func (MailboxEventBodyPlainText) isMailboxEventBody()                  {}

func (MailboxEventBodyOperatorAway) MinimumRelayProtocol() uint32 {
	return RelayOperatorAwayProtocol
}

func (MailboxEventBodyOperatorBack) MinimumRelayProtocol() uint32 {
	return RelayOperatorAwayProtocol
}

func (MailboxEventBodyReviewVerdict) MinimumRelayProtocol() uint32 {
	return RelayPushReviewProtocol
}

func (b MailboxEventBodySubagentFinished) MinimumRelayProtocol() uint32 {
	switch {
	// A worker before 55 does not render observed work facts, so a report
	// that carries them reaches it as text rendered here.
	case b.Report.Facts != nil:
		return RelayFinishFactsProtocol
	// A worker before 54 heads every report "finished" and does not know
	// promptable, so such a report reaches it rendered here.
	case !b.Report.Finished || b.Report.Promptable != nil:
		return RelaySubagentReportStateProtocol
	}
	return RelaySubagentFinishedProtocol
}

func (MailboxEventBodyNewGithubItem) MinimumRelayProtocol() uint32 {
	return RelayLegacyMailboxProtocol
}

func (MailboxEventBodyGithubComment) MinimumRelayProtocol() uint32 {
	return RelayLegacyMailboxProtocol
}

func (MailboxEventBodyGithubReview) MinimumRelayProtocol() uint32 {
	return RelayLegacyMailboxProtocol
}

func (MailboxEventBodyGithubReviewComment) MinimumRelayProtocol() uint32 {
	return RelayLegacyMailboxProtocol
}

func (b MailboxEventBodyGithubPullRequestLifecycle) MinimumRelayProtocol() uint32 {
	if b.Recipient == LifecycleRecipientInterested {
		return RelayFollowedLifecycleProtocol
	}
	return RelayLegacyMailboxProtocol
}

func (MailboxEventBodyParentMessage) MinimumRelayProtocol() uint32 {
	return RelayStructuredMailboxProtocol
}

func (MailboxEventBodySessionMessage) MinimumRelayProtocol() uint32 {
	return RelaySessionMessageProtocol
}

func (MailboxEventBodyPlainText) MinimumRelayProtocol() uint32 {
	return RelayLegacyMailboxProtocol
}

func (b MailboxEventBodyOperatorAway) MarshalJSON() ([]byte, error) {
	type plain MailboxEventBodyOperatorAway
	return transcriptMarshalTagged("type", "operator_away", plain(b))
}

func (b MailboxEventBodyOperatorBack) MarshalJSON() ([]byte, error) {
	type plain MailboxEventBodyOperatorBack
	return transcriptMarshalTagged("type", "operator_back", plain(b))
}

func (b MailboxEventBodyReviewVerdict) MarshalJSON() ([]byte, error) {
	type plain MailboxEventBodyReviewVerdict
	return transcriptMarshalTagged("type", "review_verdict", plain(b))
}

func (b MailboxEventBodySubagentFinished) MarshalJSON() ([]byte, error) {
	type plain MailboxEventBodySubagentFinished
	return transcriptMarshalTagged("type", "subagent_finished", plain(b))
}

func (b MailboxEventBodyNewGithubItem) MarshalJSON() ([]byte, error) {
	type plain MailboxEventBodyNewGithubItem
	return transcriptMarshalTagged("type", "new_github_item", plain(b))
}

func (b MailboxEventBodyGithubComment) MarshalJSON() ([]byte, error) {
	type plain MailboxEventBodyGithubComment
	return transcriptMarshalTagged("type", "github_comment", plain(b))
}

func (b MailboxEventBodyGithubReview) MarshalJSON() ([]byte, error) {
	type plain MailboxEventBodyGithubReview
	return transcriptMarshalTagged("type", "github_review", plain(b))
}

func (b MailboxEventBodyGithubReviewComment) MarshalJSON() ([]byte, error) {
	type plain MailboxEventBodyGithubReviewComment
	return transcriptMarshalTagged("type", "github_review_comment", plain(b))
}

func (b MailboxEventBodyGithubPullRequestLifecycle) MarshalJSON() ([]byte, error) {
	type plain MailboxEventBodyGithubPullRequestLifecycle
	v := plain(b)
	if v.Recipient.IsCreator() {
		v.Recipient = ""
	}
	return transcriptMarshalTagged("type", "github_pull_request_lifecycle", v)
}

func (b MailboxEventBodyParentMessage) MarshalJSON() ([]byte, error) {
	type plain MailboxEventBodyParentMessage
	return transcriptMarshalTagged("type", "parent_message", plain(b))
}

func (b MailboxEventBodySessionMessage) MarshalJSON() ([]byte, error) {
	type plain MailboxEventBodySessionMessage
	return transcriptMarshalTagged("type", "session_message", plain(b))
}

func (b MailboxEventBodyPlainText) MarshalJSON() ([]byte, error) {
	type plain MailboxEventBodyPlainText
	return transcriptMarshalTagged("type", "plain_text", plain(b))
}

// DecodeMailboxEventBody reads a MailboxEventBody, tagged by "type",
// denying unknown fields.
func DecodeMailboxEventBody(data []byte) (MailboxEventBody, error) {
	tag, err := transcriptTag(data, "type", "MailboxEventBody")
	if err != nil {
		return nil, err
	}
	switch tag {
	case "operator_away":
		type plain MailboxEventBodyOperatorAway
		var v plain
		err = transcriptStrictDecode(data, &v, true, "type")
		return MailboxEventBodyOperatorAway(v), err
	case "operator_back":
		type plain MailboxEventBodyOperatorBack
		var v plain
		err = transcriptStrictDecode(data, &v, true, "type")
		return MailboxEventBodyOperatorBack(v), err
	case "review_verdict":
		// Verdict is a sealed interface, so it is read raw and decoded by
		// its own decoder.
		var v struct {
			ReviewID   string          `json:"review_id"`
			Repository string          `json:"repository"`
			BaseCommit string          `json:"base_commit"`
			HeadCommit string          `json:"head_commit"`
			Tree       string          `json:"tree"`
			Verdict    json.RawMessage `json:"verdict"`
		}
		if err = transcriptStrictDecode(data, &v, true, "type"); err != nil {
			return nil, err
		}
		verdict, err := UnmarshalReviewVerdict(v.Verdict)
		if err != nil {
			return nil, err
		}
		return MailboxEventBodyReviewVerdict{
			ReviewID:   v.ReviewID,
			Repository: v.Repository,
			BaseCommit: v.BaseCommit,
			HeadCommit: v.HeadCommit,
			Tree:       v.Tree,
			Verdict:    verdict,
		}, nil
	case "subagent_finished":
		type plain MailboxEventBodySubagentFinished
		var v plain
		err = transcriptStrictDecode(data, &v, true, "type")
		return MailboxEventBodySubagentFinished(v), err
	case "new_github_item":
		type plain MailboxEventBodyNewGithubItem
		var v plain
		err = transcriptStrictDecode(data, &v, true, "type")
		return MailboxEventBodyNewGithubItem(v), err
	case "github_comment":
		type plain MailboxEventBodyGithubComment
		var v plain
		err = transcriptStrictDecode(data, &v, true, "type")
		return MailboxEventBodyGithubComment(v), err
	case "github_review":
		type plain MailboxEventBodyGithubReview
		var v plain
		err = transcriptStrictDecode(data, &v, true, "type")
		return MailboxEventBodyGithubReview(v), err
	case "github_review_comment":
		type plain MailboxEventBodyGithubReviewComment
		var v plain
		err = transcriptStrictDecode(data, &v, true, "type")
		return MailboxEventBodyGithubReviewComment(v), err
	case "github_pull_request_lifecycle":
		type plain MailboxEventBodyGithubPullRequestLifecycle
		var v plain
		err = transcriptStrictDecode(data, &v, true, "type")
		if v.Recipient == "" {
			v.Recipient = LifecycleRecipientCreator
		}
		return MailboxEventBodyGithubPullRequestLifecycle(v), err
	case "parent_message":
		type plain MailboxEventBodyParentMessage
		var v plain
		err = transcriptStrictDecode(data, &v, true, "type")
		return MailboxEventBodyParentMessage(v), err
	case "session_message":
		type plain MailboxEventBodySessionMessage
		var v plain
		err = transcriptStrictDecode(data, &v, true, "type")
		return MailboxEventBodySessionMessage(v), err
	case "plain_text":
		type plain MailboxEventBodyPlainText
		var v plain
		err = transcriptStrictDecode(data, &v, true, "type")
		return MailboxEventBodyPlainText(v), err
	}
	return nil, transcriptUnknownVariant(tag, "operator_away", "operator_back", "review_verdict",
		"subagent_finished", "new_github_item", "github_comment", "github_review",
		"github_review_comment", "github_pull_request_lifecycle", "parent_message",
		"session_message", "plain_text")
}

// MailboxPullRequestChange is a lifecycle change that the GitHub watcher
// reports for a session's PR.
type MailboxPullRequestChange string

const (
	MailboxPullRequestChangeMerged               MailboxPullRequestChange = "merged"
	MailboxPullRequestChangeClosedWithoutMerging MailboxPullRequestChange = "closed_without_merging"
	MailboxPullRequestChangeReopened             MailboxPullRequestChange = "reopened"
)

// LifecycleRecipient defaults to the creator.
type LifecycleRecipient string

const (
	LifecycleRecipientCreator    LifecycleRecipient = "creator"
	LifecycleRecipientInterested LifecycleRecipient = "interested"
)

// IsCreator also reads the zero value as the default, the creator.
func (r LifecycleRecipient) IsCreator() bool {
	return r == LifecycleRecipientCreator || r == ""
}

// MailboxEventDescription is the shared, human-readable account of an
// event's type and short description. The agent renderer and transcript
// notice both use this mapping.
type MailboxEventDescription struct {
	// Header is the agent-facing event header.
	Header string
	// Body is the event content, if the event type carries a body.
	Body *string
	// Context is extra classifier context displayed after a new-item header.
	Context *string
	// TranscriptLine is a one-line human transcript summary without source
	// or URL repetition.
	TranscriptLine string
}

// RenderMailboxEvent is the plain text form used when a protocol-33 worker
// cannot receive a structured event body. The caller still serializes this
// as the old text field.
func RenderMailboxEvent(event *MailboxEvent) string {
	panic("unported: mj-core/src/mailbox.rs render_mailbox_event")
}

// RenderMailboxEvents renders daemon-authored messages first, followed by
// external content in its untrusted-data wrapper. Session messages are
// outside that wrapper so their sender label stays attached; peer text is
// still escaped and states that it carries no user authority.
func RenderMailboxEvents(events []MailboxEvent) string {
	panic("unported: mj-core/src/mailbox.rs render_mailbox_events")
}

// DescribeMailboxEvent describes one event once so agent text and human
// transcript summaries keep the same event type, title, actor, and item
// reference.
func DescribeMailboxEvent(event *MailboxEvent) MailboxEventDescription {
	panic("unported: mj-core/src/mailbox.rs describe_mailbox_event")
}

func isSessionMessage(event *MailboxEvent) bool {
	switch event.Body.(type) {
	case MailboxEventBodyParentMessage, MailboxEventBodySessionMessage,
		MailboxEventBodySubagentFinished, MailboxEventBodyReviewVerdict:
		return true
	case MailboxEventBodyPlainText:
		return event.Source == "review"
	}
	return false
}

func quotedTitle(title string) string {
	encoded, err := transcriptMarshal(title)
	if err != nil {
		panic("a string title always serializes")
	}
	return string(encoded)
}

func itemKindPhrase(kind GithubItemKind) string {
	if kind == GithubItemKindIssue {
		return "issue"
	}
	return "PR"
}

func itemReference(repo *string, number uint64) string {
	prefix := ""
	if repo != nil {
		prefix = escapeUntrusted(*repo)
	}
	return fmt.Sprintf("%s#%d", prefix, number)
}

func displayReviewState(state string) string {
	return strings.ReplaceAll(strings.ToLower(state), "_", " ")
}

func quotedBody(body string) []string {
	lines := []string{}
	for _, line := range mailboxLines(body) {
		lines = append(lines, "  > "+line)
	}
	if body == "" {
		lines = append(lines, "  > (empty)")
	}
	return lines
}

func escapeUntrusted(text string) string {
	return strings.ReplaceAll(strings.ReplaceAll(text, "<", "‹"), ">", "›")
}

func shortFirstLine(text string, limit int) string {
	first := ""
	if lines := mailboxLines(text); len(lines) > 0 {
		first = strings.TrimSpace(lines[0])
	}
	var shortened []rune
	more := false
	for _, character := range first {
		if unicode.IsControl(character) {
			continue
		}
		if len(shortened) == limit {
			more = true
			break
		}
		shortened = append(shortened, character)
	}
	if more {
		shortened = append(shortened, '…')
	}
	if len(shortened) == 0 {
		return "(empty)"
	}
	return string(shortened)
}

// mailboxLines splits text as Rust's str::lines does: at "\n", dropping a
// "\r" before it, with no empty line after a final newline.
func mailboxLines(text string) []string {
	if text == "" {
		return nil
	}
	lines := strings.Split(strings.TrimSuffix(text, "\n"), "\n")
	for i, line := range lines {
		lines[i] = strings.TrimSuffix(line, "\r")
	}
	return lines
}

// MailboxDeliveryPath is the worker path that made mailbox events visible
// to the agent.
type MailboxDeliveryPath string

const (
	MailboxDeliveryPathToolHook MailboxDeliveryPath = "tool_hook"
	MailboxDeliveryPathPrompt   MailboxDeliveryPath = "prompt"
	MailboxDeliveryPathWake     MailboxDeliveryPath = "wake"
)

// MailboxDeliveryMode is when a mailbox message reaches a recipient that is
// already working.
//
// The recipient's worker decides this once and publishes it in its
// operational state; senders and the daemon read that fact and never derive
// it from configuration themselves.
type MailboxDeliveryMode string

const (
	// MailboxDeliveryModeToolBoundary: the message arrives at the
	// recipient's next tool boundary, without ending the turn.
	MailboxDeliveryModeToolBoundary MailboxDeliveryMode = "tool_boundary"
	// MailboxDeliveryModeTurnBoundary: the message waits for the current
	// turn to end, then arrives with the next prompt or starts a wake turn.
	MailboxDeliveryModeTurnBoundary MailboxDeliveryMode = "turn_boundary"
)

// MailboxDeliveryModeForCodexProvider is what a Codex session on provider
// accepts. Nil is Codex's default OpenAI provider.
//
// Codex records a PostToolUse hook's context as a developer message between
// a tool call and its output. DeepSeek's chat API rejects any history
// holding a tool call that its output does not immediately follow, so one
// mid-turn delivery poisons every later request of the session (#1277). A
// provider pointing at DeepSeek's service therefore waits for the turn
// boundary; other providers retain tool-boundary delivery, since no
// rejection has been observed for them. The provider is recognized by its
// host through CodexProvider.Kind, the same classification quota and
// utility calls use.
func MailboxDeliveryModeForCodexProvider(provider *CodexProvider) MailboxDeliveryMode {
	if provider != nil && provider.Kind() == CodexProviderKindDeepSeek {
		return MailboxDeliveryModeTurnBoundary
	}
	return MailboxDeliveryModeToolBoundary
}
