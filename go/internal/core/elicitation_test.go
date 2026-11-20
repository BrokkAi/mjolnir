// Port of the tests in mj-core/src/elicitation.rs.

package core

import (
	"encoding/json"
	"slices"
	"testing"
)

func elicitationTestRequest(t *testing.T, id, params string) ElicitationRequest {
	t.Helper()
	request, err := ElicitationRequestFromACPParams(id, json.RawMessage(params))
	if err != nil {
		t.Fatal(err)
	}
	return request
}

func elicitationTestField(t *testing.T, request *ElicitationRequest, id string) *ElicitationField {
	t.Helper()
	field := request.fieldByID(id)
	if field == nil {
		t.Fatalf("no field %s", id)
	}
	return field
}

// elicitationTestAnswer is an accepted answer of string values.
func elicitationTestAnswer(pairs ...[2]string) ElicitationResponse {
	content := map[string]ElicitationValue{}
	for _, pair := range pairs {
		content[pair[0]] = ElicitationValueString(pair[1])
	}
	return ElicitationResponse{Action: ElicitationResponseAccept, Content: content}
}

func elicitationTestHarnessResponse(t *testing.T, request *ElicitationRequest, response ElicitationResponse, want ElicitationResponse) {
	t.Helper()
	got, err := request.HarnessResponse(response)
	if err != nil {
		t.Fatal(err)
	}
	if !got.Equal(want) {
		t.Fatalf("got %#v, want %#v", got, want)
	}
}

// Hard-won: dc436f0c: Claude bridge 0.84 moved Other-answer fields and Mjolnir lost their pairing
func TestParsesClaudeChoicesPreviewAndCustomAnswerMarker(t *testing.T) {
	request := elicitationTestRequest(t, "elicit-1", `{
		"sessionId": "session-1",
		"toolCallId": "tool-1",
		"mode": "form",
		"message": "Which architecture?",
		"requestedSchema": {
			"type": "object",
			"properties": {
				"question_0": {
					"type": "string",
					"title": "CI architecture",
					"oneOf": [{
						"const": "reusable",
						"title": "Reusable workflow",
						"description": "Thin callers",
						"_meta": {"_claude/askUserQuestionOption": {"preview": "ci.yml -> reusable"}}
					}]
				},
				"question_0_custom": {
					"type": "string",
					"title": "Other",
					"_meta": {
						"_askUserQuestionCustomAnswer": {
							"questionId": "question_0",
							"isCustomAnswer": true
						},
						"jetbrains": {"air": {"version": 1, "customAnswer": {
							"questionId": "question_0",
							"isCustomAnswer": true
						}}}
					}
				}
			}
		}
	}`)
	if request.Message != "Which architecture?" {
		t.Fatal(request.Message)
	}
	single, ok := request.Fields[0].Kind.(ElicitationFieldKindSingleSelect)
	if !ok {
		t.Fatal("expected single-select field")
	}
	if single.Options[0].Description == nil || *single.Options[0].Description != "Thin callers" {
		t.Fatal("description")
	}
	if single.Options[0].Preview == nil || *single.Options[0].Preview != "ci.yml -> reusable" {
		t.Fatal("preview")
	}
	if request.Fields[1].CustomAnswerFor == nil || *request.Fields[1].CustomAnswerFor != "question_0" {
		t.Fatal("custom answer pairing")
	}
	// Claude's "Other" goes beside a pick, or answers on its own.
	if request.Fields[1].CompanionRole() != CompanionRoleAlongside {
		t.Fatal(request.Fields[1].CompanionRole())
	}
}

func TestASelectWithoutADefaultDefaultsToItsRecommendedOrFirstOption(t *testing.T) {
	request := elicitationTestRequest(t, "elicit-recommended", `{
		"sessionId": "session-1",
		"mode": "form",
		"message": "Choose",
		"requestedSchema": {
			"type": "object",
			"properties": {
				"recommended": {
					"type": "string",
					"oneOf": [
						{"const": "drop", "title": "Drop it"},
						{"const": "keep", "title": "Keep it (Recommended)"}
					]
				},
				"own_default": {
					"type": "string",
					"default": "drop",
					"oneOf": [
						{"const": "keep", "title": "Keep it (recommended)"},
						{"const": "drop", "title": "Drop it"}
					]
				},
				"unmarked": {
					"type": "string",
					"oneOf": [
						{"const": "keep", "title": "Keep it"},
						{"const": "drop", "title": "Drop it"}
					]
				},
				"targets": {
					"type": "array",
					"items": {"anyOf": [
						{"const": "linux", "title": "linux"},
						{"const": "macos", "title": "macos (Recommended)"}
					]}
				}
			}
		}
	}`)
	singleDefault := func(id string) string {
		single, ok := elicitationTestField(t, &request, id).Kind.(ElicitationFieldKindSingleSelect)
		if !ok || single.Default == nil {
			t.Fatalf("%s: expected single-select field with a default", id)
		}
		return *single.Default
	}
	for id, want := range map[string]string{"recommended": "keep", "own_default": "drop", "unmarked": "keep"} {
		if got := singleDefault(id); got != want {
			t.Fatalf("%s: got %s", id, got)
		}
	}
	multi, ok := elicitationTestField(t, &request, "targets").Kind.(ElicitationFieldKindMultiSelect)
	if !ok {
		t.Fatal("expected multi-select field")
	}
	if !slices.Equal(multi.Default, []string{"macos"}) {
		t.Fatal(multi.Default)
	}
}

func TestParsesCodexSecretOtherField(t *testing.T) {
	request := elicitationTestRequest(t, "elicit-2", `{
		"sessionId": "session-1",
		"mode": "form",
		"message": "Input requested",
		"requestedSchema": {
			"properties": {
				"token__other": {
					"type": "string",
					"_meta": {"codex": {
						"questionId": "token",
						"role": "user_note",
						"isSecret": true
					}}
				}
			}
		}
	}`)
	if !request.Fields[0].Secret {
		t.Fatal("not secret")
	}
	if request.Fields[0].CustomAnswerFor == nil || *request.Fields[0].CustomAnswerFor != "token" {
		t.Fatal("pairing")
	}
}

// Hard-won: #1312: a required Codex question answered in its note reached the harness without its own field
func TestPairsCodexUserNoteWithItsQuestion(t *testing.T) {
	// The form @brokkai/codex-acp 1.13.7 sends an Air client for a question
	// that accepts an answer outside its listed options: no "None of the
	// above" choice, and Air markers on the note.
	request := elicitationTestRequest(t, "elicit-note", `{
		"sessionId": "session-1",
		"mode": "form",
		"message": "Codex needs your input to continue.",
		"requestedSchema": {
			"type": "object",
			"required": ["scope"],
			"properties": {
				"scope": {
					"type": "string",
					"title": "Which scope?",
					"description": "Scope",
					"oneOf": [
						{"const": "Narrow", "title": "Narrow"},
						{"const": "Wide", "title": "Wide"}
					],
					"_meta": {"codex": {"isOther": true, "isSecret": false}}
				},
				"scope_note": {
					"type": "string",
					"title": "Additional answer or note",
					"_meta": {
						"codex": {
							"questionId": "scope",
							"role": "user_note",
							"isSecret": false
						},
						"_askUserQuestionCustomAnswer": true,
						"jetbrains": {"air": {"version": 1, "customAnswer": true}}
					}
				}
			}
		}
	}`)
	note := elicitationTestField(t, &request, "scope_note")
	if note.CustomAnswerFor == nil || *note.CustomAnswerFor != "scope" || note.CustomAnswerOption != nil {
		t.Fatal("pairing")
	}
	// An answer written in the note is the question's own answer. The
	// harness gets it in the question field, where codex-acp reads an Air
	// client's typed answer, so the required field is never missing.
	elicitationTestHarnessResponse(t, &request,
		elicitationTestAnswer([2]string{"scope_note", "Only the parser"}),
		elicitationTestAnswer([2]string{"scope", "Only the parser"}))
	// The note is an alternative to a pick, never beside one: codex-acp's
	// async question path reads a note in place of the pick, so sending both
	// would lose the pick.
	if note.CompanionRole() != CompanionRoleAlternative {
		t.Fatal(note.CompanionRole())
	}
	both := elicitationTestAnswer([2]string{"scope", "Narrow"}, [2]string{"scope_note", "Parser first"})
	if request.ValidateResponse(&both) == nil {
		t.Fatal("a pick and a note were both accepted")
	}
	// Neither a pick nor text leaves the required question unanswered, and
	// a submitted pick must still be an offered option.
	blank := elicitationTestAnswer([2]string{"scope_note", "  "})
	if request.ValidateResponse(&blank) == nil {
		t.Fatal("a blank note was accepted")
	}
	unoffered := elicitationTestAnswer([2]string{"scope", "Only the parser"})
	if request.ValidateResponse(&unoffered) == nil {
		t.Fatal("an unoffered pick was accepted")
	}
}

// Hard-won: #1312: plan-review feedback alone was forwarded as the plan decision
func TestPlanReviewFeedbackNeverStandsInForItsDecision(t *testing.T) {
	review := NormalizedPlanReview("plan-review-1", transcriptParse([]byte(`{"plan": "1. Do it"}`)))
	_, err := review.HarnessResponse(elicitationTestAnswer([2]string{PlanReviewFeedback, "tighten step 3"}))
	if err == nil || err.Error() != "Decision is required" {
		t.Fatalf("got %v", err)
	}
	revise := elicitationTestAnswer([2]string{PlanReviewAction, "revise"}, [2]string{PlanReviewFeedback, "tighten step 3"})
	elicitationTestHarnessResponse(t, &review, revise, revise)
}

func elicitationTestNumericRequest(t *testing.T) ElicitationRequest {
	return elicitationTestRequest(t, "elicit-3", `{
		"sessionId": "session-1",
		"mode": "form",
		"message": "Size the run",
		"requestedSchema": {
			"type": "object",
			"required": ["workers"],
			"properties": {
				"workers": {"type": "integer", "minimum": 1, "maximum": 8},
				"ratio": {"type": "number", "minimum": 0.0, "maximum": 1.0},
				"targets": {
					"type": "array",
					"minItems": 1,
					"maxItems": 2,
					"items": {"enum": ["linux", "macos", "windows"]}
				}
			}
		}
	}`)
}

func elicitationTestAccept(values map[string]ElicitationValue) *ElicitationResponse {
	return &ElicitationResponse{Action: ElicitationResponseAccept, Content: values}
}

func TestAnswersMustSatisfyTheRequestedSchema(t *testing.T) {
	request := elicitationTestNumericRequest(t)
	if err := request.ValidateResponse(elicitationTestAccept(map[string]ElicitationValue{"workers": ElicitationValueInteger(4)})); err != nil {
		t.Fatal(err)
	}
	// A whole number for a number field decodes as an integer, so both
	// shapes have to reach the range check rather than the type error.
	if err := request.ValidateResponse(elicitationTestAccept(map[string]ElicitationValue{
		"workers": ElicitationValueInteger(4), "ratio": ElicitationValueInteger(1),
	})); err != nil {
		t.Fatal(err)
	}
	if request.ValidateResponse(elicitationTestAccept(map[string]ElicitationValue{
		"workers": ElicitationValueInteger(4), "ratio": ElicitationValueNumber(1.5),
	})) == nil {
		t.Fatal("ratio out of range")
	}
	if request.ValidateResponse(elicitationTestAccept(map[string]ElicitationValue{"workers": ElicitationValueInteger(9)})) == nil {
		t.Fatal("workers out of range")
	}
	if request.ValidateResponse(elicitationTestAccept(map[string]ElicitationValue{"workers": ElicitationValueString("four")})) == nil {
		t.Fatal("a string integer")
	}
	// Required means required; declining and cancelling carry no content.
	if request.ValidateResponse(elicitationTestAccept(map[string]ElicitationValue{})) == nil {
		t.Fatal("a missing required field")
	}
	if err := request.ValidateResponse(&ElicitationResponse{Action: ElicitationResponseDecline}); err != nil {
		t.Fatal(err)
	}
	if err := request.ValidateResponse(&ElicitationResponse{Action: ElicitationResponseCancel}); err != nil {
		t.Fatal(err)
	}
}

func TestMultiSelectAnswersHonourMembershipAndItemCounts(t *testing.T) {
	request := elicitationTestNumericRequest(t)
	withTargets := func(values ...string) *ElicitationResponse {
		return elicitationTestAccept(map[string]ElicitationValue{
			"workers": ElicitationValueInteger(2),
			"targets": ElicitationValueStringArray(append([]string{}, values...)),
		})
	}
	if err := request.ValidateResponse(withTargets("linux")); err != nil {
		t.Fatal(err)
	}
	if request.ValidateResponse(withTargets("linux", "macos", "windows")) == nil {
		t.Fatal("too many")
	}
	if request.ValidateResponse(withTargets()) == nil {
		t.Fatal("too few")
	}
	if request.ValidateResponse(withTargets("plan9")) == nil {
		t.Fatal("unoffered")
	}
}
