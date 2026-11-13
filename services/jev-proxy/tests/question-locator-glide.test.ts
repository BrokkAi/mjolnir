import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import proxy from "../src/index.ts";
import frozen from "../../../mj-core/src/activity/question_locator_glide_example_n3.json" with { type: "json" };

const state = { latest_user_message: "Please proceed", replies: [
  { reply: 0, text: "Should I make this change?" }, { reply: 1, text: "Waiting for your answer." },
  { reply: 2, text: "Done." },
] };
const scores = { r0: 0.7, r1: 0.1, r2: 0.1, none: 0.1 };
let allowed = true;
const limited: string[] = [];
const environment = { FASTINO_API_KEY: "fastino-test-key", TYPESAFE_API_KEY: "",
  TURN_RATE_LIMITER: { async limit({ key }: { key: string }) { limited.push(key); return { success: allowed }; } } };
function request(body: unknown = state) {
  return new Request("https://proxy.example/v1/question-locator-glide", { method: "POST",
    headers: { "Content-Type": "application/json", "CF-Connecting-IP": "192.0.2.7" }, body: JSON.stringify(body) });
}
function upstream(t: TestContext, handler: typeof fetch) {
  const mocked = t.mock.method(globalThis, "fetch", handler);
  t.after(() => mocked.mock.restore());
}

test("locator GLiDE sends the frozen V0 question with plain-string criteria and returns probabilities", async t => {
  upstream(t, async (url, options) => {
    assert.equal(url, "https://api.fastino.ai/v1/systemone");
    assert.deepEqual(options!.headers, { "Content-Type": "application/json", "X-API-Key": "fastino-test-key" });
    assert.deepEqual(JSON.parse(options!.body as string),
      { model: "fastino/GLiDE", state, questions: frozen, store: false });
    return Response.json({ answers: { request_reply: { type: "choice", choice: "r0", confidence: 0.7, probabilities: scores } } });
  });
  const response = await proxy.fetch(request(), environment);
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), { probabilities: scores });
});

test("locator GLiDE rejects each invalid evidence shape before any provider call", async t => {
  const calls = t.mock.method(globalThis, "fetch", async () => { throw Error("provider must not be called"); });
  const invalid = [null, {}, { ...state, extra: true }, { ...state, latest_user_message: "😀".repeat(1025) },
    { ...state, latest_user_message: 4 }, { ...state, replies: [] }, { ...state, replies: Array(33).fill(state.replies[0]) },
    { ...state, replies: [{ reply: 1, text: "wrong index" }, state.replies[1]] },
    { ...state, replies: [{ reply: 0, text: "😀".repeat(753) }, state.replies[1]] }];
  for (const value of invalid) assert.equal((await proxy.fetch(request(value), environment)).status, 400);
  assert.equal(calls.mock.callCount(), 0);
});

test("locator GLiDE errors and backpressure pass through once with Retry-After", async t => {
  for (const status of [413, 425, 429, 503, 401]) {
    const mock = t.mock.method(globalThis, "fetch", async () => new Response("provider error", { status, headers: { "Retry-After": "12" } }));
    const response = await proxy.fetch(request(), environment);
    assert.equal(response.status, status);
    assert.equal(response.headers.get("Retry-After"), "12");
    assert.equal(await response.text(), "provider error");
    assert.equal(mock.mock.callCount(), 1);
    mock.mock.restore();
  }
});

test("locator GLiDE rejects malformed and incomplete distributions", async t => {
  for (const probabilities of [{ r0: 1 }, { ...scores, r1: -1 }, { ...scores, r9: 0 }, { ...scores, none: 0.9 },
    { r0: 0.7, r1: 0.1, r2: 0.1, none: "0.1" }, { ...scores, none: undefined }]) {
    upstream(t, async () => Response.json({ answers: { request_reply: { probabilities } } }));
    assert.equal((await proxy.fetch(request(), environment)).status, 502);
  }
  upstream(t, async () => Response.json({ answers: {} }));
  assert.equal((await proxy.fetch(request(), environment)).status, 502);
});

test("locator GLiDE is rate limited per IP, keyed, and bounded before any provider call", async t => {
  const calls = t.mock.method(globalThis, "fetch", async () => { throw Error("provider must not be called"); });
  allowed = false;
  limited.length = 0;
  assert.equal((await proxy.fetch(request(), environment)).status, 429);
  assert.deepEqual(limited, ["192.0.2.7"]);
  allowed = true;
  assert.equal((await proxy.fetch(request(), { ...environment, FASTINO_API_KEY: "" })).status, 503);
  const oversized = await proxy.fetch(request(JSON.stringify({ ...state, extra: "x".repeat(257 * 1024) })), environment);
  assert.equal(oversized.status, 413);
  assert.equal(calls.mock.callCount(), 0);
});
