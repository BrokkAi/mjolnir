// Port of mj-cli/src/dashboard/startup_prompts.rs.
//
// Ordered host requests and durable receipts for prompts entered before
// attachment.

package cli

import (
	"cmp"
	"context"
	"errors"
	"fmt"
	"log/slog"
	"slices"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// finishNotificationReply is the one outcome owner for detached replies,
// including admission failures and durable refusals. The card may no longer
// exist when either arrives. result is nil when the reply was accepted;
// otherwise its text is the failure.
func finishNotificationReply(dashboard *tui.DashboardState, chats map[string]*chat.ActiveChat, sessionID string, prompt *chat.StartupPrompt, result error) bool {
	if !dashboard.NotificationReplyFinished(sessionID, prompt.CommandID, result) {
		return false
	}
	if result != nil {
		var recovery string
		if dashboard.HasNotificationForSession(sessionID) {
			recovery = "The reply remains in the Notifications pane and this notice."
		} else {
			var restored *error
			if active := chats[sessionID]; !dashboard.HasStandbyComposer(sessionID) && active != nil {
				if active.Draft() == "" {
					err := active.RestoreStartupPrompt(*prompt)
					restored = &err
				}
			} else if draft, ok := dashboard.StandbyPromptDraft(sessionID); !ok || draft == "" {
				err := dashboard.RestoreStartupPrompt(sessionID, *prompt)
				restored = &err
			}
			switch {
			case restored != nil && *restored == nil:
				recovery = "The unsent text is in the session composer; open the session to retry."
			case restored != nil:
				recovery = fmt.Sprintf("Could not restore the composer (%s); copy the unsent text from this notice.", *restored)
			default:
				recovery = "The existing composer draft is kept; copy the unsent text from this notice."
			}
		}
		dashboard.ReportNotificationReplyFailure(sessionID, prompt.Payload.Text, result.Error(), recovery)
	}
	return true
}

// admissionBackoffTimer waits out one retryAdmission backoff (Tokio's
// sleep). Tests stand in a clock they advance, as Rust's paused Tokio clock
// does.
var admissionBackoffTimer = func(backoff time.Duration) <-chan time.Time { return time.After(backoff) }

// retryAdmission runs attempt until it reports done or settlement ends,
// backing off from one second, doubling to at most thirty, between
// attempts. attempt's context ends with settlement.
func retryAdmission(settlement context.Context, attempt func(ctx context.Context) bool) {
	backoff := time.Second
	for {
		// biased: a settled prompt never starts another attempt.
		if settlement.Err() != nil {
			return
		}
		if attempt(settlement) {
			return
		}
		if settlement.Err() != nil {
			return
		}
		select {
		case <-settlement.Done():
			return
		case <-admissionBackoffTimer(backoff):
		}
		backoff = min(backoff*2, 30*time.Second)
	}
}

// admissionFailureIsDefinite reports whether err proves the request was
// not accepted. A conflict describes another immutable request under the
// original ID; it cannot establish that the original request was
// unaccepted.
func admissionFailureIsDefinite(err error) bool {
	if errors.As(err, new(client.StartupPromptIdentityConflict)) {
		return false
	}
	var refusal client.DaemonRefusal
	return errors.As(err, &refusal) && !refusal.DeliveryUnconfirmed()
}

// saveJob is one ordered startup request (Rust's pinned boxed future).
type saveJob func(ctx context.Context)

// sessionMailboxCapacity is the Rust mailbox channel's bound.
const sessionMailboxCapacity = 64

// sessionMailbox runs one session's startup requests in order on a worker
// of its own, which stops when the mailbox is closed (Rust's
// AbortOnDropHandle).
type sessionMailbox struct {
	sender chan saveJob
	stop   context.CancelFunc
}

func newSessionMailbox() *sessionMailbox {
	sender := make(chan saveJob, sessionMailboxCapacity)
	ctx, stop := context.WithCancel(context.Background())
	worker := core.Spawn(func() struct{} {
		for {
			select {
			case <-ctx.Done():
				return struct{}{}
			case work := <-sender:
				work(ctx)
			}
		}
	})
	go func() {
		if _, err := worker.Join(context.Background()); err != nil {
			slog.Error("ordered startup request worker failed", "error", err)
		}
	}()
	return &sessionMailbox{sender: sender, stop: stop}
}

// admit queues work without waiting, or fails when the queue is full or
// gone.
func (m *sessionMailbox) admit(work saveJob) error {
	select {
	case m.sender <- work:
		return nil
	default:
		return errors.New("startup request queue is unavailable: no available capacity")
	}
}

// startupPrompts tracks every prompt queued before its session attached,
// from admission to a durable outcome. The zero value is Rust's Default.
type startupPrompts struct {
	mailboxes          map[string]*sessionMailbox
	pending            map[string]map[string]trackedPrompt
	polls              map[string]*outcomePoll
	nextOrder          uint64
	moves              map[string]moveAdmission
	nextMoveGeneration uint64
}

// heldSave keeps a quit waiting while a prompt is held for its move's
// origin.
type heldSave struct {
	guard *client.CriticalOperationGuard
}

// moveAdmission is the move a session's held prompts wait for.
type moveAdmission struct {
	generation uint64
	phase      moveAdmissionPhase
}

// moveAdmissionPhase is how far a session's move has come.
type moveAdmissionPhase interface{ isMoveAdmissionPhase() }

type moveAdmissionPhasePreparing struct{ id *string }

type moveAdmissionPhaseReady struct{ id string }

type moveAdmissionPhaseCompleted struct{ id string }

type moveAdmissionPhaseFailed struct{ id *string }

func (moveAdmissionPhasePreparing) isMoveAdmissionPhase() {}
func (moveAdmissionPhaseReady) isMoveAdmissionPhase()     {}
func (moveAdmissionPhaseCompleted) isMoveAdmissionPhase() {}
func (moveAdmissionPhaseFailed) isMoveAdmissionPhase()    {}

// originAdmission is what a prompt naming a move origin may do now.
type originAdmission uint8

const (
	originAdmissionHold originAdmission = iota
	originAdmissionSend
	originAdmissionRefuse
)

// trackedPrompt is one queued prompt: waiting for its outcome, or settled.
type trackedPrompt interface{ isTrackedPrompt() }

// trackedPromptWaiting is a prompt still waiting for a durable outcome; the
// map holds it by pointer so it is updated in place.
type trackedPromptWaiting struct {
	prompt         chat.StartupPrompt
	inheritedDraft *string
	recalled       bool
	admissionError *string
	order          uint64
	held           *heldSave
	// settlement ends once the prompt settles (Rust's CancellationToken);
	// settle ends it.
	settlement context.Context
	settle     context.CancelFunc
}

type trackedPromptSettled struct{}

func (*trackedPromptWaiting) isTrackedPrompt() {}
func (trackedPromptSettled) isTrackedPrompt()  {}

// outcomePoll is one session's outcome query and its backoff.
type outcomePoll struct {
	// task stops the query in flight (Rust's AbortOnDropHandle); nil when
	// none runs.
	task    context.CancelFunc
	next    time.Time
	backoff time.Duration
	offset  int
}

// newOutcomePoll is Rust's Default: due now, with a one-second backoff.
func newOutcomePoll() *outcomePoll {
	return &outcomePoll{next: time.Now(), backoff: time.Second}
}

// newSettlement is a fresh CancellationToken.
func newSettlement() (context.Context, context.CancelFunc) {
	return context.WithCancel(context.Background())
}

// moveAdmissionPhaseIdentity is the operation id a phase names, if any.
func moveAdmissionPhaseIdentity(phase moveAdmissionPhase) *string {
	switch phase := phase.(type) {
	case moveAdmissionPhasePreparing:
		return phase.id
	case moveAdmissionPhaseFailed:
		return phase.id
	case moveAdmissionPhaseReady:
		return &phase.id
	case moveAdmissionPhaseCompleted:
		return &phase.id
	}
	return nil
}

func (p *startupPrompts) ensureMoves() {
	if p.moves == nil {
		p.moves = map[string]moveAdmission{}
	}
}

func (p *startupPrompts) setMovePhase(sessionID string, phase moveAdmissionPhase) {
	p.ensureMoves()
	current, exists := p.moves[sessionID]
	changed := false
	if exists {
		old, updated := moveAdmissionPhaseIdentity(current.phase), moveAdmissionPhaseIdentity(phase)
		changed = old != nil && updated != nil && *old != *updated
	}
	var generation uint64
	if changed {
		p.nextMoveGeneration++
		generation = p.nextMoveGeneration
	} else if exists {
		generation = current.generation
	}
	p.moves[sessionID] = moveAdmission{generation: generation, phase: phase}
}

func (p *startupPrompts) beginMove(sessionID string, id *string) {
	p.ensureMoves()
	p.nextMoveGeneration++
	p.moves[sessionID] = moveAdmission{
		generation: p.nextMoveGeneration,
		phase:      moveAdmissionPhasePreparing{id: id},
	}
}

func (p *startupPrompts) prepareMove(sessionID string, id *string) {
	admission, exists := p.moves[sessionID]
	if !exists {
		return
	}
	switch phase := admission.phase.(type) {
	case moveAdmissionPhasePreparing:
		if phase.id == nil {
			p.setMovePhase(sessionID, moveAdmissionPhasePreparing{id: id})
		}
	case moveAdmissionPhaseFailed:
		if phase.id == nil {
			p.setMovePhase(sessionID, moveAdmissionPhaseFailed{id: id})
		}
	}
}

// moveOrigin is the generation and operation id of a move still preparing
// or ready; false otherwise.
func (p *startupPrompts) moveOrigin(sessionID string) (generation uint64, id *string, ok bool) {
	admission, exists := p.moves[sessionID]
	if !exists {
		return 0, nil, false
	}
	switch phase := admission.phase.(type) {
	case moveAdmissionPhasePreparing:
		return admission.generation, phase.id, true
	case moveAdmissionPhaseReady:
		readyID := phase.id
		return admission.generation, &readyID, true
	}
	return 0, nil, false
}

// terminalMove is the generation, operation id and success of a move that
// completed or failed; ok is false otherwise.
func (p *startupPrompts) terminalMove(sessionID string) (generation uint64, id *string, completed bool, ok bool) {
	admission, exists := p.moves[sessionID]
	if !exists {
		return 0, nil, false, false
	}
	switch phase := admission.phase.(type) {
	case moveAdmissionPhaseCompleted:
		completedID := phase.id
		return admission.generation, &completedID, true, true
	case moveAdmissionPhaseFailed:
		return admission.generation, phase.id, false, true
	}
	return 0, nil, false, false
}

// observeMove records a move the daemon reported; terminal is nil while it
// runs, and whether it completed once it ended.
func (p *startupPrompts) observeMove(sessionID string, id string, terminal *bool) {
	if admission, exists := p.moves[sessionID]; exists {
		switch phase := admission.phase.(type) {
		case moveAdmissionPhasePreparing:
			if phase.id == nil || *phase.id != id {
				return
			}
		case moveAdmissionPhaseReady:
			if terminal != nil && phase.id != id {
				return
			}
		case moveAdmissionPhaseCompleted:
			if terminal != nil && phase.id != id {
				return
			}
		case moveAdmissionPhaseFailed:
			if terminal != nil && phase.id != nil && *phase.id != id {
				return
			}
		}
	}
	var state moveAdmissionPhase
	switch {
	case terminal == nil:
		state = moveAdmissionPhaseReady{id: id}
	case *terminal:
		state = moveAdmissionPhaseCompleted{id: id}
	default:
		failedID := id
		state = moveAdmissionPhaseFailed{id: &failedID}
	}
	p.setMovePhase(sessionID, state)
}

func (p *startupPrompts) finishMove(sessionID string, completed *string) {
	var state moveAdmissionPhase
	if completed != nil {
		state = moveAdmissionPhaseCompleted{id: *completed}
	} else {
		_, id, _ := p.moveOrigin(sessionID)
		state = moveAdmissionPhaseFailed{id: id}
	}
	p.setMovePhase(sessionID, state)
}

func (p *startupPrompts) originAdmission(sessionID string, origin *string) originAdmission {
	if origin == nil {
		return originAdmissionSend
	}
	admission, exists := p.moves[sessionID]
	if !exists {
		return originAdmissionHold
	}
	switch phase := admission.phase.(type) {
	case moveAdmissionPhaseReady:
		if phase.id == *origin {
			return originAdmissionSend
		}
	case moveAdmissionPhaseCompleted:
		if phase.id == *origin {
			return originAdmissionSend
		}
	case moveAdmissionPhasePreparing:
		if phase.id == nil || *phase.id == *origin {
			return originAdmissionHold
		}
	}
	return originAdmissionRefuse
}

// sortedPending is the sessions with tracked prompts in Rust's BTreeMap
// order.
func (p *startupPrompts) sortedPending() []string {
	return sortedKeys(p.pending)
}

func (p *startupPrompts) flushHeld(state *core.StateState, updates *core.Unbounded[dashboardIoUpdate], tracker client.CriticalOperationTracker) {
	type readyPrompt struct {
		order     uint64
		sessionID string
		id        string
	}
	var ready []readyPrompt
	refused := map[string]map[string]client.StartupPromptOutcome{}
	for _, sessionID := range p.sortedPending() {
		entries := p.pending[sessionID]
		for _, id := range sortedKeys(entries) {
			waiting, ok := entries[id].(*trackedPromptWaiting)
			if !ok || waiting.held == nil {
				continue
			}
			switch p.originAdmission(sessionID, waiting.prompt.MoveOperationID) {
			case originAdmissionSend:
				ready = append(ready, readyPrompt{order: waiting.order, sessionID: sessionID, id: id})
			case originAdmissionRefuse:
				if refused[sessionID] == nil {
					refused[sessionID] = map[string]client.StartupPromptOutcome{}
				}
				refused[sessionID][id] = client.StartupPromptOutcomeRefused{
					EncodedDraft: waiting.prompt.Payload.EncodeDraft(),
					Reason:       "The move ended before this prompt was admitted; its draft is kept.",
				}
			}
		}
	}
	slices.SortStableFunc(ready, func(a, b readyPrompt) int { return cmp.Compare(a.order, b.order) })
	for _, entry := range ready {
		entries := p.pending[entry.sessionID]
		waiting, ok := entries[entry.id].(*trackedPromptWaiting)
		if !ok || waiting.held == nil {
			continue
		}
		delete(entries, entry.id)
		held := waiting.held
		p.enqueue(
			namedSession{name: sessionNoticeName(state, entry.sessionID), id: entry.sessionID},
			waiting.prompt,
			waiting.inheritedDraft,
			updates,
			tracker,
		)
		// The held admission's guard ends once the queued request owns the
		// quit (Rust drops `_held` at the end of this arm).
		held.guard.Release()
	}
	for _, sessionID := range sortedKeys(refused) {
		report("restoring unadmitted move prompts", updates, dashboardIoUpdate(dashboardIoUpdateStartupPromptOutcomes{
			sessionID: sessionID,
			outcomes:  refused[sessionID],
		}))
	}
}

func (p *startupPrompts) retainUnconfirmed(dashboard *tui.DashboardState, chats map[string]*chat.ActiveChat) {
	for _, sessionID := range p.sortedPending() {
		entries := p.pending[sessionID]
		for _, id := range sortedKeys(entries) {
			waiting, ok := entries[id].(*trackedPromptWaiting)
			if !ok {
				continue
			}
			if active := chats[sessionID]; active != nil && !dashboard.HasStandbyComposer(sessionID) {
				active.RetainStartupReceipt(waiting.prompt, waiting.inheritedDraft)
			} else {
				dashboard.RetainStartupReceipt(sessionID, waiting.prompt, waiting.inheritedDraft)
			}
		}
	}
}

func (p *startupPrompts) cancelUnadmitted(dashboard *tui.DashboardState, chats map[string]*chat.ActiveChat) {
	refused := map[string]map[string]client.StartupPromptOutcome{}
	for _, sessionID := range p.sortedPending() {
		for id, entry := range p.pending[sessionID] {
			waiting, ok := entry.(*trackedPromptWaiting)
			if !ok || waiting.held == nil {
				continue
			}
			if refused[sessionID] == nil {
				refused[sessionID] = map[string]client.StartupPromptOutcome{}
			}
			refused[sessionID][id] = client.StartupPromptOutcomeRefused{
				EncodedDraft: waiting.prompt.Payload.EncodeDraft(),
				Reason:       "The prompt was not admitted before closing this view.",
			}
		}
	}
	for _, sessionID := range sortedKeys(refused) {
		p.applyOutcomes(sessionID, refused[sessionID], dashboard, chats)
	}
}

func (p *startupPrompts) enqueue(session namedSession, prompt chat.StartupPrompt, inheritedDraft *string, updates *core.Unbounded[dashboardIoUpdate], tracker client.CriticalOperationTracker) {
	p.enqueueInner(session, prompt, inheritedDraft, updates, tracker, false)
}

func (p *startupPrompts) resume(session namedSession, prompt chat.StartupPrompt, inheritedDraft *string, updates *core.Unbounded[dashboardIoUpdate], tracker client.CriticalOperationTracker) {
	if _, ok := p.pending[session.id][prompt.CommandID]; ok {
		return
	}
	p.enqueueInner(session, prompt, inheritedDraft, updates, tracker, true)
}

// insertPending records a tracked prompt under its session.
func (p *startupPrompts) insertPending(sessionID, commandID string, entry trackedPrompt) {
	if p.pending == nil {
		p.pending = map[string]map[string]trackedPrompt{}
	}
	if p.pending[sessionID] == nil {
		p.pending[sessionID] = map[string]trackedPrompt{}
	}
	p.pending[sessionID][commandID] = entry
}

// mailbox is the session's mailbox, opened on first use.
func (p *startupPrompts) mailbox(sessionID string) *sessionMailbox {
	if p.mailboxes == nil {
		p.mailboxes = map[string]*sessionMailbox{}
	}
	mailbox := p.mailboxes[sessionID]
	if mailbox == nil {
		mailbox = newSessionMailbox()
		p.mailboxes[sessionID] = mailbox
	}
	return mailbox
}

func (p *startupPrompts) enqueueInner(session namedSession, prompt chat.StartupPrompt, inheritedDraft *string, updates *core.Unbounded[dashboardIoUpdate], tracker client.CriticalOperationTracker, recovering bool) {
	sessionID := session.id
	label := fmt.Sprintf("queueing a prompt for session %s", session.name)
	guard := tracker.Begin(label)
	encodedDraft := prompt.Payload.EncodeDraft()
	settlement, settle := newSettlement()
	if !recovering && p.originAdmission(sessionID, prompt.MoveOperationID) != originAdmissionSend {
		p.insertPending(sessionID, prompt.CommandID, &trackedPromptWaiting{
			prompt:         prompt,
			inheritedDraft: inheritedDraft,
			order:          p.nextOrder,
			settlement:     settlement,
			settle:         settle,
			held:           &heldSave{guard: guard},
		})
		p.nextOrder++
		return
	}
	mailbox := p.mailbox(sessionID)
	admitted := mailbox.admit(func(context.Context) {
		retryAdmission(settlement, func(ctx context.Context) bool {
			// One slot, so the toUpdate below never blocks (Rust's oneshot).
			completed := make(chan bool, 1)
			task := spawnBackgroundAsync(
				label,
				updates,
				saveAckTimeout,
				func(ctx context.Context) (error, error) {
					daemonClient, err := ConnectOrStart(ctx)
					if err != nil {
						return nil, err
					}
					defer daemonClient.Close()
					err = daemonClient.QueueStartupPrompt(
						ctx,
						sessionID,
						prompt.CommandID,
						prompt.Payload.Text,
						prompt.Payload.ContentBlocks(),
						inheritedDraft,
						encodedDraft,
						prompt.MoveOperationID,
					)
					switch {
					case err == nil:
						return nil, nil
					case admissionFailureIsDefinite(err):
						return err, nil
					default:
						return nil, err
					}
				},
				func(definite error, err error) dashboardIoUpdate {
					result, definitelyUnaccepted, terminal := definite, true, true
					if err != nil {
						result, definitelyUnaccepted, terminal = err, false, false
					}
					completed <- terminal
					return dashboardIoUpdateStartupPromptQueued{
						sessionID:            sessionID,
						prompt:               prompt,
						err:                  result,
						definitelyUnaccepted: definitelyUnaccepted,
					}
				},
			)
			select {
			case <-task.done():
			case <-ctx.Done():
				// Dropping the attempt aborts its task (AbortOnDropHandle).
				task.abort()
				return false
			}
			if err := task.join(context.Background()); err != nil {
				slog.Error("startup admission supervisor failed", "error", err)
			}
			select {
			case terminal := <-completed:
				return terminal
			default:
				return false
			}
		})
		guard.Release()
	})
	if admitted != nil {
		p.insertPending(sessionID, prompt.CommandID, &trackedPromptWaiting{
			prompt:         prompt,
			inheritedDraft: inheritedDraft,
			order:          p.nextOrder,
			settlement:     settlement,
			settle:         settle,
		})
		p.nextOrder++
		// The work was never queued, so its guard ends here.
		guard.Release()
		report("queueing startup prompt", updates, dashboardIoUpdate(dashboardIoUpdateStartupPromptQueued{
			sessionID:            sessionID,
			prompt:               prompt,
			err:                  admitted,
			definitelyUnaccepted: true,
		}))
		return
	}
	p.insertPending(sessionID, prompt.CommandID, &trackedPromptWaiting{
		prompt:         prompt,
		inheritedDraft: inheritedDraft,
		order:          p.nextOrder,
		settlement:     settlement,
		settle:         settle,
	})
	p.nextOrder++
}

func (p *startupPrompts) withdraw(session namedSession, prompt chat.StartupPrompt, updates *core.Unbounded[dashboardIoUpdate], tracker client.CriticalOperationTracker) {
	sessionID := session.id
	if waiting, ok := p.pending[sessionID][prompt.CommandID].(*trackedPromptWaiting); ok {
		waiting.recalled = true
		if waiting.held != nil {
			p.settle(sessionID, prompt.CommandID)
			report("recalling unadmitted prompt", updates, dashboardIoUpdate(dashboardIoUpdateStartupPromptWithdrawn{
				sessionID: sessionID,
				prompt:    prompt,
				withdrawn: true,
			}))
			return
		}
	}
	label := fmt.Sprintf("withdrawing a prompt for session %s", session.name)
	guard := tracker.Begin(label)
	commandID := prompt.CommandID
	mailbox := p.mailbox(sessionID)
	if err := mailbox.admit(func(context.Context) {
		job := spawnBackgroundAsync(
			label,
			updates,
			saveAckTimeout,
			func(ctx context.Context) (bool, error) {
				daemonClient, err := ConnectOrStart(ctx)
				if err != nil {
					return false, err
				}
				defer daemonClient.Close()
				return daemonClient.WithdrawStartupPrompt(ctx, sessionID, commandID)
			},
			func(withdrawn bool, err error) dashboardIoUpdate {
				return dashboardIoUpdateStartupPromptWithdrawn{
					sessionID: sessionID,
					prompt:    prompt,
					withdrawn: withdrawn,
					err:       err,
				}
			},
		)
		if err := job.join(context.Background()); err != nil {
			slog.Error("startup withdrawal supervisor failed", "error", err)
		}
		guard.Release()
	}); err != nil {
		guard.Release()
		report("withdrawing startup prompt", updates, dashboardIoUpdate(dashboardIoUpdateStartupPromptWithdrawn{
			sessionID: sessionID,
			prompt:    prompt,
			err:       err,
		}))
	}
}

func (p *startupPrompts) isSettled(sessionID, commandID string) bool {
	_, settled := p.pending[sessionID][commandID].(trackedPromptSettled)
	return settled
}

// admissionFailed records a failed admission. False means admission is
// uncertain or already settled: never create a resendable duplicate.
func (p *startupPrompts) admissionFailed(sessionID, commandID, failure string, definite bool) bool {
	entry, ok := p.pending[sessionID][commandID]
	if !ok {
		return definite
	}
	waiting, ok := entry.(*trackedPromptWaiting)
	if !ok {
		return false
	}
	if definite {
		restore := !waiting.recalled
		waiting.retire()
		p.pending[sessionID][commandID] = trackedPromptSettled{}
		return restore
	}
	waiting.admissionError = &failure
	return false
}

func (p *startupPrompts) withdrawalNeedsReceipt(sessionID, commandID string) bool {
	switch entry := p.pending[sessionID][commandID].(type) {
	case trackedPromptSettled:
		return true
	case *trackedPromptWaiting:
		return entry.admissionError != nil
	}
	return false
}

func (p *startupPrompts) poll(updates *core.Unbounded[dashboardIoUpdate]) {
	now := time.Now()
	for _, sessionID := range p.sortedPending() {
		entries := p.pending[sessionID]
		var ids []string
		for _, id := range sortedKeys(entries) {
			if waiting, ok := entries[id].(*trackedPromptWaiting); ok && waiting.held == nil {
				ids = append(ids, id)
			}
		}
		if len(ids) == 0 {
			continue
		}
		if p.polls == nil {
			p.polls = map[string]*outcomePoll{}
		}
		poll := p.polls[sessionID]
		if poll == nil {
			poll = newOutcomePoll()
			p.polls[sessionID] = poll
		}
		if poll.task != nil || poll.next.After(now) {
			continue
		}
		count := min(len(ids), 128)
		commandIDs := make([]string, 0, count)
		for index := range count {
			commandIDs = append(commandIDs, ids[(poll.offset%len(ids)+index)%len(ids)])
		}
		poll.offset = (poll.offset + len(commandIDs)) % len(ids)
		querySession := sessionID
		task := spawnBackgroundAsync(
			"checking queued prompt outcomes",
			updates,
			saveAckTimeout,
			func(ctx context.Context) (map[string]client.StartupPromptOutcome, error) {
				daemonClient, err := ConnectOrStart(ctx)
				if err != nil {
					return nil, err
				}
				defer daemonClient.Close()
				return daemonClient.StartupPromptOutcomes(ctx, querySession, commandIDs)
			},
			func(outcomes map[string]client.StartupPromptOutcome, err error) dashboardIoUpdate {
				return dashboardIoUpdateStartupPromptOutcomes{sessionID: querySession, outcomes: outcomes, err: err}
			},
		)
		poll.task = task.abort
	}
}

func (p *startupPrompts) outcomes(sessionID string, succeeded bool) {
	poll := p.polls[sessionID]
	if poll == nil {
		return
	}
	if poll.task != nil {
		poll.task()
	}
	poll.task = nil
	if succeeded {
		poll.backoff = time.Second
	} else {
		poll.backoff = min(poll.backoff*2, 30*time.Second)
	}
	poll.next = time.Now().Add(poll.backoff)
}

func (p *startupPrompts) applyOutcomes(sessionID string, outcomes map[string]client.StartupPromptOutcome, dashboard *tui.DashboardState, chats map[string]*chat.ActiveChat) {
	// Ordered by admission, then by id as the BTreeMap they came in was.
	ordered := sortedKeys(outcomes)
	slices.SortStableFunc(ordered, func(a, b string) int {
		return cmp.Compare(p.orderOf(sessionID, a), p.orderOf(sessionID, b))
	})
	for _, commandID := range ordered {
		outcome := outcomes[commandID]
		prompt, recalled, known := p.knownPrompt(sessionID, commandID)
		if !known {
			continue
		}
		var replyResult error
		switch outcome := outcome.(type) {
		case client.StartupPromptOutcomeRefused:
			replyResult = errors.New(outcome.Reason)
		case client.StartupPromptOutcomeWithdrawn:
			replyResult = errors.New("The reply was withdrawn")
		}
		if finishNotificationReply(dashboard, chats, sessionID, &prompt, replyResult) {
			p.settle(sessionID, commandID)
			continue
		}
		active := !dashboard.HasStandbyComposer(sessionID)
		activeChat := chats[sessionID]
		if !active {
			activeChat = nil
		}
		var applied error
		switch outcome := outcome.(type) {
		case client.StartupPromptOutcomeRefused:
			if !recalled {
				payload, err := chat.PromptPayloadDecodeDraft(outcome.EncodedDraft)
				if err != nil {
					applied = err
				} else {
					prompt.Payload = payload
					if activeChat != nil {
						applied = activeChat.RestoreStartupPrompt(prompt)
					} else {
						applied = dashboard.RestoreStartupPrompt(sessionID, prompt)
					}
				}
			}
			dashboard.SetFailureNotice(fmt.Sprintf("Queued prompt returned to the draft: %s", outcome.Reason))
		case client.StartupPromptOutcomeAccepted:
			if recalled {
				if activeChat != nil {
					_, applied = activeChat.StartupPromptWithdrawn(prompt, false, nil)
				} else {
					_, applied = dashboard.StartupPromptWithdrawn(sessionID, prompt, false, nil)
				}
			}
		}
		if applied != nil {
			dashboard.SetFailureNotice(fmt.Sprintf("Could not restore the queued prompt yet; it remains saved: %s", applied))
			continue
		}
		if activeChat != nil {
			activeChat.SettleStartupPreview(commandID)
		} else {
			dashboard.SettleStartupPreview(sessionID, commandID)
		}
		p.settle(sessionID, commandID)
	}
}

func (p *startupPrompts) orderOf(sessionID, commandID string) uint64 {
	if waiting, ok := p.pending[sessionID][commandID].(*trackedPromptWaiting); ok {
		return waiting.order
	}
	return ^uint64(0)
}

// knownPrompt is a waiting prompt and whether it was recalled; ok is false
// when it is unknown or settled.
func (p *startupPrompts) knownPrompt(sessionID, commandID string) (prompt chat.StartupPrompt, recalled bool, ok bool) {
	if waiting, waits := p.pending[sessionID][commandID].(*trackedPromptWaiting); waits {
		return waiting.prompt, waiting.recalled, true
	}
	return chat.StartupPrompt{}, false, false
}

func (p *startupPrompts) settle(sessionID, commandID string) {
	entries := p.pending[sessionID]
	entry, ok := entries[commandID]
	if !ok {
		return
	}
	if waiting, waits := entry.(*trackedPromptWaiting); waits {
		waiting.retire()
	}
	entries[commandID] = trackedPromptSettled{}
}

// retire ends a waiting prompt's settlement and lets go of its held save,
// which is what Rust's replacing the entry with Settled drops.
func (w *trackedPromptWaiting) retire() {
	w.settle()
	if w.held != nil {
		w.held.guard.Release()
	}
}
