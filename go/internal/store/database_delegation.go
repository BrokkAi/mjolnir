// Port of mj-store/src/database/delegation.rs.

// Durable effect identity is independent of the worker's pending request
// queue.

package store

import (
	"crypto/sha256"
	"encoding/json"
	"fmt"

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
	panic("unported: mj-store/src/database/delegation.rs prepare_delegation_spawn")
}

// LoadDelegation is the prepared effect and its stored result, if any; a nil
// PreparedDelegation means no effect is recorded.
func LoadDelegation(parent, request string) (*PreparedDelegation, *StoredDelegationResult, error) {
	panic("unported: mj-store/src/database/delegation.rs load_delegation")
}

// HasPendingMutatingDelegations reports whether a parent has an accepted
// child mutation that the daemon has not completed yet. The worker queue
// alone is insufficient: delivery removes a request before its durable effect
// is acknowledged.
func HasPendingMutatingDelegations(parent string) (bool, error) {
	panic("unported: mj-store/src/database/delegation.rs has_pending_mutating_delegations")
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
	panic("unported: mj-store/src/database/delegation.rs prepare_delegation")
}

func DelegationDelivering(parent, request string) error {
	panic("unported: mj-store/src/database/delegation.rs delegation_delivering")
}

func RecordDelegationResult(parent string, result core.SubagentToolResult) error {
	return RecordStoredDelegationResult(parent, StoredDelegationResult{Result: result})
}

// RecordStoredDelegationResult persists the result before returning it to
// the worker.
func RecordStoredDelegationResult(parent string, storedResult StoredDelegationResult) error {
	panic("unported: mj-store/src/database/delegation.rs record_stored_delegation_result")
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
	panic("unported: mj-store/src/database/delegation.rs untold_failed_deliveries")
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
	panic("unported: mj-store/src/database/delegation.rs untold_failed_deliveries_with")
}

// failuresListedInReports is the failed requests that child's reports to
// parent listed as failed; false when the parent has nothing to be told of
// its messages to child.
func failuresListedInReports(connection Queryer, parent, child string) (map[string]struct{}, bool, error) {
	panic("unported: mj-store/src/database/delegation.rs failures_listed_in_reports")
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
	panic("unported: mj-store/src/database/delegation.rs finishes_told_by_folded_markers")
}

// toldByFoldedMarker reports whether reported, a child's stored report
// marker, is the one a report before #1459 wrote for finish. Those reports
// folded the child's newest failed delivery into the marker of the finish
// they told: a Terminal with the finish's state and last completed turn,
// whose detail is `Message <id>: <error>`. The marker's own report row, which
// listed that delivery as failed, says so; what the parent's queue shows now
// does not.
func toldByFoldedMarker(connection Queryer, parent, child string, reported, finish *core.SubagentFinishIdentity) (bool, error) {
	panic("unported: mj-store/src/database/delegation.rs told_by_folded_marker")
}

// SubagentFinishReport is one finished child's report for
// EnqueueSubagentReports, with the finish identity it tells.
type SubagentFinishReport struct {
	Identity core.SubagentFinishIdentity
	Report   core.SubagentFinishedReport
}

// EnqueueSubagentReports: the outbox rows and the report marker have one
// serialized database owner. A failed delivery is told once by its own
// notice, keyed by its request, so a later delivery cannot hide it and a
// stale or concurrent reconcile cannot tell it twice. Notices go first: a
// finish enqueued here lists the same failures, and the notice's key, not
// that listing, is what records them.
func EnqueueSubagentReports(parent string, reports []SubagentFinishReport, failures []FailedDeliveryNotice) error {
	panic("unported: mj-store/src/database/delegation.rs enqueue_subagent_reports")
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
	panic("unported: mj-store/src/database/delegation.rs load_report_mail")
}

// ChildHasUndeliveredMail reports whether the parent has put a message
// (subagent-message-<request>) into the child's mailbox that the child's
// worker has not accepted yet. A park that stopped the worker under it would
// drop the child's next task.
func ChildHasUndeliveredMail(child string) (bool, error) {
	panic("unported: mj-store/src/database/delegation.rs child_has_undelivered_mail")
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
	panic("unported: mj-store/src/database/delegation.rs retire_legacy_wait")
}

// AcknowledgeDelegation: the parent has its result, so the record has done
// its one job: making a replayed request return the same outcome instead of
// running again.
func AcknowledgeDelegation(parent, request string) error {
	panic("unported: mj-store/src/database/delegation.rs acknowledge_delegation")
}

// PruneAcknowledgedDelegations removes acknowledged records from builds that
// kept them after the parent had its result. Nothing reads them; run once at
// daemon start.
func PruneAcknowledgedDelegations() error {
	panic("unported: mj-store/src/database/delegation.rs prune_acknowledged_delegations")
}
