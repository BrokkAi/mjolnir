// Port of mj-core/src/activity/question_locator.rs.

// Locate an open request in the authorization history after an input verdict.

package core

import (
	_ "embed"
	"encoding/json"
	"strings"
	"sync"
)

const maxReplies = 32
const stateBytes = 60 * 1024

type Reply struct {
	Reply int    `json:"reply"`
	Text  string `json:"text"`
}

type State struct {
	LatestUserMessage string  `json:"latest_user_message"`
	Replies           []Reply `json:"replies"`
}

type Request struct {
	State State
	// MessageIDs are the authorization message IDs in reply order. Live
	// context can carry raw ACP IDs.
	MessageIDs []string
}

// TranscriptStableID resolves an authorization assistant ID to a
// materialized transcript ID.
//
// Both the live context and transcript projection use the same relay event
// ordinal when ACP supplies no message ID. The projection can nevertheless
// coalesce that chunk into an earlier item, so the numeric fallback is not a
// reliable target without consulting the materialized transcript.
func TranscriptStableID(id string) (string, bool) {
	if id == "" {
		return "", false
	}
	if suffix, ok := strings.CutPrefix(id, "agent:"); ok {
		if strings.Trim(suffix, "0123456789") == "" {
			return "", false
		}
		return id, true
	}
	return "agent:" + id, true
}

func clip(text string, budget int) string {
	if len(text) <= budget {
		return text
	}
	head := budget * 2 / 3
	tail := budget / 3
	return text[:agentFloorCharBoundary(text, head)] + " [...] " + text[agentCeilCharBoundary(text, len(text)-tail):]
}

func Build(history *ContextHistory) Request {
	panic("unported: mj-core/src/activity/question_locator.rs build")
}

//go:embed data/activity/question_locator.json
var questionLocatorWordingJSON []byte

// wording is the bundled locator wording, member by member, each kept as
// written so its key order survives.
var wording = sync.OnceValue(func() map[string]json.RawMessage {
	var value map[string]json.RawMessage
	if err := json.Unmarshal(questionLocatorWordingJSON, &value); err != nil {
		panic("bundled locator wording: " + err.Error())
	}
	return value
})

// QuestionLocatorQuestions is the frozen V0 question: the shared
// instructions with plain-string criteria. The hosted proxy builds the same
// object, so the worker sends only the state.
func QuestionLocatorQuestions(count int) json.RawMessage {
	panic("unported: mj-core/src/activity/question_locator.rs questions")
}

// Parse applies the frozen rule to GLiDE's `probabilities` (a decoded JSON
// value) for the labels `r0..r(count-1)` and `none`.
//
// The choice is the argmax. A tie among replies goes to the newest, and
// `none` wins only when it is strictly higher than the best reply, exactly
// as the evaluation script scored it. `none` returns no index (false).
func Parse(probabilities any, count int) (int, bool, error) {
	panic("unported: mj-core/src/activity/question_locator.rs parse")
}
