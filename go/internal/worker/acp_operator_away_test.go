// Port of mj-worker/src/acp/operator_away.rs: its test-only items and
// tests. The #[cfg(unix)] test is in acp_operator_away_unix_test.go.

package worker

import (
	"bufio"
	"context"
	"fmt"
	"io"
	"net"
	"slices"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// pendingForBridge opens a bridge scope with classification disabled.
func (o *OperatorAway) pendingForBridge() PendingElicitations {
	return o.pendingForBridgeWithSource(VerdictSourceDisabled{}, nil)
}

// DefaultPendingElicitations is the test-only PendingElicitations::default():
// a bridge of a fresh presence owner.
func DefaultPendingElicitations() PendingElicitations {
	return (&OperatorAway{}).pendingForBridge()
}

// afkClassifier is a fake hosted classifier.
type afkClassifier struct {
	listener net.Listener
	source   VerdictSource
}

func newAfkClassifier(t *testing.T) *afkClassifier {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { listener.Close() })
	return &afkClassifier{
		listener: listener,
		source:   VerdictSourceHosted{Endpoint: fmt.Sprintf("http://%s/", listener.Addr())},
	}
}

// request accepts the next classification and returns its connection and
// body.
func (c *afkClassifier) request(t *testing.T) (net.Conn, any) {
	t.Helper()
	stream, err := c.listener.Accept()
	if err != nil {
		t.Fatal(err)
	}
	reader := bufio.NewReader(stream)
	var headers strings.Builder
	length := -1
	for {
		line, err := reader.ReadString('\n')
		if line == "" || err != nil {
			t.Fatal("classifier client disconnected before headers")
		}
		if line == "\r\n" {
			break
		}
		if value, ok := strings.CutPrefix(strings.ToLower(line), "content-length:"); ok {
			length, _ = strconv.Atoi(strings.TrimSpace(value))
		}
		headers.WriteString(line)
	}
	body := make([]byte, length)
	if _, err := io.ReadFull(reader, body); err != nil {
		t.Fatal(err)
	}
	value := acpFeaturesJSON(t, string(body))
	if !strings.HasPrefix(headers.String(), "POST /v1/afk-question-glide ") {
		t.Fatalf("headers %s", headers.String())
	}
	// The hosted proxy owns the question: the body is the state alone.
	if object, ok := value.(jsonvalue.Object); !ok || len(object.Keys()) != 2 {
		t.Fatalf("body %s", body)
	}
	return stream, value
}

func (c *afkClassifier) respond(t *testing.T, stream net.Conn, choice string) {
	t.Helper()
	c.writeVerdict(t, stream, choice)
	stream.Close()
}

// writeVerdict writes respond's answer and leaves the connection open, so
// the caller can wait for the client to consume it.
func (c *afkClassifier) writeVerdict(t *testing.T, stream net.Conn, choice string) {
	t.Helper()
	same, different := 0.01, 0.99
	if choice == "same" {
		same, different = 0.99, 0.01
	}
	body, err := jsonvalue.Marshal(jsonvalue.Object{{Key: "probabilities", Value: jsonvalue.Object{
		{Key: "same", Value: same}, {Key: "different", Value: different},
	}}})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := fmt.Fprintf(stream, "HTTP/1.1 200 OK\r\nContent-Length: %d\r\nConnection: close\r\n\r\n%s", len(body), body); err != nil {
		t.Fatal(err)
	}
}

// afkReadToEnd waits until the client has closed its side of stream.
func afkReadToEnd(t *testing.T, stream net.Conn) {
	t.Helper()
	if _, err := io.ReadAll(stream); err != nil {
		t.Fatal(err)
	}
	stream.Close()
}

func afkQuestion(id, text string) core.ElicitationRequest {
	return core.ElicitationRequest{ID: id, Message: text, Fields: []core.ElicitationField{}}
}

// afkAsk registers a question and awaits its answer on its own goroutine:
// the channel yields the answer, nil when there is none.
func afkAsk(pending PendingElicitations, id, text string) <-chan PendingAnswer {
	return afkAskForm(pending, afkQuestion(id, text))
}

func afkAskForm(pending PendingElicitations, question core.ElicitationRequest) <-chan PendingAnswer {
	id := question.ID
	answer := pending.register(question, "", AwayRequestQuestion{})
	result := make(chan PendingAnswer, 1)
	go func() {
		result <- awaitElicitationResponse(context.Background(), &pending, id, answer, make(chan struct{}))
	}()
	return result
}

func afkAway(t *testing.T, answer PendingAnswer) {
	t.Helper()
	if _, ok := answer.(PendingAnswerOperatorAway); !ok {
		t.Fatalf("answer %#v, want the away answer", answer)
	}
}

func afkHuman(t *testing.T, answer PendingAnswer) {
	t.Helper()
	if _, ok := answer.(PendingAnswerHuman); !ok {
		t.Fatalf("answer %#v, want the person's", answer)
	}
}

func afkDecline() core.ElicitationResponse {
	return core.ElicitationResponse{Action: core.ElicitationResponseDecline}
}

func afkResolve(t *testing.T, pending PendingElicitations, id string) {
	t.Helper()
	if err := pending.resolve(id, afkDecline()); err != nil {
		t.Fatal(err)
	}
}

func afkPointer(t *testing.T, value any, path ...any) any {
	t.Helper()
	for _, step := range path {
		switch key := step.(type) {
		case string:
			value, _ = acpFeaturesGet(value, key)
		case int:
			items, ok := value.([]any)
			if !ok || key >= len(items) {
				return nil
			}
			value = items[key]
		}
	}
	return value
}

func afkHas(t *testing.T, value any, path ...any) bool {
	t.Helper()
	last := path[len(path)-1].(string)
	_, ok := acpFeaturesGet(afkPointer(t, value, path[:len(path)-1]...), last)
	return ok
}

func afkText(t *testing.T, value any) string {
	t.Helper()
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	return string(encoded)
}

// Hard-won: #1489: new questions shared the generic Question refusal identity.
func TestAfkQuestionProtocolDistinguishesMixedFormsAndPreservesUnavailableFallback(t *testing.T) {
	classifier := newAfkClassifier(t)
	owner := &OperatorAway{}
	pending := owner.pendingForBridgeWithSource(classifier.source, nil)
	owner.set(core.OperatorPresence{Away: true, Revision: 1})
	afkAway(t, <-afkAsk(pending, "first", "Who owns #10941?"))
	newQuestion := afkAsk(pending, "mixed", "Who owns #11482, and who owns #10941?")
	stream, evidence := classifier.request(t)
	if !acpFeaturesJSONEqual(evidence, acpFeaturesJSON(t, `{"current_question":{"message":"Who owns #11482, and who owns #10941?", "fields":[]}, "previous_questions":[{"message":"Who owns #10941?", "fields":[]}]}`)) {
		t.Fatalf("evidence %s", afkText(t, evidence))
	}
	classifier.respond(t, stream, "different")
	afkAway(t, <-newQuestion)

	repeated := afkAsk(pending, "repeat", "Please assign #10941.")
	stream, evidence = classifier.request(t)
	if !acpFeaturesJSONEqual(afkPointer(t, evidence, "previous_questions"), acpFeaturesJSON(t,
		`[{"message":"Who owns #10941?", "fields":[]}, {"message":"Who owns #11482, and who owns #10941?", "fields":[]}]`)) {
		t.Fatalf("evidence %s", afkText(t, evidence))
	}
	classifier.respond(t, stream, "same")
	afkResolve(t, pending, "repeat")
	afkHuman(t, <-repeated)

	for _, body := range []string{
		"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
		"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
	} {
		fallback := afkAsk(pending, "fallback", "Who owns #12000?")
		stream, _ := classifier.request(t)
		if _, err := stream.Write([]byte(body)); err != nil {
			t.Fatal(err)
		}
		// Wait until the network attempt has consumed the response before
		// answering, so this proves fallback rather than winning a race.
		afkReadToEnd(t, stream)
		afkResolve(t, pending, "fallback")
		afkHuman(t, <-fallback)
	}
}

// Hard-won: #1489 review: evidence lost constraints and leaked secret defaults/choices.
func TestAfkQuestionEvidencePreservesChangedSelectionWithoutTheRequiredFlag(t *testing.T) {
	classifier := newAfkClassifier(t)
	owner := &OperatorAway{}
	pending := owner.pendingForBridgeWithSource(classifier.source, nil)
	owner.set(core.OperatorPresence{Away: true, Revision: 1})
	first, err := core.ElicitationRequestFromACPParams("first", []byte(`{
                "mode":"form", "sessionId":"test", "message":"Choose a destination",
            "requestedSchema":{"type":"object", "properties":{
                "destination":{"type":"string", "title":"Destination", "enum":["one","two"]},
                "password":{"type":"string", "title":"Password", "enum":["P!n12","n3w?Pin"], "default":"P!n12", "_meta":{"codex":{"isSecret":true}}},
                "password__other":{"type":"string", "title":"Other password", "default":"77!Z", "_meta":{"codex":{"questionId":"password", "role":"user_note"}}}
            }}
            }`))
	if err != nil {
		t.Fatal(err)
	}
	second, err := core.ElicitationRequestFromACPParams("second", []byte(`{
            "mode":"form", "sessionId":"test", "message":"Choose a destination",
            "requestedSchema":{"type":"object", "required":["destination"], "properties":{
                "destination":{"type":"array", "title":"Destination", "items":{"type":"string", "enum":["one","two"]}, "minItems":1},
                "password":{"type":"string", "title":"Password", "enum":["P!n12","n3w?Pin"], "default":"n3w?Pin", "_meta":{"codex":{"isSecret":true}}},
                "password__other":{"type":"string", "title":"Other password", "default":"55?X", "_meta":{"codex":{"questionId":"password", "role":"user_note"}}}
            }}
        }`))
	if err != nil {
		t.Fatal(err)
	}
	if first.QuestionText() != second.QuestionText() {
		t.Fatal("the old evidence lost the change")
	}
	afkAway(t, <-afkAskForm(pending, first))
	secondAnswer := afkAskForm(pending, second)
	stream, evidence := classifier.request(t)
	serialized := afkText(t, evidence)
	for _, secret := range []string{"P!n12", "n3w?Pin", "77!Z", "55?X"} {
		if strings.Contains(serialized, secret) {
			t.Fatalf("%s leaked: %s", secret, serialized)
		}
	}
	if afkHas(t, evidence, "current_question", "fields", 2, "default") ||
		afkHas(t, evidence, "previous_questions", 0, "fields", 2, "default") {
		t.Fatalf("a secret companion default was sent: %s", serialized)
	}
	if afkPointer(t, evidence, "current_question", "fields", 2, "secret") != true {
		t.Fatalf("companion not secret: %s", serialized)
	}
	if !acpFeaturesJSONEqual(afkPointer(t, evidence, "current_question", "fields", 1, "options"),
		afkPointer(t, evidence, "previous_questions", 0, "fields", 1, "options")) {
		t.Fatalf("secret choices differ: %s", serialized)
	}
	if acpFeaturesJSONEqual(afkPointer(t, evidence, "current_question", "fields", 1, "options", 0, "value"),
		afkPointer(t, evidence, "current_question", "fields", 1, "options", 1, "value")) {
		t.Fatalf("secret choices merged: %s", serialized)
	}
	if afkHas(t, evidence, "current_question", "fields", 1, "default") ||
		afkHas(t, evidence, "previous_questions", 0, "fields", 1, "default") {
		t.Fatalf("a secret default was sent: %s", serialized)
	}
	if afkPointer(t, evidence, "current_question", "fields", 1, "secret") != true {
		t.Fatalf("password not secret: %s", serialized)
	}
	if afkPointer(t, evidence, "current_question", "fields", 0, "kind") != "multi_select" {
		t.Fatalf("kind: %s", serialized)
	}
	// Required versus optional alone is a repeat (user rule, 2026-11-13).
	if afkHas(t, evidence, "current_question", "fields", 0, "required") {
		t.Fatalf("required was sent: %s", serialized)
	}
	if minItems, _ := acpFeaturesAsU64(afkPointer(t, evidence, "current_question", "fields", 0, "min_items")); minItems != 1 {
		t.Fatalf("min items: %s", serialized)
	}
	if afkPointer(t, evidence, "previous_questions", 0, "fields", 0, "kind") != "single_select" {
		t.Fatalf("previous kind: %s", serialized)
	}
	if afkHas(t, evidence, "previous_questions", 0, "fields", 0, "required") {
		t.Fatalf("required was sent: %s", serialized)
	}
	classifier.respond(t, stream, "different")
	afkAway(t, <-secondAnswer)
}

// Hard-won: #1489 review: redaction changed ID identity across escapes and punctuation.
func TestAfkQuestionProtocolPreservesRedactedIdentifierEquality(t *testing.T) {
	classifier := newAfkClassifier(t)
	owner := &OperatorAway{}
	pending := owner.pendingForBridgeWithSource(classifier.source, nil)
	owner.set(core.OperatorPresence{Away: true, Revision: 1})
	firstID := strings.Repeat("a", 40)
	secondID := strings.Repeat("b", 40)
	afkAway(t, <-afkAsk(pending, "first", fmt.Sprintf("Merge commit %s?", firstID)))
	second := afkAsk(pending, "second", fmt.Sprintf("Merge commit\n%s?", secondID))
	stream, evidence := classifier.request(t)
	serialized := afkText(t, evidence)
	if strings.Contains(serialized, firstID) || strings.Contains(serialized, secondID) {
		t.Fatalf("identifier leaked: %s", serialized)
	}
	if afkPointer(t, evidence, "current_question", "message") != "Merge commit\n[redacted token 0]?" {
		t.Fatalf("current %s", serialized)
	}
	if afkPointer(t, evidence, "previous_questions", 0, "message") != "Merge commit [redacted token 1]?" {
		t.Fatalf("previous %s", serialized)
	}
	classifier.respond(t, stream, "different")
	afkAway(t, <-second)
	repeated := afkAsk(pending, "repeated", fmt.Sprintf("Merge commit\t%s.", firstID))
	stream, evidence = classifier.request(t)
	if afkPointer(t, evidence, "current_question", "message") != "Merge commit\t[redacted token 0]." {
		t.Fatalf("current %s", afkText(t, evidence))
	}
	if afkPointer(t, evidence, "previous_questions", 0, "message") != "Merge commit [redacted token 0]?" {
		t.Fatalf("previous %s", afkText(t, evidence))
	}
	if acpFeaturesJSONEqual(afkPointer(t, evidence, "current_question"), afkPointer(t, evidence, "previous_questions", 1)) {
		t.Fatalf("distinct questions merged: %s", afkText(t, evidence))
	}
	classifier.respond(t, stream, "same")
	afkResolve(t, pending, "repeated")
	afkHuman(t, <-repeated)
	// A serialized newline plus a 23-character run used to cross the
	// 24-character redaction boundary and produce an invalid JSON escape.
	shortMessage := fmt.Sprintf("Merge commit\n%s?", strings.Repeat("c", 23))
	short := afkAsk(pending, "short", shortMessage)
	stream, evidence = classifier.request(t)
	if afkPointer(t, evidence, "current_question", "message") != shortMessage {
		t.Fatalf("current %s", afkText(t, evidence))
	}
	classifier.respond(t, stream, "different")
	afkAway(t, <-short)
}

func TestAfkQuestionDecisionsAreRecordedWithTheRedactedRequestAndOutcome(t *testing.T) {
	classifier := newAfkClassifier(t)
	directory := t.TempDir()
	turnContext := &transcript.TurnContext{}
	turnContext.SetSessionID("afk-session")
	log, err := core.DecisionLogOpen(directory)
	if err != nil {
		t.Fatal(err)
	}
	defer log.Close()
	turnContext.SetDecisionLog(log)
	owner := &OperatorAway{}
	pending := owner.pendingForBridgeWithSource(classifier.source, turnContext)
	owner.set(core.OperatorPresence{Away: true, Revision: 1})
	const token = "ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789"
	afkAway(t, <-afkAsk(pending, "first", fmt.Sprintf("Use %s?", token)))
	newQuestion := afkAsk(pending, "new", "Who owns #11482?")
	stream, _ := classifier.request(t)
	classifier.respond(t, stream, "different")
	afkAway(t, <-newQuestion)
	repeat := afkAsk(pending, "repeat", "Please assign #11482.")
	stream, _ = classifier.request(t)
	// Rust's current-thread test runtime has the check consume this answer
	// before the test reaches the resolve below; Go's scheduler gives no
	// such order, and resolving first cancels the check. Wait for the client
	// to finish with the connection, as the 413 case below does.
	classifier.writeVerdict(t, stream, "same")
	afkReadToEnd(t, stream)
	failed := afkAsk(pending, "failed", "Another decision?")
	stream, _ = classifier.request(t)
	if _, err := stream.Write([]byte("HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")); err != nil {
		t.Fatal(err)
	}
	afkReadToEnd(t, stream)
	// "repeat" was held and still waits; resolve both so the tasks end.
	for _, id := range []string{"repeat", "failed"} {
		afkResolve(t, pending, id)
	}
	<-repeat
	<-failed
	page, err := core.JevRead(directory, "afk-session", nil)
	if err != nil {
		t.Fatal(err)
	}
	slices.SortFunc(page.Decisions, func(a, b core.JevDecision) int {
		if a.StartedAtMs != b.StartedAtMs {
			return int(a.StartedAtMs - b.StartedAtMs)
		}
		return strings.Compare(a.ID, b.ID)
	})
	var outcomes []string
	for _, decision := range page.Decisions {
		outcomes = append(outcomes, decision.Status)
	}
	if !slices.Equal(outcomes, []string{"declined_new", "held_repeat", "failed"}) {
		t.Fatalf("outcomes %v", outcomes)
	}
	// The list omits technical details; read each record by id.
	var records []core.JevDecision
	for _, summary := range page.Decisions {
		id := summary.ID
		detail, err := core.JevRead(directory, "afk-session", &id)
		if err != nil {
			t.Fatal(err)
		}
		records = append(records, detail.Decisions[0])
	}
	for _, record := range records {
		if record.Kind != "activity" {
			t.Fatalf("kind %s", record.Kind)
		}
		if record.Checked != "Does this question repeat one already declined while away?" {
			t.Fatalf("checked %s", record.Checked)
		}
		technical := acpFeaturesJSON(t, string(record.Technical))
		if afkPointer(t, technical, "contract") != "afk-question-glide-v1" {
			t.Fatalf("technical %s", record.Technical)
		}
		if strings.Contains(string(record.Technical), token) {
			t.Fatalf("token recorded: %s", record.Technical)
		}
		message, _ := afkPointer(t, technical, "request", "previous_questions", 0, "message").(string)
		if !strings.Contains(message, "[redacted token") {
			t.Fatalf("technical %s", record.Technical)
		}
	}
	first := acpFeaturesJSON(t, string(records[0].Technical))
	if !acpFeaturesJSONEqual(afkPointer(t, first, "probabilities"), acpFeaturesJSON(t, `{"same":0.01,"different":0.99}`)) {
		t.Fatalf("probabilities %s", records[0].Technical)
	}
	third := acpFeaturesJSON(t, string(records[2].Technical))
	if failure, _ := afkPointer(t, third, "error").(string); !strings.Contains(failure, "413") {
		t.Fatalf("error %s", records[2].Technical)
	}
}

func TestAfkQuestionClassificationCannotOutliveHumanCancelReturnOrBridgeClose(t *testing.T) {
	for _, action := range []string{"human", "cancel", "return", "close"} {
		classifier := newAfkClassifier(t)
		owner := &OperatorAway{}
		pending := owner.pendingForBridgeWithSource(classifier.source, nil)
		owner.set(core.OperatorPresence{Away: true, Revision: 1})
		afkAway(t, <-afkAsk(pending, "first", "First decision?"))
		answer := pending.register(afkQuestion("racing", "Another decision?"), "", AwayRequestQuestion{})
		cancel := make(chan struct{})
		response := make(chan PendingAnswer, 1)
		go func() {
			response <- awaitElicitationResponse(context.Background(), &pending, "racing", answer, cancel)
		}()
		stream, _ := classifier.request(t)
		switch action {
		case "human":
			afkResolve(t, pending, "racing")
		case "cancel":
			close(cancel)
		case "return":
			owner.set(core.OperatorPresence{Away: false, Revision: 2})
		case "close":
			pending.close()
		}
		// Cancellation/return drops the in-flight HTTP request, rather than
		// waiting for Jev to release its response.
		afkReadToEnd(t, stream)
		if action == "return" {
			afkResolve(t, pending, "racing")
		}
		outcome := <-response
		if action == "human" || action == "return" {
			if _, ok := outcome.(PendingAnswerHuman); !ok {
				t.Fatalf("%s: %#v", action, outcome)
			}
		} else if outcome != nil {
			t.Fatalf("%s: %#v", action, outcome)
		}
	}
}

func TestConcurrentAfkQuestionsReclassifyAfterAnotherQuestionIsAnswered(t *testing.T) {
	classifier := newAfkClassifier(t)
	owner := &OperatorAway{}
	pending := owner.pendingForBridgeWithSource(classifier.source, nil)
	owner.set(core.OperatorPresence{Away: true, Revision: 1})
	afkAway(t, <-afkAsk(pending, "first", "First decision?"))
	one := afkAsk(pending, "one", "Who owns #11482?")
	streamOne, _ := classifier.request(t)
	two := afkAsk(pending, "two", "Assign #11482 to someone?")
	streamTwo, _ := classifier.request(t)
	classifier.respond(t, streamOne, "different")
	afkAway(t, <-one)
	afkReadToEnd(t, streamTwo)
	streamTwo, evidence := classifier.request(t)
	if !acpFeaturesJSONEqual(afkPointer(t, evidence, "previous_questions"), acpFeaturesJSON(t,
		`[{"message":"First decision?", "fields":[]}, {"message":"Who owns #11482?", "fields":[]}]`)) {
		t.Fatalf("evidence %s", afkText(t, evidence))
	}
	classifier.respond(t, streamTwo, "same")
	afkResolve(t, pending, "two")
	afkHuman(t, <-two)
}
