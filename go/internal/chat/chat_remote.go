// Port of mj-chat/src/chat/remote.rs.
//
// The chat's background command channel: every relay operation the view can
// ask for, the task that runs them, and how their results land back in
// state.
//
// Rust's `Result<(), String>` payloads become an `Err error` field (nil is
// Ok); a `Result<T, String>` adds the Ok value beside it (Ordinal).
package chat

import (
	"bytes"
	"context"
	"encoding/json"
	"log/slog"
	"strings"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// reviewerAnswerKey is the feedback key of a reviewer answer. Feedback keys
// are constructed here; harness request IDs remain opaque.
func reviewerAnswerKey(role *string, request *core.ElicitationRequest) string {
	var encoded bytes.Buffer
	encoder := json.NewEncoder(&encoded)
	// serde_json writes `<`, `>` and `&` as they are.
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode([]any{role, request.ID}); err != nil {
		panic(err)
	}
	return "reviewer-answer:" + strings.TrimSuffix(encoded.String(), "\n")
}

func (s *ChatState) deferredFormResultDrafts() []ChatElicitationDraft {
	panic("unported: mj-chat/src/chat/remote.rs ChatState::deferred_form_result_drafts")
}

func (s *ChatState) applyDeferredFormResults() {
	// One pass: a result waiting for another visible form is requeued.
	results := s.deferredFormResults
	s.deferredFormResults = nil
	for _, result := range results {
		applyChatRemoteResult(s, result)
	}
}

func (s *ChatState) hasPendingElicitationAnswers() bool {
	for key := range s.operationFeedback {
		if strings.HasPrefix(key, "reviewer-answer:") || strings.HasPrefix(key, "answer:") {
			return true
		}
	}
	return false
}

func (s *ChatState) reviewerAnswerPending(role *string, request *core.ElicitationRequest) bool {
	_, ok := s.operationFeedback[reviewerAnswerKey(role, request)]
	return ok
}

func (s *ChatState) reportReviewRefusal(message string) {
	s.reportTurnReviewFailure(message)
}

func finishReviewerAnswer(chat *ChatState, draft *ChatElicitationDraft, result error) {
	panic("unported: mj-chat/src/chat/remote.rs finish_reviewer_answer")
}

const chatRemoteQueueCapacity = 32

const sessionActorReplacementWait = 5 * time.Second

// chatRemoteOperation is one relay operation the view asked for.
type chatRemoteOperation interface{ isChatRemoteOperation() }

type chatRemoteOperationSync struct{}

type chatRemoteOperationRecordNotice struct {
	ID   string
	Text string
}

type chatRemoteOperationPrompt struct {
	CommandID string
	Text      string
	Images    []PromptImage
}

type chatRemoteOperationRunShell struct {
	CommandID string
	Command   string
}

type chatRemoteOperationRemoveQueuedPrompt struct {
	CommandID string
	ID        string
	Text      string
	Kind      core.QueuedCommandKind
}

type chatRemoteOperationStopBackgroundTask struct {
	ID string
}

type chatRemoteOperationGoalControl struct {
	CommandID string
	Action    core.GoalControlAction
}

type chatRemoteOperationSetConfig struct {
	CommandID string
	Key       string
	Value     string
}

type chatRemoteOperationPlanCommand struct {
	CommandID       string
	Original        string
	Control         core.PlanControl
	RequestedActive bool
	Prompt          *string
}

type chatRemoteOperationCancel struct {
	CommandID   string
	Intent      TurnControlIntent
	CancelAgent bool
	// Command is nil for the default CancelTurn.
	Command         core.RelayCommand
	ShellCommandIDs []string
}

type chatRemoteOperationReviewerAnswer struct {
	Role      string
	RequestID string
	Response  core.ElicitationResponse
	Draft     *ChatElicitationDraft
}

type chatRemoteOperationRespondElicitation struct {
	Request      core.ElicitationRequest
	Response     core.ElicitationResponse
	PlanFollowup *PlanReviewFollowup
}

func (chatRemoteOperationSync) isChatRemoteOperation()               {}
func (chatRemoteOperationRecordNotice) isChatRemoteOperation()       {}
func (chatRemoteOperationPrompt) isChatRemoteOperation()             {}
func (chatRemoteOperationRunShell) isChatRemoteOperation()           {}
func (chatRemoteOperationRemoveQueuedPrompt) isChatRemoteOperation() {}
func (chatRemoteOperationStopBackgroundTask) isChatRemoteOperation() {}
func (chatRemoteOperationGoalControl) isChatRemoteOperation()        {}
func (chatRemoteOperationSetConfig) isChatRemoteOperation()          {}
func (chatRemoteOperationPlanCommand) isChatRemoteOperation()        {}
func (chatRemoteOperationCancel) isChatRemoteOperation()             {}
func (chatRemoteOperationReviewerAnswer) isChatRemoteOperation()     {}
func (chatRemoteOperationRespondElicitation) isChatRemoteOperation() {}

// chatRemoteOperationFeedback is the progress row an operation shows while
// it runs: its feedback key and text (ChatRemoteOperation::feedback).
func chatRemoteOperationFeedback(operation chatRemoteOperation) (key, text string, ok bool) {
	panic("unported: mj-chat/src/chat/remote.rs ChatRemoteOperation::feedback")
}

// chatRemoteResult is how a remote operation ended.
type chatRemoteResult interface{ isChatRemoteResult() }

type chatRemoteResultSync struct {
	Err error
}

type chatRemoteResultNoticeRecorded struct {
	ID  string
	Err error
}

type chatRemoteResultPrompt struct {
	CommandID string
	Text      string
	Images    []PromptImage
	Ordinal   uint64
	Err       error
}

type chatRemoteResultRunShell struct {
	CommandID string
	Command   string
	Ordinal   uint64
	Err       error
}

type chatRemoteResultRemoveQueuedPrompt struct {
	ID   string
	Text string
	Kind core.QueuedCommandKind
	Err  error
}

type chatRemoteResultStopBackgroundTask struct {
	ID  string
	Err error
}

type chatRemoteResultGoalControl struct {
	Action core.GoalControlAction
	Err    error
}

type chatRemoteResultSetConfig struct {
	Key   string
	Value string
	Err   error
}

type chatRemoteResultPlanCommand struct {
	CommandID       string
	Original        string
	RequestedActive bool
	ControlApplied  bool
	// Ordinal is the follow-up prompt's ordinal, nil when there was none.
	Ordinal *uint64
	Err     error
}

type chatRemoteResultCancel struct {
	Intent TurnControlIntent
	Err    error
}

type chatRemoteResultReviewerAnswer struct {
	Draft *ChatElicitationDraft
	Err   error
}

type chatRemoteResultRespondElicitation struct {
	Request           core.ElicitationRequest
	DesiredPlanActive *bool
	Answered          bool
	Err               error
}

type chatRemoteResultDeliveryUnconfirmed struct {
	CommandID string
	Error     string
}

type chatRemoteResultFollowupUnconfirmed struct {
	CommandID         string
	FeedbackKey       string
	DesiredPlanActive *bool
	PlanCommand       bool
	Error             string
}

type chatRemoteResultWorkerFailed struct {
	Error string
}

func (chatRemoteResultSync) isChatRemoteResult()                {}
func (chatRemoteResultNoticeRecorded) isChatRemoteResult()      {}
func (chatRemoteResultPrompt) isChatRemoteResult()              {}
func (chatRemoteResultRunShell) isChatRemoteResult()            {}
func (chatRemoteResultRemoveQueuedPrompt) isChatRemoteResult()  {}
func (chatRemoteResultStopBackgroundTask) isChatRemoteResult()  {}
func (chatRemoteResultGoalControl) isChatRemoteResult()         {}
func (chatRemoteResultSetConfig) isChatRemoteResult()           {}
func (chatRemoteResultPlanCommand) isChatRemoteResult()         {}
func (chatRemoteResultCancel) isChatRemoteResult()              {}
func (chatRemoteResultReviewerAnswer) isChatRemoteResult()      {}
func (chatRemoteResultRespondElicitation) isChatRemoteResult()  {}
func (chatRemoteResultDeliveryUnconfirmed) isChatRemoteResult() {}
func (chatRemoteResultFollowupUnconfirmed) isChatRemoteResult() {}
func (chatRemoteResultWorkerFailed) isChatRemoteResult()        {}

// chatRemoteResultFeedbackKey is the feedback row a result clears
// (ChatRemoteResult::feedback_key).
func chatRemoteResultFeedbackKey(result chatRemoteResult) (string, bool) {
	panic("unported: mj-chat/src/chat/remote.rs ChatRemoteResult::feedback_key")
}

// chatRemoteResultFailureMessage is the failure a result reports, if it
// failed (ChatRemoteResult::failure_message).
func chatRemoteResultFailureMessage(result chatRemoteResult) (string, bool) {
	panic("unported: mj-chat/src/chat/remote.rs ChatRemoteResult::failure_message")
}

func publishChatRemoteResult(results *core.Unbounded[chatRemoteResult], attached *atomic.Bool, result chatRemoteResult) {
	if !attached.Load() {
		if message, ok := chatRemoteResultFailureMessage(result); ok {
			slog.Error("detached chat operation failed", "error", message)
		}
		return
	}
	if err := results.Send(result); err != nil {
		if message, ok := chatRemoteResultFailureMessage(result); ok {
			slog.Error("chat operation failed after its UI closed", "error", message)
		}
	}
}

// chatRemoteOperationSender is the sending side of the bounded operation
// queue (tokio's mpsc::Sender). A slot is reserved before the caller commits
// to sending, as Sender::try_reserve does, so a reservation is a claim on
// capacity that a later send cannot lose.
type chatRemoteOperationSender struct {
	operations chan chatRemoteOperation
	// slots holds one token per queued or reserved operation; the receiver
	// takes a token back for each operation it receives.
	slots chan struct{}
}

// chatRemoteOperationReceiver is the worker's side of the operation queue.
type chatRemoteOperationReceiver struct {
	operations <-chan chatRemoteOperation
	slots      <-chan struct{}
}

// chatRemoteOperationPermit is one reserved slot of the operation queue.
type chatRemoteOperationPermit struct {
	sender *chatRemoteOperationSender
}

// newChatRemoteOperationQueue is a bounded operation queue holding up to
// capacity operations (tokio::sync::mpsc::channel).
func newChatRemoteOperationQueue(capacity int) (*chatRemoteOperationSender, chatRemoteOperationReceiver) {
	operations := make(chan chatRemoteOperation, capacity)
	slots := make(chan struct{}, capacity)
	return &chatRemoteOperationSender{operations: operations, slots: slots},
		chatRemoteOperationReceiver{operations: operations, slots: slots}
}

// tryReserve claims a slot, or reports the queue full.
func (s *chatRemoteOperationSender) tryReserve() (chatRemoteOperationPermit, bool) {
	select {
	case s.slots <- struct{}{}:
		return chatRemoteOperationPermit{sender: s}, true
	default:
		return chatRemoteOperationPermit{}, false
	}
}

// close drops the sender: the worker stops accepting once it has drained
// the queue.
func (s *chatRemoteOperationSender) close() { close(s.operations) }

// send queues operation in the reserved slot; it never blocks.
func (p chatRemoteOperationPermit) send(operation chatRemoteOperation) {
	p.sender.operations <- operation
}

// recv waits for the next operation; false once the sender is closed and the
// queue drained, or ctx ended.
func (r chatRemoteOperationReceiver) recv(ctx context.Context) (chatRemoteOperation, bool) {
	select {
	case operation, ok := <-r.operations:
		if ok {
			<-r.slots
		}
		return operation, ok
	case <-ctx.Done():
		return nil, false
	}
}

// remoteResults is the chat's receiving end of the result feed.
type remoteResults struct {
	receiver *core.Unbounded[chatRemoteResult]
	attached *atomic.Bool
}

// close detaches the feed and logs the failures nobody will see (Rust's
// Drop).
func (r *remoteResults) close() {
	r.attached.Store(false)
	r.receiver.Close()
	for {
		result, ok := r.receiver.Recv(context.Background())
		if !ok {
			return
		}
		if message, failed := chatRemoteResultFailureMessage(result); failed {
			slog.Error("chat operation failed while detaching", "error", message)
		}
	}
}

// chatRemoteSupervisor owns the chat's background worker: the operation
// queue into it and the result feed out of it.
type chatRemoteSupervisor struct {
	// operationSender is nil once the supervisor is closed (Rust's field
	// `operations`, renamed because the method takes the name).
	operationSender *chatRemoteOperationSender
	// results is nil once transferred or closed.
	results   *remoteResults
	resultsTx *core.Unbounded[chatRemoteResult]
	attached  *atomic.Bool
	// worker is nil once taken.
	worker *core.JoinHandle[struct{}]
}

// spawnChatRemoteSupervisor starts the background worker for session
// (ChatRemoteSupervisor::spawn).
func spawnChatRemoteSupervisor(session client.SessionHandle, sessionManager client.SessionControl) *chatRemoteSupervisor {
	panic("unported: mj-chat/src/chat/remote.rs ChatRemoteSupervisor::spawn")
}

func (s *chatRemoteSupervisor) operations() *chatRemoteOperationSender {
	if s.operationSender == nil {
		panic("chat remote supervisor is attached")
	}
	return s.operationSender
}

// remoteTryRecvContext makes a receive return at once: what is queued, or nothing.
var remoteTryRecvContext = func() context.Context {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	return ctx
}()

// tryRecv takes the next result if one is waiting.
func (s *chatRemoteSupervisor) tryRecv() (chatRemoteResult, bool) {
	if s.results == nil {
		return nil, false
	}
	return s.results.receiver.Recv(remoteTryRecvContext)
}

// recv waits for the next result. false means the worker is gone and no
// further result can arrive (or ctx ended), so the caller must stop awaiting
// this feed. Cancel safe: an unfinished recv takes no message.
func (s *chatRemoteSupervisor) recv(ctx context.Context) (chatRemoteResult, bool) {
	if s.results == nil {
		<-ctx.Done()
		return nil, false
	}
	return s.results.receiver.Recv(ctx)
}

// transferResults hands the result feed to the composer: the receiver
// follows accepted work, even when the worker-facing view is dropped.
func (s *chatRemoteSupervisor) transferResults(updates *core.Unbounded[composerIoUpdate]) *remoteResultForwarder {
	panic("unported: mj-chat/src/chat/remote.rs ChatRemoteSupervisor::transfer_results")
}

// takeFinished joins the worker once it has finished: false while it still
// runs (or was already taken), and its failure if it failed.
func (s *chatRemoteSupervisor) takeFinished() (bool, error) {
	if s.worker == nil {
		return false, nil
	}
	select {
	case <-s.worker.Done():
	default:
		return false, nil
	}
	worker := s.worker
	s.worker = nil
	_, err := worker.Join(context.Background())
	return true, err
}

// close detaches the view from its worker, which finishes accepted work and
// reports a failure through the feed if anyone still listens (Rust's Drop).
func (s *chatRemoteSupervisor) close() {
	panic("unported: mj-chat/src/chat/remote.rs ChatRemoteSupervisor::drop")
}

// remoteTasks is the worker's JoinSet: the operations it has started and not
// yet joined. A task that panics is reported, never dropped.
type remoteTasks struct {
	// finished receives one value per finished task: nil, or the failure of
	// a task that panicked.
	finished chan error
	running  int
}

func newRemoteTasks() *remoteTasks { return &remoteTasks{finished: make(chan error)} }

func (t *remoteTasks) isEmpty() bool { return t.running == 0 }

func (t *remoteTasks) spawn(task func()) {
	t.running++
	handle := core.Spawn(func() struct{} { task(); return struct{}{} })
	go func() {
		_, err := handle.Join(context.Background())
		t.finished <- err
	}()
}

func runChatRemoteWorker(
	ctx context.Context,
	session client.SessionHandle,
	sessionManager client.SessionControl,
	operations chatRemoteOperationReceiver,
	results *core.Unbounded[chatRemoteResult],
	attached *atomic.Bool,
) {
	panic("unported: mj-chat/src/chat/remote.rs run_chat_remote_worker")
}

func enqueueChatRemoteOperation(
	ctx context.Context,
	session client.SessionHandle,
	sessionManager client.SessionControl,
	operation chatRemoteOperation,
	pending *remoteTasks,
	results *core.Unbounded[chatRemoteResult],
	attached *atomic.Bool,
) {
	panic("unported: mj-chat/src/chat/remote.rs enqueue_chat_remote_operation")
}

func restoreUnsentInput(chat *ChatState, input string) {
	restoreUnsentPrompt(chat, input, nil)
}

func restoreUnsentPrompt(chat *ChatState, text string, images []PromptImage) {
	panic("unported: mj-chat/src/chat/remote.rs restore_unsent_prompt")
}

// reportRefusedSubmission reports a submit the relay refused as a dated
// notice in the conversation. The composer already holds the text for a
// retry; the notice names it for someone who was away when it failed, since
// its echoed row is gone.
func reportRefusedSubmission(chat *ChatState, kind SubmissionKind, text, err string) {
	notice := kind.notSentHeadline() + ": " + err
	if preview := queuedPromptPreview(text); preview != "" {
		notice += "\n" + preview
	}
	chat.conversationNotice(notice)
}

// refusedSlashCommand is the command word of a one-line slash command such
// as `/clear`.
func refusedSlashCommand(text string, images []PromptImage) (string, bool) {
	text = strings.TrimSpace(text)
	if len(images) > 0 || strings.Contains(text, "\n") || !strings.HasPrefix(text, "/") {
		return "", false
	}
	fields := strings.Fields(text)
	if len(fields) == 0 {
		return "", false
	}
	return fields[0], true
}

func applyChatRemoteResult(chat *ChatState, result chatRemoteResult) {
	panic("unported: mj-chat/src/chat/remote.rs apply_chat_remote_result")
}

func queueChatRemoteOperation(operations *chatRemoteOperationSender, operation chatRemoteOperation, chat *ChatState) {
	panic("unported: mj-chat/src/chat/remote.rs queue_chat_remote_operation")
}
