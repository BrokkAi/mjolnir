import assert from "node:assert/strict";
import { test } from "node:test";
import proxy from "../src/index.ts";
import questions from "../../../mj-core/src/activity/handback_questions.json" with { type: "json" };
import state from "./fixtures/handback-worker.json" with { type: "json" };

let allowed = true;
const limited: string[] = [];
const environment = { FASTINO_API_KEY: "fastino-test-key", TYPESAFE_API_KEY: "",
  TURN_RATE_LIMITER: { async limit({ key }: { key: string }) { limited.push(key); return { success: allowed }; } } };
const scores = { unfinished: 0.5, needs_parent_input: 0.1, blocked: 0.1, permitted_stop: 0.1, finished: 0.1, unclear: 0.1 };
function request(body: unknown = state) {
  return new Request("https://proxy.example/v1/handback-verdict", {
    method: "POST", headers: { "Content-Type": "application/json", "CF-Connecting-IP": "192.0.2.7" },
    body: typeof body === "string" ? body : JSON.stringify(body),
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
// Hard-won: e44bee684: the paid GLiDE route ran before the proxy's IP rate limit and body bound
test("the paid route is rate limited per IP and body bounded before any provider call", async t => {
  const mock = t.mock.method(globalThis, "fetch", async () => { throw Error("provider must not be called"); });
  allowed = false;
  limited.length = 0;
  const refused = await proxy.fetch(request(), environment);
  assert.equal(refused.status, 429);
  assert.deepEqual(limited, ["192.0.2.7"]);
  allowed = true;
  const oversized = await proxy.fetch(request(JSON.stringify({ ...state, transcript_summary: "x".repeat(257 * 1024) })), environment);
  assert.equal(oversized.status, 413);
  assert.equal(mock.mock.callCount(), 0);
  mock.mock.restore();
  t.mock.method(globalThis, "fetch", async () => Response.json({ answers: { handback: { probabilities: scores } } }));
  assert.equal((await proxy.fetch(request({ ...state, transcript_summary: "x".repeat(48 * 1024), authorization: { ...state.authorization, messages: [{ id: "u", role: "user", text: "x".repeat(32 * 1024) }, { id: "a", role: "assistant", text: "x".repeat(16 * 1024) }] } }), environment)).status, 200);
});

// Hard-won: dc4c34617: a client-side byte cap made long turns permanently unclassifiable
test("GLiDE forwards a 200 KiB authorization reply without legacy field caps", async t => {
  const large = { ...state, harness: "a".repeat(100), user_prompt_tail: "x".repeat(2048),
    assistant_text_tail: "x".repeat(4096), authorization: { ...state.authorization,
    messages: [{ id: "u", role: "user", text: "Complete task" },
      { id: "a".repeat(512), role: "assistant", text: "synthetic reply ".repeat(14000) }] } };
  const mock = t.mock.method(globalThis, "fetch", async (url, options) => {
    assert.equal(url, "https://api.fastino.ai/v1/systemone");
    assert.deepEqual(JSON.parse(options!.body as string).state, large);
    return Response.json({ answers: { handback: { probabilities: scores } } });
  });
  assert.equal((await proxy.fetch(request(large), environment)).status, 200);
  assert.equal(mock.mock.callCount(), 1);
  assert.equal((await proxy.fetch(request({ ...large, authorization: {
    ...large.authorization, messages: [{ id: "bad", role: "assistant", text: 123 }]
  } }), environment)).status, 400);
  assert.equal(mock.mock.callCount(), 1);
});
