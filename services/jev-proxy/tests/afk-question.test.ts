import assert from "node:assert/strict";
import { test } from "node:test";
import proxy from "../src/index.ts";
import questions from "../../../mj-worker/src/acp/afk_question.json" with { type: "json" };

const state = {
  current_question: "Who owns #11482 and who owns #10941?",
  previous_questions: ["Who should own #10941?"],
};
const environment = { TYPESAFE_API_KEY: "private-test-key",
  TURN_RATE_LIMITER: { async limit() { return { success: true }; } } };
function request() {
  return new Request("https://proxy.example/v1/afk-question-verdict", { method: "POST",
    headers: { "Content-Type": "application/json", "CF-Connecting-IP": "192.0.2.1" },
    body: JSON.stringify(state) });
}

// Hard-won: #1489: a mixed new/repeated AFK form was mistaken for a retry.
test("AFK route preserves whole-question evidence and the shared classifier contract", async t => {
  const answer = { type: "choice", choice: "different", confidence: 0.97,
    probabilities: { same: 0.03, different: 0.97 } };
  t.mock.method(globalThis, "fetch", async (_url, options) => {
    assert.deepEqual(JSON.parse(options!.body as string), { model: "jev-latest", state, questions });
    return Response.json({ answers: { same_question: answer } });
  });
  const response = await proxy.fetch(request(), environment);
  assert.equal(response.status, 200);
  const { type: _type, ...expected } = answer;
  assert.deepEqual(await response.json(), expected);
});

test("AFK route reports unavailable or malformed classifier answers for worker fallback", async t => {
  for (const response of [new Response("unavailable", { status: 503 }),
    Response.json({ answers: { same_question: { type: "choice", choice: "different", confidence: 0.8,
      probabilities: { same: 0.8, different: 0.2 } } } }),
    Response.json({ answers: { same_question: { type: "choice", choice: "different", confidence: 0.8,
      probabilities: { same: 0.1, different: 0.2 } } } }),
  ]) {
    const mock = t.mock.method(globalThis, "fetch", async () => response);
    assert.equal((await proxy.fetch(request(), environment)).status, 502);
    mock.mock.restore();
  }
});
