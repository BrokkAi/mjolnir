// Port of brokk-anvil-client 0.28.6 src/responses_chain.rs (the part Mjolnir uses).

// Turn-to-turn continuation bookkeeping for the Responses API backends.
//
// StreamChat hands every backend the full conversation on every call and
// carries no conversation id, so a backend that wants the server's prompt
// cache to keep matching as the conversation grows has to recognize "this
// call continues the call I made last turn" on its own: hash the ordered
// message context, remember what that context produced, and on the next
// turn look for the largest stored prefix of the new message list.

package anvilclient

import (
	"encoding/binary"
	"hash/maphash"
)

// responsesChainCacheCap is how many message-context -> value entries a
// chain cache keeps before evicting the oldest.
const responsesChainCacheCap = 512

// responsesChainCache is a bounded content-keyed cache of "what did this
// exact message context produce last time", evicting in insertion order.
type responsesChainCache[V any] struct {
	entries map[uint64]V
	order   []uint64
	cap     int
}

func newResponsesChainCache[V any](capacity int) *responsesChainCache[V] {
	return &responsesChainCache[V]{entries: map[uint64]V{}, cap: capacity}
}

func (c *responsesChainCache[V]) get(key uint64) (V, bool) {
	value, ok := c.entries[key]
	return value, ok
}

func (c *responsesChainCache[V]) insert(key uint64, value V) {
	if _, ok := c.entries[key]; !ok {
		c.order = append(c.order, key)
		for len(c.order) > c.cap {
			delete(c.entries, c.order[0])
			c.order = c.order[1:]
		}
	}
	c.entries[key] = value
}

// responsesHashSeed keys the in-process context hash; the hashes never
// leave the process.
var responsesHashSeed = maphash.MakeSeed()

// hashResponsesContext hashes an ordered message context (role, content,
// tool-call and tool-result fields, in order). Any difference in message
// order, text, or tool-call identity/arguments produces a different hash.
func hashResponsesContext(messages []ChatMessage) uint64 {
	var hash maphash.Hash
	hash.SetSeed(responsesHashSeed)
	writeLength := func(length int) {
		var buffer [8]byte
		binary.LittleEndian.PutUint64(buffer[:], uint64(length))
		hash.Write(buffer[:])
	}
	writeString := func(text string) {
		writeLength(len(text))
		hash.WriteString(text)
	}
	writeOptional := func(text *string) {
		if text == nil {
			hash.WriteByte(0)
			return
		}
		hash.WriteByte(1)
		writeString(*text)
	}
	writeLength(len(messages))
	for index := range messages {
		message := &messages[index]
		writeString(message.Role)
		for _, part := range message.Content {
			switch part := part.(type) {
			case ChatContentPartText:
				hash.WriteByte(0)
				writeString(part.Text)
			case ChatContentPartImage:
				hash.WriteByte(1)
				writeString(part.ImageURL)
			}
		}
		if message.ToolCalls != nil {
			writeLength(len(message.ToolCalls))
			for _, call := range message.ToolCalls {
				writeString(call.ID)
				writeString(call.Type)
				writeString(call.Function.Name)
				writeString(call.Function.Arguments)
			}
		} else {
			writeLength(0)
		}
		writeOptional(message.ToolCallID)
		writeOptional(message.Name)
	}
	return hash.Sum64()
}

// findResponsesContinuation finds the largest cached prefix of messages
// that matches a previously stored context: walking assistant-message
// boundaries from the end, the first index a whose prefix messages[:a] has
// a stored value wins. false on a fresh conversation or an unseen prefix.
func findResponsesContinuation[V any](messages []ChatMessage, lookup func(uint64) (V, bool)) (int, V, bool) {
	for index := len(messages) - 1; index >= 0; index-- {
		if messages[index].Role != "assistant" {
			continue
		}
		if value, ok := lookup(hashResponsesContext(messages[:index])); ok {
			return index, value, true
		}
	}
	var zero V
	return 0, zero, false
}
