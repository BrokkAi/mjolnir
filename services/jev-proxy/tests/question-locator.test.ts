import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import proxy from "../src/index.ts";
import { questionLocatorQuestions } from "../src/question-locator.ts";
import question from "../../../mj-core/src/activity/question_locator.json" with { type: "json" };

const state = { latest_user_message: "Please proceed", replies: [
  { reply: 0, text: "Should I make this change?" }, { reply: 1, text: "Waiting for your answer." },
] };
const choices = { r0: 0.8, r1: 0.1, none: 0.1 };
const upstreamAnswer = { answers: { request_reply: { type: "choice", choice: "r0", confidence: 0.8, probabilities: choices } } };
function request(body: unknown = state) {
  return new Request("https://proxy.example/v1/question-locator", { method: "POST",
    headers: { "Content-Type": "application/json", "CF-Connecting-IP": "192.0.2.1" }, body: JSON.stringify(body) });
}
function environment() {
  return { TYPESAFE_API_KEY: "private-test-key", TURN_RATE_LIMITER: { async limit() { return { success: true }; } } };
}
function upstream(t: TestContext, handler: typeof fetch) {
  const mocked = t.mock.method(globalThis, "fetch", handler);
  t.after(() => mocked.mock.restore());
}

test("question locator sends the shared exact question and returns the selected distribution", async t => {
  upstream(t, async (_url, options) => {
    const payload = JSON.parse(options!.body as string);
    assert.equal(payload.model, "jev-latest");
    assert.deepEqual(payload.state, state);
    assert.deepEqual(payload.questions, questionLocatorQuestions(state));
    assert.deepEqual(payload.questions.request_reply, { type: "choice", instructions: question.instructions,
      criteria: { r0: { what: "Reply 0 (replies[0]) contains the open request." },
        r1: { what: "Reply 1 (replies[1]) contains the open request." }, none: { what: question.none } } });
    return Response.json(upstreamAnswer);
  });
  const response = await proxy.fetch(request(), environment());
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), { choice: "r0", probabilities: choices, confidence: 0.8 });
});

test("question locator rejects each invalid evidence shape", async t => {
  const calls = t.mock.method(globalThis, "fetch", async () => Response.json(upstreamAnswer));
  const invalid = [null, {}, { ...state, extra: true }, { ...state, latest_user_message: "😀".repeat(1025) },
    { ...state, latest_user_message: 4 }, { ...state, replies: [] }, { ...state, replies: Array(33).fill(state.replies[0]) },
    { ...state, replies: [{ reply: 1, text: "wrong index" }, state.replies[1]] },
    { ...state, replies: [{ ...state.replies[0], extra: true }, state.replies[1]] },
    { ...state, replies: [{ reply: 0, text: "😀".repeat(753) }, state.replies[1]] },
  ];
  for (const value of invalid) assert.equal((await proxy.fetch(request(value), environment())).status, 400);
  assert.equal(calls.mock.callCount(), 0);
});

test("question locator rejects malformed upstream choices, distributions, and non-maximal choices", async t => {
  for (const malformed of [
    { ...upstreamAnswer, answers: { request_reply: { ...upstreamAnswer.answers.request_reply, choice: "r9" } } },
    { ...upstreamAnswer, answers: { request_reply: { ...upstreamAnswer.answers.request_reply, probabilities: { r0: 1 } } } },
    { ...upstreamAnswer, answers: { request_reply: { ...upstreamAnswer.answers.request_reply, probabilities: { r0: 0.1, r1: 0.8, none: 0.1 } } } },
  ]) {
    upstream(t, async () => Response.json(malformed));
    const response = await proxy.fetch(request(), environment());
    assert.equal(response.status, 502);
    assert.deepEqual(await response.json(), { error: "invalid_upstream_response" });
  }
});
