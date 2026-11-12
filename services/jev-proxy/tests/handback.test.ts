import assert from "node:assert/strict";
import { test } from "node:test";
import proxy from "../src/index.ts";
import questions from "../../../mj-core/src/activity/handback_questions.json" with { type: "json" };
import state from "./fixtures/handback-worker.json" with { type: "json" };

const environment = { FASTINO_API_KEY: "fastino-test-key", TYPESAFE_API_KEY: "",
  TURN_RATE_LIMITER: { async limit(): Promise<never> { throw Error("must not rate limit GLiDE"); } } };
const scores = { unfinished: 0.5, needs_parent_input: 0.1, blocked: 0.1, permitted_stop: 0.1, finished: 0.1, unclear: 0.1 };
function request(body: unknown = state) {
  return new Request("https://proxy.example/v1/handback-verdict", {
    method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body),
  });
}
test("worker evidence builds the exact GLiDE request with server-owned questions", async t => {
  t.mock.method(globalThis, "fetch", async (url, options) => {
    assert.equal(url, "https://api.fastino.ai/v1/systemone");
    assert.deepEqual(options!.headers, { "Content-Type": "application/json", "X-API-Key": "fastino-test-key" });
    const body = JSON.parse(options!.body as string);
    assert.deepEqual(body, { model: "fastino/GLiDE", state, questions, store: false });
    console.log("HANDBACK_FASTINO_BODY=" + JSON.stringify(body));
    return Response.json({ answers: { handback: { type: "choice", choice: "unfinished", confidence: 0.4, probabilities: scores } } });
  });
  const response = await proxy.fetch(request(), environment);
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), { probabilities: scores });
});
test("GLiDE errors and backpressure pass through once with Retry-After", async t => {
  for (const status of [425, 429, 503, 401]) {
    const mock = t.mock.method(globalThis, "fetch", async () => new Response("provider error", { status, headers: { "Retry-After": "12" } }));
    const response = await proxy.fetch(request(), environment);
    assert.equal(response.status, status);
    assert.equal(response.headers.get("Retry-After"), "12");
    assert.equal(await response.text(), "provider error");
    assert.equal(mock.mock.callCount(), 1);
    mock.mock.restore();
  }
});
test("malformed and incomplete distributions fail visibly", async t => {
  for (const probabilities of [{ unfinished: 1 }, { ...scores, blocked: -1 }, { ...scores, extra: 0 }, { ...scores, finished: 0.9 }]) {
    const mock = t.mock.method(globalThis, "fetch", async () => Response.json({ answers: { handback: { probabilities } } }));
    assert.equal((await proxy.fetch(request(), environment)).status, 502);
    mock.mock.restore();
  }
  assert.equal((await proxy.fetch(request({ model: "client-model", state, questions }), environment)).status, 400);
});
