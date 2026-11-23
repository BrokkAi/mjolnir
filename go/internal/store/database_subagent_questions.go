// Port of mj-store/src/database/subagent_questions.rs.

// Child questions and their parent mailbox event share the projection
// transaction.

package store

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// forwardSubagentQuestionWith forwards a child's question to its live parent
// as a session message; false when nothing was forwarded.
func forwardSubagentQuestionWith(tx *Transaction, child string, seq uint64, recordedAtMs int64, request *core.ElicitationRequest) (bool, error) {
	panic("unported: mj-store/src/database/subagent_questions.rs forward_subagent_question_with")
}

// ReconcilePendingSubagentQuestions recovers the pending questions older
// workers left open, without replaying historical questions that were
// already answered before this build.
func ReconcilePendingSubagentQuestions() (int, error) {
	return submitDatabaseWrite("reconcile pending child questions", reconcilePendingSubagentQuestionsWith)
}

func reconcilePendingSubagentQuestionsWith(conn *Connection) (int, error) {
	panic("unported: mj-store/src/database/subagent_questions.rs reconcile_pending_subagent_questions_with")
}
