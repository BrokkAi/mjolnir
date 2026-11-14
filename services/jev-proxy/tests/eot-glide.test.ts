import assert from "node:assert/strict";
import { test } from "node:test";
import proxy from "../src/index.ts";
import questions from "../../../mj-core/src/activity/eot_glide_questions.json" with { type: "json" };
// Captured by relay::verdict::tests::glide_worker_projection_capture_and_large_history_bound.
import state from "./fixtures/eot-glide-worker.json" with { type: "json" };

const env = { TYPESAFE_API_KEY: "", FASTINO_API_KEY: "test-key",
  TURN_RATE_LIMITER: { async limit() { return { success: true }; } } };
const answers = Object.fromEntries(Object.entries(questions).map(([axis, question]) =>
  [axis, { probabilities: Object.fromEntries(Object.keys(question.criteria).map((key, i) => [key, i === 0 ? 1 : 0])) }]));
function request(body: unknown = state) {
  return new Request("https://proxy.example/v1/glide-turn-verdict", { method: "POST",
    headers: { "Content-Type": "application/json", "CF-Connecting-IP": "192.0.2.7" }, body: JSON.stringify(body) });
}
test("EOT GLiDE sends bundled frozen questions and validates complete distributions", async t => {
  t.mock.method(globalThis, "fetch", async (url, options) => {
    assert.equal(url, "https://api.fastino.ai/v1/systemone");
    assert.deepEqual(JSON.parse(options!.body as string), { model: "fastino/GLiDE", state, questions, store: false });
    return Response.json({ answers });
  });
  const response = await proxy.fetch(request(), env);
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), { answers });
});
test("EOT rejects incomplete provider distributions", async t => {
  const { attention, ...legacy } = answers;
  assert.ok(attention);
  t.mock.method(globalThis, "fetch", async () => Response.json({ answers: legacy }));
  assert.equal((await proxy.fetch(request(), env)).status, 502);
});
test("EOT provider backpressure preserves status and Retry-After", async t => {
  const mock = t.mock.method(globalThis, "fetch", async () => new Response("busy", { status: 429, headers: { "Retry-After": "17" } }));
  const response = await proxy.fetch(request(), env);
  assert.equal(response.status, 429);
  assert.equal(response.headers.get("Retry-After"), "17");
  assert.equal(await response.text(), "busy");
  assert.equal(mock.mock.callCount(), 1);
});
test("EOT applies admission and the GLiDE-derived body bound before provider work", async t => {
  const mock = t.mock.method(globalThis, "fetch", async () => Response.json({ answers }));
  assert.equal((await proxy.fetch(request(), { ...env, TURN_RATE_LIMITER: { async limit() { return { success: false }; } } })).status, 429);
  assert.equal((await proxy.fetch(request({ ...state, latest_user_message: "x".repeat(257 * 1024) }), env)).status, 413);
  assert.equal(mock.mock.callCount(), 0);
  assert.equal((await proxy.fetch(request({ ...state, latest_user_message: "x".repeat(81 * 1024) }), env)).status, 200);
  assert.equal((await proxy.fetch(request({ model: "override", state, questions }), env)).status, 400);
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
    return Response.json({ answers });
  });
  assert.equal((await proxy.fetch(request(large), env)).status, 200);
  assert.equal(mock.mock.callCount(), 1);
  assert.equal((await proxy.fetch(request({ ...large, authorization: {
    ...large.authorization, messages: [{ id: "bad", role: "assistant", text: 123 }]
  } }), env)).status, 400);
  assert.equal(mock.mock.callCount(), 1);
});
