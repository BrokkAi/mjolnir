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
	"errors"
	"fmt"
	"log/slog"
	"slices"
	"strings"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/acp-go/schema"
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
	var drafts []ChatElicitationDraft
	for _, result := range s.deferredFormResults {
		answer, ok := result.(chatRemoteResultReviewerAnswer)
		if !ok || answer.Draft == nil || answer.Err == nil {
			continue
		}
		message := answer.Err.Error()
		if strings.Contains(message, "is no longer pending") {
			continue
		}
		draft := *answer.Draft
		draft.form = answer.Draft.form.clone()
		if answer.Draft.reviewerRole != nil {
			role := *answer.Draft.reviewerRole
			draft.reviewerRole = &role
		}
		draft.form.setResponseError("Answer was not sent: " + message)
		drafts = append(drafts, draft)
	}
	return drafts
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
	if result == nil {
		return
	}
	message := result.Error()
	if strings.Contains(message, "is no longer pending") {
		if draft != nil {
			chat.conversationNotice("The reviewer's request had already ended (answered elsewhere, withdrawn, or expired), so this answer was not delivered.")
		}
		return
	}
	if draft != nil && !chat.restoreFailedReviewerElicitation(*draft, message) {
		return
	}
	slog.Warn("reviewer answer failed and was shown in the UI", "session_id", chat.sessionID, "error", message)
	chat.reportReviewRefusal(message)
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
	switch operation := operation.(type) {
	case chatRemoteOperationRemoveQueuedPrompt:
		return "remove:" + operation.ID, "Removing queued prompt…", true
	case chatRemoteOperationGoalControl:
		return "goal:" + operation.Action.AsStr(), "Updating goal…", true
	case chatRemoteOperationSetConfig:
		return "config:" + operation.Key, "Changing " + operation.Key + "…", true
	case chatRemoteOperationPlanCommand:
		return "plan:" + operation.CommandID, "Changing plan mode…", true
	case chatRemoteOperationCancel:
		text := "Interrupting turn…"
		switch operation.Intent {
		case TurnControlIntentSteer:
			text = "Steering turn…"
		case TurnControlIntentApplyQueued:
			text = "Applying queued input…"
		}
		return "turn-control", text, true
	case chatRemoteOperationReviewerAnswer:
		if operation.Draft != nil {
			return reviewerAnswerKey(operation.Draft.reviewerRole, operation.Draft.request()), "Sending reviewer answer…", true
		}
	case chatRemoteOperationRespondElicitation:
		return "answer:" + operation.Request.ID, "Sending answer…", true
	}
	return "", "", false
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
	switch result := result.(type) {
	case chatRemoteResultFollowupUnconfirmed:
		return result.FeedbackKey, true
	case chatRemoteResultRemoveQueuedPrompt:
		return "remove:" + result.ID, true
	case chatRemoteResultGoalControl:
		return "goal:" + result.Action.AsStr(), true
	case chatRemoteResultSetConfig:
		return "config:" + result.Key, true
	case chatRemoteResultPlanCommand:
		return "plan:" + result.CommandID, true
	case chatRemoteResultCancel:
		return "turn-control", true
	case chatRemoteResultReviewerAnswer:
		if result.Draft != nil {
			return reviewerAnswerKey(result.Draft.reviewerRole, result.Draft.request()), true
		}
	case chatRemoteResultRespondElicitation:
		return "answer:" + result.Request.ID, true
	}
	return "", false
}

// chatRemoteResultFailureMessage is the failure a result reports, if it
// failed (ChatRemoteResult::failure_message).
func chatRemoteResultFailureMessage(result chatRemoteResult) (string, bool) {
	var err error
	switch result := result.(type) {
	case chatRemoteResultSync:
		err = result.Err
	case chatRemoteResultNoticeRecorded:
		err = result.Err
	case chatRemoteResultPrompt:
		err = result.Err
	case chatRemoteResultRunShell:
		err = result.Err
	case chatRemoteResultRemoveQueuedPrompt:
		err = result.Err
	case chatRemoteResultStopBackgroundTask:
		err = result.Err
	case chatRemoteResultGoalControl:
		err = result.Err
	case chatRemoteResultSetConfig:
		err = result.Err
	case chatRemoteResultPlanCommand:
		err = result.Err
	case chatRemoteResultCancel:
		err = result.Err
	case chatRemoteResultReviewerAnswer:
		err = result.Err
	case chatRemoteResultRespondElicitation:
		err = result.Err
	case chatRemoteResultFollowupUnconfirmed:
		return result.Error, true
	case chatRemoteResultDeliveryUnconfirmed:
		return result.Error, true
	case chatRemoteResultWorkerFailed:
		return result.Error, true
	}
	if err == nil {
		return "", false
	}
	return err.Error(), true
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
	operationsTx, operationsRx := newChatRemoteOperationQueue(chatRemoteQueueCapacity)
	results := core.NewUnbounded[chatRemoteResult]()
	attached := &atomic.Bool{}
	attached.Store(true)
	worker := core.Spawn(func() struct{} {
		runChatRemoteWorker(context.Background(), session, sessionManager, operationsRx, results, attached)
		return struct{}{}
	})
	return &chatRemoteSupervisor{
		operationSender: operationsTx,
		results:         &remoteResults{receiver: results, attached: attached},
		resultsTx:       results,
		attached:        attached,
		worker:          worker,
	}
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
	results := s.results
	if results == nil {
		return nil
	}
	s.results = nil
	return spawnRemoteResultForwarder(func(ctx context.Context) {
		// The forwarder owns the feed now; detaching it when the task ends
		// logs whatever failure nobody will see.
		defer results.close()
		for {
			result, ok := results.receiver.Recv(ctx)
			if !ok {
				return
			}
			if err := updates.Send(composerIoUpdateRemote{result: result}); err != nil {
				if message, failed := chatRemoteResultFailureMessage(result); failed {
					slog.Error("chat operation failed after its composer closed", "error", message)
				}
				return
			}
		}
	})
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
	if s.results != nil {
		s.results.close()
		s.results = nil
	}
	if s.operationSender != nil {
		s.operationSender.close()
		s.operationSender = nil
	}
	worker := s.worker
	if worker == nil {
		return
	}
	s.worker = nil
	results, attached := s.resultsTx, s.attached
	core.Spawn(func() struct{} {
		if _, err := worker.Join(context.Background()); err != nil {
			publishChatRemoteResult(results, attached, chatRemoteResultWorkerFailed{
				Error: fmt.Sprintf("detached chat background worker failed: %v", err),
			})
		}
		return struct{}{}
	})
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
	pending := newRemoteTasks()
	accepting := true
	for {
		if !accepting && pending.isEmpty() {
			return
		}
		// A nil channel never delivers, as tokio::select! skips a disabled
		// branch.
		var incoming <-chan chatRemoteOperation
		if accepting {
			incoming = operations.operations
		}
		var finished chan error
		if !pending.isEmpty() {
			finished = pending.finished
		}
		select {
		case operation, ok := <-incoming:
			if !ok {
				accepting = false
				continue
			}
			<-operations.slots
			if session.IsStopped() {
				sessionID := session.SessionID()
				replacement, err := sessionManager.WaitForSession(ctx, sessionID, sessionActorReplacementWait)
				if err == nil {
					session = replacement
				} else {
					slog.Warn("could not reacquire replacement session actor before dispatch",
						"session_id", sessionID, "error", err.Error())
				}
			}
			enqueueChatRemoteOperation(ctx, session, sessionManager, operation, pending, results, attached)
		case err := <-finished:
			pending.running--
			if err != nil {
				publishChatRemoteResult(results, attached, chatRemoteResultWorkerFailed{
					Error: fmt.Sprintf("chat background operation failed: %v", err),
				})
			}
		case <-ctx.Done():
			return
		}
	}
}

// isDeliveryUnconfirmed reports whether err says a submission lost its
// acknowledgement (Rust's `error.is::<DeliveryUnconfirmed>()`).
func isDeliveryUnconfirmed(err error) bool {
	var unconfirmed client.DeliveryUnconfirmed
	return errors.As(err, &unconfirmed)
}

// relayCommandBytes is the size serde_json gives a relay command: the JSON
// without HTML escaping, which encoding/json would otherwise add.
func relayCommandBytes(command core.RelayCommand) (int, error) {
	var encoded bytes.Buffer
	encoder := json.NewEncoder(&encoded)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(command); err != nil {
		return 0, err
	}
	return encoded.Len() - 1, nil
}

func textPrompt(text string) core.RelayCommandPrompt {
	return core.RelayCommandPrompt{Prompt: []schema.ContentBlock{
		{Kind: schema.ContentBlockKindText, Text: &schema.TextContent{Text: text}},
	}}
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
	publish := func(result chatRemoteResult) { publishChatRemoteResult(results, attached, result) }
	switch operation := operation.(type) {
	case chatRemoteOperationReviewerAnswer:
		pending.spawn(func() {
			handle, err := sessionManager.Session(ctx, operation.Role)
			if err == nil {
				err = handle.RespondElicitation(ctx, operation.RequestID, operation.Response)
			}
			publish(chatRemoteResultReviewerAnswer{Draft: operation.Draft, Err: err})
		})
	case chatRemoteOperationRecordNotice:
		pending.spawn(func() {
			_, err := session.Submit(ctx, operation.ID, core.RelayCommandRecordNotice{Text: operation.Text})
			publish(chatRemoteResultNoticeRecorded{ID: operation.ID, Err: err})
		})
	case chatRemoteOperationSync:
		response, err := session.EnqueueSync(ctx)
		if err != nil {
			publish(chatRemoteResultSync{Err: err})
			return
		}
		pending.spawn(func() {
			publish(chatRemoteResultSync{Err: response.Wait(ctx)})
		})
	case chatRemoteOperationPrompt:
		commandID, text, images := operation.CommandID, operation.Text, operation.Images
		for _, image := range images {
			if image.Image.Reference == nil {
				publish(chatRemoteResultPrompt{CommandID: commandID, Text: text, Images: images, Err: errors.New("Image attachment is not ready")})
				return
			}
		}
		normalized := images
		payload := PromptPayload{Text: text, Images: slices.Clone(normalized)}
		command := core.RelayCommandPrompt{Prompt: payload.ContentBlocks()}
		if size, err := relayCommandBytes(command); err == nil && size > core.RelayCommandByteBudget {
			publish(chatRemoteResultPrompt{
				CommandID: commandID, Text: text, Images: slices.Clone(normalized),
				Err: errors.New("Prompt exceeds the relay command budget; shorten the text"),
			})
			return
		}
		if len(normalized) > 0 {
			view := session.View()
			if view.Snapshot == nil || !view.Snapshot.Operational.Get().AcceptsPromptImages() {
				publish(chatRemoteResultPrompt{CommandID: commandID, Text: text, Images: normalized, Err: errors.New(imageCapabilityNotice)})
				return
			}
		}
		response, err := session.EnqueueSubmit(ctx, commandID, command)
		if err != nil {
			publish(chatRemoteResultPrompt{CommandID: commandID, Text: text, Images: normalized, Err: err})
			return
		}
		pending.spawn(func() {
			ordinal, err := response.Wait(ctx)
			if err != nil {
				if isDeliveryUnconfirmed(err) {
					publish(chatRemoteResultDeliveryUnconfirmed{CommandID: commandID, Error: err.Error()})
					return
				}
				publish(chatRemoteResultPrompt{CommandID: commandID, Text: text, Images: slices.Clone(normalized), Err: err})
				return
			}
			publish(chatRemoteResultPrompt{CommandID: commandID, Text: text, Images: slices.Clone(normalized), Ordinal: ordinal})
		})
	case chatRemoteOperationRunShell:
		commandID, command := operation.CommandID, operation.Command
		response, err := session.EnqueueSubmit(ctx, commandID, core.RelayCommandRunUserShell{Command: command})
		if err != nil {
			publish(chatRemoteResultRunShell{CommandID: commandID, Command: command, Err: err})
			return
		}
		pending.spawn(func() {
			ordinal, err := response.Wait(ctx)
			if err != nil && isDeliveryUnconfirmed(err) {
				publish(chatRemoteResultDeliveryUnconfirmed{CommandID: commandID, Error: err.Error()})
				return
			}
			publish(chatRemoteResultRunShell{CommandID: commandID, Command: command, Ordinal: ordinal, Err: err})
		})
	case chatRemoteOperationRemoveQueuedPrompt:
		response, err := session.EnqueueSubmit(ctx, operation.CommandID, core.RelayCommandRemoveQueuedPrompt{QueuedCommandID: operation.ID})
		if err != nil {
			publish(chatRemoteResultRemoveQueuedPrompt{ID: operation.ID, Text: operation.Text, Kind: operation.Kind, Err: err})
			return
		}
		pending.spawn(func() {
			_, err := response.Wait(ctx)
			publish(chatRemoteResultRemoveQueuedPrompt{ID: operation.ID, Text: operation.Text, Kind: operation.Kind, Err: err})
		})
	case chatRemoteOperationStopBackgroundTask:
		pending.spawn(func() {
			publish(chatRemoteResultStopBackgroundTask{ID: operation.ID, Err: session.StopBackgroundTask(ctx, operation.ID)})
		})
	case chatRemoteOperationGoalControl:
		response, err := session.EnqueueSubmit(ctx, operation.CommandID, core.RelayCommandGoalControl{Action: operation.Action})
		if err != nil {
			publish(chatRemoteResultGoalControl{Action: operation.Action, Err: err})
			return
		}
		pending.spawn(func() {
			_, err := response.Wait(ctx)
			publish(chatRemoteResultGoalControl{Action: operation.Action, Err: err})
		})
	case chatRemoteOperationSetConfig:
		response, err := session.EnqueueSubmit(ctx, operation.CommandID, core.RelayCommandSetConfig{Key: operation.Key, Value: operation.Value})
		if err != nil {
			publish(chatRemoteResultSetConfig{Key: operation.Key, Value: operation.Value, Err: err})
			return
		}
		pending.spawn(func() {
			_, err := response.Wait(ctx)
			publish(chatRemoteResultSetConfig{Key: operation.Key, Value: operation.Value, Err: err})
		})
	case chatRemoteOperationPlanCommand:
		pending.spawn(func() {
			commandID := operation.CommandID
			controlApplied, unconfirmed := false, false
			var ordinal *uint64
			err := func() error {
				if err := session.ApplyPlanControl(ctx, commandID, operation.Control); err != nil {
					return err
				}
				controlApplied = true
				if operation.Prompt == nil {
					return nil
				}
				response, err := session.EnqueueSubmit(ctx, commandID+"-prompt", textPrompt(*operation.Prompt))
				if err != nil {
					return fmt.Errorf("mode changed, but prompt was not queued: %w", err)
				}
				accepted, err := response.Wait(ctx)
				if err != nil {
					unconfirmed = isDeliveryUnconfirmed(err)
					return fmt.Errorf("mode changed, but prompt failed: %w", err)
				}
				ordinal = &accepted
				return nil
			}()
			if unconfirmed {
				requested := operation.RequestedActive
				publish(chatRemoteResultFollowupUnconfirmed{
					CommandID:         commandID + "-prompt",
					FeedbackKey:       "plan:" + commandID,
					DesiredPlanActive: &requested,
					PlanCommand:       true,
					Error:             err.Error(),
				})
				return
			}
			publish(chatRemoteResultPlanCommand{
				CommandID:       commandID,
				Original:        operation.Original,
				RequestedActive: operation.RequestedActive,
				ControlApplied:  controlApplied,
				Ordinal:         ordinal,
				Err:             err,
			})
		})
	case chatRemoteOperationCancel:
		pending.spawn(func() {
			var failures []string
			if operation.CancelAgent {
				command := operation.Command
				if command == nil {
					command = core.RelayCommandCancelTurn{}
				}
				if _, err := session.Submit(ctx, operation.CommandID, command); err != nil {
					failures = append(failures, "agent: "+err.Error())
				}
			}
			for index, shellCommandID := range operation.ShellCommandIDs {
				id := fmt.Sprintf("%s-shell-%d", operation.CommandID, index)
				if _, err := session.Submit(ctx, id, core.RelayCommandCancelUserShell{ShellCommandID: shellCommandID}); err != nil {
					failures = append(failures, "shell: "+err.Error())
				}
			}
			var err error
			if len(failures) > 0 {
				err = errors.New(strings.Join(failures, "; "))
			}
			publish(chatRemoteResultCancel{Intent: operation.Intent, Err: err})
		})
	case chatRemoteOperationRespondElicitation:
		pending.spawn(func() {
			request := operation.Request
			answered, unconfirmed := false, false
			var desiredPlanActive *bool
			if operation.PlanFollowup != nil {
				desired := operation.PlanFollowup.desiredActive
				desiredPlanActive = &desired
			}
			err := func() error {
				if err := session.RespondElicitation(ctx, request.ID, operation.Response); err != nil {
					return err
				}
				answered = true
				followup := operation.PlanFollowup
				if followup == nil {
					return nil
				}
				if followup.control != nil {
					if err := session.ApplyPlanControl(ctx, "plan-review-"+request.ID+"-mode", followup.control); err != nil {
						return fmt.Errorf("review answered, but plan mode was not changed: %w", err)
					}
				}
				if followup.prompt != nil {
					if _, err := session.Submit(ctx, "plan-review-"+request.ID+"-feedback", textPrompt(*followup.prompt)); err != nil {
						unconfirmed = isDeliveryUnconfirmed(err)
						return fmt.Errorf("review answered, but revision feedback failed: %w", err)
					}
				}
				return nil
			}()
			if unconfirmed {
				publish(chatRemoteResultFollowupUnconfirmed{
					CommandID:         "plan-review-" + request.ID + "-feedback",
					FeedbackKey:       "answer:" + request.ID,
					DesiredPlanActive: desiredPlanActive,
					PlanCommand:       false,
					Error:             err.Error(),
				})
				return
			}
			publish(chatRemoteResultRespondElicitation{
				Request:           request,
				DesiredPlanActive: desiredPlanActive,
				Answered:          answered,
				Err:               err,
			})
		})
	}
}

func restoreUnsentInput(chat *ChatState, input string) {
	restoreUnsentPrompt(chat, input, nil)
}

func restoreUnsentPrompt(chat *ChatState, text string, images []PromptImage) {
	payload := PromptPayload{Text: text, Images: images}
	if draft := chat.draftPayload(); draft.Equal(&payload) {
		return
	}
	if chat.input != "" {
		payload.Text += "\n\n"
	}
	chat.replaceInputRange(ByteRange{Start: 0, End: 0}, &payload)
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
	// A parked composer owns delivery, but not the session/review
	// projection. Keep projection-dependent results intact until a live view
	// owns both.
	_, respond := result.(chatRemoteResultRespondElicitation)
	_, reviewerAnswer := result.(chatRemoteResultReviewerAnswer)
	if (chat.standby && (respond || reviewerAnswer)) || (!chat.reviewProjectionKnown && reviewerAnswer) {
		chat.deferredFormResults = append(chat.deferredFormResults, result)
		return
	}
	if key, ok := chatRemoteResultFeedbackKey(result); ok {
		if cancel, isCancel := result.(chatRemoteResultCancel); !isCancel || cancel.Err != nil {
			delete(chat.operationFeedback, key)
		}
	}
	// Keep diagnostics in the process log as well as in recoverable rows.
	// Publishing a conversation notice must never recursively publish
	// failure. Reviewer recovery can wait for another form; it logs only
	// when applied, rather than on each attempt to surface the saved result.
	if message, failed := chatRemoteResultFailureMessage(result); failed && !reviewerAnswer {
		slog.Warn("chat operation failed and was shown in the UI", "session_id", chat.sessionID, "error", message)
	}
	switch result := result.(type) {
	// The local row remains until projection confirms publication. A
	// failure is logged without publishing another notice about it.
	case chatRemoteResultNoticeRecorded:
		if result.Err != nil {
			chat.refuseNotice(result.ID)
		}
	case chatRemoteResultSync:
		if result.Err == nil {
			chat.connectionFeedback = nil
			chat.setTranscriptLoading(false)
		} else {
			chat.setTranscriptLoading(false)
			chat.setConnectionNotice("Connection failed: " + result.Err.Error())
		}
	case chatRemoteResultPrompt:
		if result.Err == nil {
			chat.finishSubmission(result.CommandID, true)
			return
		}
		if !chat.finishSubmission(result.CommandID, false) {
			return
		}
		restoreUnsentPrompt(chat, result.Text, slices.Clone(result.Images))
		// A refusal is an event in the conversation, and the draft already
		// holds the text for a retry. It becomes a dated notice in time
		// order instead of a row pinned below every later turn (I1-12).
		if command, ok := refusedSlashCommand(result.Text, result.Images); ok {
			chat.conversationNotice(command + " was not run: " + result.Err.Error())
			return
		}
		reportRefusedSubmission(chat, SubmissionKindPrompt, result.Text, result.Err.Error())
	case chatRemoteResultRunShell:
		if result.Err == nil {
			chat.finishSubmission(result.CommandID, true)
			return
		}
		if !chat.finishSubmission(result.CommandID, false) {
			return
		}
		restoreUnsentInput(chat, "!"+result.Command)
		reportRefusedSubmission(chat, SubmissionKindShell, result.Command, result.Err.Error())
	case chatRemoteResultRemoveQueuedPrompt:
		if result.Err != nil {
			chat.failQueuedPromptRemoval(result.ID, result.Text, result.Kind)
			chat.conversationNotice("Queued prompt was not removed: " + result.Err.Error())
		}
	case chatRemoteResultStopBackgroundTask:
		// The provider's acknowledgement only means it accepted the stop
		// request. The next activity snapshot remains authoritative for
		// removing the row and its pending label.
		if result.Err != nil {
			chat.failBackgroundStop(result.ID, result.Err.Error())
		}
	case chatRemoteResultGoalControl:
		if result.Err != nil {
			restoreUnsentInput(chat, "/goal "+result.Action.AsStr())
			chat.conversationNotice(fmt.Sprintf("/goal %s failed: %s", result.Action.AsStr(), result.Err.Error()))
		}
	case chatRemoteResultSetConfig:
		if result.Err != nil {
			restoreUnsentInput(chat, core.ConfigCommandText(result.Key, result.Value))
			chat.conversationNotice("Configuration was not changed: " + result.Err.Error())
		}
	case chatRemoteResultPlanCommand:
		if result.Err == nil {
			chat.planCommandPending = false
			chat.finishPlanModeChange(result.RequestedActive)
			chat.finishSubmission(result.CommandID+"-prompt", true)
			return
		}
		unsent := chat.finishSubmission(result.CommandID+"-prompt", false)
		chat.planCommandPending = false
		chat.finishPlanModeChange(result.ControlApplied == result.RequestedActive)
		if unsent {
			restoreUnsentInput(chat, result.Original)
		}
		chat.conversationNotice("Plan command was not completed: " + result.Err.Error())
	case chatRemoteResultCancel:
		chat.turnControlSubmitting = false
		if result.Err != nil {
			chat.turnControlAwaitingState = nil
		}
		if chat.turnControlAwaitingState == nil && chat.cancellingPromptID == nil &&
			!(chat.steering != nil && chat.steering.HoldsQueue()) {
			delete(chat.operationFeedback, "turn-control")
		}
		if result.Err != nil {
			notice := result.Intent.failureNotice(result.Err.Error())
			chat.turnControlError = &notice
			chat.turnControlDialogOpen = result.Intent != TurnControlIntentCancel
			chat.conversationNotice(result.Intent.failureNotice(result.Err.Error()))
		}
	case chatRemoteResultReviewerAnswer:
		finishReviewerAnswer(chat, result.Draft, result.Err)
	case chatRemoteResultRespondElicitation:
		feedback := "plan-review-" + result.Request.ID + "-feedback"
		if result.Err == nil {
			if result.DesiredPlanActive != nil {
				chat.finishPlanModeChange(*result.DesiredPlanActive)
			}
			chat.finishSubmission(feedback, true)
			return
		}
		chat.finishSubmission(feedback, false)
		if !result.Answered {
			chat.restoreFailedElicitation(result.Request, "Answer was not sent: "+result.Err.Error())
		}
		if result.Answered {
			chat.conversationNotice("Answer sent; follow-up failed: " + result.Err.Error())
		} else {
			chat.conversationNotice("Answer was not sent: " + result.Err.Error())
		}
	case chatRemoteResultFollowupUnconfirmed:
		if result.PlanCommand {
			chat.planCommandPending = false
		}
		if result.DesiredPlanActive != nil {
			chat.finishPlanModeChange(*result.DesiredPlanActive)
		}
		chat.unconfirmSubmission(result.CommandID, result.Error)
	case chatRemoteResultDeliveryUnconfirmed:
		chat.unconfirmSubmission(result.CommandID, result.Error)
	case chatRemoteResultWorkerFailed:
		chat.unconfirmAllSubmissions(result.Error)
		clear(chat.operationFeedback)
		if chat.failAllBackgroundStops() {
			chat.conversationNotice("Background task could not be stopped: " + result.Error)
		} else {
			chat.conversationNotice(result.Error)
		}
	}
}

func queueChatRemoteOperation(operations *chatRemoteOperationSender, operation chatRemoteOperation, chat *ChatState) {
	// Reserve admission before replacing an unconfirmed receipt. If the
	// local queue refuses the retry, the original identity remains
	// recoverable.
	permit, admitted := operations.tryReserve()
	retrying := false
	switch operation := operation.(type) {
	case chatRemoteOperationPrompt:
		id, ok := chat.recoverableSubmissionID()
		retrying = ok && id == operation.CommandID
	case chatRemoteOperationRunShell:
		id, ok := chat.recoverableSubmissionID()
		retrying = ok && id == operation.CommandID
	}
	if retrying && !admitted {
		chat.SetNotice("The session command queue could not accept the resend; try again")
		return
	}
	feedbackKey, feedbackText, hasFeedback := chatRemoteOperationFeedback(operation)
	if hasFeedback {
		if chat.operationFeedback == nil {
			chat.operationFeedback = map[string]string{}
		}
		chat.operationFeedback[feedbackKey] = feedbackText
	}
	switch operation := operation.(type) {
	case chatRemoteOperationPrompt:
		chat.beginSubmission(operation.CommandID, SubmissionKindPrompt,
			PromptPayload{Text: operation.Text, Images: slices.Clone(operation.Images)}, "Sending…")
	case chatRemoteOperationRunShell:
		chat.beginSubmission(operation.CommandID, SubmissionKindShell, PromptPayloadText(operation.Command), "Sending…")
	case chatRemoteOperationPlanCommand:
		if operation.Prompt != nil {
			chat.beginSubmission(operation.CommandID+"-prompt", SubmissionKindPrompt, PromptPayloadText(*operation.Prompt), "Changing mode…")
		}
	case chatRemoteOperationRespondElicitation:
		if operation.PlanFollowup != nil && operation.PlanFollowup.prompt != nil {
			chat.beginSubmission("plan-review-"+operation.Request.ID+"-feedback", SubmissionKindPrompt,
				PromptPayloadText(*operation.PlanFollowup.prompt), "Sending answer…")
		}
	}
	if admitted {
		permit.send(operation)
		return
	}
	if hasFeedback {
		delete(chat.operationFeedback, feedbackKey)
	}
	const queueFull = "session command queue is full"
	switch operation := operation.(type) {
	case chatRemoteOperationPrompt:
		chat.finishSubmission(operation.CommandID, false)
		restoreUnsentPrompt(chat, operation.Text, operation.Images)
		reportRefusedSubmission(chat, SubmissionKindPrompt, operation.Text, queueFull)
	case chatRemoteOperationRunShell:
		chat.finishSubmission(operation.CommandID, false)
		reportRefusedSubmission(chat, SubmissionKindShell, operation.Command, queueFull)
		restoreUnsentInput(chat, "!"+operation.Command)
	case chatRemoteOperationRemoveQueuedPrompt:
		chat.failQueuedPromptRemoval(operation.ID, operation.Text, operation.Kind)
	case chatRemoteOperationStopBackgroundTask:
		chat.failBackgroundStop(operation.ID, queueFull)
	case chatRemoteOperationGoalControl:
		restoreUnsentInput(chat, "/goal "+operation.Action.AsStr())
	case chatRemoteOperationSetConfig:
		restoreUnsentInput(chat, core.ConfigCommandText(operation.Key, operation.Value))
	case chatRemoteOperationPlanCommand:
		chat.finishSubmission(operation.CommandID+"-prompt", false)
		chat.planCommandPending = false
		chat.finishPlanModeChange(!operation.RequestedActive)
		restoreUnsentInput(chat, operation.Original)
	case chatRemoteOperationReviewerAnswer:
		finishReviewerAnswer(chat, operation.Draft, errors.New(queueFull))
	case chatRemoteOperationRespondElicitation:
		chat.finishSubmission("plan-review-"+operation.Request.ID+"-feedback", false)
		chat.restoreFailedElicitation(operation.Request, "Answer was not sent: "+queueFull)
	case chatRemoteOperationCancel:
		chat.turnControlSubmitting = false
		chat.turnControlAwaitingState = nil
		notice := operation.Intent.failureNotice(queueFull)
		chat.turnControlError = &notice
		chat.turnControlDialogOpen = operation.Intent == TurnControlIntentSteer
	case chatRemoteOperationSync, chatRemoteOperationRecordNotice:
	}
	chat.SetNotice("The session command queue is full; the command was not sent")
}
