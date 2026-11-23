// Port of mj-store/src/database/delegation.rs.

// Durable effect identity is independent of the worker's pending request
// queue.

package store

import (
	"crypto/sha256"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"reflect"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type StoredDelegationResult struct {
	Result core.SubagentToolResult
	// ReportedFinishes decodes result rows persisted by wait-based builds;
	// new results never collect reports. Read but never written (serde
	// default, skip_serializing).
	ReportedFinishes []ReportedFinish
}

func (r StoredDelegationResult) MarshalJSON() ([]byte, error) {
	return databaseMarshalJSON(struct {
		Result core.SubagentToolResult `json:"result"`
	}{r.Result})
}

func (r *StoredDelegationResult) UnmarshalJSON(data []byte) error {
	if err := databaseCheckFields(data, false, nil, []string{"result"}); err != nil {
		return err
	}
	var wire struct {
		Result           core.SubagentToolResult `json:"result"`
		ReportedFinishes []ReportedFinish        `json:"reported_finishes"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	*r = StoredDelegationResult{Result: wire.Result, ReportedFinishes: wire.ReportedFinishes}
	return nil
}

// ReportedFinish is one (child session, finish identity) pair of
// StoredDelegationResult.ReportedFinishes, a JSON array of two.
type ReportedFinish struct {
	Child    string
	Identity core.SubagentFinishIdentity
}

func (f ReportedFinish) MarshalJSON() ([]byte, error) {
	return databaseMarshalJSON([]any{f.Child, f.Identity})
}

func (f *ReportedFinish) UnmarshalJSON(data []byte) error {
	var pair []json.RawMessage
	if err := json.Unmarshal(data, &pair); err != nil {
		return err
	}
	if len(pair) != 2 {
		return fmt.Errorf("invalid length %d, expected a tuple of size 2", len(pair))
	}
	if err := json.Unmarshal(pair[0], &f.Child); err != nil {
		return err
	}
	return json.Unmarshal(pair[1], &f.Identity)
}

func decodeStoredResult(body string) (StoredDelegationResult, error) {
	var value any
	if err := json.Unmarshal([]byte(body), &value); err != nil {
		return StoredDelegationResult{}, err
	}
	if object, ok := value.(map[string]any); ok {
		if _, present := object["result"]; present {
			var stored StoredDelegationResult
			err := json.Unmarshal([]byte(body), &stored)
			return stored, err
		}
	}
	// Results written before report identities were persisted are still
	// replayable; their marker list is empty because the old format did not
	// record which children the answer contained.
	var result core.SubagentToolResult
	if err := json.Unmarshal([]byte(body), &result); err != nil {
		return StoredDelegationResult{}, err
	}
	return StoredDelegationResult{Result: result}, nil
}

type PreparedDelegation struct {
	Request core.SubagentToolRequest `json:"request"`
	// TurnTarget nil is a persisted decision that there was no running turn
	// to target.
	TurnTarget *string        `json:"turn_target"`
	Spawn      *PreparedSpawn `json:"spawn"`
}

// UnmarshalJSON requires request, as serde does.
func (d *PreparedDelegation) UnmarshalJSON(data []byte) error {
	if err := databaseCheckFields(data, false, nil, []string{"request"}); err != nil {
		return err
	}
	type plain PreparedDelegation
	return json.Unmarshal(data, (*plain)(d))
}

type PreparedSpawn struct {
	ProfileID string  `json:"profile_id"`
	Model     string  `json:"model"`
	Effort    *string `json:"effort"`
	FastMode  bool    `json:"fast_mode"`
	Prompt    string  `json:"prompt"`
}

// UnmarshalJSON requires every member but effort, as serde does.
func (s *PreparedSpawn) UnmarshalJSON(data []byte) error {
	if err := databaseCheckFields(data, false, nil,
		[]string{"profile_id", "model", "fast_mode", "prompt"}); err != nil {
		return err
	}
	type plain PreparedSpawn
	return json.Unmarshal(data, (*plain)(s))
}

func PrepareDelegationSpawn(parent, request string, spawn PreparedSpawn) (PreparedSpawn, error) {
	return submitDatabaseWrite("prepare delegation spawn", func(connection *Connection) (PreparedSpawn, error) {
		var stored string
		if err := connection.QueryRow("SELECT prepared_json FROM delegation_effects WHERE parent_session_id=?1 AND request_id=?2",
			parent, request).Scan(&stored); err != nil {
			return PreparedSpawn{}, err
		}
		var prepared PreparedDelegation
		if err := json.Unmarshal([]byte(stored), &prepared); err != nil {
			return PreparedSpawn{}, err
		}
		if prepared.Spawn != nil {
			return *prepared.Spawn, nil
		}
		chosen := spawn
		prepared.Spawn = &chosen
		encoded, err := databaseMarshalJSON(prepared)
		if err != nil {
			return PreparedSpawn{}, err
		}
		if _, err := connection.Exec("UPDATE delegation_effects SET prepared_json=?3 WHERE parent_session_id=?1 AND request_id=?2",
			parent, request, string(encoded)); err != nil {
			return PreparedSpawn{}, err
		}
		return spawn, nil
	})
}

// LoadDelegation is the prepared effect and its stored result, if any; a nil
// PreparedDelegation means no effect is recorded.
func LoadDelegation(parent, request string) (*PreparedDelegation, *StoredDelegationResult, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, nil, err
	}
	defer connection.Close()
	var preparedText string
	var resultText *string
	err = connection.QueryRow("SELECT prepared_json, result_json FROM delegation_effects WHERE parent_session_id=?1 AND request_id=?2",
		parent, request).Scan(&preparedText, &resultText)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil, nil
	}
	if err != nil {
		return nil, nil, err
	}
	var prepared PreparedDelegation
	if err := json.Unmarshal([]byte(preparedText), &prepared); err != nil {
		return nil, nil, err
	}
	if resultText == nil {
		return &prepared, nil, nil
	}
	result, err := decodeStoredResult(*resultText)
	if err != nil {
		return nil, nil, err
	}
	return &prepared, &result, nil
}

// HasPendingMutatingDelegations reports whether a parent has an accepted
// child mutation that the daemon has not completed yet. The worker queue
// alone is insufficient: delivery removes a request before its durable effect
// is acknowledged.
func HasPendingMutatingDelegations(parent string) (bool, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return false, err
	}
	defer connection.Close()
	return hasPendingMutatingDelegationsWith(connection, parent)
}

func hasPendingMutatingDelegationsWith(connection Queryer, parent string) (bool, error) {
	rows, err := connection.Query(`SELECT prepared_json FROM delegation_effects
         WHERE parent_session_id=?1 AND result_json IS NULL`, parent)
	if err != nil {
		return false, err
	}
	defer rows.Close()
	for rows.Next() {
		var body string
		var prepared PreparedDelegation
		if err := rows.Scan(&body); err != nil {
			return false, err
		}
		if err := json.Unmarshal([]byte(body), &prepared); err != nil {
			return false, err
		}
		if prepared.Request.Action.MutatesChildState() {
			return true, nil
		}
	}
	return false, rows.Err()
}

func PrepareDelegation(parent string, prepared PreparedDelegation) (PreparedDelegation, error) {
	return submitDatabaseWrite("prepare delegation", func(connection *Connection) (PreparedDelegation, error) {
		transaction, err := connection.Begin()
		if err != nil {
			return PreparedDelegation{}, err
		}
		defer transaction.Rollback()
		if prepared.Request.Action.MutatesChildState() {
			if err := ensureContainerParentAvailable(transaction, parent); err != nil {
				return PreparedDelegation{}, err
			}
		}
		encoded, err := databaseMarshalJSON(prepared)
		if err != nil {
			return PreparedDelegation{}, err
		}
		if _, err := transaction.Exec("INSERT INTO delegation_effects(parent_session_id,request_id,phase,prepared_json,result_json) VALUES (?1,?2,'prepared',?3,NULL) ON CONFLICT(parent_session_id,request_id) DO NOTHING",
			parent, prepared.Request.RequestID, string(encoded)); err != nil {
			return PreparedDelegation{}, err
		}
		var stored string
		if err := transaction.QueryRow("SELECT prepared_json FROM delegation_effects WHERE parent_session_id=?1 AND request_id=?2",
			parent, prepared.Request.RequestID).Scan(&stored); err != nil {
			return PreparedDelegation{}, err
		}
		if err := transaction.Commit(); err != nil {
			return PreparedDelegation{}, err
		}
		var restored PreparedDelegation
		if err := json.Unmarshal([]byte(stored), &restored); err != nil {
			return PreparedDelegation{}, err
		}
		return restored, nil
	})
}

func DelegationDelivering(parent, request string) error {
	_, err := submitDatabaseWrite("deliver delegation effect", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec("UPDATE delegation_effects SET phase='delivering' WHERE parent_session_id=?1 AND request_id=?2 AND result_json IS NULL",
			parent, request)
		return struct{}{}, err
	})
	return err
}

func RecordDelegationResult(parent string, result core.SubagentToolResult) error {
	return RecordStoredDelegationResult(parent, StoredDelegationResult{Result: result})
}

// RecordStoredDelegationResult persists the result before returning it to
// the worker.
func RecordStoredDelegationResult(parent string, storedResult StoredDelegationResult) error {
	_, err := submitDatabaseWrite("record delegation result", func(connection *Connection) (struct{}, error) {
		result := storedResult.Result
		transaction, err := connection.BeginWith(TransactionImmediate)
		if err != nil {
			return struct{}{}, err
		}
		defer transaction.Rollback()
		body, err := databaseMarshalJSON(StoredDelegationResult{
			Result:           result,
			ReportedFinishes: storedResult.ReportedFinishes,
		})
		if err != nil {
			return struct{}{}, err
		}
		updated, err := transaction.Exec("UPDATE delegation_effects SET phase='result', result_json=?3 WHERE parent_session_id=?1 AND request_id=?2 AND result_json IS NULL",
			parent, result.RequestID, string(body))
		if err != nil {
			return struct{}{}, err
		}
		changed, err := updated.RowsAffected()
		if err != nil {
			return struct{}{}, err
		}
		if changed != 1 {
			return struct{}{}, errors.New("delegation result has no pending durable effect")
		}
		return struct{}{}, transaction.Commit()
	})
	return err
}

// FailedDeliveryNotice is a notice that one message a parent sent its child
// was not delivered. The notice is not a finish: it never moves the child's
// reported_finish.
type FailedDeliveryNotice struct {
	RequestID string
	Report    core.SubagentFinishedReport
}

// UntoldFailedDeliveries is the notices among notices whose failure the
// parent has not been told of. This read only spares the writer a
// transaction when nothing is new; EnqueueSubagentReports asks the same
// question again where it decides.
func UntoldFailedDeliveries(parent string, notices []FailedDeliveryNotice) ([]FailedDeliveryNotice, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	return untoldFailedDeliveriesWith(connection, parent, notices)
}

// untoldFailedDeliveriesWith is the notices whose failure the parent has not
// been told of. A parent has nothing to be told of a child that is not its
// own, or of a reviewer, which its review reports.
//
// A failure was told when its notice's outbox row exists, and report rows
// are never pruned while the parent exists. Reports before #1459 told
// failures only inside report rows: they folded the child's newest failure
// into its finish marker and listed all of the child's deliveries, so a
// failure that any of the child's reports listed as failed was told too.
// Every such row is read, not only the latest: the deliveries a reader sees
// can shrink and grow back (a parent queue that could not be read, a bounded
// window that pruning lets older messages back into), so the latest report
// need not list a failure that an earlier one did. The oracle's report path
// never showed delivery lines, so an oracle report told no failure.
func untoldFailedDeliveriesWith(connection Queryer, parent string, notices []FailedDeliveryNotice) ([]FailedDeliveryNotice, error) {
	type listing struct {
		told     bool
		requests map[string]struct{}
	}
	reported := map[string]listing{}
	untold := make([]FailedDeliveryNotice, 0, len(notices))
	for _, notice := range notices {
		child := notice.Report.ChildSessionID
		key, err := failedDeliveryKey(parent, child, notice.RequestID)
		if err != nil {
			return nil, err
		}
		var exists bool
		if err := connection.QueryRow("SELECT EXISTS(SELECT 1 FROM mailbox_outbox WHERE event_key=?1)", key).Scan(&exists); err != nil {
			return nil, err
		}
		if exists {
			continue
		}
		listed, ok := reported[child]
		if !ok {
			requests, applies, err := failuresListedInReports(connection, parent, child)
			if err != nil {
				return nil, err
			}
			listed = listing{told: !applies, requests: requests}
			reported[child] = listed
		}
		if listed.told {
			continue
		}
		if _, ok := listed.requests[notice.RequestID]; ok {
			continue
		}
		untold = append(untold, notice)
	}
	return untold, nil
}

// failuresListedInReports is the failed requests that child's reports to
// parent listed as failed; false when the parent has nothing to be told of
// its messages to child.
func failuresListedInReports(connection Queryer, parent, child string) (map[string]struct{}, bool, error) {
	stored, err := environmentOptionalString(connection,
		"SELECT record_json FROM subagent_sessions WHERE child_session_id=?1 AND parent_session_id=?2", child, parent)
	if err != nil {
		return nil, false, err
	}
	if stored == nil {
		return nil, false, nil
	}
	var relation core.SubagentRecord
	if err := json.Unmarshal([]byte(*stored), &relation); err != nil {
		return nil, false, err
	}
	if relation.Kind == core.SubagentKindReviewer {
		return nil, false, nil
	}
	rows, err := connection.Query(`SELECT DISTINCT json_extract(delivery.value, '$.request_id')
         FROM mailbox_outbox AS report,
              json_each(report.event_json, '$.body.report.message_deliveries') AS delivery
         WHERE report.target_session_id = ?1
           AND report.event_key LIKE 'subagent-finished-%'
           AND json_extract(report.event_json, '$.body.report.child_session_id') = ?2
           AND IFNULL(json_extract(report.event_json, '$.body.report.oracle'), 0) = 0
           AND json_extract(delivery.value, '$.status') = 'failed'`, parent, child)
	if err != nil {
		return nil, false, err
	}
	defer rows.Close()
	listed := map[string]struct{}{}
	for rows.Next() {
		var request *string
		if err := rows.Scan(&request); err != nil {
			return nil, false, err
		}
		if request != nil {
			listed[*request] = struct{}{}
		}
	}
	return listed, true, rows.Err()
}

// FoldedMarkerCandidate is one candidate of FinishesToldByFoldedMarkers: a
// child, its stored report marker and its finish now.
type FoldedMarkerCandidate struct {
	Child    string
	Reported core.SubagentFinishIdentity
	Finish   core.SubagentFinishIdentity
}

// FinishesToldByFoldedMarkers is the children among candidates whose finish
// a report before #1459 already told. This read only spares the writer a
// transaction; EnqueueSubagentReports asks the same question again where it
// decides.
func FinishesToldByFoldedMarkers(parent string, candidates []FoldedMarkerCandidate) (map[string]struct{}, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	told := map[string]struct{}{}
	for _, candidate := range candidates {
		folded, err := toldByFoldedMarker(connection, parent, candidate.Child, &candidate.Reported, &candidate.Finish)
		if err != nil {
			return nil, err
		}
		if folded {
			told[candidate.Child] = struct{}{}
		}
	}
	return told, nil
}

// toldByFoldedMarker reports whether reported, a child's stored report
// marker, is the one a report before #1459 wrote for finish. Those reports
// folded the child's newest failed delivery into the marker of the finish
// they told: a Terminal with the finish's state and last completed turn,
// whose detail is `Message <id>: <error>`. The marker's own report row, which
// listed that delivery as failed, says so; what the parent's queue shows now
// does not.
func toldByFoldedMarker(connection Queryer, parent, child string, reported, finish *core.SubagentFinishIdentity) (bool, error) {
	if reported.Kind != core.SubagentFinishIdentityTerminal || reported.Detail == nil {
		return false, nil
	}
	detail := *reported.Detail
	var completed *uint64
	if ordinal, ok := finish.CompletedOrdinalOf(); ok {
		completed = &ordinal
	}
	if reported.State != finish.State || !committedOptionalEqual(reported.LastCompletedOrdinal, completed) {
		return false, nil
	}
	key, err := subagentReportKey(parent, child, reported)
	if err != nil {
		return false, err
	}
	stored, err := environmentOptionalString(connection, "SELECT event_json FROM mailbox_outbox WHERE event_key=?1", key)
	if err != nil || stored == nil {
		return false, err
	}
	var event core.MailboxEvent
	if err := json.Unmarshal([]byte(*stored), &event); err != nil {
		return false, err
	}
	body, ok := event.Body.(core.MailboxEventBodySubagentFinished)
	if !ok || body.Report == nil {
		return false, nil
	}
	for _, raw := range body.Report.MessageDeliveries {
		delivery, err := jsonvalue.Parse(raw)
		if err != nil {
			return false, err
		}
		status, _ := jsonValueGet(delivery, "status")
		if status != "failed" {
			continue
		}
		requestValue, _ := jsonValueGet(delivery, "request_id")
		request, _ := requestValue.(string)
		errorValue, _ := jsonValueGet(delivery, "error")
		failure, ok := errorValue.(string)
		if !ok {
			failure = "delivery failed"
		}
		if detail == fmt.Sprintf("Message %s: %s", request, failure) {
			return true, nil
		}
	}
	return false, nil
}

// SubagentFinishReport is one finished child's report for
// EnqueueSubagentReports, with the finish identity it tells.
type SubagentFinishReport struct {
	Identity core.SubagentFinishIdentity
	Report   core.SubagentFinishedReport
}

// subagentFinishedEvent is the mailbox event that carries report under key.
func subagentFinishedEvent(key string, report core.SubagentFinishedReport) (string, error) {
	event := core.MailboxEvent{
		Key:         key,
		Source:      "mjolnir",
		Wake:        true,
		CreatedAtMs: uint64(max(core.EpochMillis(), 0)),
		Body:        core.MailboxEventBodySubagentFinished{Report: &report},
	}
	encoded, err := databaseMarshalJSON(event)
	if err != nil {
		return "", err
	}
	return string(encoded), nil
}

// EnqueueSubagentReports: the outbox rows and the report marker have one
// serialized database owner. A failed delivery is told once by its own
// notice, keyed by its request, so a later delivery cannot hide it and a
// stale or concurrent reconcile cannot tell it twice. Notices go first: a
// finish enqueued here lists the same failures, and the notice's key, not
// that listing, is what records them.
func EnqueueSubagentReports(parent string, reports []SubagentFinishReport, failures []FailedDeliveryNotice) error {
	inserted, err := submitDatabaseWrite("enqueue finished child reports", func(connection *Connection) (bool, error) {
		transaction, err := connection.Begin()
		if err != nil {
			return false, err
		}
		defer transaction.Rollback()
		inserted := false
		untold, err := untoldFailedDeliveriesWith(transaction, parent, failures)
		if err != nil {
			return false, err
		}
		for _, notice := range untold {
			child := notice.Report.ChildSessionID
			key, err := failedDeliveryKey(parent, child, notice.RequestID)
			if err != nil {
				return false, err
			}
			event, err := subagentFinishedEvent(key, notice.Report)
			if err != nil {
				return false, err
			}
			enqueued, err := EnqueueMailboxEventWith(transaction, key, parent, event, true, false)
			if err != nil {
				return false, err
			}
			inserted = enqueued.Inserted() || inserted
		}
		for _, finished := range reports {
			identity := finished.Identity
			childID := finished.Report.ChildSessionID
			var stored string
			if err := transaction.QueryRow("SELECT record_json FROM subagent_sessions WHERE child_session_id=?1 AND parent_session_id=?2",
				childID, parent).Scan(&stored); err != nil {
				return false, err
			}
			var relation core.SubagentRecord
			if err := json.Unmarshal([]byte(stored), &relation); err != nil {
				return false, err
			}
			if relation.Kind == core.SubagentKindReviewer {
				continue
			}
			if relation.ReportedFinish != nil && reflect.DeepEqual(*relation.ReportedFinish, identity) {
				continue
			}
			if relation.ReportedFinish != nil {
				told, err := toldByFoldedMarker(transaction, parent, childID, relation.ReportedFinish, &identity)
				if err != nil {
					return false, err
				}
				if told {
					continue
				}
			}
			key, err := subagentReportKey(parent, childID, &identity)
			if err != nil {
				return false, err
			}
			// A stale reconciliation may arrive after a newer finish. Its
			// event is already durable; neither replace its data nor move
			// the latest marker back to that older finish.
			var alreadyEnqueued bool
			if err := transaction.QueryRow("SELECT EXISTS(SELECT 1 FROM mailbox_outbox WHERE event_key=?1)", key).Scan(&alreadyEnqueued); err != nil {
				return false, err
			}
			if alreadyEnqueued {
				continue
			}
			event, err := subagentFinishedEvent(key, finished.Report)
			if err != nil {
				return false, err
			}
			if _, err := EnqueueMailboxEventWith(transaction, key, parent, event, true, false); err != nil {
				return false, err
			}
			inserted = true
			relation.ReportedFinish = &identity
			encoded, err := databaseMarshalJSON(relation)
			if err != nil {
				return false, err
			}
			if _, err := transaction.Exec("UPDATE subagent_sessions SET record_json=?2 WHERE child_session_id=?1",
				relation.ChildSessionID, string(encoded)); err != nil {
				return false, err
			}
		}
		return inserted, transaction.Commit()
	})
	if err != nil {
		return err
	}
	if inserted {
		NotifyMailboxOutboxChanged()
	}
	return nil
}

// ReportMail is where the parent's copy of a child's latest report stands in
// the mailbox outbox: the event key and the journal ordinal at which the
// parent's worker accepted it, once it has.
type ReportMail struct {
	Key             string
	AcceptedOrdinal *uint64
}

// LoadReportMail is nil when no report was enqueued for the child's latest
// finish.
func LoadReportMail(parent, child string) (*ReportMail, error) {
	relation, err := LoadSubagent(child)
	if err != nil {
		return nil, err
	}
	if relation == nil || relation.ReportedFinish == nil {
		return nil, nil
	}
	key, err := subagentReportKey(parent, child, relation.ReportedFinish)
	if err != nil {
		return nil, err
	}
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	var accepted *int64
	err = connection.QueryRow("SELECT accepted_ordinal FROM mailbox_outbox WHERE event_key=?1", key).Scan(&accepted)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	mail := &ReportMail{Key: key}
	if accepted != nil && *accepted >= 0 {
		ordinal := uint64(*accepted)
		mail.AcceptedOrdinal = &ordinal
	}
	return mail, nil
}

// ChildHasUndeliveredMail reports whether the parent has put a message
// (subagent-message-<request>) into the child's mailbox that the child's
// worker has not accepted yet. A park that stopped the worker under it would
// drop the child's next task.
func ChildHasUndeliveredMail(child string) (bool, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return false, err
	}
	defer connection.Close()
	var undelivered bool
	err = connection.QueryRow(`SELECT EXISTS(SELECT 1 FROM mailbox_outbox
         WHERE target_session_id=?1 AND accepted_at IS NULL
           AND event_key LIKE 'subagent-message-%')`, child).Scan(&undelivered)
	return undelivered, err
}

func subagentReportKey(parent, child string, identity *core.SubagentFinishIdentity) (string, error) {
	encoded, err := databaseMarshalJSON([]any{parent, child, identity})
	if err != nil {
		return "", err
	}
	digest := sha256.Sum256(encoded)
	return "subagent-finished-" + core.LowerHex(digest[:]), nil
}

// failedDeliveryKey is one key per failed request. It must not start with
// subagent-message-, which names a parent's message to its child.
func failedDeliveryKey(parent, child, requestID string) (string, error) {
	encoded, err := databaseMarshalJSON([]any{parent, child, requestID})
	if err != nil {
		return "", err
	}
	digest := sha256.Sum256(encoded)
	return "subagent-delivery-failed-" + core.LowerHex(digest[:]), nil
}

// RetireLegacyWait moves old wait-owned reports to the mailbox and durably
// replaces its answer. The compatibility result remains replayable until
// worker acknowledgement.
func RetireLegacyWait(parent, request string) (*StoredDelegationResult, error) {
	retired, err := submitDatabaseWrite("retire legacy wait", func(connection *Connection) (*StoredDelegationResult, error) {
		transaction, err := connection.Begin()
		if err != nil {
			return nil, err
		}
		defer transaction.Rollback()
		var preparedText string
		var resultText *string
		err = transaction.QueryRow("SELECT prepared_json, result_json FROM delegation_effects WHERE parent_session_id=?1 AND request_id=?2",
			parent, request).Scan(&preparedText, &resultText)
		if errors.Is(err, sql.ErrNoRows) {
			return nil, nil
		}
		if err != nil {
			return nil, err
		}
		var prepared PreparedDelegation
		if err := json.Unmarshal([]byte(preparedText), &prepared); err != nil {
			return nil, err
		}
		switch prepared.Request.Action.(type) {
		case core.SubagentToolActionLegacyWaitAgents, core.SubagentToolActionLegacyAwaitChildren:
		default:
			return nil, nil
		}
		if resultText == nil {
			return nil, nil
		}
		stored, err := decodeStoredResult(*resultText)
		if err != nil {
			return nil, err
		}
		if len(stored.ReportedFinishes) > 0 {
			payload, err := jsonvalue.Parse([]byte(stored.Result.Message))
			if err != nil {
				return nil, err
			}
			finishes := stored.ReportedFinishes
			stored.ReportedFinishes = nil
			for _, finished := range finishes {
				child, identity := finished.Child, finished.Identity
				relationText, err := environmentOptionalString(transaction,
					"SELECT record_json FROM subagent_sessions WHERE child_session_id=?1 AND parent_session_id=?2", child, parent)
				if err != nil {
					return nil, err
				}
				if relationText == nil {
					continue
				}
				var relation core.SubagentRecord
				if err := json.Unmarshal([]byte(*relationText), &relation); err != nil {
					return nil, err
				}
				if relation.ReportedFinish == nil || !reflect.DeepEqual(*relation.ReportedFinish, identity) {
					continue
				}
				var entry jsonvalue.Object
				agentsValue, _ := jsonValueGet(payload, "agents")
				if agents, ok := agentsValue.([]any); ok {
					for _, candidate := range agents {
						if id, _ := jsonValueGet(candidate, "child_session_id"); id == child {
							entry = candidate.(jsonvalue.Object).Clone()
							break
						}
					}
				}
				if entry == nil {
					return nil, errors.New("legacy wait is missing its marked child report")
				}
				// Name the child as the session list does now; a child
				// whose record is gone keeps the task name.
				var title string
				var titleOverride, acpTitle *string
				err = transaction.QueryRow(`SELECT title, session_title_override, acp_session_title
                             FROM sessions WHERE session_id=?1`, child).Scan(&title, &titleOverride, &acpTitle)
				switch {
				case err == nil:
					entry.Set("child_name", core.BoundedOneLine(
						core.ListedSessionTitle(child, title, titleOverride, acpTitle), core.MessageNameChars))
				case !errors.Is(err, sql.ErrNoRows):
					return nil, err
				}
				entry.Set("task_name", relation.TaskName)
				var report core.SubagentFinishedReport
				if err := jsonValueDecode(entry, &report); err != nil {
					return nil, err
				}
				key, err := subagentReportKey(parent, child, &identity)
				if err != nil {
					return nil, err
				}
				var exists bool
				if err := transaction.QueryRow("SELECT EXISTS(SELECT 1 FROM mailbox_outbox WHERE event_key=?1)", key).Scan(&exists); err != nil {
					return nil, err
				}
				if exists {
					continue
				}
				// The old model may have seen this report before an
				// acknowledgement was lost. Deliver it again rather than
				// risk losing it on upgrade.
				event, err := subagentFinishedEvent(key, report)
				if err != nil {
					return nil, err
				}
				if _, err := EnqueueMailboxEventWith(transaction, key, parent, event, true, false); err != nil {
					return nil, err
				}
			}
		}
		stored.Result.IsError = false
		message, err := databaseMarshalJSON(struct {
			Status     string `json:"status"`
			NextAction string `json:"next_action"`
		}{"reports_arrive_as_messages", core.ReportNotificationNextAction})
		if err != nil {
			return nil, err
		}
		stored.Result.Message = string(message)
		encoded, err := databaseMarshalJSON(stored)
		if err != nil {
			return nil, err
		}
		if _, err := transaction.Exec("UPDATE delegation_effects SET phase='result', result_json=?3 WHERE parent_session_id=?1 AND request_id=?2",
			parent, request, string(encoded)); err != nil {
			return nil, err
		}
		if err := transaction.Commit(); err != nil {
			return nil, err
		}
		return &stored, nil
	})
	if err != nil {
		return nil, err
	}
	if retired != nil {
		NotifyMailboxOutboxChanged()
	}
	return retired, nil
}

// AcknowledgeDelegation: the parent has its result, so the record has done
// its one job: making a replayed request return the same outcome instead of
// running again.
func AcknowledgeDelegation(parent, request string) error {
	_, err := submitDatabaseWrite("acknowledge delegation", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec("DELETE FROM delegation_effects WHERE parent_session_id=?1 AND request_id=?2 AND result_json IS NOT NULL",
			parent, request)
		return struct{}{}, err
	})
	return err
}

// PruneAcknowledgedDelegations removes acknowledged records from builds that
// kept them after the parent had its result. Nothing reads them; run once at
// daemon start.
func PruneAcknowledgedDelegations() error {
	_, err := submitDatabaseWrite("prune acknowledged delegations", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec("DELETE FROM delegation_effects WHERE phase='acknowledged'")
		return struct{}{}, err
	})
	return err
}
