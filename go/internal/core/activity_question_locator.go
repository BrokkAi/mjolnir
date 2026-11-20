// Port of mj-core/src/activity/question_locator.rs.

// Locate an open request in the authorization history after an input verdict.

package core

import (
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"strconv"
	"strings"
	"sync"

	"github.com/BrokkAi/acp-go/jsonvalue"
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
	lastUser := -1
	for index := len(history.Messages) - 1; index >= 0; index-- {
		if history.Messages[index].Role == "user" {
			lastUser = index
			break
		}
	}
	user := ""
	if lastUser >= 0 {
		user = history.Messages[lastUser].Text
	}
	latestUserMessage := user[agentCeilCharBoundary(user, max(len(user)-4096, 0)):]
	var candidates []EvidenceMessage
	for _, message := range history.Messages[lastUser+1:] {
		if message.Role == "assistant" {
			candidates = append(candidates, message)
		}
	}
	if len(candidates) > maxReplies {
		candidates = candidates[len(candidates)-maxReplies:]
	}
	messageIDs := make([]string, 0, len(candidates))
	for _, message := range candidates {
		messageIDs = append(messageIDs, message.ID)
	}
	budget := 3000
	for {
		replies := make([]Reply, 0, len(candidates))
		for reply, message := range candidates {
			replies = append(replies, Reply{Reply: reply, Text: clip(message.Text, budget)})
		}
		state := State{LatestUserMessage: latestUserMessage, Replies: replies}
		encoded, err := agentMarshalJSON(state)
		if err != nil {
			panic("locator state serializes: " + err.Error())
		}
		if len(encoded) <= stateBytes {
			return Request{State: state, MessageIDs: messageIDs}
		}
		budget = budget * 4 / 5
	}
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

// questionLocatorWordingValue reads one bundled wording member as a JSON
// value.
func questionLocatorWordingValue(raw json.RawMessage) any {
	if raw == nil {
		return nil
	}
	value, err := jsonvalue.Parse(raw)
	if err != nil {
		panic("bundled locator wording: " + err.Error())
	}
	return value
}

// QuestionLocatorQuestions is the frozen V0 question: the shared
// instructions with plain-string criteria. The hosted proxy builds the same
// object, so the worker sends only the state.
func QuestionLocatorQuestions(count int) json.RawMessage {
	criteria := make(jsonvalue.Object, 0, count+1)
	for index := 0; index < count; index++ {
		criteria = append(criteria, jsonvalue.Member{
			Key:   fmt.Sprintf("r%d", index),
			Value: fmt.Sprintf("Reply %d (replies[%d]) contains the open request.", index, index),
		})
	}
	words := wording()
	criteria = append(criteria, jsonvalue.Member{Key: "none", Value: questionLocatorWordingValue(words["none"])})
	question := jsonvalue.Object{{Key: "request_reply", Value: jsonvalue.Object{
		{Key: "type", Value: "choice"},
		{Key: "instructions", Value: questionLocatorWordingValue(words["instructions"])},
		{Key: "criteria", Value: criteria},
	}}}
	encoded, err := question.MarshalJSON()
	if err != nil {
		panic("locator question serializes: " + err.Error())
	}
	return encoded
}

// Parse applies the frozen rule to GLiDE's `probabilities` (a decoded JSON
// value) for the labels `r0..r(count-1)` and `none`.
//
// The choice is the argmax. A tie among replies goes to the newest, and
// `none` wins only when it is strictly higher than the best reply, exactly
// as the evaluation script scored it. `none` returns no index (false).
func Parse(probabilities any, count int) (int, bool, error) {
	members, ok := probabilities.(jsonvalue.Object)
	if !ok {
		var isObject bool
		members, isObject = agentJSONMembers(probabilities)
		if !isObject {
			return 0, false, errors.New("missing locator probabilities")
		}
	}
	if _, hasNone := agentJSONGet(probabilities, "none"); len(members) != count+1 || !hasNone {
		return 0, false, errors.New("incomplete locator probabilities")
	}
	var sum, none float64
	best, bestIndex, found := 0.0, 0, false
	for _, member := range members {
		p, isNumber := agentJSONFloat(member.Value)
		if !isNumber {
			return 0, false, errors.New("invalid locator probability")
		}
		if math.IsInf(p, 0) || math.IsNaN(p) || p < 0 || p > 1 {
			return 0, false, errors.New("invalid locator probability")
		}
		sum += p
		if member.Key == "none" {
			none = p
			continue
		}
		digits, hasPrefix := strings.CutPrefix(member.Key, "r")
		index, err := strconv.ParseUint(digits, 10, 64)
		if !hasPrefix || err != nil || index >= uint64(count) || fmt.Sprintf("r%d", index) != member.Key {
			return 0, false, errors.New("invalid locator probability key")
		}
		if !found || p > best || (p == best && int(index) > bestIndex) {
			best, bestIndex, found = p, int(index), true
		}
	}
	if math.Abs(sum-1.0) > 0.03 {
		return 0, false, errors.New("invalid locator distribution")
	}
	if !found {
		return 0, false, errors.New("no locator reply probabilities")
	}
	if none <= best {
		return bestIndex, true, nil
	}
	return 0, false, nil
}
